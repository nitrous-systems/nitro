//! End to end: spawn the `nitro-gpu-vulkan` helper on a socketpair, feed it
//! layer lists, and check the output pixels by readback.
//!
//! Without a render node or a usable vendor ICD (the GPU-less dev
//! machine) every test **skips with a note**; `NITRO_GPU_TEST=require`
//! turns the skip into a failure (`just box-gpu-test` sets it). NV12
//! cases need `/dev/udmabuf` access to build a dma-buf from a memfd; they
//! skip that part with a note when it is missing.

#![allow(clippy::many_single_char_names, clippy::too_many_lines)] // x, y, w, h; scripted cases

use std::os::fd::OwnedFd;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nitro_core::IRect;
use nitro_gpu::client::fence_signalled;
use nitro_gpu::proto::{
    AR24, Blend, ColorEncoding, ColorRange, Composite, DeviceInfo, DmabufDesc, ErrorCode, Layer,
    MOD_I915_X_TILED, MOD_LINEAR, NV12, PROTO_VERSION, PlaneDesc, ShadowDesc, ShadowPath, XR24,
};
use nitro_gpu::{Conn, FromHelper, ToHelper};

const T: Duration = Duration::from_secs(10);

struct Helper {
    conn: Conn,
    child: Child,
    info: DeviceInfo,
    startup: Duration,
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.conn.send(&ToHelper::Shutdown, vec![]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
    }
}

fn required() -> bool {
    std::env::var("NITRO_GPU_TEST").is_ok_and(|v| v == "require")
}

fn skip(why: &str) -> Option<Helper> {
    assert!(!required(), "NITRO_GPU_TEST=require: {why}");
    eprintln!("SKIP (no GPU): {why}");
    None
}

/// Start a helper, or `None` (skip) when this machine cannot run one.
fn helper(env: &[(&str, &str)]) -> Option<Helper> {
    let has_node = std::fs::read_dir("/dev/dri").is_ok_and(|d| {
        d.flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
    });
    if !has_node {
        return skip("no /dev/dri/renderD*");
    }
    let (ours, theirs) = nitro_wire::io::pair().unwrap();
    let t0 = Instant::now();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nitro-gpu-vulkan"));
    cmd.stdin(Stdio::from(theirs.into_fd()));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().unwrap();
    let mut conn = Conn::new(ours);
    match conn.call(
        &ToHelper::Hello {
            version: PROTO_VERSION,
        },
        vec![],
        T,
    ) {
        Ok((FromHelper::HelloReply { info, .. }, _)) => {
            let startup = t0.elapsed();
            eprintln!(
                "helper: {} ({}) up in {startup:?}; {} sampleable, {} render format/modifier pairs",
                info.device,
                info.driver,
                info.sampleable.len(),
                info.render.len()
            );
            Some(Helper {
                conn,
                child,
                info,
                startup,
            })
        }
        Ok((other, _)) => panic!("unexpected {other:?}"),
        Err(e) => {
            let mut child = child;
            let _ = child.kill();
            skip(&format!("helper did not start ({e}); see its stderr above"))
        }
    }
}

fn page_padded(len: usize) -> u64 {
    let page = 4096;
    len.div_ceil(page) as u64 * page as u64
}

/// A sealed memfd with `bytes`, padded to whole pages (udmabuf wants that).
fn memfd(bytes: &[u8]) -> OwnedFd {
    let fd = nitro_shm::create_sealed("pixels", page_padded(bytes.len())).unwrap();
    write_at(&fd, bytes, 0);
    fd
}

fn write_at(fd: &OwnedFd, bytes: &[u8], mut off: u64) {
    let mut b = bytes;
    while !b.is_empty() {
        let n = rustix::io::pwrite(fd, b, off).unwrap();
        b = &b[n..];
        off += n as u64;
    }
}

/// BGRA bytes of an RGBA colour.
fn bgra(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    [b, g, r, a]
}

fn fill(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&f(x, y));
        }
    }
    v
}

impl Helper {
    fn call(&mut self, m: &ToHelper, fds: Vec<OwnedFd>) -> (FromHelper, Vec<OwnedFd>) {
        self.conn.call(m, fds, T).unwrap()
    }

    fn shadow(&mut self, id: u32, w: u32, h: u32, fourcc: u32, px: &[u8]) -> OwnedFd {
        let fd = memfd(px);
        let dup = rustix::io::fcntl_dupfd_cloexec(&fd, 0).unwrap();
        let d = ShadowDesc {
            id,
            w,
            h,
            stride: w * 4,
            fourcc,
        };
        let (r, _) = self.call(&ToHelper::ImportShadow(d), vec![fd]);
        assert_eq!(r, FromHelper::Imported { id });
        dup
    }

    fn ring(&mut self, n: u32, w: u32, h: u32, modifiers: Vec<u64>) -> u64 {
        let m = ToHelper::AllocOutputRing {
            n,
            w,
            h,
            fourcc: XR24,
            modifiers,
        };
        match self.call(&m, vec![]) {
            (
                FromHelper::OutputRing {
                    modifier, slots, ..
                },
                fds,
            ) => {
                assert_eq!(slots.len(), n as usize);
                assert_eq!(fds.len(), n as usize);
                modifier
            }
            other => panic!("ring: {other:?}"),
        }
    }

    fn frame(&mut self, serial: u64, slot: u32, damage: Vec<IRect>, layers: Vec<Layer>) -> OwnedFd {
        let c = Composite {
            serial,
            out_idx: slot,
            damage,
            layers,
            fence_mask: 0,
        };
        match self.call(&ToHelper::Composite(c), vec![]) {
            (FromHelper::Composited { serial: s }, mut fds) if s == serial => fds.pop().unwrap(),
            other => panic!("frame {serial}: {other:?}"),
        }
    }

    fn readback(&mut self, slot: u32) -> Image {
        match self.call(&ToHelper::ReadBack { out_idx: slot }, vec![]) {
            (FromHelper::ReadBackReply { w, h, stride, .. }, mut fds) => {
                let len = stride as usize * h as usize;
                let map = nitro_shm::Mapping::map(fds.pop().unwrap(), len).unwrap();
                Image {
                    px: map.as_bytes().to_vec(),
                    w,
                    stride,
                }
            }
            other => panic!("readback: {other:?}"),
        }
    }

    /// NV12 `w × h` of one colour as a LINEAR dma-buf via udmabuf, or
    /// `None` with a note if this box cannot make one.
    fn nv12(&mut self, id: u32, w: u32, h: u32, yuv: (u8, u8, u8)) -> Option<()> {
        let lin = self
            .info
            .sampleable
            .iter()
            .any(|f| f.fourcc == NV12 && f.modifier == MOD_LINEAR);
        if !lin {
            eprintln!(
                "NOTE: LINEAR NV12 not sampleable on {}; NV12 part skipped",
                self.info.driver
            );
            return None;
        }
        let (ysz, csz) = ((w * h) as usize, (w * h / 2) as usize);
        let mut bytes = vec![yuv.0; ysz];
        for _ in 0..csz / 2 {
            bytes.push(yuv.1);
            bytes.push(yuv.2);
        }
        let fd = memfd(&bytes);
        let buf = match nitro_gpu_vulkan::sys::udmabuf(&fd, 0, page_padded(bytes.len())) {
            Ok(b) => b,
            Err(e) => {
                assert!(
                    !(required()
                        && std::env::var("NITRO_GPU_TEST_NV12").is_ok_and(|v| v == "require")),
                    "udmabuf: {e}"
                );
                eprintln!("NOTE: /dev/udmabuf unavailable ({e}); NV12 part skipped");
                return None;
            }
        };
        let d = DmabufDesc {
            id,
            w,
            h,
            fourcc: NV12,
            modifier: MOD_LINEAR,
            planes: vec![
                PlaneDesc {
                    offset: 0,
                    pitch: w,
                },
                PlaneDesc {
                    offset: w * h,
                    pitch: w,
                },
            ],
            encoding: ColorEncoding::Bt709,
            range: ColorRange::Limited,
        };
        let dup = rustix::io::fcntl_dupfd_cloexec(&buf, 0).unwrap();
        let (r, _) = self.call(&ToHelper::ImportDmabuf(d), vec![buf, dup]);
        assert_eq!(r, FromHelper::Imported { id });
        Some(())
    }
}

impl Helper {
    /// Premultiplied AR24 `w × h` from `px` as a LINEAR dma-buf via
    /// udmabuf (#3952), or `None` with a note if this box cannot.
    fn ar24(&mut self, id: u32, w: u32, h: u32, px: &[u8]) -> Option<()> {
        let fd = memfd(px);
        let buf = match nitro_gpu_vulkan::sys::udmabuf(&fd, 0, page_padded(px.len())) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("NOTE: /dev/udmabuf unavailable ({e}); AR24 part skipped");
                return None;
            }
        };
        let d = DmabufDesc {
            id,
            w,
            h,
            fourcc: AR24,
            modifier: MOD_LINEAR,
            planes: vec![PlaneDesc {
                offset: 0,
                pitch: w * 4,
            }],
            ..DmabufDesc::default()
        };
        let (r, _) = self.call(&ToHelper::ImportDmabuf(d), vec![buf]);
        assert_eq!(r, FromHelper::Imported { id });
        Some(())
    }
}

struct Image {
    px: Vec<u8>,
    w: u32,
    stride: u32,
}

impl Image {
    /// RGB at (x, y).
    fn rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let o = (y * self.stride + x * 4) as usize;
        [self.px[o + 2], self.px[o + 1], self.px[o]]
    }

    #[track_caller]
    fn assert_near(&self, x: u32, y: u32, want: [u8; 3], tol: u8) {
        let got = self.rgb(x, y);
        let ok = got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= tol);
        assert!(
            ok,
            "pixel ({x},{y}) of {}-wide output: got {got:?}, want {want:?} ±{tol}",
            self.w
        );
    }
}

fn layer(tex: u32, tw: u32, th: u32, dst: IRect, blend: Blend) -> Layer {
    Layer {
        tex,
        src: [0.0, 0.0, tw as f32, th as f32],
        dst,
        blend,
    }
}

fn full(w: u32, h: u32) -> IRect {
    IRect::new(0, 0, w.cast_signed(), h.cast_signed())
}

const NV12_RGB: [u8; 3] = [188, 119, 71]; // Y=128 U=100 V=160, BT.709 narrow (#3903)

fn shadow_only(env: &[(&str, &str)], want_path: ShadowPath) {
    let Some(mut h) = helper(env) else { return };
    let (w, ht) = (96, 64);
    let px = fill(w, ht, |x, y| bgra((x * 2) as u8, (y * 3) as u8, 200, 255));
    h.shadow(1, w, ht, XR24, &px);
    h.ring(2, w, ht, vec![MOD_LINEAR]);
    let f = h.frame(
        1,
        0,
        vec![full(w, ht)],
        vec![layer(1, w, ht, full(w, ht), Blend::Opaque)],
    );
    assert!(fence_signalled(&f, T), "sync_file never signalled");
    let img = h.readback(0);
    for (x, y) in [(0, 0), (95, 0), (0, 63), (95, 63), (40, 21)] {
        img.assert_near(x, y, [(x * 2) as u8, (y * 3) as u8, 200], 0);
    }
    match h.call(&ToHelper::GetStats, vec![]).0 {
        FromHelper::Stats(s) => {
            eprintln!("shadow path: {:?}", s.shadow_path);
            if want_path != ShadowPath::Unknown {
                assert_eq!(s.shadow_path, want_path);
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn case1_shadow_only_matches_the_shadow() {
    shadow_only(&[], ShadowPath::Unknown);
}

#[test]
fn case1_shadow_only_staging_path() {
    shadow_only(&[("NITRO_GPU_SHADOW", "staging")], ShadowPath::Staging);
}

#[test]
fn case2_hole_shows_nv12_under_the_shadow() {
    let Some(mut h) = helper(&[]) else { return };
    let (w, ht) = (128, 96);
    // Premultiplied AR24 shadow: grey, with a transparent hole
    // [32,32)-[96,80) and one 50 % red pixel inside it at (40, 40).
    let px = fill(w, ht, |x, y| {
        if (x, y) == (40, 40) {
            bgra(128, 0, 0, 128)
        } else if (32..96).contains(&x) && (32..80).contains(&y) {
            [0, 0, 0, 0]
        } else {
            bgra(60, 60, 60, 255)
        }
    });
    h.shadow(1, w, ht, AR24, &px);
    h.ring(2, w, ht, vec![MOD_LINEAR]);
    let Some(()) = h.nv12(2, 64, 64, (128, 100, 160)) else {
        return;
    };
    let hole = IRect::new(32, 24, 64, 64);
    let f = h.frame(
        1,
        0,
        vec![full(w, ht)],
        vec![
            layer(2, 64, 64, hole, Blend::Opaque),
            layer(1, w, ht, full(w, ht), Blend::PremulOver),
        ],
    );
    assert!(fence_signalled(&f, T));
    let img = h.readback(0);
    img.assert_near(64, 60, NV12_RGB, 2);
    img.assert_near(5, 5, [60, 60, 60], 0);
    img.assert_near(64, 28, [60, 60, 60], 0); // NV12 layer covered by opaque shadow
    let blended = [128 + NV12_RGB[0] / 2, NV12_RGB[1] / 2, NV12_RGB[2] / 2];
    img.assert_near(40, 40, blended, 3);
}

/// A translucent client window (#3952): its opaque body under the
/// shadow's hole, its premultiplied ring blended over the shadow.
#[test]
fn case2b_premultiplied_ar24_over_the_shadow() {
    let Some(mut h) = helper(&[]) else { return };
    let (w, ht) = (128, 96);
    // Shadow: blue, a transparent hole where the body goes.
    let body = IRect::new(40, 40, 48, 32);
    let px = fill(w, ht, |x, y| {
        if body.contains(x.cast_signed(), y.cast_signed()) {
            [0, 0, 0, 0]
        } else {
            bgra(0, 0, 200, 255)
        }
    });
    h.shadow(1, w, ht, AR24, &px);
    h.ring(2, w, ht, vec![MOD_LINEAR]);
    // The window, 64×48 at (32, 32): an opaque green body inset by 8,
    // a 50 % premultiplied red ring.
    let (cw, ch) = (64, 48);
    let client = fill(cw, ch, |x, y| {
        if (8..56).contains(&x) && (8..40).contains(&y) {
            bgra(0, 180, 0, 255)
        } else {
            bgra(100, 0, 0, 128)
        }
    });
    let Some(()) = h.ar24(2, cw, ch, &client) else {
        return;
    };
    let at = |r: IRect| Layer {
        tex: 2,
        src: [(r.x - 32) as f32, (r.y - 32) as f32, r.w as f32, r.h as f32],
        dst: r,
        blend: Blend::Opaque,
    };
    let ring = IRect::new(32, 32, 64, 8); // the top band of the ring
    let f = h.frame(
        1,
        0,
        vec![full(w, ht)],
        vec![
            at(body),
            layer(1, w, ht, full(w, ht), Blend::PremulOver),
            Layer {
                blend: Blend::PremulOver,
                ..at(ring)
            },
        ],
    );
    assert!(fence_signalled(&f, T));
    let img = h.readback(0);
    img.assert_near(60, 50, [0, 180, 0], 0); // the body, stored
    img.assert_near(5, 5, [0, 0, 200], 0); // the shadow elsewhere
    // The ring: 100 + (1 - 128/255) × the shadow's blue.
    img.assert_near(60, 34, [100, 0, 100], 2);
}

#[test]
fn case3_scaling_keeps_orientation() {
    let Some(mut h) = helper(&[]) else { return };
    // 64×64: top half left red / right blue, bottom half green.
    let px = fill(64, 64, |x, y| {
        if y >= 32 {
            bgra(0, 255, 0, 255)
        } else if x < 32 {
            bgra(255, 0, 0, 255)
        } else {
            bgra(0, 0, 255, 255)
        }
    });
    h.shadow(1, 64, 64, XR24, &px);
    h.ring(1, 200, 120, vec![MOD_LINEAR]);
    let dst = IRect::new(20, 10, 160, 96);
    let f = h.frame(
        1,
        0,
        vec![full(200, 120)],
        vec![layer(1, 64, 64, dst, Blend::Opaque)],
    );
    assert!(fence_signalled(&f, T));
    let img = h.readback(0);
    img.assert_near(30, 20, [255, 0, 0], 0);
    img.assert_near(170, 20, [0, 0, 255], 0);
    img.assert_near(100, 100, [0, 255, 0], 0);
    img.assert_near(5, 5, [0, 0, 0], 0); // outside dst: the fresh slot's clear
    img.assert_near(190, 115, [0, 0, 0], 0);
}

#[test]
fn case4_damage_clip_and_ring_age() {
    let Some(mut h) = helper(&[]) else { return };
    let (w, ht) = (64u32, 64u32);
    let grey = bgra(80, 80, 80, 255);
    let shadow = h.shadow(1, w, ht, XR24, &fill(w, ht, |_, _| grey));
    h.ring(2, w, ht, vec![MOD_LINEAR]);
    let paint = |fd: &OwnedFd, r: IRect, c: [u8; 4]| {
        for y in r.y..r.bottom() {
            let row: Vec<u8> = (0..r.w).flat_map(|_| c).collect();
            write_at(fd, &row, (y as u64 * u64::from(w) + r.x as u64) * 4);
        }
    };
    let l = vec![layer(1, w, ht, full(w, ht), Blend::Opaque)];
    let up = |h: &mut Helper, r: IRect| {
        h.conn
            .send(
                &ToHelper::UploadDamage {
                    id: 1,
                    rects: vec![r],
                },
                vec![],
            )
            .unwrap();
    };
    let f1 = h.frame(1, 0, vec![full(w, ht)], l.clone());
    assert!(fence_signalled(&f1, T));
    let r2 = IRect::new(0, 0, 16, 16);
    paint(&shadow, r2, bgra(255, 0, 0, 255));
    up(&mut h, r2);
    let f2 = h.frame(2, 1, vec![r2], l.clone());
    assert!(fence_signalled(&f2, T));
    let r3 = IRect::new(40, 40, 8, 8);
    paint(&shadow, r3, bgra(0, 0, 255, 255));
    up(&mut h, r3);
    // Changed but never reported: must stay grey in slot 0.
    let r4 = IRect::new(0, 48, 8, 8);
    paint(&shadow, r4, bgra(0, 255, 0, 255));
    up(&mut h, r4);
    let f3 = h.frame(3, 0, vec![r3], l);
    assert!(fence_signalled(&f3, T));
    let img = h.readback(0);
    img.assert_near(8, 8, [255, 0, 0], 0); // frame 2's damage, carried by age
    img.assert_near(44, 44, [0, 0, 255], 0); // frame 3's own damage
    img.assert_near(4, 52, [80, 80, 80], 0); // outside the clip: unchanged
    img.assert_near(30, 30, [80, 80, 80], 0);
}

#[test]
fn case5_sync_file_signals_and_case8_modifier_choice() {
    let Some(mut h) = helper(&[]) else { return };
    h.shadow(1, 32, 32, XR24, &fill(32, 32, |_, _| bgra(1, 2, 3, 255)));
    let want = [MOD_I915_X_TILED, MOD_LINEAR];
    let m = h.ring(2, 256, 128, want.to_vec());
    eprintln!("ring modifier {m:#x}");
    assert!(want.contains(&m));
    let t0 = Instant::now();
    let f = h.frame(
        1,
        0,
        vec![],
        vec![layer(1, 32, 32, IRect::new(0, 0, 32, 32), Blend::Opaque)],
    );
    let reply = t0.elapsed();
    assert!(fence_signalled(&f, T), "sync_file never became POLLIN");
    eprintln!(
        "Composited reply after {reply:?}, signalled after {:?}",
        t0.elapsed()
    );
}

#[test]
fn case6_release_of_an_in_flight_texture_waits() {
    let Some(mut h) = helper(&[]) else { return };
    let (w, ht) = (1920, 1080);
    h.shadow(
        1,
        w,
        ht,
        XR24,
        &fill(w, ht, |x, _| bgra(x as u8, 0, 0, 255)),
    );
    h.ring(1, w, ht, vec![MOD_LINEAR]);
    let f = h.frame(
        1,
        0,
        vec![full(w, ht)],
        vec![layer(1, w, ht, full(w, ht), Blend::Opaque)],
    );
    let (r, _) = h.call(&ToHelper::Release { id: 1 }, vec![]);
    assert_eq!(r, FromHelper::Released { id: 1 });
    assert!(
        fence_signalled(&f, Duration::ZERO),
        "Released before the frame signalled"
    );
}

#[test]
fn case7_errors_do_not_kill_the_helper() {
    let Some(mut h) = helper(&[]) else { return };
    h.shadow(1, 16, 16, XR24, &fill(16, 16, |_, _| bgra(9, 9, 9, 255)));
    h.ring(1, 16, 16, vec![MOD_LINEAR]);
    let bad_id = Composite {
        serial: 1,
        layers: vec![layer(7, 16, 16, full(16, 16), Blend::Opaque)],
        ..Composite::default()
    };
    match h.call(&ToHelper::Composite(bad_id), vec![]).0 {
        FromHelper::Error { code, .. } => assert_eq!(code, ErrorCode::BadId),
        other => panic!("{other:?}"),
    }
    let bad_rect = Composite {
        serial: 2,
        layers: vec![layer(1, 16, 16, IRect::new(8, 8, 16, 16), Blend::Opaque)],
        ..Composite::default()
    };
    match h.call(&ToHelper::Composite(bad_rect), vec![]).0 {
        FromHelper::Error { code, .. } => assert_eq!(code, ErrorCode::BadRect),
        other => panic!("{other:?}"),
    }
    let bad_mod = DmabufDesc {
        id: 3,
        w: 16,
        h: 16,
        fourcc: XR24,
        modifier: 0x00de_adbe_ef00,
        planes: vec![PlaneDesc {
            offset: 0,
            pitch: 64,
        }],
        ..DmabufDesc::default()
    };
    match h
        .call(&ToHelper::ImportDmabuf(bad_mod), vec![memfd(&[0; 1024])])
        .0
    {
        FromHelper::Error { code, .. } => assert_eq!(code, ErrorCode::BadFormat),
        other => panic!("{other:?}"),
    }
    let f = h.frame(
        4,
        0,
        vec![full(16, 16)],
        vec![layer(1, 16, 16, full(16, 16), Blend::Opaque)],
    );
    assert!(fence_signalled(&f, T));
    h.readback(0).assert_near(3, 3, [9, 9, 9], 0);
}

/// `cargo test -p nitro-gpu-vulkan -- --ignored --nocapture footprint`
#[test]
fn case9_capture_draws_layers_into_a_temporary_target_and_frees_it() {
    // #3962: a shot's composite. No ring: the target is made and freed
    // inside the call.
    let Some(mut h) = helper(&[]) else { return };
    let px = fill(40, 30, |_, _| bgra(20, 200, 40, 255));
    h.shadow(1, 40, 30, XR24, &px);
    let mut layers = vec![layer(1, 40, 30, IRect::new(0, 0, 50, 30), Blend::Opaque)];
    let nv12 = h.nv12(2, 64, 64, (128, 100, 160)).is_some();
    if nv12 {
        layers.push(layer(2, 64, 64, IRect::new(50, 20, 32, 32), Blend::Opaque));
    }
    // #3975: the first submit in the process makes a one-time step in
    // drm_total (driver state/instruction pools, pipeline upload: +16 KiB
    // on hasvk, ~4 MiB on anv) that does not grow with the target size.
    // Warm up with one small capture, then sample.
    capture(&mut h, 4, 100, 70, layers.clone());
    let before = drm_total(&mut h);
    let img = capture(&mut h, 5, 100, 70, layers.clone());
    img.assert_near(0, 0, [20, 200, 40], 0);
    img.assert_near(49, 29, [20, 200, 40], 0);
    if nv12 {
        img.assert_near(60, 30, NV12_RGB, 3);
        img.assert_near(81, 51, NV12_RGB, 3);
    }
    // A 1080p XR24 target is ~8 MiB: a leaked one cannot hide.
    let big = capture(&mut h, 6, 1920, 1080, layers);
    big.assert_near(0, 0, [20, 200, 40], 0);
    let after = drm_total(&mut h);
    eprintln!("capture: drm_total {before} -> {after}");
    assert!(after <= before, "the capture targets were freed");
}

fn drm_total(h: &mut Helper) -> u64 {
    match h.call(&ToHelper::GetStats, vec![]).0 {
        FromHelper::Stats(s) => s.drm_total,
        other => panic!("{other:?}"),
    }
}

fn capture(h: &mut Helper, serial: u64, w: u32, ht: u32, layers: Vec<Layer>) -> Image {
    match h.call(
        &ToHelper::Capture {
            serial,
            w,
            h: ht,
            layers,
        },
        vec![],
    ) {
        (
            FromHelper::Captured {
                serial: s,
                w: cw,
                h: ch,
                stride,
            },
            mut fds,
        ) => {
            assert_eq!((s, cw, ch), (serial, w, ht));
            let len = stride as usize * ch as usize;
            let map = nitro_shm::Mapping::map(fds.pop().unwrap(), len).unwrap();
            Image {
                px: map.as_bytes().to_vec(),
                w,
                stride,
            }
        }
        other => panic!("capture: {other:?}"),
    }
}

#[test]
#[ignore = "measurement; run on a GPU box"]
fn footprint() {
    let Some(mut h) = helper(&[]) else { return };
    let stats = |h: &mut Helper| match h.call(&ToHelper::GetStats, vec![]).0 {
        FromHelper::Stats(s) => s,
        other => panic!("{other:?}"),
    };
    let s0 = stats(&mut h);
    let t0 = Instant::now();
    h.ring(3, 1920, 1080, vec![MOD_I915_X_TILED, MOD_LINEAR]);
    let ring_t = t0.elapsed();
    let s1 = stats(&mut h);
    let (w, ht) = (1920, 1080);
    h.shadow(1, w, ht, AR24, &fill(w, ht, |_, _| bgra(10, 20, 30, 255)));
    let t0 = Instant::now();
    let f = h.frame(
        1,
        0,
        vec![full(w, ht)],
        vec![layer(1, w, ht, full(w, ht), Blend::PremulOver)],
    );
    let first = t0.elapsed();
    assert!(fence_signalled(&f, T));
    let first_done = t0.elapsed();
    let s2 = stats(&mut h);
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    eprintln!(
        "FOOTPRINT driver={} device={}",
        h.info.driver, h.info.device
    );
    eprintln!("FOOTPRINT startup(spawn→HelloReply)={:?}", h.startup);
    eprintln!(
        "FOOTPRINT idle-after-init rss={:.1}MiB pss={:.1}MiB drm_total={:.1}MiB drm_resident={:.1}MiB",
        mib(s0.rss),
        mib(s0.pss),
        mib(s0.drm_total),
        mib(s0.drm_resident)
    );
    eprintln!(
        "FOOTPRINT +3-slot 1080p ring ({ring_t:?}) rss={:.1}MiB pss={:.1}MiB drm_total={:.1}MiB drm_resident={:.1}MiB",
        mib(s1.rss),
        mib(s1.pss),
        mib(s1.drm_total),
        mib(s1.drm_resident)
    );
    eprintln!(
        "FOOTPRINT +1080p shadow +first frame (submit {first:?}, done {first_done:?}, path {:?}) rss={:.1}MiB pss={:.1}MiB drm_total={:.1}MiB drm_resident={:.1}MiB",
        s2.shadow_path,
        mib(s2.rss),
        mib(s2.pss),
        mib(s2.drm_total),
        mib(s2.drm_resident)
    );
}

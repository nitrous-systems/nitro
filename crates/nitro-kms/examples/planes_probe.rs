//! Plane inventory and `TEST_ONLY` layouts for every output of a DRM card.
//! Opens the card directly (no libseat: run as root with no other master),
//! lights each output with one black frame, prints what every plane on its
//! CRTC can do, then asks the kernel about a set of representative layouts.
//! Nothing but that one frame reaches the screen.
//!
//! ```text
//! sudo planes_probe [--flip] /dev/dri/card1
//! ```
//!
//! `--flip` then really *shows* three layouts for ~2 s each through the
//! multi-plane frame path (`set_plane_state` + `commit_planes`): a YUYV
//! window on the overlay above the primary, YUYV full-screen with the
//! primary off, and NV12 on the primary with the output buffer as AR24
//! on the overlay (a hole cut in it). The video buffers are left as
//! allocated (zero), which is solid green in YCbCr: no `unsafe` mapping
//! of the exported dma-buf is needed to see which plane is which. Each
//! is exported (PRIME) and the fd's size checked against `buffer_info`,
//! and the `Flipped` events and buffer releases are printed.
//!
//! The output is plain text meant to be pasted into `README.md`.

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

/// One layer of a probed layout: plane, format (`None` = the output's
/// front buffer), source size, destination.
type Layer = (PlaneId, Option<Fourcc>, (u32, u32), Rect);
/// A lazily allocated buffer: key and allocation result.
type BufSlot = ((Fourcc, u32, u32), Result<BufferId, String>);

use nitro_kms::planes::{errno_name, modifier_name, rotation};
use nitro_kms::{
    Backend, BufferId, ColorEncoding, ColorRange, DrmBackend, DrmOptions, Error, Event, Fourcc,
    OutputId, OutputInfo, PlaneAssignment, PlaneConfig, PlaneId, PlaneInfo, PlaneKind, PlaneSource,
    Rect, SrcRect,
};

fn open(path: &str) -> Result<DrmBackend<'static>, Error> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| Error::Io {
            op: "open card",
            source: e,
        })?;
    let fd: OwnedFd = file.into();
    DrmBackend::open(
        fd,
        &DrmOptions {
            hotplug: false,
            ..DrmOptions::default()
        },
    )
}

/// Commit one black frame per output and wait for the flips, so every
/// output is lit (a `TEST_ONLY` layout needs a CRTC that is ours).
fn light_all(kms: &mut DrmBackend<'_>) -> Result<(), Error> {
    let ids: Vec<OutputId> = kms.outputs().iter().map(|o| o.id).collect();
    for &id in &ids {
        let mut buf = kms.back_buffer(id)?;
        let (w, h) = (buf.width, buf.height);
        buf.fill_rect(Rect::new(0, 0, w, h), 0);
        kms.commit(id, &[])?;
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut events = Vec::new();
    while ids.iter().any(|&id| kms.flip_pending(id)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        kms.dispatch(&mut events)?;
    }
    Ok(())
}

fn print_plane(p: &PlaneInfo) {
    let zpos = p.zpos.map_or_else(
        || "-".to_owned(),
        |z| {
            format!(
                "{} [{}..{}]{}",
                z.current,
                z.min,
                z.max,
                if z.immutable { " immutable" } else { "" }
            )
        },
    );
    println!(
        "  {} {:<7} crtcs={:#04b} zpos={} rotation={}",
        p.id,
        p.kind.to_string(),
        p.crtc_mask,
        zpos,
        rotation::describe(p.rotations)
    );
    let list = |v: &[String]| {
        if v.is_empty() {
            "-".to_owned()
        } else {
            v.join(", ")
        }
    };
    println!(
        "    COLOR_ENCODING: {}; COLOR_RANGE: {}",
        list(&p.color_encodings),
        list(&p.color_ranges)
    );
    println!(
        "    blend: {}; alpha={} damage_clips={} in_fence={}",
        list(&p.blend_modes),
        p.alpha,
        p.damage_clips,
        p.in_fence
    );
    // Group formats by their modifier set, which keeps the list short.
    let mut groups: Vec<(Vec<u64>, Vec<Fourcc>)> = Vec::new();
    for (f, mods) in &p.formats {
        match groups.iter_mut().find(|(m, _)| m == mods) {
            Some((_, fs)) => fs.push(*f),
            None => groups.push((mods.clone(), vec![*f])),
        }
    }
    for (mods, fs) in groups {
        let m: Vec<String> = mods.iter().map(|&m| modifier_name(m)).collect();
        let f: Vec<String> = fs.iter().map(ToString::to_string).collect();
        println!("    [{}]: {}", m.join(", "), f.join(" "));
    }
}

/// Per-output probe state: buffers by (format, w, h), allocated lazily.
struct Probe<'k, 'fd> {
    kms: &'k mut DrmBackend<'fd>,
    out: OutputInfo,
    buffers: Vec<BufSlot>,
}

impl Probe<'_, '_> {
    fn buffer(&mut self, f: Fourcc, w: u32, h: u32) -> Result<BufferId, String> {
        if let Some((_, r)) = self.buffers.iter().find(|(k, _)| *k == (f, w, h)) {
            return r.clone();
        }
        let r = self.kms.alloc_buffer(f, w, h).map_err(|e| match &e {
            Error::Io { source, .. } => format!(
                "addfb {f} {w}x{h} failed ({})",
                source
                    .raw_os_error()
                    .map_or_else(|| source.to_string(), errno_name)
            ),
            _ => format!("{f} {w}x{h}: {e}"),
        });
        self.buffers.push(((f, w, h), r.clone()));
        r
    }

    fn primary_full(&self, primary: PlaneId) -> PlaneAssignment<'static> {
        let (w, h) = (self.out.width, self.out.height);
        PlaneAssignment::new(
            primary,
            PlaneSource::OutputFront,
            SrcRect::whole(w, h),
            Rect::new(0, 0, w, h),
        )
    }

    /// Print one layout's verdict. `layers` are (plane, format, src size,
    /// dst rect); `None` format means the output's own front buffer.
    fn test(&mut self, label: &str, layers: &[Layer]) {
        let mut layout = Vec::new();
        for &(plane, format, (sw, sh), dst) in layers {
            let source = match format {
                None => PlaneSource::OutputFront,
                Some(f) => match self.buffer(f, sw, sh) {
                    Ok(b) => PlaneSource::Buffer(b),
                    Err(e) => {
                        println!("  {label:<58} SKIP: {e}");
                        return;
                    }
                },
            };
            let mut a = PlaneAssignment::new(plane, source, SrcRect::whole(sw, sh), dst);
            if format.is_some_and(Fourcc::is_yuv) {
                a.color_encoding = Some(ColorEncoding::Bt709);
                a.color_range = Some(ColorRange::Limited);
            }
            layout.push(a);
        }
        match self.kms.test_layout(self.out.id, &layout) {
            Ok(v) => println!("  {label:<58} {v}"),
            Err(e) => println!("  {label:<58} ERROR: {e}"),
        }
    }

    fn free(self) {
        for (_, r) in self.buffers {
            if let Ok(b) = r {
                self.kms.free_buffer(b);
            }
        }
    }
}

#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
fn probe_output(kms: &mut DrmBackend<'_>, out: &OutputInfo) {
    let planes = kms.planes(out.id);
    println!(
        "{} {} {}x{}: {} planes",
        out.id,
        out.name,
        out.width,
        out.height,
        planes.len()
    );
    for p in &planes {
        print_plane(p);
    }
    let of = |k: PlaneKind| planes.iter().filter(move |p| p.kind == k).map(|p| p.id);
    let Some(primary) = of(PlaneKind::Primary).next() else {
        println!("  no primary plane; nothing to test");
        return;
    };
    let overlays: Vec<PlaneId> = of(PlaneKind::Overlay).collect();
    let cursor = of(PlaneKind::Cursor).next();
    let (w, h) = (out.width, out.height);
    let fullscreen = Rect::new(0, 0, w, h);
    let window = Rect::new(
        (w.cast_signed() - 960) / 2,
        (h.cast_signed() - 540) / 2,
        960.min(w),
        540.min(h),
    );
    let mut pr = Probe {
        kms,
        out: out.clone(),
        buffers: Vec::new(),
    };
    let front = None;
    println!("  layouts (TEST_ONLY, no ALLOW_MODESET; unlisted planes on the CRTC disabled):");
    pr.test(
        "(a) XRGB fullscreen on primary",
        &[(primary, front, (w, h), fullscreen)],
    );
    pr.test(
        &format!("(a2) XRGB {w}x{h} buffer on primary"),
        &[(primary, Some(Fourcc::XRGB8888), (w, h), fullscreen)],
    );
    pr.test(
        "(a3) XRGB primary scaled 1280x720 -> fullscreen",
        &[(primary, Some(Fourcc::XRGB8888), (1280, 720), fullscreen)],
    );
    pr.test(
        "(a4) XRGB primary as a 960x540 window",
        &[(primary, Some(Fourcc::XRGB8888), (960, 540), window)],
    );
    let Some(&ov) = overlays.first() else {
        println!("  no overlay plane; overlay layouts skipped");
        return pr.free();
    };
    for fmt in [Fourcc::NV12, Fourcc::YUYV] {
        let tag = fmt.to_string();
        pr.test(
            &format!("(b) {tag} {w}x{h} overlay 1:1 above primary"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (w, h), fullscreen),
            ],
        );
        pr.test(
            &format!("(b2) {tag} {w}x{h} overlay 1:1 alone (primary off)"),
            &[(ov, Some(fmt), (w, h), fullscreen)],
        );
        pr.test(
            &format!("(b3) {tag} 960x540 overlay 1:1 window above primary"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (960, 540), window),
            ],
        );
        // (c) below the primary. With a fixed zpos that is only possible
        // by assignment (the video on the lower plane), which is what
        // the primary itself carrying the video and the overlay the UI
        // amounts to.
        let fixed = planes
            .iter()
            .filter(|p| p.id == primary || p.id == ov)
            .any(|p| p.zpos.is_none_or(|z| z.immutable));
        if fixed {
            pr.test(
                &format!("(c) {tag} on primary, XRGB UI on overlay (fixed zpos)"),
                &[
                    (primary, Some(fmt), (w, h), fullscreen),
                    (ov, Some(Fourcc::XRGB8888), (960, 540), window),
                ],
            );
        } else {
            let z = |id| planes.iter().find(|p| p.id == id).and_then(|p| p.zpos);
            let (Some(zp), Some(zo)) = (z(primary), z(ov)) else {
                unreachable!("mutable zpos checked above")
            };
            let label = format!("(c) {tag} overlay below primary (zpos swap)");
            match pr.buffer(fmt, w, h) {
                Ok(b) => {
                    let mut a = PlaneAssignment::new(
                        ov,
                        PlaneSource::Buffer(b),
                        SrcRect::whole(w, h),
                        fullscreen,
                    );
                    a.zpos = Some(zp.min.min(zo.min));
                    a.color_encoding = Some(ColorEncoding::Bt709);
                    a.color_range = Some(ColorRange::Limited);
                    let layout = [pr.primary_full(primary).with_zpos(zo.max.max(zp.max)), a];
                    match pr.kms.test_layout(out.id, &layout) {
                        Ok(v) => println!("  {label:<58} {v}"),
                        Err(e) => println!("  {label:<58} ERROR: {e}"),
                    }
                }
                Err(e) => println!("  {label:<58} SKIP: {e}"),
            }
        }
        pr.test(
            &format!("(d) {tag} 1280x720 scaled to fullscreen"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (1280, 720), fullscreen),
            ],
        );
        pr.test(
            &format!("(d2) {tag} 1280x720 scaled to a 960x540 window"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (1280, 720), window),
            ],
        );
        // Gen9+ additions (#3911): a video-sized source downscaled into a
        // window, and NV12's alignment rules (odd position, odd size).
        pr.test(
            &format!("(d3) {tag} 1920x1080 scaled down to a 960x540 window"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (1920, 1080), window),
            ],
        );
        pr.test(
            &format!("(f) {tag} 960x540 1:1 window at an odd position"),
            &[
                (primary, front, (w, h), fullscreen),
                (
                    ov,
                    Some(fmt),
                    (960, 540),
                    Rect::new(window.x + 1, window.y + 1, 960, 540),
                ),
            ],
        );
        pr.test(
            &format!("(f2) {tag} 960x540 scaled to an odd 961x541 window"),
            &[
                (primary, front, (w, h), fullscreen),
                (
                    ov,
                    Some(fmt),
                    (960, 540),
                    Rect::new(window.x, window.y, 961, 541),
                ),
            ],
        );
        pr.test(
            &format!("(i) XRGB primary 1280x720 up + {tag} 960x540 up to 1280x720"),
            &[
                (primary, Some(Fourcc::XRGB8888), (1280, 720), fullscreen),
                (ov, Some(fmt), (960, 540), Rect::new(0, 0, 1280, 720)),
            ],
        );
        if let Some(&ov2) = overlays.get(1) {
            pr.test(
                &format!("(e) two {tag} 960x540 overlays + primary"),
                &[
                    (primary, front, (w, h), fullscreen),
                    (ov, Some(fmt), (960, 540), Rect::new(0, 0, 960, 540)),
                    (ov2, Some(fmt), (960, 540), Rect::new(960, 540, 960, 540)),
                ],
            );
        } else {
            println!(
                "  {:<58} SKIP: one overlay plane",
                format!("(e) two {tag} overlays")
            );
        }
    }
    for fmt in [Fourcc::XRGB8888, Fourcc::ARGB8888] {
        pr.test(
            &format!("(g) {fmt} 960x540 overlay 1:1 above primary"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (960, 540), window),
            ],
        );
        pr.test(
            &format!("(g2) {fmt} 960x540 overlay 2x to fullscreen"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (960, 540), fullscreen),
            ],
        );
        pr.test(
            &format!("(g3) {fmt} {w}x{h} overlay downscaled to 960x540"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (w, h), window),
            ],
        );
        pr.test(
            &format!("(g5) {fmt} 1024x576 overlay 0.94x to 960x540"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (1024, 576), window),
            ],
        );
        pr.test(
            &format!("(g4) {fmt} 1280x720 overlay 0.75x to 960x540"),
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(fmt), (1280, 720), window),
            ],
        );
    }
    if let Some(cur) = cursor {
        for s in [64, 128, 256] {
            pr.test(
                &format!("(h) ARGB {s}x{s} on cursor + primary"),
                &[
                    (primary, front, (w, h), fullscreen),
                    (
                        cur,
                        Some(Fourcc::ARGB8888),
                        (s, s),
                        Rect::new(100, 100, s, s),
                    ),
                ],
            );
        }
        pr.test(
            "(h2) ARGB 64x64 cursor + YUYV overlay + primary",
            &[
                (primary, front, (w, h), fullscreen),
                (ov, Some(Fourcc::YUYV), (960, 540), window),
                (
                    cur,
                    Some(Fourcc::ARGB8888),
                    (64, 64),
                    Rect::new(100, 100, 64, 64),
                ),
            ],
        );
    }
    pr.free();
}

/// Dispatch until `id` flips (1 s at most); print the event and how long
/// it took.
fn wait_flip(kms: &mut DrmBackend<'_>, id: OutputId, what: &str, t0: Instant) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
        if let Err(e) = kms.dispatch(&mut events) {
            println!("    {what}: dispatch ERROR: {e}");
            return false;
        }
        for ev in events.drain(..) {
            if let Event::Flipped {
                output, sequence, ..
            } = ev
                && output == id
            {
                println!(
                    "    {what}: Flipped seq={sequence} after {:.1} ms",
                    t0.elapsed().as_secs_f64() * 1e3
                );
                return true;
            }
        }
    }
    println!("    {what}: NO Flipped within 1 s");
    false
}

/// Allocate, export and size-check a video buffer.
#[allow(clippy::many_single_char_names)]
fn video_buffer(kms: &mut DrmBackend<'_>, f: Fourcc, w: u32, h: u32) -> Result<BufferId, String> {
    let id = kms
        .alloc_buffer(f, w, h)
        .map_err(|e| format!("alloc {f} {w}x{h}: {e}"))?;
    let info = kms.buffer_info(id).ok_or("no buffer_info")?;
    match kms.export_buffer(id) {
        Ok(fd) => {
            let len = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0)).unwrap_or(0);
            println!(
                "    {f} {w}x{h}: pitches={:?} offsets={:?} size={} PRIME fd size={len}",
                info.pitches, info.offsets, info.size
            );
        }
        Err(e) => println!("    {f} {w}x{h}: export ERROR: {e}"),
    }
    Ok(id)
}

/// Test, show for 2 s, go back to the default, report the release.
/// `free_early` frees the buffer while it is still on screen.
fn show(
    kms: &mut DrmBackend<'_>,
    id: OutputId,
    label: &str,
    layout: &[PlaneConfig],
    buf: BufferId,
    free_early: bool,
) {
    println!("  {label}");
    let assigns: Vec<PlaneAssignment<'_>> = layout.iter().map(|c| c.assignment(None)).collect();
    match kms.test_layout(id, &assigns) {
        Ok(v) if v.accepted() => {}
        Ok(v) => {
            println!("    SKIP: TEST_ONLY {v}");
            kms.free_buffer(buf);
            return;
        }
        Err(e) => {
            println!("    SKIP: {e}");
            kms.free_buffer(buf);
            return;
        }
    }
    let t0 = Instant::now();
    if let Err(e) = kms
        .set_plane_state(id, layout)
        .and_then(|()| kms.commit_planes(id))
    {
        println!("    commit ERROR: {e}");
        let _ = kms.set_plane_state(id, &[]);
        kms.free_buffer(buf);
        return;
    }
    wait_flip(kms, id, "on", t0);
    std::thread::sleep(Duration::from_secs(2));
    if free_early {
        kms.free_buffer(buf);
        println!("    freed while on screen (deferred)");
    }
    let t0 = Instant::now();
    if let Err(e) = kms
        .set_plane_state(id, &[])
        .and_then(|()| kms.commit_planes(id))
    {
        println!("    back to default: ERROR: {e}");
        return;
    }
    wait_flip(kms, id, "off", t0);
    let rel = kms.take_released_buffers();
    println!("    released: {rel:?} (buffer {buf})");
    if !free_early {
        kms.free_buffer(buf);
    }
}

fn flip_output(kms: &mut DrmBackend<'_>, out: &OutputInfo) {
    let planes = kms.planes(out.id);
    let of = |k: PlaneKind| planes.iter().find(|p| p.kind == k).map(|p| p.id);
    let (Some(primary), Some(ov)) = (of(PlaneKind::Primary), of(PlaneKind::Overlay)) else {
        println!("{} {}: needs a primary and an overlay", out.id, out.name);
        return;
    };
    let (w, h) = (out.width, out.height);
    println!(
        "{} {} {w}x{h}: multi-plane flips (--flip)",
        out.id, out.name
    );
    let yuv = |c: PlaneConfig| PlaneConfig {
        color_encoding: Some(ColorEncoding::Bt709),
        color_range: Some(ColorRange::Limited),
        ..c
    };
    let front = PlaneConfig::new(
        primary,
        PlaneSource::OutputFront,
        SrcRect::whole(w, h),
        Rect::new(0, 0, w, h),
    );

    // (a) a 1280x720 YUYV window on the overlay above the primary.
    let (vw, vh) = (1280.min(w), 720.min(h));
    match video_buffer(kms, Fourcc::YUYV, vw, vh) {
        Ok(b) => {
            let win = Rect::new(
                (w - vw).cast_signed() / 2,
                (h - vh).cast_signed() / 2,
                vw,
                vh,
            );
            let ovc = yuv(PlaneConfig::new(
                ov,
                PlaneSource::Buffer(b),
                SrcRect::whole(vw, vh),
                win,
            ));
            show(
                kms,
                out.id,
                "(a) YUYV window above the primary",
                &[front, ovc],
                b,
                false,
            );
        }
        Err(e) => println!("  (a) SKIP: {e}"),
    }

    // (b2) YUYV full-screen on the overlay, primary off; freed while on
    // screen, to see the deferred free go through.
    match video_buffer(kms, Fourcc::YUYV, w, h) {
        Ok(b) => {
            let ovc = yuv(PlaneConfig::new(
                ov,
                PlaneSource::Buffer(b),
                SrcRect::whole(w, h),
                Rect::new(0, 0, w, h),
            ));
            show(
                kms,
                out.id,
                "(b2) YUYV full-screen overlay, primary off",
                &[ovc],
                b,
                true,
            );
        }
        Err(e) => println!("  (b2) SKIP: {e}"),
    }

    // (c) NV12 on the primary, the output buffer as AR24 on the overlay
    // with a transparent hole in the middle.
    println!("  (c) NV12 on the primary, AR24 output buffer on the overlay");
    if let Err(e) = kms.set_scanout_alpha(out.id, true) {
        println!("    SKIP: {e}");
        return;
    }
    let painted = kms.back_buffer(out.id).map(|mut buf| {
        buf.fill_rect(Rect::new(0, 0, w, h), 0xFF40_4040);
        buf.fill_rect(
            Rect::new((w / 4).cast_signed(), (h / 4).cast_signed(), w / 2, h / 2),
            0,
        );
    });
    let t0 = Instant::now();
    if let Err(e) = painted.and_then(|()| kms.commit(out.id, &[])) {
        println!("    shadow commit ERROR: {e}");
    } else {
        wait_flip(kms, out.id, "shadow (primary, AR24)", t0);
    }
    match video_buffer(kms, Fourcc::NV12, w, h) {
        Ok(b) => {
            let pri = yuv(PlaneConfig::new(
                primary,
                PlaneSource::Buffer(b),
                SrcRect::whole(w, h),
                Rect::new(0, 0, w, h),
            ));
            let top = PlaneConfig { plane: ov, ..front };
            show(kms, out.id, "    layout", &[pri, top], b, false);
        }
        Err(e) => println!("    SKIP: {e}"),
    }
    let _ = kms.set_scanout_alpha(out.id, false);
}

fn main() -> Result<(), Error> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let flip = args.iter().any(|a| a == "--flip");
    args.retain(|a| a != "--flip");
    let path = args
        .into_iter()
        .next()
        .unwrap_or_else(|| "/dev/dri/card1".to_owned());
    let mut kms = open(&path)?;
    light_all(&mut kms)?;
    let outs: Vec<OutputInfo> = kms.outputs().to_vec();
    if outs.is_empty() {
        println!("no connected outputs");
    }
    for o in outs {
        if flip {
            flip_output(&mut kms, &o);
        } else {
            probe_output(&mut kms, &o);
        }
    }
    Ok(())
}

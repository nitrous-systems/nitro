//! The helper loop over a real socketpair with the fake backend: codec
//! round trips, hostile input, deferred release, ring damage, exits.

#![allow(clippy::many_single_char_names, clippy::too_many_lines)] // test fixtures: c(onn), h(andle), w/h

use std::os::fd::OwnedFd;
use std::thread::JoinHandle;
use std::time::Duration;

use nitro_core::IRect;
use nitro_gpu::fake::{Call, FakeBackend};
use nitro_gpu::proto::{
    Blend, ColorEncoding, ColorRange, Composite, DeviceInfo, DmabufDesc, ErrorCode, FormatMod,
    Layer, NV12, PROTO_VERSION, PlaneDesc, ShadowDesc, ShadowPath, SlotLayout, Stats, XR24, op,
};
use nitro_gpu::{Config, Conn, Exit, FromHelper, Message, ToHelper};
use nitro_wire::{Framer, Writer};

const T: Duration = Duration::from_secs(5);

fn memfd(len: u64) -> OwnedFd {
    nitro_shm::create_sealed("test", len).unwrap()
}

fn start(fake: FakeBackend, cfg: Config) -> (Conn, JoinHandle<Exit>) {
    let (a, b) = nitro_wire::io::pair().unwrap();
    let h = std::thread::spawn(move || nitro_gpu::run(b, fake, cfg).unwrap());
    let mut c = Conn::new(a);
    let (r, _) = c
        .call(
            &ToHelper::Hello {
                version: PROTO_VERSION,
            },
            vec![],
            T,
        )
        .unwrap();
    assert!(
        matches!(
            r,
            FromHelper::HelloReply {
                version: PROTO_VERSION,
                ..
            }
        ),
        "{r:?}"
    );
    (c, h)
}

fn expect(c: &mut Conn) -> (FromHelper, Vec<OwnedFd>) {
    c.recv(T).unwrap()
}

fn expect_error(c: &mut Conn, code: ErrorCode) {
    match expect(c).0 {
        FromHelper::Error { code: got, .. } => assert_eq!(got, code),
        other => panic!("wanted Error {code:?}, got {other:?}"),
    }
}

fn shadow(c: &mut Conn, id: u32, w: u32, h: u32) {
    let d = ShadowDesc {
        id,
        w,
        h,
        stride: w * 4,
        fourcc: XR24,
    };
    let (r, _) = c
        .call(
            &ToHelper::ImportShadow(d),
            vec![memfd(u64::from(w * 4 * h))],
            T,
        )
        .unwrap();
    assert_eq!(r, FromHelper::Imported { id });
}

fn ring(c: &mut Conn, n: u32, w: u32, h: u32) -> Vec<OwnedFd> {
    let msg = ToHelper::AllocOutputRing {
        n,
        w,
        h,
        fourcc: XR24,
        modifiers: vec![0],
    };
    let (r, fds) = c.call(&msg, vec![], T).unwrap();
    assert!(matches!(r, FromHelper::OutputRing { .. }), "{r:?}");
    assert_eq!(fds.len(), n as usize);
    fds
}

fn layer(tex: u32, w: i32, h: i32) -> Layer {
    Layer {
        tex,
        src: [0.0, 0.0, w as f32, h as f32],
        dst: IRect::new(0, 0, w, h),
        blend: Blend::Opaque,
    }
}

fn frame(serial: u64, out_idx: u32, damage: Vec<IRect>, layers: Vec<Layer>) -> ToHelper {
    ToHelper::Composite(Composite {
        serial,
        out_idx,
        damage,
        layers,
        fence_mask: 0,
    })
}

fn composited(c: &mut Conn, serial: u64) -> OwnedFd {
    let (r, mut fds) = expect(c);
    assert_eq!(r, FromHelper::Composited { serial });
    fds.pop().unwrap()
}

fn shutdown(mut c: Conn, h: JoinHandle<Exit>) {
    c.send(&ToHelper::Shutdown, vec![]).unwrap();
    assert_eq!(h.join().unwrap(), Exit::Shutdown);
}

// ---------------------------------------------------------------------------

fn round_trip<M: Message + PartialEq + std::fmt::Debug>(m: &M, nfds: usize) {
    let mut w = Writer::new();
    let fds = (0..nfds).map(|_| memfd(1)).collect();
    m.encode(&mut w, fds).unwrap();
    let (bytes, fds) = w.take();
    let mut f = Framer::new();
    f.feed(&bytes, fds);
    let frame = f.next_frame().unwrap().unwrap();
    let (back, fds) = M::decode(frame).unwrap();
    assert_eq!(&back, m);
    assert_eq!(fds.len(), nfds);
}

#[test]
fn every_message_round_trips() {
    let dm = DmabufDesc {
        id: 7,
        w: 64,
        h: 32,
        fourcc: NV12,
        modifier: 0x0100_0000_0000_0002,
        planes: vec![
            PlaneDesc {
                offset: 0,
                pitch: 64,
            },
            PlaneDesc {
                offset: 2048,
                pitch: 64,
            },
        ],
        encoding: ColorEncoding::Bt2020,
        range: ColorRange::Full,
    };
    let comp = Composite {
        serial: u64::MAX,
        out_idx: 1,
        damage: vec![IRect::new(1, 2, 3, 4)],
        layers: vec![
            layer(1, 10, 10),
            Layer {
                blend: Blend::PremulOver,
                src: [0.5, 0.25, 3.5, 2.0],
                ..layer(2, 5, 5)
            },
        ],
        fence_mask: 0b10,
    };
    let to = [
        (ToHelper::Hello { version: 1 }, 0),
        (ToHelper::ImportDmabuf(dm), 2),
        (
            ToHelper::ImportShadow(ShadowDesc {
                id: 1,
                w: 2,
                h: 3,
                stride: 8,
                fourcc: XR24,
            }),
            1,
        ),
        (
            ToHelper::UploadDamage {
                id: 1,
                rects: vec![IRect::new(0, 0, 1, 1)],
            },
            0,
        ),
        (
            ToHelper::AllocOutputRing {
                n: 3,
                w: 1920,
                h: 1080,
                fourcc: XR24,
                modifiers: vec![0, 1 << 56 | 1],
            },
            0,
        ),
        (ToHelper::Composite(comp), 1),
        (ToHelper::Release { id: 9 }, 0),
        (ToHelper::ReadBack { out_idx: 2 }, 0),
        (ToHelper::GetStats, 0),
        (ToHelper::Shutdown, 0),
        (
            ToHelper::Capture {
                serial: 7,
                w: 1920,
                h: 1080,
                layers: vec![layer(3, 64, 64)],
            },
            0,
        ),
    ];
    for (m, n) in &to {
        round_trip(m, *n);
    }
    let from = [
        (
            FromHelper::HelloReply {
                version: 1,
                info: DeviceInfo {
                    device: "Intel(R) UHD 620".into(),
                    driver: "anv".into(),
                    sampleable: vec![FormatMod {
                        fourcc: NV12,
                        modifier: 0,
                    }],
                    render: vec![FormatMod {
                        fourcc: XR24,
                        modifier: 1 << 56 | 1,
                    }],
                },
            },
            0,
        ),
        (FromHelper::Imported { id: 3 }, 0),
        (
            FromHelper::Error {
                op: 6,
                what: 77,
                code: ErrorCode::Busy,
                msg: "slot".into(),
            },
            0,
        ),
        (
            FromHelper::OutputRing {
                w: 4,
                h: 4,
                fourcc: XR24,
                modifier: 0,
                slots: vec![
                    SlotLayout {
                        offset: 0,
                        pitch: 16,
                        size: 64
                    };
                    2
                ],
            },
            2,
        ),
        (FromHelper::Composited { serial: 5 }, 1),
        (FromHelper::Released { id: 3 }, 0),
        (
            FromHelper::Captured {
                serial: 7,
                w: 4,
                h: 4,
                stride: 16,
            },
            1,
        ),
        (
            FromHelper::ReadBackReply {
                out_idx: 0,
                w: 4,
                h: 4,
                stride: 16,
            },
            1,
        ),
        (
            FromHelper::Stats(Stats {
                frames: 1,
                imports: 2,
                errors: 3,
                textures_live: 4,
                in_flight: 5,
                submit_us_avg: 6,
                submit_us_max: 7,
                shadow_path: ShadowPath::Udmabuf,
                drm_total: 8,
                drm_resident: 9,
                rss: 10,
                pss: 11,
            }),
            0,
        ),
    ];
    for (m, n) in &from {
        round_trip(m, *n);
    }
}

#[test]
fn encode_refuses_wrong_fd_counts_and_limits() {
    let mut w = Writer::new();
    let c = Composite {
        layers: vec![layer(1, 1, 1)],
        fence_mask: 1,
        ..Composite::default()
    };
    assert!(
        ToHelper::Composite(c.clone())
            .encode(&mut w, vec![])
            .is_err()
    );
    let nine = Composite {
        layers: (0..9).map(|i| layer(i, 1, 1)).collect(),
        fence_mask: 0x1ff,
        ..Composite::default()
    };
    let fds = (0..9).map(|_| memfd(1)).collect();
    assert!(
        ToHelper::Composite(nine).encode(&mut w, fds).is_err(),
        "over MAX_FDS"
    );
    let many = Composite {
        layers: (0..17).map(|i| layer(i, 1, 1)).collect(),
        ..Composite::default()
    };
    assert!(
        ToHelper::Composite(many).encode(&mut w, vec![]).is_err(),
        "over MAX_LAYERS"
    );
    let bad_mask = Composite {
        layers: vec![layer(1, 1, 1)],
        fence_mask: 0b10,
        ..Composite::default()
    };
    assert!(
        ToHelper::Composite(bad_mask)
            .encode(&mut w, vec![memfd(1)])
            .is_err()
    );
    assert!(w.is_empty(), "nothing written on error");
}

#[test]
fn hostile_input_gets_errors_and_the_helper_survives() {
    let (mut c, h) = start(FakeBackend::auto_signal(), Config::default());
    // Raw frames the typed API would refuse to build.
    let raw = |c: &mut Conn, opc: u16, body: &[u8], fds: Vec<OwnedFd>| {
        c.writer()
            .frame(opc, |w| {
                for b in body {
                    w.put_u8(*b);
                }
                for f in fds {
                    w.put_fd(f);
                }
                Ok(())
            })
            .unwrap();
        c.flush().unwrap();
    };
    raw(&mut c, op::COMPOSITE, &[1, 2, 3], vec![]);
    expect_error(&mut c, ErrorCode::Protocol);
    raw(&mut c, 0x7f, &[], vec![]);
    expect_error(&mut c, ErrorCode::Protocol);
    // ImportShadow with two fds.
    let mut w = Writer::new();
    ToHelper::ImportShadow(ShadowDesc {
        id: 1,
        w: 1,
        h: 1,
        stride: 4,
        fourcc: XR24,
    })
    .encode_body(&mut w);
    let body = w.bytes().to_vec();
    raw(&mut c, op::IMPORT_SHADOW, &body, vec![memfd(4), memfd(4)]);
    expect_error(&mut c, ErrorCode::Protocol);
    // Unsealed shadow memfd.
    let unsealed = rustix::fs::memfd_create("x", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    rustix::fs::ftruncate(&unsealed, 4).unwrap();
    c.send(
        &ToHelper::ImportShadow(ShadowDesc {
            id: 1,
            w: 1,
            h: 1,
            stride: 4,
            fourcc: XR24,
        }),
        vec![unsealed],
    )
    .unwrap();
    expect_error(&mut c, ErrorCode::BadBuffer);
    // Composite before a ring.
    shadow(&mut c, 1, 8, 8);
    c.send(&frame(1, 0, vec![], vec![layer(1, 8, 8)]), vec![])
        .unwrap();
    expect_error(&mut c, ErrorCode::NoRing);
    let _ring = ring(&mut c, 2, 8, 8);
    // Unknown texture, out-of-bounds dst, bad slot, duplicate id.
    c.send(&frame(2, 0, vec![], vec![layer(9, 8, 8)]), vec![])
        .unwrap();
    expect_error(&mut c, ErrorCode::BadId);
    c.send(&frame(3, 0, vec![], vec![layer(1, 9, 8)]), vec![])
        .unwrap();
    expect_error(&mut c, ErrorCode::BadRect);
    c.send(&frame(4, 5, vec![], vec![layer(1, 8, 8)]), vec![])
        .unwrap();
    expect_error(&mut c, ErrorCode::NoRing);
    c.send(
        &ToHelper::ImportShadow(ShadowDesc {
            id: 1,
            w: 1,
            h: 1,
            stride: 4,
            fourcc: XR24,
        }),
        vec![memfd(4)],
    )
    .unwrap();
    expect_error(&mut c, ErrorCode::DuplicateId);
    // Fence mask with no fd attached: decode refuses (MissingFd).
    let mut w = Writer::new();
    ToHelper::Composite(Composite {
        layers: vec![layer(1, 8, 8)],
        fence_mask: 1,
        ..Composite::default()
    })
    .encode_body(&mut w);
    let body = w.bytes().to_vec();
    raw(&mut c, op::COMPOSITE, &body, vec![]);
    expect_error(&mut c, ErrorCode::Protocol);
    // Unsupported dma-buf format.
    let d = DmabufDesc {
        id: 5,
        w: 8,
        h: 8,
        fourcc: 0x1234,
        planes: vec![PlaneDesc {
            offset: 0,
            pitch: 32,
        }],
        ..DmabufDesc::default()
    };
    c.send(&ToHelper::ImportDmabuf(d), vec![memfd(256)])
        .unwrap();
    expect_error(&mut c, ErrorCode::BadFormat);
    // Still alive and counting.
    c.send(&frame(10, 0, vec![], vec![layer(1, 8, 8)]), vec![])
        .unwrap();
    let _ = composited(&mut c, 10);
    let (r, _) = c.call(&ToHelper::GetStats, vec![], T).unwrap();
    let FromHelper::Stats(s) = r else {
        panic!("{r:?}")
    };
    assert_eq!(s.frames, 1);
    assert_eq!(s.errors, 11);
    shutdown(c, h);
}

#[test]
fn release_waits_for_the_frames_that_sample_it() {
    let fake = FakeBackend::new();
    let (mut c, h) = start(fake.clone(), Config::default());
    shadow(&mut c, 1, 4, 4);
    let _r = ring(&mut c, 2, 4, 4);
    c.send(&frame(1, 0, vec![], vec![layer(1, 4, 4)]), vec![])
        .unwrap();
    let f1 = composited(&mut c, 1);
    c.send(&frame(2, 1, vec![], vec![layer(1, 4, 4)]), vec![])
        .unwrap();
    let f2 = composited(&mut c, 2);
    c.send(&ToHelper::Release { id: 1 }, vec![]).unwrap();
    // New frames can no longer use it.
    c.send(&frame(3, 0, vec![], vec![layer(1, 4, 4)]), vec![])
        .unwrap();
    expect_error(&mut c, ErrorCode::BadId);
    assert!(!nitro_gpu::client::fence_signalled(&f1, Duration::ZERO));
    assert!(fake.signal());
    assert!(nitro_gpu::client::fence_signalled(&f1, T));
    // One frame still holds it: nothing arrives yet.
    assert!(c.recv(Duration::from_millis(100)).is_err());
    assert!(!fake.calls().contains(&Call::Release(1)));
    assert!(fake.signal());
    assert!(nitro_gpu::client::fence_signalled(&f2, T));
    assert_eq!(expect(&mut c).0, FromHelper::Released { id: 1 });
    assert!(fake.calls().contains(&Call::Release(1)));
    shutdown(c, h);
}

#[test]
fn busy_slot_is_refused_until_its_fence_signals() {
    let fake = FakeBackend::new();
    let (mut c, h) = start(fake.clone(), Config::default());
    shadow(&mut c, 1, 4, 4);
    let _r = ring(&mut c, 1, 4, 4);
    c.send(&frame(1, 0, vec![], vec![layer(1, 4, 4)]), vec![])
        .unwrap();
    let _f = composited(&mut c, 1);
    c.send(&frame(2, 0, vec![], vec![layer(1, 4, 4)]), vec![])
        .unwrap();
    expect_error(&mut c, ErrorCode::Busy);
    fake.signal();
    c.send(&frame(3, 0, vec![], vec![layer(1, 4, 4)]), vec![])
        .unwrap();
    let _ = composited(&mut c, 3);
    fake.signal();
    shutdown(c, h);
}

#[test]
fn ring_damage_accumulates_per_slot() {
    let fake = FakeBackend::auto_signal();
    let (mut c, h) = start(fake.clone(), Config::default());
    shadow(&mut c, 1, 100, 100);
    let _r = ring(&mut c, 2, 100, 100);
    let d1 = IRect::new(0, 0, 10, 10);
    let d2 = IRect::new(50, 50, 10, 10);
    let d3 = IRect::new(80, 0, 5, 5);
    for (s, slot, d) in [(1, 0, d1), (2, 1, d2), (3, 0, d3)] {
        c.send(&frame(s, slot, vec![d], vec![layer(1, 100, 100)]), vec![])
            .unwrap();
        let f = composited(&mut c, s);
        assert!(nitro_gpu::client::fence_signalled(&f, T));
        // Let the helper see the fence before the next frame on the slot.
        let _ = c.call(&ToHelper::GetStats, vec![], T).unwrap();
    }
    let clips: Vec<Vec<IRect>> = fake
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Composite(_, clip, _, _) => Some(clip),
            _ => None,
        })
        .collect();
    assert_eq!(
        clips[0],
        vec![IRect::new(0, 0, 100, 100)],
        "slot 0 new: full"
    );
    assert_eq!(
        clips[1],
        vec![IRect::new(0, 0, 100, 100)],
        "slot 1 new: full"
    );
    assert!(
        clips[2].contains(&d2) && clips[2].contains(&d3),
        "{:?}",
        clips[2]
    );
    assert!(!clips[2].iter().any(|r| r.contains(1, 1)), "{:?}", clips[2]);
    shutdown(c, h);
}

#[test]
fn eof_ends_the_loop_and_frees_everything() {
    let fake = FakeBackend::auto_signal();
    let (mut c, h) = start(fake.clone(), Config::default());
    shadow(&mut c, 1, 4, 4);
    drop(c);
    assert_eq!(h.join().unwrap(), Exit::Eof);
    assert!(fake.calls().contains(&Call::Release(1)));
}

#[test]
fn idle_exit_only_when_nothing_is_held() {
    let cfg = Config {
        idle_exit: Some(Duration::from_millis(200)),
    };
    let (mut c, h) = start(FakeBackend::auto_signal(), cfg);
    shadow(&mut c, 1, 4, 4);
    std::thread::sleep(Duration::from_millis(400));
    assert!(!h.is_finished(), "a texture is held");
    let (r, _) = c.call(&ToHelper::Release { id: 1 }, vec![], T).unwrap();
    assert_eq!(r, FromHelper::Released { id: 1 });
    assert_eq!(h.join().unwrap(), Exit::Idle);
}

#[test]
fn version_mismatch_is_refused() {
    let (a, b) = nitro_wire::io::pair().unwrap();
    let h = std::thread::spawn(move || {
        nitro_gpu::run(b, FakeBackend::new(), Config::default()).unwrap()
    });
    let mut c = Conn::new(a);
    c.send(&ToHelper::Hello { version: 99 }, vec![]).unwrap();
    expect_error(&mut c, ErrorCode::Version);
    drop(c);
    assert_eq!(h.join().unwrap(), Exit::Eof);
}

#[test]
fn readback_and_upload_reach_the_backend() {
    let fake = FakeBackend::auto_signal();
    let (mut c, h) = start(fake.clone(), Config::default());
    shadow(&mut c, 1, 4, 4);
    c.send(
        &ToHelper::UploadDamage {
            id: 1,
            rects: vec![IRect::new(-2, -2, 4, 4), IRect::new(9, 9, 1, 1)],
        },
        vec![],
    )
    .unwrap();
    let _r = ring(&mut c, 1, 4, 4);
    let (r, fds) = c
        .call(&ToHelper::ReadBack { out_idx: 0 }, vec![], T)
        .unwrap();
    assert_eq!(
        r,
        FromHelper::ReadBackReply {
            out_idx: 0,
            w: 4,
            h: 4,
            stride: 16
        }
    );
    assert_eq!(fds.len(), 1);
    assert!(
        fake.calls()
            .contains(&Call::Upload(1, vec![IRect::new(0, 0, 2, 2)]))
    );
    shutdown(c, h);
}

#[test]
fn capture_draws_the_layers_into_a_memfd_and_refuses_bad_ones() {
    let fake = FakeBackend::auto_signal();
    let (mut c, h) = start(fake.clone(), Config::default());
    shadow(&mut c, 1, 4, 4);
    let l = Layer {
        tex: 1,
        src: [0.0, 0.0, 4.0, 4.0],
        dst: IRect::new(1, 1, 2, 2),
        ..Layer::default()
    };
    // No ring needed.
    let (r, fds) = c
        .call(
            &ToHelper::Capture {
                serial: 9,
                w: 4,
                h: 4,
                layers: vec![l],
            },
            vec![],
            T,
        )
        .unwrap();
    assert_eq!(
        r,
        FromHelper::Captured {
            serial: 9,
            w: 4,
            h: 4,
            stride: 16
        }
    );
    let fd = fds.into_iter().next().unwrap();
    let len = nitro_shm::sealed_len(&fd).unwrap();
    assert_eq!(len, 64);
    let m = nitro_shm::Mapping::map(fd, 64).unwrap();
    let px = m.as_bytes();
    assert_eq!(&px[(16 + 4)..(16 + 8)], &nitro_gpu::fake::CAPTURE_COLOR);
    assert_eq!(&px[0..4], &[0, 0, 0, 0]);
    assert!(fake.calls().contains(&Call::Capture(4, 4, vec![1])));

    // An unknown texture, a dst outside the target, a backend failure:
    // refused with the serial, and the helper carries on.
    c.send(
        &ToHelper::Capture {
            serial: 10,
            w: 4,
            h: 4,
            layers: vec![Layer { tex: 5, ..l }],
        },
        vec![],
    )
    .unwrap();
    expect_error(&mut c, ErrorCode::BadId);
    c.send(
        &ToHelper::Capture {
            serial: 11,
            w: 2,
            h: 2,
            layers: vec![l],
        },
        vec![],
    )
    .unwrap();
    expect_error(&mut c, ErrorCode::BadRect);
    fake.state().fail_next_capture = true;
    c.send(
        &ToHelper::Capture {
            serial: 12,
            w: 4,
            h: 4,
            layers: vec![l],
        },
        vec![],
    )
    .unwrap();
    expect_error(&mut c, ErrorCode::Backend);
    shutdown(c, h);
}

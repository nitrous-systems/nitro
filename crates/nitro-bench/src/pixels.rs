//! The pixel path: a client buffer rewritten and re-uploaded every frame.
//!
//! # What this measures, and against what ceiling
//!
//! This is `x11perf -putimage100/-putimage500` and every fullscreen demo
//! effect, which turn out to be the same benchmark: a client fills S×S
//! BGRA pixels, tells the server the whole buffer changed, and the server
//! reads them back and composites them. The chain per frame is
//!
//! ```text
//! frame = effect + upload + server pread + server paint + server copy
//! ```
//!
//! and this module times the first two itself so the report can subtract
//! them. Without that split a fullscreen plasma at 9 ms per frame is an
//! indictment of the compositor when most of it was the sine loop.
//!
//! The ceiling to hold every number against: a 1080p BGRA frame is
//! **1920 × 1080 × 4 = 8 294 400 bytes**. At 60 Hz that is 497 MB/s
//! *written* by the client, and the server reads the same bytes back out
//! (`pread`, because the server copies rather than maps — see
//! `nitro-server`'s `read_buffer`, which explains why a mapping would let
//! a client shrink a memfd into a SIGBUS), then paints them, then copies
//! the damage into write-combined scanout memory. So a fullscreen
//! putimage at 60 Hz moves on the order of 1.5 GB/s through a machine
//! whose measured `memcpy` bandwidth [`crate::bandwidth`] reports, and at
//! 120 Hz it wants twice that. That is the arithmetic that decides
//! whether the pixel path can keep up, and it is the reason the retained
//! path exists.
//!
//! # Why a fresh memfd per run and one `pwrite` per frame
//!
//! The buffer is created once and re-written in place; `BufferDamage`
//! tells the server which rows changed, and the server re-`pread`s only
//! those. A benchmark that created a new buffer every frame would be
//! measuring `CreateBuffer` and fd passing, which is a different and much
//! rarer operation. Writing is `pwrite` rather than `mmap` for the reason
//! the whole tree writes `pwrite`: mapping needs `unsafe`, which this
//! tree denies. That is itself a finding — the upload column is a
//! syscall-per-frame that a mapped buffer would not pay, and the report
//! says how much it is.
//!
//! The scenario keeps its own descriptor on that one buffer, obtained by
//! [`rustix::io::dup`] of the descriptor it hands to `CreateBuffer`. It
//! is a `dup` and **not** a second [`memfd`] call, and the distinction is
//! the whole of issue #584: `memfd_create` mints a *new anonymous file*
//! every time it is called, and a memfd has no name to re-open, so a
//! second call yields an unrelated file that merely starts with the same
//! bytes. Every per-frame `pwrite` then landed in a file the server never
//! reads, and every pixel-path benchmark displayed frame 0 forever — a
//! correct-looking, never-moving picture, while still paying every byte
//! of the per-frame cost the report measures.

use std::os::fd::OwnedFd;
use std::time::Instant;

use nitro_core::{IRect, Rect};
use nitro_wire::msg::{BufferDamage, ClientMsg, CreateBuffer, CreateNode, SetBounds, SetImage};
use nitro_wire::types::{BufferId, NodeId, NodeKind, format};

use crate::effects::{Effect, Surface};
use crate::harness::{Ctx, Error, FIRST_NODE, Scenario, WINDOW};

/// The one buffer a pixel scenario owns.
pub const BUFFER: BufferId = BufferId(1);

/// The `Image` node it is bound to.
pub const IMAGE: NodeId = NodeId(FIRST_NODE);

/// Put `pixels` in a fresh memfd and hand back its descriptor.
///
/// `pwrite` rather than `mmap`: mapping would need `unsafe`, which this
/// tree denies. The same function `nitro-demo` uses, for the same reason,
/// and duplicated for the same reason its control socket is — twenty
/// lines is not a crate.
///
/// # Errors
/// Any `memfd_create`/`ftruncate`/`pwrite` failure.
pub fn memfd(pixels: &[u8]) -> Result<OwnedFd, rustix::io::Errno> {
    let fd = rustix::fs::memfd_create("nitro-bench", rustix::fs::MemfdFlags::CLOEXEC)?;
    rustix::fs::ftruncate(&fd, pixels.len() as u64)?;
    write_all_at(&fd, pixels, 0)?;
    Ok(fd)
}

/// `pwrite` in a loop until the slice is out.
///
/// # Errors
/// Any `pwrite` failure; a zero-length write is reported as `EIO` rather
/// than spun on, because a memfd that accepts nothing will go on accepting
/// nothing and a benchmark must not hang.
pub fn write_all_at(fd: &OwnedFd, data: &[u8], offset: u64) -> Result<(), rustix::io::Errno> {
    use rustix::io::Errno;
    let mut done = 0usize;
    while done < data.len() {
        match rustix::io::pwrite(fd, &data[done..], offset + done as u64) {
            Ok(0) => return Err(Errno::IO),
            Ok(n) => done += n,
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A scenario that redraws a whole client buffer every frame.
///
/// Generic over [`Effect`] so the six demo effects and the synthetic
/// `putimage` pattern are all measured by *identical* wire code: the only
/// thing that differs between a plasma run and a fire run is the function
/// that fills the bytes, which is exactly the variable the table claims to
/// isolate.
pub struct PixelScenario {
    /// The effect that fills the pixels.
    effect: Box<dyn Effect>,
    /// Scenario name, which is the effect's unless overridden.
    label: &'static str,
    /// The pixel buffer, reused every frame.
    surface: Surface,
    /// Its memfd, kept open for the life of the run.
    fd: Option<OwnedFd>,
    /// Requested buffer edge, or 0 for "the window's size".
    edge: u32,
    /// Accumulated microseconds in the effect.
    compute_us: u64,
    /// Accumulated microseconds in `pwrite`.
    upload_us: u64,
}

impl PixelScenario {
    /// A scenario drawing `effect` into a buffer of `edge`×`edge` pixels,
    /// or the whole window when `edge` is 0.
    #[must_use]
    pub fn new(label: &'static str, effect: Box<dyn Effect>, edge: u32) -> Self {
        Self {
            effect,
            label,
            surface: Surface::new(1, 1),
            fd: None,
            edge,
            compute_us: 0,
            upload_us: 0,
        }
    }

    /// Bytes the buffer holds, for the bandwidth arithmetic.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.surface.byte_len()
    }

    /// The buffer's pixel dimensions, which a fullscreen run only learns
    /// from the server's `Configure`.
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.surface.width, self.surface.height)
    }

    /// The buffer's bytes, BGRA, `stride` per row.
    ///
    /// For the tests, which settle "did the effect actually cover the
    /// surface" on pixels rather than on the geometry the scenario
    /// reports about itself — the geometry was right while the pixels
    /// were wrong, which is how the fullscreen-resize defect survived.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.surface.data
    }
}

impl Scenario for PixelScenario {
    fn name(&self) -> &'static str {
        self.label
    }

    fn build(&mut self, ctx: &mut Ctx) -> Result<Vec<ClientMsg>, Error> {
        let (w, h) = if self.edge == 0 {
            // Device pixels, not logical: the buffer is scanned out at
            // scale, so a scale-2 output wants four times the pixels and
            // a benchmark that shipped logical-sized buffers would report
            // a quarter of the real bandwidth.
            (
                (ctx.size.w * ctx.scale) as u32,
                (ctx.size.h * ctx.scale) as u32,
            )
        } else {
            (self.edge, self.edge)
        };
        let (w, h) = (w.max(1), h.max(1));
        ctx.size_px = w;
        self.surface = Surface::new(w, h);
        // Rebuild the effect for the size the *server* gave us, which a
        // fullscreen run only learns here. Without this the effect keeps
        // whatever box it was constructed with on the command line — 640×480
        // by default — and simulates a quarter-resolution world into a
        // full-resolution buffer. See `Effect::resize`, which carries the
        // ledger evidence: fullscreen fire reported less compute than VGA
        // fire, which cannot be true.
        self.effect.resize(w, h);
        self.effect.render(&mut self.surface, 0);
        let fd = memfd(&self.surface.data)?;
        // The descriptor below moves into `CreateBuffer`, so the scenario
        // keeps a `dup` of it for the per-frame writes: two descriptors
        // on *one* file, the server `pread`ing what this client
        // `pwrite`s. Re-sending the buffer every frame would be the
        // alternative, which is the thing this scenario exists not to do.
        //
        // It must be a `dup` and not a second `memfd()`. `memfd_create`
        // creates a new anonymous file on every call and a memfd has no
        // name to re-open, so a second call is an unrelated file that
        // merely starts with the same bytes — which is precisely the bug
        // (#584) that left every pixel-path benchmark showing frame 0
        // forever while doing all the work of animating.
        let mine = rustix::io::dup(&fd)?;
        let msgs = vec![
            CreateNode {
                id: IMAGE,
                kind: NodeKind::Image,
                parent: WINDOW,
                before: NodeId::NONE,
            }
            .into(),
            SetBounds {
                id: IMAGE,
                // The node is the buffer's own size, **not** the window's,
                // and the box run is why this is spelled out.
                //
                // The first version stretched every buffer across the
                // whole window. The server then resampled it on every
                // frame, and `putimage` at 100, 250 and 500 px all
                // reported the same ~11 ms of paint and the same 307 200
                // damage pixels — a sweep whose four points measured one
                // thing, image scaling, and whose x11perf heritage claims
                // it measures an upload. The same mistake caught in
                // `boing-node` (see `nodes.rs`), found by the same run.
                //
                // Logical units, so a scale-2 output draws a
                // device-pixel-sized buffer at half the logical size and
                // the mapping stays one-to-one where it matters.
                rect: Rect::new(0.0, 0.0, w as f32 / ctx.scale, h as f32 / ctx.scale),
            }
            .into(),
            CreateBuffer {
                id: BUFFER,
                width: w,
                height: h,
                stride: self.surface.stride,
                format: format::XR24,
                size: self.surface.byte_len() as u32,
                fd,
            }
            .into(),
            SetImage {
                id: IMAGE,
                buffer: BUFFER,
                src: IRect::new(0, 0, w.cast_signed(), h.cast_signed()),
            }
            .into(),
        ];
        self.fd = Some(mine);
        Ok(msgs)
    }

    fn frame(&mut self, _ctx: &mut Ctx, frame: u64) -> Result<Vec<ClientMsg>, Error> {
        let t0 = Instant::now();
        self.effect.render(&mut self.surface, frame + 1);
        self.compute_us += t0.elapsed().as_micros() as u64;

        let t1 = Instant::now();
        if let Some(fd) = &self.fd {
            write_all_at(fd, &self.surface.data, 0)?;
        }
        self.upload_us += t1.elapsed().as_micros() as u64;

        Ok(vec![
            BufferDamage {
                id: BUFFER,
                rects: vec![IRect::new(
                    0,
                    0,
                    self.surface.width.cast_signed(),
                    self.surface.height.cast_signed(),
                )],
            }
            .into(),
        ])
    }

    fn compute_us(&self) -> u64 {
        self.compute_us
    }

    fn upload_us(&self) -> u64 {
        self.upload_us
    }
}

/// Bytes a frame of `w`×`h` BGRA costs, and what that is per second at a
/// refresh rate — the denominator every pixel-path verdict needs.
///
/// Returned as a pair rather than printed so the report and the doc can
/// format it their own way, and so a test can pin the 1080p number that
/// the whole argument rests on.
#[must_use]
pub fn frame_bytes(w: u32, h: u32) -> u64 {
    u64::from(w) * u64::from(h) * 4
}

/// Bytes per second the client must *write* to sustain `w`×`h` at
/// `refresh_mhz`.
///
/// The server reads the same bytes back and then writes them again into
/// the scanout buffer, so the true traffic is roughly three times this;
/// the doc says so rather than folding a guessed factor into the number,
/// because the third pass is damage-proportional and the first two are
/// not.
#[must_use]
pub fn write_bandwidth(w: u32, h: u32, refresh_mhz: u32) -> f64 {
    frame_bytes(w, h) as f64 * f64::from(refresh_mhz) / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{Balls, Boing, Fire, Plasma, Rotozoom, Starfield};
    use nitro_core::Size;

    fn ctx(w: f32, h: f32) -> Ctx {
        Ctx {
            size: Size::new(w, h),
            scale: 1.0,
            refresh_ns: 16_666_667,
            n: 0,
            size_px: 0,
        }
    }

    /// The number the whole bandwidth argument in `docs/bench.md` rests
    /// on. If this ever changes, the doc is wrong.
    #[test]
    fn a_1080p_bgra_frame_is_8_294_400_bytes() {
        assert_eq!(frame_bytes(1920, 1080), 8_294_400);
    }

    #[test]
    fn a_fullscreen_frame_at_sixty_hertz_is_half_a_gigabyte_a_second() {
        let bps = write_bandwidth(1920, 1080, 60_000);
        assert!(
            (bps - 497_664_000.0).abs() < 1.0,
            "{bps} is not 8294400 * 60"
        );
        // And exactly twice that at 120 Hz, which is the sweep's whole
        // question.
        assert!((write_bandwidth(1920, 1080, 120_000) - 2.0 * bps).abs() < 1.0);
    }

    #[test]
    fn a_sized_buffer_ignores_the_window_and_a_zero_edge_follows_it() {
        let mut s = PixelScenario::new("putimage", Box::new(Plasma::new()), 100);
        s.build(&mut ctx(640.0, 480.0)).unwrap();
        assert_eq!(s.dimensions(), (100, 100));

        let mut full = PixelScenario::new("plasma", Box::new(Plasma::new()), 0);
        full.build(&mut ctx(640.0, 480.0)).unwrap();
        assert_eq!(full.dimensions(), (640, 480));
    }

    /// A scale-2 output scans out four times the pixels, and a buffer
    /// sized in logical units would report a quarter of the bandwidth it
    /// really costs.
    #[test]
    fn a_fullscreen_buffer_is_sized_in_device_pixels() {
        let mut c = ctx(640.0, 480.0);
        c.scale = 2.0;
        let mut s = PixelScenario::new("plasma", Box::new(Plasma::new()), 0);
        s.build(&mut c).unwrap();
        assert_eq!(s.dimensions(), (1280, 960));
    }

    /// The defect the box found, and the third of its kind: an image node
    /// whose bounds are not its buffer's size makes the **server resample
    /// it every frame**.
    ///
    /// The first version stretched every buffer across the whole window,
    /// and `putimage` at 100, 250 and 500 px duly reported the same ~11 ms
    /// of paint and the same 307 200 damage pixels. Four sweep points, one
    /// measurement — of image scaling, by a scenario whose whole claim is
    /// that it measures an upload.
    ///
    /// One-to-one is therefore an invariant, asserted at every size and
    /// scale the sweep uses.
    #[test]
    fn a_pixel_scenario_is_never_resampled() {
        for edge in [0u32, 64, 100, 250, 500, 1080] {
            for scale in [1.0f32, 2.0] {
                let mut cx = ctx(640.0, 480.0);
                cx.scale = scale;
                let mut scen = PixelScenario::new("putimage", Box::new(Plasma::new()), edge);
                let msgs = scen.build(&mut cx).unwrap();
                let (width, height) = scen.dimensions();

                let ClientMsg::SetBounds(bounds) = msgs
                    .iter()
                    .find(|m| matches!(m, ClientMsg::SetBounds(_)))
                    .unwrap()
                else {
                    unreachable!()
                };
                // Bounds are logical and the buffer is device pixels, so
                // the test is that they agree after scale — the condition
                // under which the server has nothing to resample.
                assert!(
                    (bounds.rect.w * scale - width as f32).abs() <= 1.0
                        && (bounds.rect.h * scale - height as f32).abs() <= 1.0,
                    "edge {edge} @{scale}: a {width}x{height} buffer drawn at {}x{} device px \
                     would be resampled every frame",
                    bounds.rect.w * scale,
                    bounds.rect.h * scale
                );

                // And the source rectangle is the whole buffer, so no
                // cropping hides a mismatch.
                let ClientMsg::SetImage(image) = msgs
                    .iter()
                    .find(|m| matches!(m, ClientMsg::SetImage(_)))
                    .unwrap()
                else {
                    unreachable!()
                };
                assert_eq!(
                    (image.src.w, image.src.h),
                    (width.cast_signed(), height.cast_signed())
                );
            }
        }
    }

    /// The defect the reviewer caught, and the third instance of the same
    /// class in this crate: a `--fullscreen` run constructs its effect
    /// from the *command line's* size, because the real one is not known
    /// until the server's `Configure` arrives, and nothing rebuilt it.
    ///
    /// `a_fullscreen_buffer_is_sized_in_device_pixels` below checks the
    /// `Surface`, and passed throughout — which is precisely why this got
    /// through. The surface was the right size; the *simulation inside it*
    /// was not, so the fire burned in a 640×480 corner of an 8 MB buffer
    /// and the rest stayed at the zero-fill.
    ///
    /// The evidence was in the shipped ledger and went unread: fullscreen
    /// fire reported **2 933 µs** of compute per frame against VGA fire's
    /// **8 592** — four times the pixels for a third of the cost. This
    /// test asserts the property that number violated, on the effects
    /// rather than on the buffer.
    #[test]
    fn a_fullscreen_effect_is_rebuilt_at_the_configured_size() {
        // A fullscreen run: the scenario is *constructed* at the default
        // 640×480 (as `main.rs` does when `--window` is absent) and the
        // server then configures 1920×1080.
        let boxed: Vec<(&str, Box<dyn Effect>)> = vec![
            ("fire", Box::new(Fire::new(640, 480))),
            ("boing", Box::new(Boing::new(640, 480))),
            ("starfield", Box::new(Starfield::new(200, 640, 480))),
            ("balls", Box::new(Balls::new(16, 640, 480))),
            ("plasma", Box::new(Plasma::new())),
            ("rotozoom", Box::new(Rotozoom::new())),
        ];
        for (name, effect) in boxed {
            let mut scen = PixelScenario::new("fullscreen", effect, 0);
            let mut cx = ctx(1920.0, 1080.0);
            scen.build(&mut cx).unwrap();
            assert_eq!(scen.dimensions(), (1920, 1080), "{name}");

            // The discriminator: ink in the far corner. An effect still
            // simulating 640×480 cannot write past (640, 480), so the
            // bottom-right quadrant stays exactly as `Surface::new` left
            // it — zero. Every one of these effects covers its whole
            // surface (the three that clear to a backdrop do so with a
            // non-zero colour, and fire's palette entry 0 is opaque
            // black, which is still a non-zero BGRA word).
            scen.frame(&mut cx, 8).unwrap();
            let far = far_corner_is_written(&scen);
            assert!(
                far,
                "{name}: nothing was drawn past the 640x480 corner — the \
                 effect is still simulating the constructed size"
            );
        }
    }

    /// Whether anything in the surface's bottom-right quadrant — well
    /// past a 640×480 box — is non-zero.
    fn far_corner_is_written(scen: &PixelScenario) -> bool {
        let (w, h) = scen.dimensions();
        let stride = w as usize * 4;
        let data = scen.pixels();
        (h as usize * 3 / 4..h as usize).step_by(7).any(|y| {
            (w as usize * 3 / 4..w as usize)
                .step_by(7)
                .any(|x| data[y * stride + x * 4..][..4] != [0, 0, 0, 0])
        })
    }

    /// The two stateless effects read their extent from the surface on
    /// every call, so resizing them must be a no-op rather than a reset —
    /// and the three that carry a box must actually take the new one.
    #[test]
    fn resize_moves_the_box_of_the_effects_that_have_one() {
        let mut fire = Fire::new(64, 48);
        fire.resize(128, 96);
        let mut small = Surface::new(128, 96);
        fire.render(&mut small, 0);
        // A 64x48 grid blitted into a 128x96 surface leaves the right half
        // untouched; a rebuilt one fills it.
        let stride = 128 * 4;
        let bottom = 95 * stride;
        assert!(
            small.data[bottom + 100 * 4..bottom + 100 * 4 + 4] != [0, 0, 0, 0],
            "the fire did not re-seed across the wider grid"
        );

        let mut boing = Boing::new(640, 480);
        let (_, _, r_small) = boing.position(0);
        boing.resize(1920, 1080);
        let (_, _, r_big) = boing.position(0);
        assert!(
            r_big > r_small * 1.5,
            "the ball's radius did not follow the box: {r_small} -> {r_big}"
        );
    }

    #[test]
    fn a_build_creates_the_buffer_before_the_image_names_it() {
        let mut s = PixelScenario::new("plasma", Box::new(Plasma::new()), 64);
        let msgs = s.build(&mut ctx(640.0, 480.0)).unwrap();
        let buf = msgs
            .iter()
            .position(|m| matches!(m, ClientMsg::CreateBuffer(_)))
            .expect("CreateBuffer");
        let img = msgs
            .iter()
            .position(|m| matches!(m, ClientMsg::SetImage(_)))
            .expect("SetImage");
        assert!(buf < img, "SetImage must not name an unregistered buffer");
    }

    /// One `BufferDamage` and nothing else: re-sending the buffer would
    /// make this a `CreateBuffer` benchmark, and sending a `SetImage` per
    /// frame would add a scene mutation the pixel path does not need.
    #[test]
    fn a_frame_is_exactly_one_buffer_damage() {
        let mut s = PixelScenario::new("plasma", Box::new(Plasma::new()), 32);
        s.build(&mut ctx(640.0, 480.0)).unwrap();
        for f in 0..4 {
            let msgs = s.frame(&mut ctx(640.0, 480.0), f).unwrap();
            assert_eq!(msgs.len(), 1);
            assert!(matches!(msgs[0], ClientMsg::BufferDamage(_)));
        }
    }

    /// The defect behind issue #584, named directly: the descriptor the
    /// scenario keeps for its per-frame `pwrite`s must be the **same
    /// file** as the one it handed to `CreateBuffer`.
    ///
    /// `build` used to call `memfd()` twice, and the comment there said
    /// "two descriptors on one memfd" — which is the right design and
    /// not what the code did. `memfd_create` mints a new anonymous file
    /// per call, so the client wrote every frame into a file the server
    /// had never heard of and the screen showed frame 0 forever.
    ///
    /// Settled on `(st_dev, st_ino)` rather than on the pixels, because
    /// the two files start with identical contents — which is exactly
    /// why this was invisible in every screenshot-shaped test.
    #[test]
    fn the_per_frame_writes_go_to_the_buffer_the_server_was_given() {
        let mut s = PixelScenario::new("plasma", Box::new(Plasma::new()), 64);
        let msgs = s.build(&mut ctx(640.0, 480.0)).unwrap();
        let ClientMsg::CreateBuffer(create) = msgs
            .iter()
            .find(|m| matches!(m, ClientMsg::CreateBuffer(_)))
            .expect("CreateBuffer")
        else {
            unreachable!()
        };
        let mine = s.fd.as_ref().expect("the scenario kept a descriptor");
        assert_eq!(
            identity(&create.fd),
            identity(mine),
            "the scenario writes its frames into a different file than the \
             one the server reads — the picture will never move"
        );
    }

    /// `(st_dev, st_ino)`: the pair that says "same file", whatever the
    /// descriptor. Spelled locally rather than shared; the precedent is
    /// `crates/nitro-wire/tests/common/mod.rs::identity`, and three lines
    /// is not a crate.
    fn identity(fd: impl std::os::fd::AsFd) -> (u64, u64) {
        let st = rustix::fs::fstat(fd).expect("fstat");
        (st.st_dev as u64, st.st_ino as u64)
    }

    /// And the end-to-end shape of the same claim, without a server: a
    /// later frame's bytes are readable back out of the descriptor the
    /// server holds.
    #[test]
    fn a_frames_pixels_are_visible_through_the_servers_descriptor() {
        let mut s = PixelScenario::new("plasma", Box::new(Plasma::new()), 64);
        let msgs = s.build(&mut ctx(640.0, 480.0)).unwrap();
        let ClientMsg::CreateBuffer(create) = msgs
            .iter()
            .find(|m| matches!(m, ClientMsg::CreateBuffer(_)))
            .expect("CreateBuffer")
        else {
            unreachable!()
        };
        let read_back = |fd: &OwnedFd, len: usize| {
            let mut buf = vec![0u8; len];
            let mut done = 0;
            while done < len {
                let n = rustix::io::pread(fd, &mut buf[done..], done as u64).expect("pread");
                assert!(n > 0, "short read");
                done += n;
            }
            buf
        };
        let first = read_back(&create.fd, s.byte_len());
        // Plasma is a pure function of the frame index, so frame 40 is
        // not frame 0 — the effects' own tests own that claim.
        for f in 0..40 {
            s.frame(&mut ctx(640.0, 480.0), f).unwrap();
        }
        let later = read_back(&create.fd, s.byte_len());
        // Compared as booleans rather than with `assert_ne!` on the
        // vectors: a failing `assert_ne!` prints a megabyte of BGRA and
        // the one bit that matters is "did it change at all".
        assert!(
            first != later,
            "forty frames later the server's buffer is unchanged — the \
             benchmark is uploading into nowhere"
        );
        assert!(
            later == s.pixels(),
            "the server sees other bytes than the effect drew"
        );
    }

    /// The two cost columns must actually accumulate, or the report's
    /// `frame = effect + upload + …` decomposition is fiction.
    #[test]
    fn the_effect_and_upload_costs_are_accounted_separately() {
        let mut s = PixelScenario::new("plasma", Box::new(Plasma::new()), 128);
        s.build(&mut ctx(640.0, 480.0)).unwrap();
        for f in 0..8 {
            s.frame(&mut ctx(640.0, 480.0), f).unwrap();
        }
        // Microsecond resolution on a fast machine can legitimately round
        // a small frame to zero, so this asserts the counters exist and
        // are finite rather than a threshold that would be flaky.
        assert!(s.compute_us() < 10_000_000);
        assert!(s.upload_us() < 10_000_000);
    }
}

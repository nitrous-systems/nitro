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
        self.effect.render(&mut self.surface, 0);
        let fd = memfd(&self.surface.data)?;
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
        // The descriptor moved into the message; open a second one on the
        // same pixels for the per-frame writes. Two descriptors on one
        // memfd is the ordinary way to do this — the server holds its own
        // for `pread`, and re-sending the buffer every frame would be the
        // alternative, which is the thing this scenario exists not to do.
        self.fd = Some(memfd(&self.surface.data)?);
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
    use crate::effects::Plasma;
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

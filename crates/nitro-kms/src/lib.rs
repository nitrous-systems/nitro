//! `nitro-kms` — the display backend of the nitro server.
//!
//! Two implementations of one trait:
//!
//! - [`DrmBackend`]: atomic KMS over an already-open DRM fd. CPU-writable
//!   dumb buffers (two per output), `NONBLOCK` page flips with vblank
//!   events, `FB_DAMAGE_CLIPS`, hotplug via a raw kernel uevent netlink
//!   socket. No `unsafe`, no libdrm, no libudev.
//! - [`FakeBackend`]: the same contract over plain memory with a
//!   timerfd-driven vblank, so the server and its tests run headless.
//!
//! The server codes against [`Backend`] only and never sees a DRM type.
//! Everything is single-threaded and non-blocking: a backend hands out the
//! fds it wants watched ([`Backend::poll_fds`]) and the caller invokes
//! [`Backend::dispatch`] when any of them is readable.
//!
//! Pixel format is `XRGB8888` or `ARGB8888` — the same bytes either way
//! (little-endian `u32` per pixel: `0xAARRGGBB`); byte 3 is ignored for
//! `XRGB8888` and is premultiplied alpha when the output scans out
//! `ARGB8888` ([`Backend::set_scanout_alpha`]). Stride is in bytes and
//! not necessarily `width * 4`.
//!
//! See `README.md` for the contract in prose: back-buffer borrow rules,
//! `flip_pending`, pause/resume, and the exact commit sequence.

pub mod drm;
pub mod fake;
pub mod planes;
pub mod uevent;

pub use crate::drm::select::ModeCandidate;
pub use crate::drm::{DrmBackend, DrmFd, DrmOptions, ModeRequest, Modeline};
pub use crate::fake::{FakeBackend, FakeOutputSpec, FakePlaneSpec, TestRecord};
pub use crate::planes::{
    BufferId, ColorEncoding, ColorRange, Fourcc, ImportDesc, MOD_LINEAR, PlaneAssignment,
    PlaneConfig, PlaneId, PlaneInfo, PlaneKind, PlaneSource, ScanoutBufferInfo, SrcRect, Verdict,
    Zpos,
};

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::time::Duration;

/// Bytes per pixel of the output formats (`XRGB8888` / `ARGB8888`).
pub const BYTES_PER_PIXEL: u32 = 4;

/// Identifies one output (a connector driven by a CRTC) for the lifetime
/// of the backend. Ids are never reused, even after hotplug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub u32);

impl fmt::Display for OutputId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "output#{}", self.0)
    }
}

/// A connected output with its chosen mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// Stable id, see [`OutputId`].
    pub id: OutputId,
    /// Connector name as the kernel spells it, e.g. `HDMI-A-1`.
    pub name: String,
    /// Mode width in pixels.
    pub width: u32,
    /// Mode height in pixels.
    pub height: u32,
    /// Vertical refresh in millihertz (`60_000` = 60 Hz).
    pub refresh_mhz: u32,
    /// Physical size in millimetres, `(0, 0)` when unknown.
    pub phys_mm: (u32, u32),
    /// The mode came from a user-supplied modeline rather than the
    /// connector's own list.
    ///
    /// Reported rather than kept private because it changes what a number
    /// on screen *means*: a custom mode was never validated against the
    /// monitor's EDID, so "the panel is dark" is a possible and expected
    /// outcome, and the `outputs` line says `(custom)` so whoever is
    /// reading it knows which question to ask.
    pub custom_mode: bool,
}

/// An axis-aligned rectangle in output pixel space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
}

impl Rect {
    /// Construct a rectangle.
    #[must_use]
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// True when the rectangle covers no pixels.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Clip to `[0, width) × [0, height)`. Returns `None` when nothing is
    /// left.
    #[must_use]
    pub fn clipped_to(&self, width: u32, height: u32) -> Option<Rect> {
        let x1 = i64::from(self.x).max(0);
        let y1 = i64::from(self.y).max(0);
        let x2 = (i64::from(self.x) + i64::from(self.w)).min(i64::from(width));
        let y2 = (i64::from(self.y) + i64::from(self.h)).min(i64::from(height));
        if x2 <= x1 || y2 <= y1 {
            return None;
        }
        Some(Rect {
            x: x1 as i32,
            y: y1 as i32,
            w: (x2 - x1) as u32,
            h: (y2 - y1) as u32,
        })
    }
}

/// A CPU-readable copy of a front buffer: `XRGB8888`/`ARGB8888` bytes,
/// tightly packed (`stride == width * 4`). Byte 3 is copied verbatim;
/// it is premultiplied alpha when the output scans out `ARGB8888` (see
/// [`Image::alpha`]) and meaningless otherwise, which is why
/// [`Image::pixel`] masks it off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row stride in bytes; always `width * 4` for an `Image`.
    pub stride: u32,
    /// Pixel data, `height * stride` bytes.
    pub data: Vec<u8>,
}

impl Image {
    /// Pixel at `(x, y)` as `0x00RRGGBB`: byte 3 (alpha) is masked off,
    /// see [`Image::alpha`] for it.
    ///
    /// # Panics
    /// If `(x, y)` is outside the image.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        let o = self.offset(x, y);
        u32::from_le_bytes([self.data[o], self.data[o + 1], self.data[o + 2], 0])
    }

    /// Byte 3 of the pixel at `(x, y)`: premultiplied alpha when the
    /// output scans out `ARGB8888`, whatever was written otherwise.
    ///
    /// # Panics
    /// If `(x, y)` is outside the image.
    #[must_use]
    pub fn alpha(&self, x: u32, y: u32) -> u8 {
        self.data[self.offset(x, y) + 3]
    }

    fn offset(&self, x: u32, y: u32) -> usize {
        assert!(x < self.width && y < self.height, "pixel out of bounds");
        (y * self.stride + x * BYTES_PER_PIXEL) as usize
    }

    /// Write the image as a binary PPM (`P6`), dropping byte 3 (alpha).
    ///
    /// # Errors
    /// Any I/O error while writing.
    pub fn write_ppm(&self, path: impl AsRef<std::path::Path>) -> io::Result<()> {
        use std::io::Write as _;
        let mut out = io::BufWriter::new(std::fs::File::create(path)?);
        write!(out, "P6\n{} {}\n255\n", self.width, self.height)?;
        for y in 0..self.height {
            let row =
                &self.data[(y * self.stride) as usize..][..(self.width * BYTES_PER_PIXEL) as usize];
            for px in row.chunks_exact(BYTES_PER_PIXEL as usize) {
                out.write_all(&[px[2], px[1], px[0]])?;
            }
        }
        out.flush()
    }
}

/// A mutable borrow of an output's back buffer for CPU rendering.
///
/// Rows are `stride` bytes apart; only the first `width * 4` bytes of each
/// row are visible. The memory is typically write-combined: write whole
/// rows sequentially, never read back from it (use
/// [`Backend::read_front`] for that).
#[derive(Debug)]
pub struct BufferMut<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: u32,
    /// `height * stride` bytes of `XRGB8888`/`ARGB8888` (same layout; byte
    /// 3 is premultiplied alpha when the output scans out `ARGB8888`,
    /// ignored otherwise — write 255 there unless punching a hole).
    pub data: &'a mut [u8],
}

impl BufferMut<'_> {
    /// Fill a rectangle (clipped to the buffer) with one `0xAARRGGBB`
    /// colour, written verbatim (byte 3 included).
    pub fn fill_rect(&mut self, rect: Rect, color: u32) {
        let Some(r) = rect.clipped_to(self.width, self.height) else {
            return;
        };
        let px = color.to_le_bytes();
        for y in r.y as u32..r.y as u32 + r.h {
            let start = (y * self.stride + r.x as u32 * BYTES_PER_PIXEL) as usize;
            let row = &mut self.data[start..start + (r.w * BYTES_PER_PIXEL) as usize];
            for dst in row.chunks_exact_mut(BYTES_PER_PIXEL as usize) {
                dst.copy_from_slice(&px);
            }
        }
    }
}

/// Something the backend wants the server to know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A previously committed buffer is now being scanned out.
    Flipped {
        /// Which output.
        output: OutputId,
        /// The kernel's vblank counter for the flip (32-bit on DRM,
        /// widened; monotonically increasing per output on the fake).
        sequence: u64,
        /// `CLOCK_MONOTONIC` timestamp of the vblank.
        time: Duration,
    },
    /// A connector changed state; call [`Backend::rescan`].
    Hotplug,
}

/// Errors from a backend. Every variant names what went wrong in terms of
/// the backend contract, not the ioctl.
#[derive(Debug)]
pub enum Error {
    /// A system call failed. `op` says which one, in KMS terms.
    Io {
        /// What we were doing, e.g. `"atomic commit"`.
        op: &'static str,
        /// The underlying error.
        source: io::Error,
    },
    /// The device does not support atomic modesetting (or universal planes).
    Unsupported(&'static str),
    /// A KMS object lacks a property this crate needs.
    MissingProperty {
        /// `"connector"`, `"crtc"` or `"plane"`.
        object: &'static str,
        /// The property name.
        name: &'static str,
    },
    /// No such output (stale id after hotplug).
    NoSuchOutput(OutputId),
    /// A commit is in flight; the back buffer is not writable yet.
    FlipPending(OutputId),
    /// The backend is paused (session inactive); commits are refused.
    Paused,
    /// A connector is connected but no CRTC + primary plane could be found
    /// for it. Carries the connector name.
    NoCrtc(String),
    /// The output has not been committed yet, so its CRTC is not ours
    /// and a `TEST_ONLY` layout on it would answer the wrong question.
    NotLit(OutputId),
    /// No such plane on this output, or no such scanout buffer.
    NoSuchObject(&'static str, u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { op, source } => write!(f, "{op}: {source}"),
            Error::Unsupported(what) => write!(f, "device does not support {what}"),
            Error::MissingProperty { object, name } => {
                write!(f, "{object} has no `{name}` property")
            }
            Error::NoSuchOutput(id) => write!(f, "no such output: {id}"),
            Error::FlipPending(id) => write!(f, "{id}: commit already in flight"),
            Error::Paused => write!(f, "backend is paused"),
            Error::NoCrtc(name) => write!(f, "{name}: no free CRTC + primary plane"),
            Error::NotLit(id) => write!(f, "{id}: not lit yet (commit a frame first)"),
            Error::NoSuchObject(what, id) => write!(f, "no such {what}: {id}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Error {
    pub(crate) fn io(op: &'static str) -> impl FnOnce(io::Error) -> Error {
        move |source| Error::Io { op, source }
    }
}

/// The display backend contract. Object-safe; the server may hold a
/// `Box<dyn Backend>` or be generic over it.
///
/// Per output the backend owns two buffers. Exactly one is *front* (the
/// last one committed) and one is *back*. [`Backend::commit`] hands the
/// back buffer to the display and swaps the roles once the flip completes
/// (`Event::Flipped`). Between `commit` and `Flipped` neither buffer may be
/// written and [`Backend::back_buffer`] fails with
/// [`Error::FlipPending`]. The back buffer contains what was on screen two
/// frames ago, so callers must accumulate damage over two frames or repaint
/// fully.
pub trait Backend {
    /// Connected outputs with their chosen mode. Stable between calls
    /// unless [`Backend::rescan`] reported a change.
    fn outputs(&self) -> &[OutputInfo];

    /// Borrow the back buffer of `output` for CPU writes.
    ///
    /// # Errors
    /// [`Error::FlipPending`] while a commit is in flight,
    /// [`Error::NoSuchOutput`] for a stale id.
    fn back_buffer(&mut self, output: OutputId) -> Result<BufferMut<'_>, Error>;

    /// Atomically present the back buffer. `damage` lists the rectangles
    /// changed since the buffer was last on screen (output space; an empty
    /// slice means "everything"). Completes asynchronously with
    /// [`Event::Flipped`].
    ///
    /// The planes show the layout staged with [`Backend::set_plane_state`]
    /// (by default: this buffer full-screen on the primary), with any
    /// fences from [`Backend::set_plane_fence`]. A rejected commit is
    /// `Err` and changes nothing: the previous picture stays on screen,
    /// the buffers keep their roles, the staged layout stays staged (the
    /// caller changes it, typically back to the default) and the fences
    /// are dropped.
    ///
    /// # Errors
    /// [`Error::FlipPending`] if one is already in flight, [`Error::Paused`]
    /// while paused, [`Error::Io`] if the kernel rejects the commit.
    fn commit(&mut self, output: OutputId, damage: &[Rect]) -> Result<(), Error>;

    /// Whether a commit is in flight for `output` (`false` for unknown ids).
    fn flip_pending(&self, output: OutputId) -> bool;

    /// The fds to watch for readability. Re-query after `rescan`.
    fn poll_fds(&self) -> Vec<BorrowedFd<'_>>;

    /// Drain everything readable and append the resulting events to
    /// `events`. Never blocks; safe to call when nothing is pending.
    ///
    /// # Errors
    /// [`Error::Io`] on a read failure other than "would block".
    fn dispatch(&mut self, events: &mut Vec<Event>) -> Result<(), Error>;

    /// Re-probe connectors and modes (after `Event::Hotplug` or a session
    /// resume). Returns whether [`Backend::outputs`] changed. Outputs that
    /// vanished release their buffers. A new one is not modeset here: its
    /// first [`Backend::commit`] lights it, with that frame, as it does
    /// every output. Outputs already lit are modeset again unless paused,
    /// in which case [`Backend::resume`] does it. That modeset drops
    /// every lit output's plane layout back to the default.
    ///
    /// # Errors
    /// [`Error::Io`] if enumeration fails.
    fn rescan(&mut self) -> Result<bool, Error>;

    /// The session went inactive (VT switch). Stop committing; keep all
    /// state, including a flip already in flight: it stays pending here,
    /// and if the kernel does deliver its completion while the session is
    /// away, [`Backend::dispatch`] retires it in the usual way. Nothing is
    /// stranded if it never arrives, because [`Backend::resume`] clears
    /// the flag unconditionally.
    fn pause(&mut self);

    /// The session is active again. Re-modesets every output (DRM master
    /// may have been revoked and re-granted; CRTC state does not survive
    /// that, buffers do). An output that has not been committed yet has
    /// nothing of its own to restore, and is left to its first
    /// [`Backend::commit`].
    ///
    /// the caller must repaint fully. The modeset also drops every
    /// output's staged plane layout back to the default (see
    /// [`Backend::set_plane_state`]).
    ///
    /// **Post-condition:** any flip in flight is abandoned, so
    /// [`Backend::flip_pending`] is false for every output afterwards and
    /// the caller must repaint fully. This is a promise, not an
    /// observation about the kernel: correctness must not depend on a
    /// page-flip event still being delivered on the fd after DRM master
    /// was revoked and re-granted. Today's kernel does deliver it, which
    /// is exactly why the guarantee is worth stating — without it the
    /// server's next paint would be turned away with
    /// [`Error::FlipPending`] for a completion that may never come, and
    /// the output would stall until the next resume or hotplug. A stale
    /// event arriving later finds nothing pending and retires nothing.
    ///
    /// # Errors
    /// [`Error::Io`] if the modeset is rejected.
    fn resume(&mut self) -> Result<(), Error>;

    /// A tightly packed copy of the front buffer — the most recently
    /// committed one, whether or not its flip has completed.
    ///
    /// # Errors
    /// [`Error::NoSuchOutput`] for a stale id.
    fn read_front(&mut self, output: OutputId) -> Result<Image, Error>;

    /// Simulate a connector appearing, for a backend that can.
    ///
    /// Returns `false` on a real backend, where an output exists because
    /// a connector reports a mode and inventing one would mean lying to
    /// the modesetting code. [`FakeBackend`](crate::fake::FakeBackend)
    /// implements it, which is what lets a server test drive the "no
    /// output yet, then one appears" state in-process.
    ///
    /// A narrow hook rather than an `Any` downcast on purpose: the DRM
    /// backend borrows its device, so it is not `'static` and cannot be
    /// downcast at all, and a one-method escape hatch is easier to reason
    /// about than a general one.
    fn simulate_plug(&mut self, _width: u32, _height: u32) -> bool {
        false
    }

    /// Simulate a connector disappearing, for a backend that can.
    ///
    /// The other half of [`Backend::simulate_plug`], and the one a window
    /// manager cares about more: output *removal* is what orphans windows
    /// and forces them to migrate. Returns `false` on a real backend, and
    /// on a fake one with no outputs to remove.
    fn simulate_unplug(&mut self) -> bool {
        false
    }

    /// Re-apply the per-connector mode configuration, returning whether
    /// [`Backend::outputs`] changed.
    ///
    /// A **full modeset**, not a property flip: the CRTC is retimed and
    /// the screen blanks for the duration. That is why it is a separate
    /// method rather than something `rescan` reads — the caller decides
    /// when it is worth doing, and the answer is "when the configuration
    /// actually changed", which is what an unchanged map returning `false`
    /// without touching the hardware enforces.
    ///
    /// The default is `Ok(false)`: a backend with no modes to set (the
    /// fake one) has nothing to do, and saying so is not an error.
    ///
    /// # Errors
    /// [`Error::Io`] if the re-probe or the modeset fails.
    fn set_modes(&mut self, _modes: &HashMap<String, ModeRequest>) -> Result<bool, Error> {
        Ok(false)
    }

    /// Take whatever the backend has to say about the mode configuration.
    ///
    /// A request that matched no listed mode, or a modeline the kernel
    /// refused: both are non-fatal — the output comes up on its default
    /// mode — and both are things the user has to be told, because they
    /// wrote a line that did not do what it says. This crate has no
    /// logger, so the sentences come out here and the caller logs them.
    fn take_warnings(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// Every mode `output`'s connector lists, in the kernel's own order.
    ///
    /// What `output.<connector>.mode` may choose from, and therefore what
    /// a user needs to see before writing that line — the server's `modes`
    /// control command is this list and nothing else. An unknown id gives
    /// an empty list rather than an error: it is a question, not a
    /// command.
    fn available_modes(&self, _output: OutputId) -> Vec<ModeCandidate> {
        Vec::new()
    }

    /// Every plane that can go on `output`'s CRTC, primary first, then
    /// overlays, then cursors (each group by id).
    ///
    /// Read once per [`Backend::rescan`], so this is cheap. An unknown
    /// id, or a backend that does not report planes, gives an empty list.
    fn planes(&self, _output: OutputId) -> Vec<PlaneInfo> {
        Vec::new()
    }

    /// Allocate a linear scanout buffer of `format` (`XRGB8888`,
    /// `ARGB8888`, `YUYV`/`UYVY` or `NV12`) for use in a
    /// [`PlaneAssignment`]. Contents are unspecified (zeroed today).
    ///
    /// # Errors
    /// [`Error::Unsupported`] on a backend without scanout buffers or for
    /// a format it cannot allocate; [`Error::Io`] when the kernel refuses
    /// the buffer or its framebuffer (i915 checks the format at `AddFB2`,
    /// so an unsupported format can fail here, before any test).
    fn alloc_buffer(
        &mut self,
        _format: Fourcc,
        _width: u32,
        _height: u32,
    ) -> Result<BufferId, Error> {
        Err(Error::Unsupported("scanout buffers"))
    }

    /// Free a buffer from [`Backend::alloc_buffer`]. Unknown ids are
    /// ignored. A buffer that a committed layout still scans out (on
    /// screen or in flight) is freed only once the flip that stops
    /// using it completes; the id is unusable from this call on, and is
    /// then not reported by [`Backend::take_released_buffers`].
    fn free_buffer(&mut self, _id: BufferId) {}

    /// The memory layout of a scanout buffer; `None` for an unknown or
    /// freed id.
    fn buffer_info(&self, _id: BufferId) -> Option<ScanoutBufferInfo> {
        None
    }

    /// A new fd for the buffer's memory, mappable read-write by whoever
    /// fills it (a DRM PRIME dma-buf, `O_RDWR | O_CLOEXEC`; a sealed
    /// memfd on the fake backend, where the `DMA_BUF_IOCTL_SYNC` ioctl
    /// fails with `ENOTTY`). Every export shares the same memory. Layout:
    /// [`Backend::buffer_info`].
    ///
    /// # Errors
    /// [`Error::NoSuchObject`] for an unknown or freed id,
    /// [`Error::Unsupported`] on a backend without scanout buffers,
    /// [`Error::Io`] if the export fails.
    fn export_buffer(&mut self, _id: BufferId) -> Result<OwnedFd, Error> {
        Err(Error::Unsupported("buffer export"))
    }

    /// Import a client's dma-buf (one fd per plane; fds may be dups of one
    /// buffer) as a framebuffer usable in a [`PlaneAssignment`]. The
    /// caller keeps its fds.
    ///
    /// The id shares the space of [`Backend::alloc_buffer`]:
    /// [`Backend::free_buffer`] (deferred while on screen),
    /// [`Backend::buffer_info`] (the first two planes of `desc`, `size`
    /// 0) and [`Backend::take_released_buffers`] work the same.
    /// [`Backend::export_buffer`] of an import is
    /// [`Error::Unsupported`].
    ///
    /// # Errors
    /// [`Error::Unsupported`] by default, for a bad shape (`planes` not
    /// in 1..=4 or not `fds.len()`, zero width or height), or a
    /// format+modifier that is not scanout-able; [`Error::Io`] when the
    /// kernel refuses (PRIME import, `AddFB2`).
    fn import_buffer(
        &mut self,
        _desc: &ImportDesc,
        _fds: &[BorrowedFd<'_>],
    ) -> Result<BufferId, Error> {
        Err(Error::Unsupported("dma-buf import"))
    }

    /// `dev_t` (`st_rdev`) of the KMS device, for Wayland-style dmabuf
    /// feedback `main_device`. `None` when unknown (default, fake).
    fn device_id(&self) -> Option<u64> {
        None
    }

    /// Stage the **whole CRTC layout** for `output`'s next
    /// [`Backend::commit`] or [`Backend::commit_planes`]. Every plane on
    /// the CRTC that is not listed is disabled, the primary included (a
    /// layout without the primary turns it off). Empty is the default:
    /// the output buffer full-screen on the primary.
    /// [`PlaneSource::OutputFront`] is the output buffer the commit flips
    /// to, scanned out as `ARGB8888` while scanout alpha is on.
    ///
    /// The layout persists across commits until changed, **or until a
    /// modeset drops every lit output back to the default**: `resume`, a
    /// `rescan` or `set_modes` that changed anything, and the first
    /// commit of another output (which lights it with a modeset of every
    /// lit output). The buffers such a reset stops using show up in
    /// [`Backend::take_released_buffers`], which is how a caller notices;
    /// it then re-decides and repaints fully.
    ///
    /// Nothing is checked against the display engine here: ask
    /// [`Backend::test_layout`] first, and expect `commit` to fail if the
    /// kernel refuses.
    ///
    /// # Errors
    /// [`Error::NoSuchOutput`], [`Error::Paused`], [`Error::NotLit`]
    /// before the output's first commit, [`Error::NoSuchObject`] for a
    /// plane that is not this output's or an unknown/freed buffer,
    /// [`Error::Unsupported`] on a backend without planes (a non-empty
    /// layout).
    fn set_plane_state(&mut self, _output: OutputId, layout: &[PlaneConfig]) -> Result<(), Error> {
        if layout.is_empty() {
            Ok(())
        } else {
            Err(Error::Unsupported("plane layouts"))
        }
    }

    /// Commit the staged layout with the **current** front buffer: no
    /// swap, no back-buffer rotation, full damage. What a video frame on
    /// a plane uses, so it flips without the caller painting or copying
    /// the output buffer. Delivers [`Event::Flipped`], and
    /// `flip_pending`/[`Error::FlipPending`] behave as for
    /// [`Backend::commit`]; so does a rejection.
    ///
    /// # Errors
    /// As [`Backend::commit`], plus [`Error::NotLit`] before the output's
    /// first commit and [`Error::Unsupported`] on a backend without
    /// planes.
    fn commit_planes(&mut self, _output: OutputId) -> Result<(), Error> {
        Err(Error::Unsupported("plane commits"))
    }

    /// Give `plane` a `sync_file` to wait on (`IN_FENCE_FD`) in the next
    /// commit, which consumes it whether it succeeds or not. Optional:
    /// CPU-written buffers need none. A second fence for the same plane
    /// replaces the first; a fence for a plane the commit does not show
    /// is dropped.
    ///
    /// # Errors
    /// [`Error::NoSuchOutput`], [`Error::NoSuchObject`] for a plane that
    /// is not this output's, [`Error::Unsupported`] when the plane has no
    /// `IN_FENCE_FD` (or the backend no planes).
    fn set_plane_fence(
        &mut self,
        _output: OutputId,
        _plane: PlaneId,
        _fence: OwnedFd,
    ) -> Result<(), Error> {
        Err(Error::Unsupported("IN_FENCE_FD"))
    }

    /// Buffers the screen stopped reading, each reported once: after the
    /// [`Event::Flipped`] of the commit that stopped using it has been
    /// dispatched, or after a modeset reset the layout. Call it after
    /// [`Backend::dispatch`] (and after `resume`/`rescan`/`set_modes`/a
    /// first commit). Buffers already passed to `free_buffer` are
    /// destroyed then instead, and not reported.
    fn take_released_buffers(&mut self) -> Vec<BufferId> {
        Vec::new()
    }

    /// Ask whether `layout` would work as `output`'s next commit, without
    /// touching the screen (`DRM_MODE_ATOMIC_TEST_ONLY`).
    ///
    /// The layout describes the **whole** CRTC: every plane that can go
    /// on it and is not listed is disabled in the test, the primary
    /// included. No `ALLOW_MODESET`, so a layout that would need a
    /// modeset is rejected — the truth for a decision taken at flip time.
    ///
    /// `Ok(Rejected(errno))` means the display engine said no. `Err`
    /// means the question could not be asked.
    ///
    /// # Errors
    /// [`Error::Unsupported`] on a backend without planes,
    /// [`Error::NoSuchOutput`], [`Error::NotLit`] before the output's
    /// first commit, [`Error::Paused`], [`Error::NoSuchObject`] for a
    /// plane that is not this output's or an unknown buffer.
    fn test_layout(
        &mut self,
        _output: OutputId,
        _layout: &[PlaneAssignment<'_>],
    ) -> Result<Verdict, Error> {
        Err(Error::Unsupported("plane layouts"))
    }

    /// Whether `output`'s primary plane can scan out `ARGB8888`, i.e.
    /// whether [`Backend::set_scanout_alpha`]`(output, true)` can succeed.
    /// `false` for an unknown id. Default `false`.
    fn scanout_alpha(&self, _output: OutputId) -> bool {
        false
    }

    /// Scan `output`'s buffers out as `ARGB8888` (premultiplied alpha in
    /// byte 3, so a plane below the primary shows through where it is
    /// below 255) instead of `XRGB8888`. Same buffers, same bytes; only
    /// the framebuffer format changes. Takes effect at the next
    /// [`Backend::commit`] (or modeset). Turning it off is always
    /// allowed.
    ///
    /// # Errors
    /// [`Error::Unsupported`] when turning it on and
    /// [`Backend::scanout_alpha`] is false, [`Error::NoSuchOutput`] for a
    /// stale id, [`Error::Io`] if the kernel refuses the `ARGB8888`
    /// framebuffer.
    fn set_scanout_alpha(&mut self, _output: OutputId, on: bool) -> Result<(), Error> {
        if on {
            Err(Error::Unsupported("ARGB8888 scanout"))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_clipping() {
        assert_eq!(
            Rect::new(-5, -5, 10, 10).clipped_to(100, 100),
            Some(Rect::new(0, 0, 5, 5))
        );
        assert_eq!(
            Rect::new(95, 95, 10, 10).clipped_to(100, 100),
            Some(Rect::new(95, 95, 5, 5))
        );
        assert_eq!(Rect::new(100, 0, 10, 10).clipped_to(100, 100), None);
        assert_eq!(Rect::new(0, 0, 0, 10).clipped_to(100, 100), None);
        assert_eq!(
            Rect::new(0, 0, 1000, 1000).clipped_to(100, 100),
            Some(Rect::new(0, 0, 100, 100))
        );
    }

    #[test]
    fn buffer_fill_respects_stride() {
        let mut data = vec![0u8; 3 * 16];
        let mut buf = BufferMut {
            width: 2,
            height: 3,
            stride: 16,
            data: &mut data,
        };
        buf.fill_rect(Rect::new(1, 1, 5, 1), 0x00AA_BBCC);
        let img = Image {
            width: 2,
            height: 3,
            stride: 16,
            data,
        };
        assert_eq!(img.pixel(0, 1), 0);
        assert_eq!(img.pixel(1, 1), 0x00AA_BBCC);
        assert_eq!(img.pixel(1, 0), 0);
        assert_eq!(img.pixel(1, 2), 0);
        // padding bytes untouched
        assert_eq!(&img.data[16 + 8..32], &[0u8; 8]);
    }

    #[test]
    fn image_pixel_masks_alpha_and_alpha_reads_it() {
        let img = Image {
            width: 2,
            height: 1,
            stride: 8,
            data: vec![0x33, 0x22, 0x11, 0xFF, 0x66, 0x55, 0x44, 0x00],
        };
        assert_eq!(img.pixel(0, 0), 0x0011_2233);
        assert_eq!(img.pixel(1, 0), 0x0044_5566);
        assert_eq!(img.alpha(0, 0), 0xFF);
        assert_eq!(img.alpha(1, 0), 0);
    }

    #[test]
    #[should_panic(expected = "pixel out of bounds")]
    fn image_alpha_out_of_bounds_panics() {
        let img = Image {
            width: 1,
            height: 1,
            stride: 4,
            data: vec![0; 4],
        };
        let _ = img.alpha(1, 0);
    }

    #[test]
    fn error_display_names_property() {
        let e = Error::MissingProperty {
            object: "plane",
            name: "FB_ID",
        };
        assert_eq!(e.to_string(), "plane has no `FB_ID` property");
    }
}

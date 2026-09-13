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
//! Pixel format is always `XRGB8888` (little-endian `u32` per pixel:
//! `0x00RRGGBB`), stride in bytes and not necessarily `width * 4`.
//!
//! See `README.md` for the contract in prose: back-buffer borrow rules,
//! `flip_pending`, pause/resume, and the exact commit sequence.

pub mod drm;
pub mod fake;
pub mod uevent;

pub use crate::drm::{DrmBackend, DrmFd, DrmOptions};
pub use crate::fake::{FakeBackend, FakeOutputSpec};

use std::fmt;
use std::io;
use std::os::fd::BorrowedFd;
use std::time::Duration;

/// Bytes per pixel of the only format this crate speaks (`XRGB8888`).
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

/// A CPU-readable copy of a front buffer: `XRGB8888`, tightly packed
/// (`stride == width * 4`).
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
    /// Pixel at `(x, y)` as `0x00RRGGBB`.
    ///
    /// # Panics
    /// If `(x, y)` is outside the image.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        assert!(x < self.width && y < self.height, "pixel out of bounds");
        let o = (y * self.stride + x * BYTES_PER_PIXEL) as usize;
        u32::from_le_bytes([
            self.data[o],
            self.data[o + 1],
            self.data[o + 2],
            self.data[o + 3],
        ])
    }

    /// Write the image as a binary PPM (`P6`), dropping the unused byte.
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
    /// `height * stride` bytes of `XRGB8888`.
    pub data: &'a mut [u8],
}

impl BufferMut<'_> {
    /// Fill a rectangle (clipped to the buffer) with one `0x00RRGGBB` colour.
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
    /// vanished release their buffers; new ones are modeset immediately
    /// unless paused, in which case [`Backend::resume`] does it.
    ///
    /// # Errors
    /// [`Error::Io`] if enumeration fails.
    fn rescan(&mut self) -> Result<bool, Error>;

    /// The session went inactive (VT switch). Stop committing; keep all
    /// state. A flip already in flight will still complete and report.
    fn pause(&mut self);

    /// The session is active again. Re-modesets every output (DRM master
    /// may have been revoked and re-granted; CRTC state does not survive
    /// that, buffers do). Any flip in flight is abandoned:
    /// [`Backend::flip_pending`] is false for every output afterwards, and
    /// the caller must repaint fully.
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
    fn error_display_names_property() {
        let e = Error::MissingProperty {
            object: "plane",
            name: "FB_ID",
        };
        assert_eq!(e.to_string(), "plane has no `FB_ID` property");
    }
}

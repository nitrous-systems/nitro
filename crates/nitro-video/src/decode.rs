//! The decoder seam: what a backend is asked for and what it hands back.
//!
//! v1 has one backend, [`crate::ffmpeg::SoftwareDecoder`], which writes
//! NV12 rows straight into a shared-memory ring slot the player lends
//! it ([`FrameSink`]). A hardware backend (VA-API, the follow-up to
//! #3903) will not touch shm at all: it exports each decoded surface as
//! an NV12 dma-buf, which is what [`FrameBuf`] will grow a `DmaBuf`
//! variant for. Nothing else in the player needs to change for that —
//! it presents whatever buffer the frame names.

use std::os::fd::BorrowedFd;

use crate::mp4::Track;

/// The geometry of one tightly packed NV12 frame: a `width × height` luma
/// plane, then interleaved `CbCr` at half resolution; both planes have a
/// stride of `width` bytes. Width and height are even.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nv12Layout {
    /// Width in pixels (even).
    pub width: u32,
    /// Height in pixels (even).
    pub height: u32,
}

impl Nv12Layout {
    /// The layout for a video of `width × height`, rounded down to even.
    #[must_use]
    pub fn for_video(width: u32, height: u32) -> Self {
        Self {
            width: (width & !1).max(2),
            height: (height & !1).max(2),
        }
    }

    /// Bytes in the luma plane, which is also the chroma plane's offset.
    #[must_use]
    pub fn luma_len(self) -> usize {
        self.width as usize * self.height as usize
    }

    /// Bytes in a whole frame.
    #[must_use]
    pub fn frame_len(self) -> usize {
        self.luma_len() * 3 / 2
    }
}

/// Where a software decoder writes: the player's ring of NV12 buffers.
pub trait FrameSink {
    /// Reserve a free slot for a frame; `None` when the ring is full,
    /// which is the back-pressure: the decoder stops reading and its
    /// child blocks.
    fn reserve(&mut self) -> Option<usize>;
    /// The bytes of slot `i` (exactly one frame long).
    fn slot(&mut self, i: usize) -> &mut [u8];
}

/// Where a decoded frame's pixels are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBuf {
    /// In ring slot `n`, written through [`FrameSink::slot`].
    Shm(usize),
    // `DmaBuf(..)`: the VA-API follow-up's variant — an exported NV12
    // surface the player registers and presents without a copy.
}

/// One decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    /// Presentation time, in the track's timescale ticks.
    pub pts: i64,
    /// Its pixels.
    pub buf: FrameBuf,
}

/// The answer to [`Decoder::next_frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    /// A frame is complete.
    Frame(Decoded),
    /// Nothing complete yet (or no free slot): wait for readiness.
    Pending,
    /// The stream is over.
    Eof,
}

/// A video decoder backend.
pub trait Decoder {
    /// (Re)start decoding `track` from sample `from` (a sync sample).
    ///
    /// # Errors
    /// A message for the user when the backend cannot start.
    fn start(&mut self, track: &Track, from: usize) -> Result<(), String>;

    /// A descriptor that turns readable when [`Decoder::next_frame`] may
    /// make progress; `None` for a backend that is polled.
    fn readiness_fd(&self) -> Option<BorrowedFd<'_>>;

    /// Make progress without blocking.
    ///
    /// # Errors
    /// A decode failure, with the backend's own explanation.
    fn next_frame(&mut self, sink: &mut dyn FrameSink) -> Result<Poll, String>;

    /// Stop decoding and release everything the current run holds. A
    /// slot reserved for an unfinished frame is the caller's to free.
    fn stop(&mut self);
}

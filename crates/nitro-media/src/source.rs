//! The video source trait: what a backend is asked for and hands back.

use crate::frame::{DmabufFrame, HwInfo, Nv12Layout, StreamInfo};
use crate::node::RunMode;

/// A video source: one output port producing NV12 frames, run on the
/// player's decode thread (so `Send`, and allowed to block).
///
/// This is the first concrete form of `docs/media.md`'s generic
/// `Source`. Its signatures are the in-process decoder seam nitro-video
/// has used since #3906; the generation-carrying `seek`/`next` of the
/// design arrive with the remote source (phase 2), which needs them.
/// The video extras (`hw`, `next_dmabuf`, `release`, `set_scale`) stay
/// on the video source.
pub trait VideoSource: Send {
    /// The stream's description.
    fn info(&self) -> &StreamInfo;

    /// Continue from the keyframe at or before `secs`; frames before
    /// `secs` still come out and are the caller's to drop.
    ///
    /// # Errors
    /// A message for the user.
    fn seek(&mut self, secs: f64) -> Result<(), String>;

    /// Decode the next frame, in presentation order, into `dst` (exactly
    /// `layout.frame_len()` bytes). Returns its presentation time in
    /// microseconds from the start, or `None` at the end of the stream.
    ///
    /// # Errors
    /// A decode failure, with the backend's own explanation.
    fn next_frame(&mut self, dst: &mut [u8], layout: Nv12Layout) -> Result<Option<i64>, String>;

    /// A hardware decoder's facts; `None` for software (the default).
    fn hw(&self) -> Option<HwInfo> {
        None
    }

    /// Decode the next frame into one of the decoder's own surfaces and
    /// hold it there until [`VideoSource::release`]. `None` at the end.
    ///
    /// # Errors
    /// A decode or export failure; the default has no dma-bufs.
    fn next_dmabuf(&mut self) -> Result<Option<DmabufFrame>, String> {
        Err("this decoder has no dma-bufs".to_owned())
    }

    /// The player (and the server) are done with surface `key`.
    fn release(&mut self, key: u32) {
        let _ = key;
    }

    /// Scale every later [`VideoSource::next_dmabuf`] frame to `size` (even,
    /// no larger than the stream) on the GPU's video engine, or stop
    /// (`None`), #3956. Scaled frames come from a new pool: new keys.
    ///
    /// # Errors
    /// Why not (then frames stay native); the default cannot scale.
    fn set_scale(&mut self, size: Option<(u32, u32)>) -> Result<(), String> {
        match size {
            None => Ok(()),
            Some(_) => Err("this decoder cannot scale".to_owned()),
        }
    }

    /// Why scaling stopped by itself mid-stream (frames are native
    /// again), once.
    fn take_scale_error(&mut self) -> Option<String> {
        None
    }

    /// Realtime (frames may be dropped to keep up) or offline (every
    /// frame, as fast as the consumer takes them). In-process sources
    /// behave the same either way; the default ignores it.
    fn set_mode(&mut self, mode: RunMode) {
        let _ = mode;
    }
}

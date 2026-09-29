//! The decoder seam: what a backend is asked for and what it hands back.
//!
//! v1 has one real backend, [`crate::ffmpeg::LibavDecoder`] (`FFmpeg`'s
//! libavformat + libavcodec, software decode), which writes NV12 rows
//! straight into a shared-memory ring slot the player lends it. It runs
//! on the player's decode thread, so a backend is `Send` and blocking.
//!
//! The hardware follow-up (VA-API through `FFmpeg`'s hwaccel,
//! `AV_HWDEVICE_TYPE_VAAPI` + DRM PRIME export) will not touch shm: it
//! hands back each decoded surface as an NV12 dma-buf. That is what
//! [`FrameBuf`] will grow a `DmaBuf` variant for; nothing in the player's
//! pacing or controls changes, it presents whatever buffer a frame names.
//!
//! No `FFmpeg` type appears here or anywhere outside `ffmpeg.rs`, so the
//! backend can move into a `nitro-media` crate without touching the player.

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
    /// The layout for a video of `width × height`, rounded down to even
    /// (the odd last row or column is cropped).
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

/// The YUV matrix a stream was encoded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    /// ITU-R BT.601 (SD).
    Bt601,
    /// ITU-R BT.709 (HD).
    Bt709,
    /// ITU-R BT.2020 (UHD).
    Bt2020,
}

/// What is known about a video stream once it is open.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Length in seconds; 0 when unknown.
    pub duration: f64,
    /// The colour matrix: the stream's own, or the conventional guess
    /// (BT.709 from 720 lines up, BT.601 below) when it says nothing.
    pub matrix: Matrix,
    /// Full ("PC") range rather than limited ("TV").
    pub full_range: bool,
    /// The decoder's name, for messages and `--stats`.
    pub codec: String,
}

impl StreamInfo {
    /// The matrix to assume when the stream does not say.
    #[must_use]
    pub fn default_matrix(height: u32) -> Matrix {
        if height >= 720 {
            Matrix::Bt709
        } else {
            Matrix::Bt601
        }
    }

    /// Width / height.
    #[must_use]
    pub fn aspect(&self) -> f32 {
        self.width.max(1) as f32 / self.height.max(1) as f32
    }
}

/// Where a decoded frame's pixels are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBuf {
    /// In ring slot `n`, written by [`Decoder::next_frame`].
    Shm(usize),
    // `DmaBuf(..)`: the VA-API follow-up's variant — an exported NV12
    // surface the player registers and presents without a copy.
}

/// A video decoder backend, run on the decode thread.
pub trait Decoder: Send {
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
}

/// A decoder that makes its frames up: a luma ramp whose level is the
/// frame number, at a fixed rate, with a keyframe every
/// [`SyntheticDecoder::GOP`] frames.
///
/// For the tests (which run without any video file) and for pacing
/// measurements that should not include decode cost.
#[derive(Debug, Clone)]
pub struct SyntheticDecoder {
    info: StreamInfo,
    fps: u32,
    frames: u32,
    next: u32,
}

impl SyntheticDecoder {
    /// Frames between keyframes, which is where a seek lands.
    pub const GOP: u32 = 10;

    /// `frames` frames of `width × height` at `fps`.
    #[must_use]
    pub fn new(width: u32, height: u32, fps: u32, frames: u32) -> Self {
        let fps = fps.max(1);
        Self {
            info: StreamInfo {
                width,
                height,
                duration: f64::from(frames) / f64::from(fps),
                matrix: StreamInfo::default_matrix(height),
                full_range: false,
                codec: "synthetic".to_owned(),
            },
            fps,
            frames,
            next: 0,
        }
    }

    /// The luma level frame `n` is filled with.
    #[must_use]
    pub fn level(n: u32) -> u8 {
        (16 + n % 200) as u8
    }

    /// Frame `n`'s presentation time, microseconds.
    #[must_use]
    pub fn pts_us(&self, n: u32) -> i64 {
        i64::from(n) * 1_000_000 / i64::from(self.fps)
    }
}

impl Decoder for SyntheticDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, secs: f64) -> Result<(), String> {
        let n = (secs.max(0.0) * f64::from(self.fps)) as u32;
        self.next = (n / Self::GOP * Self::GOP).min(self.frames);
        Ok(())
    }

    fn next_frame(&mut self, dst: &mut [u8], layout: Nv12Layout) -> Result<Option<i64>, String> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let n = self.next;
        self.next += 1;
        let (y, uv) = dst.split_at_mut(layout.luma_len());
        y.fill(Self::level(n));
        uv.fill(128);
        Ok(Some(self.pts_us(n)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_round_down_to_even() {
        let l = Nv12Layout::for_video(641, 361);
        assert_eq!((l.width, l.height), (640, 360));
        assert_eq!(l.frame_len(), 640 * 360 * 3 / 2);
        assert_eq!(Nv12Layout::for_video(0, 1).width, 2);
    }

    #[test]
    fn the_synthetic_decoder_seeks_to_a_keyframe() {
        let mut d = SyntheticDecoder::new(4, 2, 10, 25);
        let l = Nv12Layout::for_video(4, 2);
        let mut buf = vec![0; l.frame_len()];
        d.seek(1.7).unwrap();
        assert_eq!(d.next_frame(&mut buf, l).unwrap(), Some(1_000_000));
        assert_eq!(buf[0], SyntheticDecoder::level(10));
        d.seek(9.0).unwrap();
        assert_eq!(d.next_frame(&mut buf, l).unwrap(), None);
        assert_eq!(StreamInfo::default_matrix(720), Matrix::Bt709);
        assert_eq!(StreamInfo::default_matrix(480), Matrix::Bt601);
    }
}

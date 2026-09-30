//! Frame types: where a decoded video frame is and what describes it.
//!
//! Moved from `nitro-video`'s `decode.rs` (#3988) unchanged, so a local
//! source (`FFmpeg` in-process today) and, later, a helper's replies
//! (`crate::proto`) describe frames in the same words.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameBuf {
    /// In ring slot `n`, written by [`VideoSource::next_frame`](crate::VideoSource::next_frame).
    Shm(usize),
    /// In the decoder's surface `key` ([`VideoSource::next_dmabuf`](crate::VideoSource::next_dmabuf)): an
    /// exported NV12 dma-buf the player registers once and presents
    /// without a copy, held until [`VideoSource::release`](crate::VideoSource::release).
    DmaBuf(u32),
}

/// One plane of an exported frame.
#[derive(Debug)]
pub struct DmabufPlane {
    /// The dma-buf (owned; a dup per plane).
    pub fd: std::os::fd::OwnedFd,
    /// Byte offset of the plane in `fd`.
    pub offset: u32,
    /// Bytes per row.
    pub stride: u32,
}

/// What registering a decoder surface needs: an NV12 dma-buf layout.
#[derive(Debug)]
pub struct DmabufDesc {
    /// The surface's width in pixels: the stream's, or the VPP scale
    /// target's (#3956).
    pub width: u32,
    /// The surface's height in pixels.
    pub height: u32,
    /// DRM format modifier (tiling).
    pub modifier: u64,
    /// Luma, then interleaved chroma.
    pub planes: Vec<DmabufPlane>,
}

/// A frame from [`VideoSource::next_dmabuf`](crate::VideoSource::next_dmabuf).
#[derive(Debug)]
pub struct DmabufFrame {
    /// Presentation time, microseconds from the start.
    pub pts_us: i64,
    /// The surface it is in; stays the decoder's key for that surface.
    pub key: u32,
    /// The surface's export (fresh fds every time; whoever registered
    /// the key already drops them).
    pub desc: DmabufDesc,
}

/// A hardware decoder's facts, for the player's output choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwInfo {
    /// The exported surfaces' DRM modifier;
    /// `nitro_wire::types::modifier::INVALID` if they cannot be exported.
    pub modifier: u64,
    /// Surfaces in a fixed-size pool, 0 when the pool grows on demand.
    pub pool: usize,
}

/// `--hwdec`: how to decode and present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwDec {
    /// VA-API if it takes the stream; dma-bufs when the server shows them
    /// as they are, else download. Software otherwise.
    #[default]
    Auto,
    /// VA-API, presenting its surfaces as dma-bufs whenever the server
    /// imports them at all (a tiled one the server cannot put on a plane
    /// shows a placeholder, #3938).
    DmaBuf,
    /// VA-API, downloading every frame into the shm ring.
    Download,
    /// Software decode only.
    Off,
}

impl HwDec {
    /// Parse a `--hwdec` value.
    ///
    /// # Errors
    /// The accepted names, for an unknown one.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "dmabuf" | "vaapi-dmabuf" => Ok(Self::DmaBuf),
            "download" | "vaapi-download" => Ok(Self::Download),
            "off" | "software" | "no" => Ok(Self::Off),
            _ => Err(format!(
                "--hwdec: {s:?} is not auto, dmabuf, download or off"
            )),
        }
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
}

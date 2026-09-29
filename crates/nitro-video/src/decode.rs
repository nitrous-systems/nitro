//! The decoder seam: what a backend is asked for and what it hands back.
//!
//! v1 has one real backend, [`crate::ffmpeg::LibavDecoder`] (`FFmpeg`'s
//! libavformat + libavcodec, software decode), which writes NV12 rows
//! straight into a shared-memory ring slot the player lends it. It runs
//! on the player's decode thread, so a backend is `Send` and blocking.
//!
//! The same backend decodes on VA-API when it can (#3923: `FFmpeg`'s
//! hwaccel, `AV_HWDEVICE_TYPE_VAAPI` + DRM PRIME export). A hardware
//! decoder has two outputs, and the player picks one at start
//! ([`choose_output`]): **download** into the shm ring through
//! [`Decoder::next_frame`] as before, or hand each decoded surface back as
//! an NV12 dma-buf through [`Decoder::next_dmabuf`] ([`FrameBuf::DmaBuf`],
//! registered once per surface, returned with [`Decoder::release`]).
//! Nothing in the player's pacing or controls changes: it presents
//! whatever buffer a frame names.
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
    /// In the decoder's surface `key` ([`Decoder::next_dmabuf`]): an
    /// exported NV12 dma-buf the player registers once and presents
    /// without a copy, held until [`Decoder::release`].
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
    /// DRM format modifier (tiling).
    pub modifier: u64,
    /// Luma, then interleaved chroma.
    pub planes: Vec<DmabufPlane>,
}

/// A frame from [`Decoder::next_dmabuf`].
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
    /// imports them at all (a tiled one shows a placeholder until direct
    /// scanout, #3899).
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
            _ => Err(format!("--hwdec: {s:?} is not auto, dmabuf, download or off")),
        }
    }
}

/// How the decode thread hands frames over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// NV12 copies into the shm ring ([`Decoder::next_frame`]).
    Shm,
    /// The decoder's own surfaces as dma-bufs ([`Decoder::next_dmabuf`]).
    DmaBuf,
}

/// Most dma-buf registrations the player keeps: headroom under the
/// server's 32 buffers per client. A decoder that uses more surfaces
/// than this (a pool that grew) has its least recently used idle ones
/// re-registered on demand.
pub const MAX_DMABUF_BUFFERS: usize = 24;

/// Pick the output for a decoder: `hw` its facts (`None`: software),
/// `granted` whether the server gave `DMABUF`, `direct_scanout` its
/// `DIRECT_SCANOUT` bit, `formats` its default feedback (`None`: none
/// arrived). Returns the output and, for a hardware decoder that ends up
/// downloading, why.
#[must_use]
pub fn choose_output(
    pref: HwDec,
    hw: Option<HwInfo>,
    granted: bool,
    direct_scanout: bool,
    formats: Option<&[nitro_wire::types::DmabufFormat]>,
) -> (Output, Option<String>) {
    use nitro_wire::types::{dmabuf_flags, format, modifier};
    let Some(hw) = hw else {
        return (Output::Shm, None);
    };
    let why = |s: String| (Output::Shm, Some(s));
    match pref {
        HwDec::Download | HwDec::Off => return (Output::Shm, None),
        HwDec::Auto | HwDec::DmaBuf => {}
    }
    if hw.modifier == modifier::INVALID {
        return why("the VA surfaces cannot be exported as dma-bufs".into());
    }
    if !granted {
        return why("the server did not grant caps::DMABUF".into());
    }
    let Some(formats) = formats else {
        return why("no DmabufFeedback from the server".into());
    };
    let Some(f) = formats
        .iter()
        .find(|f| f.format == format::NV12 && f.modifier == hw.modifier)
        .filter(|f| f.flags & dmabuf_flags::IMPORT != 0)
    else {
        return why(format!(
            "the server does not import NV12 with modifier {:#x}",
            hw.modifier
        ));
    };
    let shown = f.flags & dmabuf_flags::CPU != 0
        || (f.flags & dmabuf_flags::SCANOUT != 0 && direct_scanout);
    if pref == HwDec::Auto && !shown {
        return why(format!(
            "NV12 with modifier {:#x} would be a placeholder until direct scanout (#3899)",
            hw.modifier
        ));
    }
    if hw.pool > MAX_DMABUF_BUFFERS {
        return why(format!(
            "a {}-surface pool is more than {MAX_DMABUF_BUFFERS} registrations",
            hw.pool
        ));
    }
    (Output::DmaBuf, None)
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

    /// A hardware decoder's facts; `None` for software (the default).
    fn hw(&self) -> Option<HwInfo> {
        None
    }

    /// Decode the next frame into one of the decoder's own surfaces and
    /// hold it there until [`Decoder::release`]. `None` at the end.
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
}

/// A decoder that makes its frames up: a luma ramp whose level is the
/// frame number, at a fixed rate, with a keyframe every
/// [`SyntheticDecoder::GOP`] frames.
///
/// For the tests (which run without any video file) and for pacing
/// measurements that should not include decode cost.
#[derive(Debug)]
pub struct SyntheticDecoder {
    info: StreamInfo,
    fps: u32,
    frames: u32,
    next: u32,
    hw: Option<SynthHw>,
}

/// [`SyntheticDecoder::with_dmabuf`]'s emulated hardware.
#[derive(Debug)]
struct SynthHw {
    modifier: u64,
    /// One sealed memfd per surface (mapped per fill: a mapping is not
    /// `Send`, and the decoder moves to the decode thread).
    surfaces: Vec<std::os::fd::OwnedFd>,
    held: Vec<bool>,
    /// Where the round-robin search for a free surface starts, so every
    /// surface of the pool gets used, as a real decoder's might.
    cursor: usize,
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
            hw: None,
        }
    }

    /// As [`SyntheticDecoder::new`], emulating a VA-API decoder with
    /// `pool` surfaces, used round-robin and exported with `modifier`
    /// (it reports a pool that grows on demand, [`HwInfo::pool`] 0): sealed memfds
    /// (which the server takes as linear dma-bufs) filled like
    /// [`Decoder::next_frame`] would. A frame needing a surface when all
    /// are held is an error, which is how a test sees a player holding
    /// more than it should.
    ///
    /// # Errors
    /// A memfd failure.
    pub fn with_dmabuf(
        width: u32,
        height: u32,
        fps: u32,
        frames: u32,
        pool: usize,
        modifier: u64,
    ) -> Result<Self, String> {
        let mut d = Self::new(width, height, fps, frames);
        let l = Nv12Layout::for_video(width, height);
        let mut surfaces = Vec::with_capacity(pool);
        for _ in 0..pool {
            let fd = nitro_shm::create_sealed("nitro-video-synth", l.frame_len() as u64)
                .map_err(|e| format!("memfd: {e}"))?;
            surfaces.push(fd);
        }
        "synthetic-hw".clone_into(&mut d.info.codec);
        d.hw = Some(SynthHw {
            modifier,
            surfaces,
            held: vec![false; pool],
            cursor: 0,
        });
        Ok(d)
    }

    /// Surfaces currently held by the player (tests).
    #[must_use]
    pub fn held(&self) -> usize {
        self.hw
            .as_ref()
            .map_or(0, |h| h.held.iter().filter(|&&b| b).count())
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

    fn hw(&self) -> Option<HwInfo> {
        self.hw.as_ref().map(|h| HwInfo {
            modifier: h.modifier,
            pool: 0,
        })
    }

    fn next_dmabuf(&mut self) -> Result<Option<DmabufFrame>, String> {
        let l = Nv12Layout::for_video(self.info.width, self.info.height);
        if self.hw.is_none() {
            return Err("this decoder has no dma-bufs".to_owned());
        }
        if self.next >= self.frames {
            return Ok(None);
        }
        let n = self.next;
        let pts_us = self.pts_us(n);
        let hw = self.hw.as_mut().expect("checked");
        let len = hw.held.len();
        let Some(key) = (0..len)
            .map(|i| (hw.cursor + i) % len)
            .find(|&k| !hw.held[k])
        else {
            return Err(format!(
                "all {} surfaces are held: the player keeps too many frames",
                hw.surfaces.len()
            ));
        };
        self.next += 1;
        hw.held[key] = true;
        hw.cursor = (key + 1) % len;
        let fd = &hw.surfaces[key];
        let mut map = nitro_shm::MappingMut::map_mut(std::os::fd::AsFd::as_fd(fd), l.frame_len())
            .map_err(|e| format!("map: {e}"))?;
        let (y, uv) = map.as_bytes_mut().split_at_mut(l.luma_len());
        y.fill(Self::level(n));
        uv.fill(128);
        drop(map);
        let dup = |fd: &std::os::fd::OwnedFd| rustix::io::dup(fd).map_err(|e| format!("dup: {e}"));
        let desc = DmabufDesc {
            modifier: hw.modifier,
            planes: vec![
                DmabufPlane {
                    fd: dup(fd)?,
                    offset: 0,
                    stride: l.width,
                },
                DmabufPlane {
                    fd: dup(fd)?,
                    offset: l.luma_len() as u32,
                    stride: l.width,
                },
            ],
        };
        Ok(Some(DmabufFrame {
            pts_us,
            key: key as u32,
            desc,
        }))
    }

    fn release(&mut self, key: u32) {
        if let Some(h) = self.hw.as_mut().and_then(|hw| hw.held.get_mut(key as usize)) {
            *h = false;
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

    #[test]
    fn the_synthetic_hw_decoder_refuses_overholding() {
        let mut d = SyntheticDecoder::with_dmabuf(4, 2, 10, 25, 2, 0).unwrap();
        let a = d.next_dmabuf().unwrap().unwrap();
        let b = d.next_dmabuf().unwrap().unwrap();
        assert_eq!(a.desc.planes.len(), 2);
        assert_eq!(a.desc.planes[1].offset, 8);
        assert_ne!(a.key, b.key);
        assert_eq!(d.held(), 2);
        assert!(d.next_dmabuf().is_err(), "a third frame with both surfaces held");
        d.release(a.key);
        let c = d.next_dmabuf().unwrap().unwrap();
        assert_eq!(c.key, a.key);
        assert_eq!(c.pts_us, 200_000);
    }

    #[test]
    fn the_output_policy() {
        use nitro_wire::types::{DmabufFormat, dmabuf_flags as fl, format, modifier};
        let lin = HwInfo {
            modifier: modifier::LINEAR,
            pool: 0,
        };
        let y = HwInfo {
            modifier: modifier::I915_Y_TILED,
            pool: 0,
        };
        let fb = [
            DmabufFormat {
                format: format::NV12,
                modifier: modifier::LINEAR,
                flags: fl::CPU | fl::IMPORT,
            },
            DmabufFormat {
                format: format::NV12,
                modifier: modifier::I915_Y_TILED,
                flags: fl::SCANOUT | fl::IMPORT,
            },
        ];
        let go = |p, hw, ds, f: Option<&[DmabufFormat]>| choose_output(p, hw, true, ds, f).0;
        assert_eq!(go(HwDec::Auto, None, false, Some(&fb)), Output::Shm);
        assert_eq!(go(HwDec::Auto, Some(lin), false, Some(&fb)), Output::DmaBuf);
        assert_eq!(go(HwDec::Auto, Some(y), false, Some(&fb)), Output::Shm);
        assert_eq!(go(HwDec::Auto, Some(y), true, Some(&fb)), Output::DmaBuf);
        assert_eq!(go(HwDec::DmaBuf, Some(y), false, Some(&fb)), Output::DmaBuf);
        assert_eq!(go(HwDec::DmaBuf, Some(y), false, Some(&fb[..1])), Output::Shm);
        assert_eq!(go(HwDec::Download, Some(lin), false, Some(&fb)), Output::Shm);
        assert_eq!(go(HwDec::Auto, Some(lin), false, None), Output::Shm);
        let big = HwInfo { pool: 30, ..lin };
        assert_eq!(go(HwDec::Auto, Some(big), false, Some(&fb)), Output::Shm);
        let (o, why) = choose_output(HwDec::Auto, Some(lin), false, false, Some(&fb));
        assert_eq!(o, Output::Shm);
        assert!(why.unwrap().contains("DMABUF"));
    }
}

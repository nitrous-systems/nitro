//! [`LibavDecoder`]: decode through the system `FFmpeg`, on VA-API when the
//! hardware takes the stream and in software otherwise ([`open`]).
//!
//! `FFmpeg` does the whole media job: libavformat demuxes (MP4, MKV/WebM,
//! and the rest), libavcodec decodes (H.264, HEVC, VP9, AV1 where the
//! system build has it), and libavutil carries the frames. It is linked
//! **dynamically** from the system; see `DEPENDENCIES.md` for why, and
//! for the plan to move this backend into a `nitro-media` crate.
//!
//! # The C boundary is eight functions wide
//!
//! `src/shim.c` wraps `FFmpeg` in `nv_open`/`nv_info`/`nv_hw_info`/
//! `nv_seek`/`nv_next`/`nv_next_hw`/`nv_release`/`nv_close`, which take
//! and return only integers, doubles, C strings, byte buffers and fds. So
//! the Rust side declares **no `FFmpeg` struct layouts**: those change
//! between `FFmpeg` majors, and transcribing `AVFrame` by hand is where
//! FFI goes wrong. The `unsafe` in this file is those calls, taking
//! ownership of the fds `nv_next_hw` dups, and the `Send` claim, each
//! with its reasoning.
//!
//! # VA-API (#3923)
//!
//! [`open`] tries VA-API first (unless `--hwdec off`): the shim creates a
//! VA device on the render node, lets libavcodec's hwaccel decode into VA
//! surfaces, and decodes the first frame to prove the driver takes the
//! stream's codec and profile. Anything it cannot do (no device, no
//! hwaccel for the codec, a profile the driver lacks, not 8-bit 4:2:0) is
//! a fallback reason, and the file is opened again in software.
//!
//! No `FFmpeg` type leaves this file: the player sees a [`Decoder`].

#![allow(unsafe_code)] // The FFI exception; listed in DEPENDENCIES.md.

use std::ffi::{CString, c_char, c_double, c_int};
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::path::Path;
use std::ptr::NonNull;

use crate::decode::{
    Decoder, DmabufDesc, DmabufFrame, DmabufPlane, HwDec, HwInfo, Matrix, Nv12Layout, StreamInfo,
};

/// The shim's opaque context.
#[repr(C)]
struct NvCtx {
    _private: [u8; 0],
}

// SAFETY (for the block): the declarations match `src/shim.c` exactly;
// every pointer argument is either the context `nv_open` returned or a
// buffer whose length is passed alongside it.
unsafe extern "C" {
    fn nv_open(
        path: *const c_char,
        threads: c_int,
        hw: c_int,
        device: *const c_char,
        err: *mut c_char,
        errlen: c_int,
    ) -> *mut NvCtx;
    fn nv_hw_info(c: *const NvCtx, pool: *mut c_int, modifier: *mut u64) -> c_int;
    fn nv_next_hw(
        c: *mut NvCtx,
        pts_us: *mut i64,
        key: *mut u32,
        width: *mut c_int,
        height: *mut c_int,
        fds: *mut c_int,
        offsets: *mut u32,
        pitches: *mut u32,
        nplanes: *mut c_int,
        modifier: *mut u64,
        err: *mut c_char,
        errlen: c_int,
    ) -> c_int;
    fn nv_release(c: *mut NvCtx, key: u32) -> c_int;
    fn nv_info(
        c: *const NvCtx,
        width: *mut c_int,
        height: *mut c_int,
        duration: *mut c_double,
        colorspace: *mut c_int,
        full_range: *mut c_int,
        codec: *mut c_char,
        codeclen: c_int,
    );
    fn nv_seek(c: *mut NvCtx, secs: c_double, err: *mut c_char, errlen: c_int) -> c_int;
    fn nv_next(
        c: *mut NvCtx,
        dst: *mut u8,
        dstlen: usize,
        w: c_int,
        h: c_int,
        pts_us: *mut i64,
        err: *mut c_char,
        errlen: c_int,
    ) -> c_int;
    fn nv_close(c: *mut NvCtx);
}

/// An error buffer the shim writes a NUL-terminated message into.
struct ErrBuf([u8; 256]);

impl ErrBuf {
    fn new() -> Self {
        Self([0; 256])
    }

    fn ptr(&mut self) -> *mut c_char {
        self.0.as_mut_ptr().cast()
    }

    #[allow(clippy::unused_self)]
    fn len(&self) -> c_int {
        256
    }

    fn message(&self, fallback: &str) -> String {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(self.0.len());
        let s = String::from_utf8_lossy(&self.0[..end]).into_owned();
        if s.is_empty() { fallback.to_owned() } else { s }
    }
}

/// One open file: demuxer and decoder. See the module docs.
pub struct LibavDecoder {
    ctx: NonNull<NvCtx>,
    info: StreamInfo,
    hw: Option<HwInfo>,
}

/// The default VA-API device.
pub const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// Why [`LibavDecoder::open_hw`] did not give a hardware decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HwError {
    /// The hardware cannot do this stream; software can try.
    Unsupported(String),
    /// The file itself is the problem (missing, not a video).
    Fatal(String),
}

/// What [`open`] made.
pub struct Opened {
    /// The decoder.
    pub decoder: LibavDecoder,
    /// Why VA-API was not used, when it was wanted and is not.
    pub fallback: Option<String>,
}

/// Open `path` for `pref`: VA-API on `device` first (unless
/// [`HwDec::Off`]), software with `threads` when the hardware cannot.
///
/// # Errors
/// As [`LibavDecoder::open`], for the software attempt.
pub fn open(path: &Path, pref: HwDec, device: &str, threads: u32) -> Result<Opened, String> {
    if pref == HwDec::Off {
        return LibavDecoder::open(path, threads).map(|decoder| Opened {
            decoder,
            fallback: None,
        });
    }
    let why = match LibavDecoder::open_hw(path, device) {
        Ok(decoder) => {
            return Ok(Opened {
                decoder,
                fallback: None,
            });
        }
        Err(HwError::Fatal(e)) => return Err(e),
        Err(HwError::Unsupported(e)) => e,
    };
    LibavDecoder::open(path, threads).map(|decoder| Opened {
        decoder,
        fallback: Some(why),
    })
}

// SAFETY: the context is owned by this value alone and every use goes
// through `&mut self` (or `&self` for `nv_info`, which only reads), so it
// is never touched from two threads at once. FFmpeg contexts have no
// thread affinity: moving one between threads is allowed, sharing is not.
unsafe impl Send for LibavDecoder {}

impl LibavDecoder {
    /// Open `path` and its best video stream. `threads` is the decoder's
    /// thread count (0 lets `FFmpeg` choose) for streams above 1080p; up
    /// to 1080p the shim decodes on one thread (#3924).
    ///
    /// # Errors
    /// A message for the user: the file is missing, not a video, or has
    /// no decoder in this `FFmpeg`.
    pub fn open(path: &Path, threads: u32) -> Result<Self, String> {
        Self::open_with(path, threads, None).map_err(|e| match e {
            HwError::Fatal(e) | HwError::Unsupported(e) => e,
        })
    }

    /// Open `path` on VA-API through the render node `device`.
    ///
    /// # Errors
    /// [`HwError::Fatal`] for a missing file; [`HwError::Unsupported`]
    /// for everything else (the software decoder may still open it, or
    /// say what is wrong with the file).
    pub fn open_hw(path: &Path, device: &str) -> Result<Self, HwError> {
        Self::open_with(path, 1, Some(device))
    }

    fn open_with(path: &Path, threads: u32, device: Option<&str>) -> Result<Self, HwError> {
        use std::os::unix::ffi::OsStrExt as _;
        let name = path.display();
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| HwError::Fatal(format!("{name}: path contains a NUL byte")))?;
        let cdev = device
            .map(|d| CString::new(d).map_err(|_| HwError::Unsupported("bad device path".into())))
            .transpose()?;
        if !path.exists() {
            return Err(HwError::Fatal(format!("{name}: cannot open: No such file or directory")));
        }
        let mut err = ErrBuf::new();
        let threads = c_int::try_from(threads).unwrap_or(0);
        // SAFETY: `cpath` (and `cdev`, or NULL) is NUL-terminated and
        // outlives the call; `err` is 256 writable bytes and says so.
        let raw = unsafe {
            nv_open(
                cpath.as_ptr(),
                threads,
                c_int::from(cdev.is_some()),
                cdev.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
                err.ptr(),
                err.len(),
            )
        };
        let ctx = NonNull::new(raw).ok_or_else(|| {
            let e = err.message("cannot open");
            // Any hardware failure is a reason to try software, which
            // then says what is wrong if the file itself is bad.
            if device.is_some() {
                HwError::Unsupported(e)
            } else {
                HwError::Fatal(format!("{name}: {e}"))
            }
        })?;
        let (mut w, mut h, mut cs, mut full) = (0, 0, 0, 0);
        let mut duration = 0.0;
        let mut codec = ErrBuf::new();
        // SAFETY: `ctx` is live (just opened); every out-pointer is a
        // local, and `codec` is 256 writable bytes.
        unsafe {
            nv_info(
                ctx.as_ptr(),
                &raw mut w,
                &raw mut h,
                &raw mut duration,
                &raw mut cs,
                &raw mut full,
                codec.ptr(),
                codec.len(),
            );
        }
        let (width, height) = (w.max(0).cast_unsigned(), h.max(0).cast_unsigned());
        let matrix = match cs {
            0 => Matrix::Bt601,
            1 => Matrix::Bt709,
            2 => Matrix::Bt2020,
            _ => StreamInfo::default_matrix(height),
        };
        let (mut pool, mut modifier) = (0, 0u64);
        // SAFETY: `ctx` is live; the out-pointers are locals.
        let hw = unsafe { nv_hw_info(ctx.as_ptr(), &raw mut pool, &raw mut modifier) } != 0;
        let hw = hw.then(|| HwInfo {
            modifier,
            pool: usize::try_from(pool).unwrap_or(0),
        });
        let codec_name = codec.message("?");
        Ok(Self {
            ctx,
            hw,
            info: StreamInfo {
                width,
                height,
                duration: if duration.is_finite() {
                    duration.max(0.0)
                } else {
                    0.0
                },
                matrix,
                full_range: full != 0,
                codec: if hw.is_some() {
                    format!("{codec_name} (vaapi)")
                } else {
                    codec_name
                },
            },
        })
    }
}

impl Decoder for LibavDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, secs: f64) -> Result<(), String> {
        let mut err = ErrBuf::new();
        // SAFETY: `ctx` is live and exclusively ours (`&mut self`).
        let r = unsafe { nv_seek(self.ctx.as_ptr(), secs.max(0.0), err.ptr(), err.len()) };
        if r < 0 {
            return Err(err.message("seek failed"));
        }
        Ok(())
    }

    fn next_frame(&mut self, dst: &mut [u8], layout: Nv12Layout) -> Result<Option<i64>, String> {
        let (Ok(w), Ok(h)) = (
            c_int::try_from(layout.width),
            c_int::try_from(layout.height),
        ) else {
            return Err("frame too large".to_owned());
        };
        if dst.len() < layout.frame_len() {
            return Err("destination too small".to_owned());
        }
        let mut pts = 0i64;
        let mut err = ErrBuf::new();
        // SAFETY: `ctx` is live and exclusively ours; `dst` is
        // `dst.len()` writable bytes, which the shim checks against
        // `w * h * 3 / 2` before writing; `pts` and `err` are locals.
        let r = unsafe {
            nv_next(
                self.ctx.as_ptr(),
                dst.as_mut_ptr(),
                dst.len(),
                w,
                h,
                &raw mut pts,
                err.ptr(),
                err.len(),
            )
        };
        match r {
            1 => Ok(Some(pts)),
            0 => Ok(None),
            _ => Err(err.message("decode failed")),
        }
    }

    fn hw(&self) -> Option<HwInfo> {
        self.hw
    }

    fn next_dmabuf(&mut self) -> Result<Option<DmabufFrame>, String> {
        if self.hw.is_none() {
            return Err("software decode has no dma-bufs".to_owned());
        }
        let (mut pts, mut key, mut w, mut h, mut n, mut modifier) = (0i64, 0u32, 0, 0, 0, 0u64);
        let mut fds: [c_int; 4] = [-1; 4];
        let (mut offsets, mut pitches) = ([0u32; 4], [0u32; 4]);
        let mut err = ErrBuf::new();
        // SAFETY: `ctx` is live and exclusively ours; every out-pointer
        // is a local, the three arrays are 4 elements as the shim writes
        // at most 4 (it refuses more than 2 planes).
        let r = unsafe {
            nv_next_hw(
                self.ctx.as_ptr(),
                &raw mut pts,
                &raw mut key,
                &raw mut w,
                &raw mut h,
                fds.as_mut_ptr(),
                offsets.as_mut_ptr(),
                pitches.as_mut_ptr(),
                &raw mut n,
                &raw mut modifier,
                err.ptr(),
                err.len(),
            )
        };
        match r {
            1 => {}
            0 => return Ok(None),
            _ => return Err(err.message("decode failed")),
        }
        let n = usize::try_from(n).unwrap_or(0).min(4);
        let planes: Vec<DmabufPlane> = (0..n)
            .map(|i| DmabufPlane {
                // SAFETY: on success the shim returns `n` fresh fds from
                // F_DUPFD_CLOEXEC that nothing else owns or closes.
                fd: unsafe { OwnedFd::from_raw_fd(fds[i]) },
                offset: offsets[i],
                stride: pitches[i],
            })
            .collect();
        if (w, h) != (self.info.width.cast_signed(), self.info.height.cast_signed()) {
            self.release(key);
            return Err(format!(
                "the stream changed size ({}x{} → {w}x{h})",
                self.info.width, self.info.height
            ));
        }
        Ok(Some(DmabufFrame {
            pts_us: pts,
            key,
            desc: DmabufDesc { modifier, planes },
        }))
    }

    fn release(&mut self, key: u32) {
        // SAFETY: `ctx` is live and exclusively ours.
        let _ = unsafe { nv_release(self.ctx.as_ptr(), key) };
    }
}

impl Drop for LibavDecoder {
    fn drop(&mut self) {
        // SAFETY: `ctx` came from `nv_open` and is closed exactly once.
        unsafe { nv_close(self.ctx.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_a_message_not_a_crash() {
        let Err(e) = LibavDecoder::open(Path::new("/nonexistent/clip.mp4"), 0) else {
            panic!("opened a missing file");
        };
        assert!(e.contains("/nonexistent/clip.mp4"), "{e}");
    }

    #[test]
    fn a_non_video_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("nitro-video-notvideo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.txt");
        std::fs::write(&p, b"hello, this is not a video").unwrap();
        assert!(LibavDecoder::open(&p, 0).is_err());
        assert!(open(&p, HwDec::Auto, "/nonexistent/renderD128", 1).is_err());
    }

    #[test]
    fn a_missing_file_is_fatal_for_the_hardware_path_too() {
        let r = LibavDecoder::open_hw(Path::new("/nonexistent/clip.mp4"), DEFAULT_VAAPI_DEVICE);
        assert!(matches!(r, Err(HwError::Fatal(_))), "{:?}", r.err());
    }
}

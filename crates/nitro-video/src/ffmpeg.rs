//! [`LibavDecoder`]: software decode through the system `FFmpeg`.
//!
//! `FFmpeg` does the whole media job: libavformat demuxes (MP4, MKV/WebM,
//! and the rest), libavcodec decodes (H.264, HEVC, VP9, AV1 where the
//! system build has it), and libavutil carries the frames. It is linked
//! **dynamically** from the system; see `DEPENDENCIES.md` for why, and
//! for the plan to move this backend into a `nitro-media` crate.
//!
//! # The C boundary is five functions wide
//!
//! `src/shim.c` wraps `FFmpeg` in `nv_open`/`nv_info`/`nv_seek`/`nv_next`
//! /`nv_close`, which take and return only integers, doubles, C strings
//! and byte buffers. So the Rust side declares **no `FFmpeg` struct
//! layouts**: those change between `FFmpeg` majors, and transcribing
//! `AVFrame` by hand is where FFI goes wrong. The `unsafe` in this file
//! is the five calls and the `Send` claim, each with its reasoning.
//!
//! No `FFmpeg` type leaves this file: the player sees a [`Decoder`].

#![allow(unsafe_code)] // The FFI exception; listed in DEPENDENCIES.md.

use std::ffi::{CString, c_char, c_double, c_int};
use std::path::Path;
use std::ptr::NonNull;

use crate::decode::{Decoder, Matrix, Nv12Layout, StreamInfo};

/// The shim's opaque context.
#[repr(C)]
struct NvCtx {
    _private: [u8; 0],
}

// SAFETY (for the block): the declarations match `src/shim.c` exactly;
// every pointer argument is either the context `nv_open` returned or a
// buffer whose length is passed alongside it.
unsafe extern "C" {
    fn nv_open(path: *const c_char, threads: c_int, err: *mut c_char, errlen: c_int) -> *mut NvCtx;
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
}

// SAFETY: the context is owned by this value alone and every use goes
// through `&mut self` (or `&self` for `nv_info`, which only reads), so it
// is never touched from two threads at once. FFmpeg contexts have no
// thread affinity: moving one between threads is allowed, sharing is not.
unsafe impl Send for LibavDecoder {}

impl LibavDecoder {
    /// Open `path` and its best video stream. `threads` is the decoder's
    /// thread count (0 lets `FFmpeg` choose).
    ///
    /// # Errors
    /// A message for the user: the file is missing, not a video, or has
    /// no decoder in this `FFmpeg`.
    pub fn open(path: &Path, threads: u32) -> Result<Self, String> {
        use std::os::unix::ffi::OsStrExt as _;
        let name = path.display();
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("{name}: path contains a NUL byte"))?;
        let mut err = ErrBuf::new();
        let threads = c_int::try_from(threads).unwrap_or(0);
        // SAFETY: `cpath` is NUL-terminated and outlives the call; `err`
        // is 256 writable bytes and says so.
        let raw = unsafe { nv_open(cpath.as_ptr(), threads, err.ptr(), err.len()) };
        let ctx =
            NonNull::new(raw).ok_or_else(|| format!("{name}: {}", err.message("cannot open")))?;
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
        Ok(Self {
            ctx,
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
                codec: codec.message("?"),
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
    }
}

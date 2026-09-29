//! Client-supplied pixel buffers.

use std::fmt;

use crate::{ClientId, Error, key::define_key};
use nitro_core::IRect;

define_key!(
    /// A handle to a buffer in the scene's buffer arena.
    BufferKey
);

/// The shape of a buffer's pixels.
///
/// The format is an opaque fourcc (`DRM_FORMAT_*`); the scene never looks
/// inside a pixel, it only checks that the described bytes exist. The one
/// thing it does record about the format is [`is_opaque`](Self::is_opaque),
/// and even that is a boolean the *caller* computed: the scene stores it, it
/// does not learn what any fourcc means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferDesc {
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
    /// Bytes per row; must be at least `w * bytes_per_pixel`.
    pub stride: u32,
    /// Pixel format as a fourcc code.
    pub format: u32,
    /// Byte offset of the first (or only) plane; 0 for a plain buffer.
    pub offset0: u32,
    /// A second plane, as `(offset, stride, rows)` in bytes/rows: NV12's
    /// chroma. Geometry only; the scene does not know what is in it.
    pub plane1: Option<(u32, u32, u32)>,
    /// Whether every pixel of this format is fully opaque.
    ///
    /// Private and defaulting to `false`, because the default has to be the
    /// conservative one: an unset flag costs an occlusion opportunity, a
    /// wrongly set one shows a hole where the background was skipped.
    opaque: bool,
}

impl BufferDesc {
    /// Construct a description. The format is assumed to carry alpha; call
    /// [`with_opaque`](Self::with_opaque) when the caller knows it does not.
    pub const fn new(w: u32, h: u32, stride: u32, format: u32) -> Self {
        Self {
            w,
            h,
            stride,
            format,
            offset0: 0,
            plane1: None,
            opaque: false,
        }
    }

    /// Set the planes of a multi-planar (or offset) buffer: plane 0 starts
    /// at `offset0` with the description's `stride` and `h` rows; `plane1`
    /// is `(offset, stride, rows)`.
    #[must_use]
    pub const fn with_planes(mut self, offset0: u32, plane1: Option<(u32, u32, u32)>) -> Self {
        self.offset0 = offset0;
        self.plane1 = plane1;
        self
    }

    /// Declare whether the format's pixels are fully opaque.
    ///
    /// Only the caller knows: the scene does not interpret fourccs. Setting
    /// this true for a format that in fact carries alpha is a correctness
    /// bug, not a performance one — it lets a rasterizer skip content that
    /// shows through.
    #[must_use]
    pub const fn with_opaque(mut self, opaque: bool) -> Self {
        self.opaque = opaque;
        self
    }

    /// Whether the format's pixels were declared fully opaque.
    #[must_use]
    pub const fn is_opaque(&self) -> bool {
        self.opaque
    }

    /// The number of bytes the description implies: the end of the
    /// furthest plane.
    ///
    /// A plain buffer is `stride * h`. One with planes
    /// ([`with_planes`](Self::with_planes)) need not pad its last rows, so
    /// each plane ends at `offset + stride * (rows - 1) + 1` — the loosest
    /// bound the scene can check without knowing the format (the server
    /// checks the exact one).
    pub const fn byte_len(&self) -> usize {
        if self.offset0 == 0 && self.plane1.is_none() {
            return (self.stride as usize) * (self.h as usize);
        }
        let p0 = plane_end(self.offset0, self.stride, self.h);
        match self.plane1 {
            Some((off, stride, rows)) => {
                let p1 = plane_end(off, stride, rows);
                if p1 > p0 { p1 } else { p0 }
            }
            None => p0,
        }
    }

    /// The whole buffer as a source rect.
    pub fn full_rect(&self) -> IRect {
        IRect::new(0, 0, self.w.cast_signed(), self.h.cast_signed())
    }

    /// Reject descriptions the scene cannot index.
    pub(crate) fn validate(&self, data_len: usize) -> Result<(), Error> {
        if self.w == 0 || self.h == 0 || self.stride == 0 {
            return Err(Error::BadBuffer);
        }
        if self.w > i32::MAX.cast_unsigned() || self.h > i32::MAX.cast_unsigned() {
            return Err(Error::BadBuffer);
        }
        // One byte per pixel is the loosest bound the scene can check without
        // knowing the format; the server checks the format-specific one.
        if (self.stride as usize) < (self.w as usize) {
            return Err(Error::BadBuffer);
        }
        if data_len < self.byte_len() {
            return Err(Error::BadBuffer);
        }
        Ok(())
    }
}

/// One byte past the start of a plane's last row.
const fn plane_end(off: u32, stride: u32, rows: u32) -> usize {
    if rows == 0 {
        off as usize
    } else {
        off as usize + (stride as usize) * (rows as usize - 1) + 1
    }
}

/// Where a buffer's bytes live.
///
/// The scene does not care whether the pixels are a heap copy or a mapping
/// of the client's own memfd; it only reads them. This trait is the seam:
/// `Vec<u8>` implements it for tests and for buffers the server owns
/// outright, and the server's mapped-buffer type implements it without the
/// scene learning what a mapping is (the scene stays `forbid(unsafe_code)`
/// and dependency-free of `rustix`).
///
/// [`bytes_mut`](Self::bytes_mut) is `None` for a store whose bytes are
/// not the scene's to change — a read-only mapping of a client's buffer,
/// which the *client* writes and the server only reads.
pub trait PixelStore: fmt::Debug {
    /// The pixel bytes.
    fn bytes(&self) -> &[u8];
    /// The pixel bytes, writable, or `None` if the store is read-only.
    fn bytes_mut(&mut self) -> Option<&mut [u8]>;
}

impl PixelStore for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }

    fn bytes_mut(&mut self) -> Option<&mut [u8]> {
        Some(self)
    }
}

/// A buffer owned by the scene.
///
/// The server hands in a [`PixelStore`] at `create_buffer` time — a mapping
/// of the client's sealed memfd in production, a `Vec<u8>` in tests. The
/// scene owns the store for the buffer's life and drops it on
/// `destroy_buffer`, which for a mapping is the `munmap`.
#[derive(Debug)]
pub struct Buffer {
    pub(crate) desc: BufferDesc,
    pub(crate) client: ClientId,
    pub(crate) data: Box<dyn PixelStore>,
    /// Whether an image node has ever been pointed at this buffer. A buffer
    /// that has never been shown has no "previous frame" its damage could
    /// be relative to, so swapping to it repaints the whole node.
    pub(crate) shown: bool,
}

impl Buffer {
    /// The buffer's description.
    pub fn desc(&self) -> BufferDesc {
        self.desc
    }

    /// The client that owns the buffer.
    pub fn client(&self) -> ClientId {
        self.client
    }

    /// The pixel bytes.
    pub fn data(&self) -> &[u8] {
        self.data.bytes()
    }
}

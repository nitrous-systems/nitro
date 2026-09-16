//! Client-supplied pixel buffers.

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
            opaque: false,
        }
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

    /// The number of bytes the description implies.
    pub const fn byte_len(&self) -> usize {
        (self.stride as usize) * (self.h as usize)
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

/// A buffer owned by the scene.
///
/// The server copies client bytes in at `create_buffer` time; from then on the
/// scene owns them and hands out `&mut [u8]` for in-place updates.
#[derive(Debug)]
pub struct Buffer {
    pub(crate) desc: BufferDesc,
    pub(crate) client: ClientId,
    pub(crate) data: Vec<u8>,
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
        &self.data
    }
}

//! `nitro-png` — a PNG decoder for icons: no dependencies, no `unsafe`, and
//! nothing in it that an icon file does not need.
//!
//! One entry point, [`decode`], which takes the bytes of a PNG and returns
//! straight-alpha **BGRA** pixels in the layout the toolkit's `Image` widget
//! and `nitro-raster`'s blitter already take (`[b, g, r, a]` per pixel, rows
//! top to bottom) — the same convention `nitro-wallpaper/src/ppm.rs`
//! documents. A caller that wants an icon on screen gets one; there is no
//! second step and no colour-type enum to match on.
//!
//! ```
//! # fn main() -> Result<(), nitro_png::Error> {
//! let bytes = nitro_png::tests_support::ONE_PIXEL_RGBA;
//! let img = nitro_png::decode(bytes)?;
//! assert_eq!((img.width, img.height), (1, 1));
//! assert_eq!(img.data, vec![0x30, 0x20, 0x10, 0xFF]); // B, G, R, A
//! # Ok(()) }
//! ```
//!
//! # What it supports
//!
//! Every colour type PNG defines — greyscale (0), truecolour (2), palette
//! (3), greyscale+alpha (4) and truecolour+alpha (6) — at bit depths 1, 2,
//! 4, 8 and 16, with `tRNS` transparency for the first three. 16-bit
//! samples are truncated to 8 by taking the high byte, which is what every
//! 8-bit-per-channel consumer does and what the surrounding toolkit can
//! show. Ancillary chunks are skipped; `gAMA`, `iCCP` and `sRGB` are
//! **ignored**, so an image with a non-default gamma is decoded as its raw
//! samples. Icons do not carry one.
//!
//! # What it refuses
//!
//! - **Interlaced (Adam7) images**, with [`Error::Interlaced`]. Adam7 is
//!   seven passes of the whole unfilter-and-deinterleave machinery for a
//!   feature whose entire purpose is progressive display over a slow link;
//!   an icon is decoded from a local file in microseconds. Measured on the
//!   corpus: see `crates/nitro-png/README.md` — zero interlaced files.
//! - **APNG**: the `acTL`/`fcTL`/`fdAT` chunks are ancillary-or-unknown and
//!   skipped, so an animated PNG decodes as its first (still) frame, which
//!   is exactly what an icon loader wants.
//! - Anything malformed, with an [`Error`] — **never** a panic. See the
//!   crate's `tests/fuzz.rs`, which truncates and corrupts every file it can
//!   find and asserts the same.
//!
//! # Bounds
//!
//! [`MAX_PIXELS`] caps `width * height` before a byte is allocated: the
//! header is two integers a hostile file can claim anything in, and
//! `width * height * 4` on such a header is how an icon becomes an OOM. The
//! decompressor is likewise given the exact expected size of the filtered
//! raster as a hard cap, so a zip-bomb IDAT is an error rather than an
//! allocation.

#![deny(missing_docs)]

mod inflate;
mod png;

pub use inflate::{adler32, inflate, zlib_decompress};
pub use png::{Image, ImageInfo, MAX_PIXELS, decode, decode_header};

/// Everything that can be wrong with a PNG, and nothing that cannot.
///
/// The variants carry `&'static str` rather than `String` where a message
/// helps, so an error costs no allocation on the failure path — a decoder
/// that is fuzzed a million times should not allocate a million strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The file does not start with the 8-byte PNG signature.
    NotPng,
    /// A chunk header or payload runs past the end of the file.
    Truncated(&'static str),
    /// A chunk's CRC-32 does not match its contents.
    BadCrc(&'static str),
    /// The first chunk is not `IHDR`, or `IHDR` is the wrong size.
    BadHeader,
    /// `IHDR` names a colour type / bit depth combination PNG does not define.
    BadColorType {
        /// The colour type byte as it appeared in `IHDR`.
        color_type: u8,
        /// The bit depth byte as it appeared in `IHDR`.
        bit_depth: u8,
    },
    /// Width or height is zero.
    ZeroDimension,
    /// `width * height` exceeds [`MAX_PIXELS`].
    TooLarge,
    /// The image is Adam7-interlaced, which this decoder does not do.
    Interlaced,
    /// A filter byte at the start of a scanline is not 0..=4.
    BadFilter(u8),
    /// A palette index with no `PLTE` entry, or a `PLTE` of the wrong size.
    BadPalette,
    /// The file has no `IDAT` chunks, or none before `IEND`.
    NoImageData,
    /// The zlib header of the `IDAT` stream is malformed.
    BadZlibHeader,
    /// The zlib stream asks for a preset dictionary, which PNG forbids.
    PresetDictionary,
    /// A deflate block header names block type 3.
    BadBlockType,
    /// A stored block's `LEN` and `NLEN` are not complements.
    BadStoredBlock,
    /// A Huffman table is over-subscribed or otherwise impossible.
    BadHuffmanTable,
    /// A code that the Huffman table does not contain.
    BadCode,
    /// A back-reference points before the start of the output.
    BadDistance,
    /// The decompressed stream is larger than the header says it can be.
    TooMuchOutput,
    /// The decompressed stream is smaller than the header says it must be.
    TooLittleOutput,
    /// The zlib stream's Adler-32 does not match what was decompressed.
    BadChecksum,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotPng => write!(f, "not a PNG: wrong signature"),
            Self::Truncated(what) => write!(f, "truncated: {what}"),
            Self::BadCrc(what) => write!(f, "CRC mismatch in the {what} chunk"),
            Self::BadHeader => write!(f, "malformed IHDR"),
            Self::BadColorType {
                color_type,
                bit_depth,
            } => write!(
                f,
                "colour type {color_type} at bit depth {bit_depth} is not a PNG"
            ),
            Self::ZeroDimension => write!(f, "width or height is zero"),
            Self::TooLarge => write!(f, "image larger than {MAX_PIXELS} pixels"),
            Self::Interlaced => write!(
                f,
                "Adam7-interlaced PNG is not supported (icon themes do not use it)"
            ),
            Self::BadFilter(b) => write!(f, "filter type {b} is not 0..=4"),
            Self::BadPalette => write!(f, "palette index out of range, or a malformed PLTE"),
            Self::NoImageData => write!(f, "no IDAT chunk"),
            Self::BadZlibHeader => write!(f, "malformed zlib header on the IDAT stream"),
            Self::PresetDictionary => write!(f, "zlib preset dictionary, which PNG forbids"),
            Self::BadBlockType => write!(f, "reserved deflate block type"),
            Self::BadStoredBlock => write!(f, "stored block LEN/NLEN mismatch"),
            Self::BadHuffmanTable => write!(f, "malformed Huffman table"),
            Self::BadCode => write!(f, "Huffman code not in the table"),
            Self::BadDistance => write!(f, "back-reference before the start of the stream"),
            Self::TooMuchOutput => write!(f, "IDAT decompresses to more than the header allows"),
            Self::TooLittleOutput => write!(f, "IDAT decompresses to less than the header needs"),
            Self::BadChecksum => write!(f, "Adler-32 mismatch on the IDAT stream"),
        }
    }
}

impl std::error::Error for Error {}

/// Fixtures the doc-test and the unit tests share. Not part of the API in
/// any meaningful sense; public only so the doc example above can use one.
#[doc(hidden)]
pub mod tests_support {
    /// A 1×1 RGBA PNG of `#102030` at full alpha, written by hand.
    pub const ONE_PIXEL_RGBA: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, // signature
        0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R', // IHDR, 13 bytes
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1 x 1
        0x08, 0x06, 0x00, 0x00, 0x00, // depth 8, colour 6, deflate, adaptive, no interlace
        0x1F, 0x15, 0xC4, 0x89, // CRC
        0x00, 0x00, 0x00, 0x10, b'I', b'D', b'A', b'T', // IDAT, 16 bytes
        0x78, 0x01, // zlib header
        0x01, 0x05, 0x00, 0xFA, 0xFF, // stored, final, LEN=5 NLEN=~5
        0x00, 0x10, 0x20, 0x30, 0xFF, // filter 0, R G B A
        0x02, 0x04, 0x01, 0x60, // Adler-32 of those five bytes
        0x91, 0x05, 0x9F, 0x9D, // CRC
        0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', //
        0xAE, 0x42, 0x60, 0x82,
    ];
}

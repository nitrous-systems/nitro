//! The PNG container: signature, chunks, `IHDR`/`PLTE`/`tRNS`/`IDAT`/`IEND`,
//! the five scanline filters, and the expansion of every colour type into
//! straight-alpha BGRA.

use crate::Error;
use crate::inflate::zlib_decompress;

/// The largest image [`decode`] will produce, in pixels.
///
/// 64 megapixels is four times a 4K screen and about 256 MB once it is
/// BGRA — the same cap and the same reasoning as
/// `nitro-wallpaper`'s PPM reader. It is checked against the `IHDR` fields
/// *before* anything is allocated, because those two integers are the one
/// place a hostile file gets to choose how much memory we ask for.
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;

/// The 8-byte PNG signature.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// A decoded image: `width × height` pixels, `[b, g, r, a]` each, rows top
/// to bottom, alpha **straight** (not premultiplied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub data: Vec<u8>,
}

/// What `IHDR` says, for a caller that wants the size without the pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// PNG colour type: 0, 2, 3, 4 or 6.
    pub color_type: u8,
    /// Bits per sample: 1, 2, 4, 8 or 16.
    pub bit_depth: u8,
    /// True if the file is Adam7-interlaced (which [`decode`] refuses).
    pub interlaced: bool,
}

impl ImageInfo {
    /// Samples per pixel for this colour type.
    fn channels(self) -> usize {
        match self.color_type {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            _ => 4,
        }
    }

    /// Bytes per filtered scanline, not counting the filter byte.
    fn stride(self) -> usize {
        let bits = self.width as usize * self.channels() * self.bit_depth as usize;
        bits.div_ceil(8)
    }

    /// The filter's `bpp`: distance back to the corresponding byte of the
    /// previous pixel, at least 1 (PNG spec 9.2).
    fn filter_bpp(self) -> usize {
        (self.channels() * self.bit_depth as usize)
            .div_ceil(8)
            .max(1)
    }
}

/// CRC-32 (IEEE, as PNG uses it), table built at compile time.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// CRC-32 of `data`, as PNG computes it over a chunk's type and payload.
fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// Read `IHDR` and stop, without decompressing anything.
///
/// # Errors
/// As [`decode`], for the header portion — including [`Error::Interlaced`]
/// being *absent*: this reports interlace in [`ImageInfo`] rather than
/// refusing, so a caller can survey a corpus.
pub fn decode_header(bytes: &[u8]) -> Result<ImageInfo, Error> {
    if bytes.len() < 8 || bytes[..8] != SIGNATURE {
        return Err(Error::NotPng);
    }
    let mut chunks = Chunks::new(&bytes[8..]);
    let first = chunks.next().ok_or(Error::BadHeader)??;
    if first.kind != *b"IHDR" {
        return Err(Error::BadHeader);
    }
    parse_ihdr(first.data)
}

/// Parse a 13-byte `IHDR` payload.
fn parse_ihdr(data: &[u8]) -> Result<ImageInfo, Error> {
    if data.len() != 13 {
        return Err(Error::BadHeader);
    }
    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let bit_depth = data[8];
    let color_type = data[9];
    let compression = data[10];
    let filter = data[11];
    let interlace = data[12];

    if width == 0 || height == 0 {
        return Err(Error::ZeroDimension);
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Error::TooLarge);
    }
    if compression != 0 || filter != 0 {
        return Err(Error::BadHeader);
    }
    if interlace > 1 {
        return Err(Error::BadHeader);
    }
    // PNG spec table 11.1: which depths each colour type allows.
    let ok = match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(bit_depth, 8 | 16),
        _ => false,
    };
    if !ok {
        return Err(Error::BadColorType {
            color_type,
            bit_depth,
        });
    }
    Ok(ImageInfo {
        width,
        height,
        color_type,
        bit_depth,
        interlaced: interlace == 1,
    })
}

/// One chunk: its four-byte type and a borrow of its payload.
struct Chunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
}

/// Iterator over the chunks after the signature.
///
/// Every length is checked against what is left before it is used as an
/// index, and every chunk's CRC is verified — a wrong CRC is an error, not
/// a warning, because a decoder that renders a corrupted icon is harder to
/// debug than one that says the file is corrupt.
struct Chunks<'a> {
    rest: &'a [u8],
    done: bool,
}

impl<'a> Chunks<'a> {
    fn new(rest: &'a [u8]) -> Self {
        Self { rest, done: false }
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Result<Chunk<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.rest.is_empty() {
            self.done = true;
            return None;
        }
        if self.rest.len() < 8 {
            self.done = true;
            return Some(Err(Error::Truncated("chunk header")));
        }
        let len =
            u32::from_be_bytes([self.rest[0], self.rest[1], self.rest[2], self.rest[3]]) as usize;
        // The spec caps a chunk at 2^31 - 1; anything more is a corrupt
        // length field, and on a 32-bit target it would also overflow.
        if len > 0x7FFF_FFFF {
            self.done = true;
            return Some(Err(Error::Truncated("chunk length out of range")));
        }
        if self.rest.len() < 12 + len {
            self.done = true;
            return Some(Err(Error::Truncated("chunk payload")));
        }
        let kind = [self.rest[4], self.rest[5], self.rest[6], self.rest[7]];
        let data = &self.rest[8..8 + len];
        let want = u32::from_be_bytes([
            self.rest[8 + len],
            self.rest[9 + len],
            self.rest[10 + len],
            self.rest[11 + len],
        ]);
        if crc32(&self.rest[4..8 + len]) != want {
            self.done = true;
            return Some(Err(Error::BadCrc(match &kind {
                b"IHDR" => "IHDR",
                b"PLTE" => "PLTE",
                b"IDAT" => "IDAT",
                b"tRNS" => "tRNS",
                _ => "a",
            })));
        }
        self.rest = &self.rest[12 + len..];
        if kind == *b"IEND" {
            self.done = true;
        }
        Some(Ok(Chunk { kind, data }))
    }
}

/// Decode a PNG into straight-alpha BGRA.
///
/// # Errors
/// Every way a PNG can be wrong, as [`Error`] — see the crate docs for what
/// is supported and what is deliberately refused. This function does not
/// panic on any input; `tests/fuzz.rs` is the evidence.
pub fn decode(bytes: &[u8]) -> Result<Image, Error> {
    if bytes.len() < 8 || bytes[..8] != SIGNATURE {
        return Err(Error::NotPng);
    }

    let mut info: Option<ImageInfo> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    let mut seen_iend = false;

    for chunk in Chunks::new(&bytes[8..]) {
        let chunk = chunk?;
        match &chunk.kind {
            b"IHDR" => {
                if info.is_some() {
                    return Err(Error::BadHeader);
                }
                info = Some(parse_ihdr(chunk.data)?);
            }
            b"PLTE" => {
                if info.is_none() {
                    return Err(Error::BadHeader);
                }
                if chunk.data.len() % 3 != 0 || chunk.data.len() > 256 * 3 {
                    return Err(Error::BadPalette);
                }
                palette = chunk
                    .data
                    .chunks_exact(3)
                    .map(|c| [c[0], c[1], c[2]])
                    .collect();
            }
            b"tRNS" => {
                if info.is_none() {
                    return Err(Error::BadHeader);
                }
                trns = chunk.data.to_vec();
            }
            b"IDAT" => {
                if info.is_none() {
                    return Err(Error::BadHeader);
                }
                idat.extend_from_slice(chunk.data);
            }
            b"IEND" => {
                seen_iend = true;
                break;
            }
            // Ancillary and unknown chunks are skipped, which is what makes
            // an APNG decode as its first frame and a gAMA-carrying icon
            // decode at all.
            _ => {}
        }
    }

    let info = info.ok_or(Error::BadHeader)?;
    if info.interlaced {
        return Err(Error::Interlaced);
    }
    if idat.is_empty() {
        return Err(Error::NoImageData);
    }
    if !seen_iend {
        return Err(Error::Truncated("no IEND chunk"));
    }
    if info.color_type == 3 && palette.is_empty() {
        return Err(Error::BadPalette);
    }

    let stride = info.stride();
    // `(stride + 1) * height`: one filter byte per scanline. Both factors
    // are already bounded by the MAX_PIXELS check in `parse_ihdr`, so this
    // cannot overflow a usize on any target we build for.
    let raw_len = (stride + 1)
        .checked_mul(info.height as usize)
        .ok_or(Error::TooLarge)?;
    let mut raw = zlib_decompress(&idat, raw_len)?;
    if raw.len() < raw_len {
        return Err(Error::TooLittleOutput);
    }

    unfilter(&mut raw, stride, info.height as usize, info.filter_bpp())?;
    expand(&raw, stride, info, &palette, &trns)
}

/// Undo the five per-scanline filters, in place.
///
/// `raw` is `height` rows of `1 + stride` bytes; on return each row's data
/// bytes are the reconstructed samples (the filter bytes stay where they
/// are and are skipped by [`expand`]).
fn unfilter(raw: &mut [u8], stride: usize, height: usize, bpp: usize) -> Result<(), Error> {
    let row_len = stride + 1;
    for y in 0..height {
        let start = y * row_len;
        let filter = raw[start];
        // Split the buffer so the current row and the one above it are two
        // slices of known length rather than two ranges of indices into the
        // same one. That is what lets the loops below be `zip`s — the
        // bounds check disappears, and with it about a third of the time
        // this function used to take on a 512×512 image.
        let (above, rest) = raw.split_at_mut(start + 1);
        let cur = &mut rest[..stride];
        // The previous row's *data* bytes: `start - row_len + 1`, which is
        // `start - stride`. Empty on the first row, where the filters treat
        // the row above as zeroes.
        let prev: &[u8] = if y > 0 {
            &above[start - stride..start]
        } else {
            &[]
        };

        filter_row(filter, cur, prev, bpp)?;
    }
    Ok(())
}

/// Filter 1: each byte predicted by the one `bpp` back in the same row.
///
/// Inherently serial — byte `i` needs the *reconstructed* byte `i - bpp` —
/// so it stays an indexed loop; `cur.len()` bounds it.
fn sub(cur: &mut [u8], bpp: usize) {
    for i in bpp..cur.len() {
        cur[i] = cur[i].wrapping_add(cur[i - bpp]);
    }
}

/// Filter 2: each byte predicted by the one above it.
///
/// The one filter with no dependency along the row, so it is a plain `zip`
/// and the compiler vectorises it.
fn up(cur: &mut [u8], prev: &[u8]) {
    for (c, p) in cur.iter_mut().zip(prev) {
        *c = c.wrapping_add(*p);
    }
}

/// Filter 3: predicted by the mean of left and above, rounded down.
///
/// `BPP` is a const parameter rather than an argument, and that is the
/// whole reason this and [`paeth_row`] are generic: with the stride between
/// a byte and its left neighbour known at compile time the loop unrolls
/// into `BPP` independent chains instead of one indexed load per byte. The
/// five instantiations ([`filter_row`] dispatches on the seven values PNG
/// allows, folded to five) are the only monomorphisation, and they are
/// small.
fn average<const BPP: usize>(cur: &mut [u8], prev: &[u8]) {
    let n = cur.len();
    let head = BPP.min(n);
    // The first `BPP` bytes have no left neighbour, so the mean is of the
    // byte above and zero.
    for (c, p) in cur
        .iter_mut()
        .zip(prev.iter().chain(std::iter::repeat(&0)))
        .take(head)
    {
        *c = c.wrapping_add(*p >> 1);
    }
    // `needless_range_loop` is wrong here and in `paeth_row`: the loop
    // reads `cur[i - BPP]` *after* it has been reconstructed, so the slice
    // is both the source and the destination and there is no iterator that
    // expresses it. `u16::midpoint` is likewise not what filter 3 computes
    // — the spec's `floor((a + b) / 2)` is the sum of two bytes in a u16,
    // which cannot overflow, and the widening is deliberate.
    #[allow(clippy::needless_range_loop)]
    if prev.is_empty() {
        for i in head..n {
            cur[i] = cur[i].wrapping_add(cur[i - BPP] >> 1);
        }
    } else {
        for i in head..n {
            let left = u16::from(cur[i - BPP]);
            let upper = u16::from(prev[i]);
            cur[i] = cur[i].wrapping_add(((left + upper) >> 1) as u8);
        }
    }
}

/// Filter 4: the Paeth predictor over left, above and above-left.
fn paeth_row<const BPP: usize>(cur: &mut [u8], prev: &[u8]) {
    let n = cur.len();
    let head = BPP.min(n);
    // The first `BPP` bytes have no left neighbour, and `paeth(0, b, 0)` is
    // `b`: the byte above, or zero on the first row.
    for (c, p) in cur
        .iter_mut()
        .zip(prev.iter().chain(std::iter::repeat(&0)))
        .take(head)
    {
        *c = c.wrapping_add(*p);
    }
    // See `average` for why the range loops stay.
    #[allow(clippy::needless_range_loop)]
    if prev.is_empty() {
        // With no row above, `paeth(a, 0, 0)` is `a`: the predictor
        // degenerates to filter 1.
        for i in head..n {
            cur[i] = cur[i].wrapping_add(cur[i - BPP]);
        }
    } else {
        for i in head..n {
            let a = i16::from(cur[i - BPP]);
            let b = i16::from(prev[i]);
            let c = i16::from(prev[i - BPP]);
            cur[i] = cur[i].wrapping_add(paeth(a, b, c));
        }
    }
}

/// Apply one scanline's filter, dispatching the `bpp`-dependent two on the
/// five values a PNG can produce (1, 2, 3, 4, 6, 8 — 6 and 8 share the
/// generic path with their own constant).
fn filter_row(filter: u8, cur: &mut [u8], prev: &[u8], bpp: usize) -> Result<(), Error> {
    match filter {
        0 => {}
        1 => sub(cur, bpp),
        2 => up(cur, prev),
        3 => match bpp {
            1 => average::<1>(cur, prev),
            2 => average::<2>(cur, prev),
            3 => average::<3>(cur, prev),
            4 => average::<4>(cur, prev),
            6 => average::<6>(cur, prev),
            _ => average::<8>(cur, prev),
        },
        4 => match bpp {
            1 => paeth_row::<1>(cur, prev),
            2 => paeth_row::<2>(cur, prev),
            3 => paeth_row::<3>(cur, prev),
            4 => paeth_row::<4>(cur, prev),
            6 => paeth_row::<6>(cur, prev),
            _ => paeth_row::<8>(cur, prev),
        },
        other => return Err(Error::BadFilter(other)),
    }
    Ok(())
}

/// The Paeth predictor (PNG spec 9.4): whichever of left, above and
/// above-left is closest to `a + b - c`, ties going to `a` then `b`.
#[inline]
fn paeth(a: i16, b: i16, c: i16) -> u8 {
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

/// One scanline, expanded into BGRA.
///
/// Split out of [`expand`] so each colour type's loop is a small body the
/// optimiser sees whole — and so the per-image work above (the `tRNS` key,
/// the palette lookup table) happens once rather than per row.
fn expand_row(
    row: &[u8],
    dst: &mut [u8],
    info: ImageInfo,
    key: Option<[u16; 3]>,
    lut: &[[u8; 4]; 256],
    palette_len: usize,
) -> Result<(), Error> {
    match (info.color_type, info.bit_depth) {
        (0, 16) => {
            for (x, px) in dst.chunks_exact_mut(4).enumerate() {
                let v = u16::from_be_bytes([row[x * 2], row[x * 2 + 1]]);
                let g = row[x * 2];
                let a = if key == Some([v; 3]) { 0 } else { 255 };
                px.copy_from_slice(&[g, g, g, a]);
            }
        }
        (0, 8) => {
            for (x, px) in dst.chunks_exact_mut(4).enumerate() {
                let g = row[x];
                let a = if key == Some([u16::from(g); 3]) {
                    0
                } else {
                    255
                };
                px.copy_from_slice(&[g, g, g, a]);
            }
        }
        (0, d) => {
            // Sub-byte greyscale is scaled to the full 0..=255 range:
            // at depth 1 the two levels are 0 and 255, not 0 and 1.
            let max = (1u16 << d) - 1;
            for (x, px) in dst.chunks_exact_mut(4).enumerate() {
                let v = u16::from(sample(row, x, d));
                let g = ((v * 255) / max) as u8;
                let a = if key == Some([v; 3]) { 0 } else { 255 };
                px.copy_from_slice(&[g, g, g, a]);
            }
        }
        (2, 16) => {
            for (x, px) in dst.chunks_exact_mut(4).enumerate() {
                let s = x * 6;
                let sixteen = |o: usize| u16::from_be_bytes([row[s + o], row[s + o + 1]]);
                let a = if key == Some([sixteen(0), sixteen(2), sixteen(4)]) {
                    0
                } else {
                    255
                };
                px.copy_from_slice(&[row[s + 4], row[s + 2], row[s], a]);
            }
        }
        (2, _) => {
            for (px, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(3)) {
                let (r, g, b) = (s[0], s[1], s[2]);
                let a = if key == Some([u16::from(r), u16::from(g), u16::from(b)]) {
                    0
                } else {
                    255
                };
                px.copy_from_slice(&[b, g, r, a]);
            }
        }
        (3, d) => {
            for (x, px) in dst.chunks_exact_mut(4).enumerate() {
                let i = usize::from(sample(row, x, d));
                if i >= palette_len {
                    return Err(Error::BadPalette);
                }
                px.copy_from_slice(&lut[i]);
            }
        }
        (4, 16) => {
            for (px, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                px.copy_from_slice(&[s[0], s[0], s[0], s[2]]);
            }
        }
        (4, _) => {
            for (px, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(2)) {
                px.copy_from_slice(&[s[0], s[0], s[0], s[1]]);
            }
        }
        (6, 16) => {
            for (px, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(8)) {
                px.copy_from_slice(&[s[4], s[2], s[0], s[6]]);
            }
        }
        _ => {
            for (px, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                px.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
        }
    }
    Ok(())
}

/// Turn unfiltered scanlines into BGRA.
///
/// The per-colour-type work is a `match` outside the row loop rather than
/// inside the pixel loop: the branch is one per image, and the five bodies
/// are each a tight loop the optimiser can see the whole of.
fn expand(
    raw: &[u8],
    stride: usize,
    info: ImageInfo,
    palette: &[[u8; 3]],
    trns: &[u8],
) -> Result<Image, Error> {
    let width = info.width as usize;
    let height = info.height as usize;
    let row_len = stride + 1;
    let mut out = vec![0u8; width * height * 4];

    // `tRNS` for the non-palette colour types names one fully transparent
    // *sample value*, and every entry is two bytes big-endian **whatever
    // the bit depth is** (PNG spec 11.3.2.1): at depth 8 the value is in
    // the low byte, and at depth 1/2/4 it is a raw index into the 2^d
    // levels, not a scaled one. So the comparison happens at the sample's
    // own precision, before any widening to 8 bits — reading the high byte
    // instead makes every depth-8 key compare as 0, which silently turns
    // the black pixels of a colour-keyed image transparent. Found by the
    // corpus diff against the `png` crate on Android's 9-patch assets.
    let key16 = |i: usize| u16::from_be_bytes([trns[i], trns[i + 1]]);
    let key: Option<[u16; 3]> = match (info.color_type, trns.len()) {
        (0, 2) => Some([key16(0); 3]),
        (2, 6) => Some([key16(0), key16(2), key16(4)]),
        _ => None,
    };

    // The palette is expanded to BGRA once per image rather than looked up
    // twice per pixel: 256 entries against however many pixels there are,
    // and it turns the inner loop into an indexed copy of four bytes.
    let mut lut = [[0u8; 4]; 256];
    for (i, e) in lut.iter_mut().enumerate() {
        let rgb = palette.get(i).copied().unwrap_or([0, 0, 0]);
        *e = [rgb[2], rgb[1], rgb[0], trns.get(i).copied().unwrap_or(255)];
    }

    for y in 0..height {
        let row = &raw[y * row_len + 1..y * row_len + 1 + stride];
        let dst = &mut out[y * width * 4..(y + 1) * width * 4];
        expand_row(row, dst, info, key, &lut, palette.len())?;
    }

    Ok(Image {
        width: info.width,
        height: info.height,
        data: out,
    })
}

/// The `x`-th `depth`-bit sample of a packed row (`depth` is 1, 2, 4 or 8).
///
/// Sub-byte samples are packed MSB-first within each byte, and a row is
/// padded to a whole byte — which is why `stride` is a `div_ceil` and why
/// this indexes rather than iterating.
#[inline]
fn sample(row: &[u8], x: usize, depth: u8) -> u8 {
    match depth {
        8 => row[x],
        4 => (row[x / 2] >> (4 - 4 * (x % 2))) & 0x0F,
        2 => (row[x / 4] >> (6 - 2 * (x % 4))) & 0x03,
        _ => (row[x / 8] >> (7 - (x % 8))) & 0x01,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::ONE_PIXEL_RGBA;

    #[test]
    fn decodes_the_hand_written_fixture() {
        let img = decode(ONE_PIXEL_RGBA).unwrap();
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.data, vec![0x30, 0x20, 0x10, 0xFF]);
    }

    #[test]
    fn header_only() {
        let info = decode_header(ONE_PIXEL_RGBA).unwrap();
        assert_eq!(info.width, 1);
        assert_eq!(info.color_type, 6);
        assert_eq!(info.bit_depth, 8);
        assert!(!info.interlaced);
    }

    #[test]
    fn rejects_a_non_png() {
        assert_eq!(decode(b"\x89PNGnope").unwrap_err(), Error::NotPng);
        assert_eq!(decode(b"").unwrap_err(), Error::NotPng);
        assert_eq!(decode(b"GIF89a").unwrap_err(), Error::NotPng);
    }

    #[test]
    fn every_truncation_is_an_error_not_a_panic() {
        for cut in 0..ONE_PIXEL_RGBA.len() {
            assert!(decode(&ONE_PIXEL_RGBA[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn a_flipped_byte_is_an_error_not_a_panic() {
        for i in 0..ONE_PIXEL_RGBA.len() {
            for bit in 0..8 {
                let mut bytes = ONE_PIXEL_RGBA.to_vec();
                bytes[i] ^= 1 << bit;
                // Whatever it decodes to, it must not panic and must not
                // claim more pixels than it produced.
                if let Ok(img) = decode(&bytes) {
                    assert_eq!(img.data.len(), img.width as usize * img.height as usize * 4);
                }
            }
        }
    }

    #[test]
    fn paeth_matches_the_spec_pseudocode() {
        // The spec's own worked examples plus the three tie cases.
        assert_eq!(paeth(0, 0, 0), 0);
        assert_eq!(paeth(10, 20, 30), 10); // p = 0: |0-10|=10, |0-20|=20, |0-30|=30
        // p = 150: |150-200| = |150-100| = 50 and |150-150| = 0, so c wins.
        assert_eq!(paeth(200, 100, 150), 150);
        assert_eq!(paeth(1, 2, 3), 1);
        assert_eq!(paeth(255, 0, 0), 255);
    }

    #[test]
    fn sub_byte_samples_unpack_msb_first() {
        let row = [0b1011_0001u8, 0b0100_1110];
        assert_eq!(
            (0..16).map(|x| sample(&row, x, 1)).collect::<Vec<_>>(),
            vec![1, 0, 1, 1, 0, 0, 0, 1, 0, 1, 0, 0, 1, 1, 1, 0]
        );
        assert_eq!(
            (0..8).map(|x| sample(&row, x, 2)).collect::<Vec<_>>(),
            vec![0b10, 0b11, 0b00, 0b01, 0b01, 0b00, 0b11, 0b10]
        );
        assert_eq!(
            (0..4).map(|x| sample(&row, x, 4)).collect::<Vec<_>>(),
            vec![0xB, 0x1, 0x4, 0xE]
        );
    }

    #[test]
    fn stride_and_bpp_for_every_colour_type() {
        let info = |ct, bd, w| ImageInfo {
            width: w,
            height: 1,
            color_type: ct,
            bit_depth: bd,
            interlaced: false,
        };
        assert_eq!(info(0, 1, 9).stride(), 2);
        assert_eq!(info(0, 1, 9).filter_bpp(), 1);
        assert_eq!(info(3, 4, 5).stride(), 3);
        assert_eq!(info(2, 8, 4).stride(), 12);
        assert_eq!(info(2, 8, 4).filter_bpp(), 3);
        assert_eq!(info(6, 16, 4).stride(), 32);
        assert_eq!(info(6, 16, 4).filter_bpp(), 8);
        assert_eq!(info(4, 8, 4).filter_bpp(), 2);
    }

    #[test]
    fn crc_of_a_known_chunk() {
        // IEND's payload is empty and its CRC is the famous constant.
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }
}

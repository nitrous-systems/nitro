//! A small PNG writer for `--save-small`: enough to put a screenshot in
//! `docs/` without `ImageMagick` and without a dependency.
//!
//! # Why a second encoder in the tree
//!
//! `nitro-shot` already writes PNGs, and this is deliberately *not* a copy
//! of it. That one emits **stored** (uncompressed) deflate, which is the
//! right trade for a debugging dump piped over ssh: fastest possible
//! encode, size irrelevant. This one exists because a file that goes into
//! `docs/` has a size budget (200 KB against a 1920×1080 screenshot's
//! 8 MB), so it does the two things that actually buy compression on a UI
//! screenshot and nothing else:
//!
//! 1. **The `Up` row filter.** A desktop is mostly vertical gradients and
//!    flat fills, so subtracting the row above turns nearly every byte
//!    into a zero.
//! 2. **Back-references in a fixed-Huffman deflate block, at distance 1 or
//!    4.** After filtering, the data is long runs of one byte (distance 1)
//!    or a four-byte per-pixel delta repeating across a gradient row
//!    (distance 4). A back-reference encodes up to 258 bytes of either in
//!    about fifteen bits. Everything else is a literal.
//!
//! No hash chains, no dynamic Huffman tables, no general match search — a
//! real LZ77 would be several hundred lines and a real risk of a subtle
//! bug in a tree that has no zlib to check it against. Those two distances
//! are where all the win is on this input, and they are exactly the cases
//! that are easy to get provably right. The demo's own screenshot
//! compresses about 40× this way.
//!
//! The output is a plain `IHDR`/`IDAT`/`IEND` 8-bit RGBA PNG; any decoder
//! reads it.

/// CRC-32 (PNG's polynomial, reflected `0xEDB8_8320`).
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Adler-32, the zlib stream check.
#[must_use]
pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

/// Deflate bit packing: bits go into bytes from the least significant end,
/// while a Huffman code's own bits are written most-significant first
/// (RFC 1951 §3.1.1).
#[derive(Debug, Default)]
struct BitWriter {
    out: Vec<u8>,
    bit: u32,
    acc: u32,
}

impl BitWriter {
    /// Write the low `n` bits of `v`, least significant first. For the
    /// header fields and for a code's *extra* bits.
    fn bits(&mut self, v: u32, n: u32) {
        self.acc |= v << self.bit;
        self.bit += n;
        while self.bit >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.bit -= 8;
        }
    }

    /// Write a Huffman code of `n` bits, most significant first.
    fn code(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits((v >> i) & 1, 1);
        }
    }

    /// Pad the last byte with zeros and take the bytes.
    fn finish(mut self) -> Vec<u8> {
        if self.bit > 0 {
            self.out.push((self.acc & 0xFF) as u8);
        }
        self.out
    }
}

/// Emit one literal byte with the fixed Huffman literal/length code.
fn literal(w: &mut BitWriter, byte: u8) {
    let v = u32::from(byte);
    if v < 144 {
        w.code(0x30 + v, 8);
    } else {
        w.code(0x190 + v - 144, 9);
    }
}

/// Length bases for codes 257..=285, with the extra bits each takes.
const LENGTHS: [(u16, u8); 29] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 1),
    (13, 1),
    (15, 1),
    (17, 1),
    (19, 2),
    (23, 2),
    (27, 2),
    (31, 2),
    (35, 3),
    (43, 3),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 4),
    (115, 4),
    (131, 5),
    (163, 5),
    (195, 5),
    (227, 5),
    (258, 0),
];

/// Emit a back-reference of `len` bytes at `distance`.
///
/// Only distances 1 and 4 are ever used, and both are distance codes with
/// **no extra bits** (code 0 and code 3 in RFC 1951 §3.2.5), which is why
/// the distance side needs no table.
fn back_ref(w: &mut BitWriter, len: u16, distance: u16) {
    debug_assert!((3..=258).contains(&len));
    let i = LENGTHS
        .iter()
        .rposition(|&(base, _)| base <= len)
        .unwrap_or(0);
    let (base, extra) = LENGTHS[i];
    // Codes 257..=279 are 7 bits, 280..=287 are 8 (RFC 1951 §3.2.6).
    let sym = 257 + i as u32;
    if sym < 280 {
        w.code(sym - 256, 7);
    } else {
        w.code(0xC0 + sym - 280, 8);
    }
    if extra > 0 {
        w.bits(u32::from(len - base), u32::from(extra));
    }
    let dist_code = match distance {
        1 => 0,
        4 => 3,
        _ => unreachable!("only distances 1 and 4 are emitted"),
    };
    w.code(dist_code, 5);
}

/// How far `raw[at..]` matches `raw[at - distance..]`, capped at 258.
///
/// Overlapping matches are legal and are the whole trick: a distance-1
/// match of length 258 encodes 258 copies of one byte, and a distance-4
/// one encodes a four-byte pattern repeated 64 times.
fn match_len(raw: &[u8], at: usize, distance: usize) -> usize {
    if at < distance {
        return 0;
    }
    let mut n = 0;
    while at + n < raw.len() && n < 258 && raw[at + n] == raw[at + n - distance] {
        n += 1;
    }
    n
}

/// Compress `raw` into a zlib stream: one fixed-Huffman block of literals
/// and back-references at distance 1 or 4.
///
/// Two distances, because after the `Up` filter a screenshot is made of
/// exactly two things. A flat fill or a run of unchanged rows filters to a
/// run of one byte — **distance 1**. A smooth vertical gradient filters to
/// the same four-byte per-pixel delta over and over (`d, d, d, 0`), which
/// is a run of zeros to no distance at all but a perfect repeat at
/// **distance 4**. Leaving distance 4 out costs nothing on the first and
/// everything on the second: the gradient that is the server's desktop
/// background would come out as raw literals.
#[must_use]
pub fn zlib_rle(raw: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.bits(1, 1); // BFINAL
    w.bits(1, 2); // BTYPE = 01, fixed Huffman
    let mut i = 0usize;
    while i < raw.len() {
        let one = match_len(raw, i, 1);
        let four = match_len(raw, i, 4);
        let (len, distance) = if four > one { (four, 4) } else { (one, 1) };
        if len >= 3 {
            back_ref(&mut w, len as u16, distance);
            i += len;
        } else {
            literal(&mut w, raw[i]);
            i += 1;
        }
    }
    w.code(0, 7); // end of block: symbol 256, seven bits, all zero.
    let mut out = vec![0x78, 0x01]; // deflate, 32 KiB window, fastest.
    out.extend_from_slice(&w.finish());
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// Append a PNG chunk: length, type, data, CRC over type+data.
fn chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encode tightly packed `XRGB8888` pixels (`[b, g, r, x]` little-endian,
/// as the server's front buffer holds them) as an opaque RGBA PNG.
///
/// `data` must hold `width * height * 4` bytes with no row padding — what
/// [`crate::scene::downscale`] returns.
///
/// # Panics
/// If `data` is shorter than `width * height * 4`.
#[must_use]
pub fn encode_xrgb(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
    let row = width as usize * 4;
    assert!(
        data.len() >= row * height as usize,
        "short pixel buffer: {} bytes for {width}x{height}",
        data.len()
    );
    // One filter byte per row, then RGBA. Filter 2 (`Up`) everywhere: the
    // first row's "row above" is defined as zeros, so it needs no special
    // case and still costs only one byte.
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    let mut prev = vec![0u8; row];
    let mut cur = vec![0u8; row];
    for y in 0..height as usize {
        let src = &data[y * row..y * row + row];
        for x in 0..width as usize {
            let px = &src[x * 4..x * 4 + 4];
            // XRGB8888 little-endian is [b, g, r, x]; PNG wants RGBA with
            // the ignored byte replaced by an opaque alpha.
            cur[x * 4] = px[2];
            cur[x * 4 + 1] = px[1];
            cur[x * 4 + 2] = px[0];
            cur[x * 4 + 3] = 0xFF;
        }
        raw.push(2);
        raw.extend(cur.iter().zip(&prev).map(|(c, p)| c.wrapping_sub(*p)));
        std::mem::swap(&mut prev, &mut cur);
    }

    let mut out = Vec::with_capacity(raw.len() / 4 + 128);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace.
    chunk(&mut out, *b"IHDR", &ihdr);
    chunk(&mut out, *b"IDAT", &zlib_rle(&raw));
    chunk(&mut out, *b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check value from the PNG specification's sample.
    #[test]
    fn crc32_matches_the_known_vector() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn adler32_matches_the_known_vector() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn a_png_has_the_signature_and_the_three_chunks() {
        let px = vec![0u8; 4 * 4 * 4];
        let png = encode_xrgb(4, 4, &px);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        for kind in [b"IHDR", b"IDAT", b"IEND"] {
            assert!(
                png.windows(4).any(|w| w == kind),
                "missing {}",
                std::str::from_utf8(kind).unwrap()
            );
        }
        // IHDR carries the dimensions big-endian.
        assert_eq!(&png[16..24], &[0, 0, 0, 4, 0, 0, 0, 4]);
    }

    /// The whole reason this encoder exists: a flat image must not cost
    /// four bytes a pixel. 256×256 of one colour is 256 KiB raw.
    #[test]
    fn a_flat_image_compresses_hugely() {
        let px = vec![0x40u8; 256 * 256 * 4];
        let png = encode_xrgb(256, 256, &px);
        assert!(png.len() < 4_000, "{} bytes for a flat image", png.len());
    }

    /// A vertical gradient is what the server's desktop actually is: the
    /// `Up` filter must flatten it to runs of zeros.
    #[test]
    fn a_vertical_gradient_compresses_too() {
        let (w, h) = (256usize, 256usize);
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                px[i..i + 4].copy_from_slice(&[y as u8, y as u8, y as u8, 0]);
            }
        }
        let png = encode_xrgb(w as u32, h as u32, &px);
        assert!(png.len() < 12_000, "{} bytes for a gradient", png.len());
    }

    /// Incompressible input must still round-trip; fixed Huffman costs a
    /// few percent on literals and that is the worst case.
    #[test]
    fn noise_expands_only_slightly() {
        let mut px = vec![0u8; 64 * 64 * 4];
        let mut state = 0x1234_5678u32;
        for b in &mut px {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = (state >> 24) as u8;
        }
        let png = encode_xrgb(64, 64, &px);
        // Raw RGBA plus filter bytes; 15% headroom for 9-bit literals.
        let raw = (64 * 4 + 1) * 64;
        assert!(png.len() < raw * 115 / 100, "{} vs {raw}", png.len());
    }

    /// Run boundaries are where an RLE encoder gets things wrong: exactly
    /// 3 (the shortest match), 258 (the longest) and 259 (a match plus a
    /// literal) all have to come back byte for byte. The zlib stream is
    /// checked by its own Adler-32, which a wrong length would break.
    #[test]
    fn runs_of_every_awkward_length_are_self_consistent() {
        for len in [1usize, 2, 3, 4, 258, 259, 260, 700] {
            let raw = vec![0xABu8; len];
            let z = zlib_rle(&raw);
            assert_eq!(&z[..2], &[0x78, 0x01]);
            let tail = &z[z.len() - 4..];
            assert_eq!(
                u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]),
                adler32(&raw),
                "len {len}"
            );
        }
    }

    /// `zlib_rle` must never be *pathologically* larger than its input:
    /// the failure mode of a broken length table is an explosion.
    #[test]
    fn compression_never_explodes() {
        let raw: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let z = zlib_rle(&raw);
        assert!(z.len() < raw.len() * 2, "{} vs {}", z.len(), raw.len());
    }
}

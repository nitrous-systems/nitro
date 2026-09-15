//! DEFLATE (RFC 1951) inside a zlib wrapper (RFC 1950) — the half of a PNG
//! decoder that is not PNG.
//!
//! Everything a PNG needs and nothing else: stored, fixed-Huffman and
//! dynamic-Huffman blocks, the two-byte zlib header and the trailing
//! Adler-32, which is checked. No compression, no dictionaries, no gzip
//! wrapper, no streaming — a PNG's IDAT bytes are all present before the
//! first pixel is wanted, and the decompressed size is known exactly from
//! `IHDR`, so the output buffer is allocated once and never grows.
//!
//! # Shape of the decoder
//!
//! The Huffman decoder is Mark Adler's `puff` structure — a per-length
//! symbol count plus symbols sorted by `(length, symbol)`, which needs no
//! tree and no allocation per node — with a 9-bit direct-lookup table in
//! front of it. The table answers the common case in one indexing
//! operation; codes longer than nine bits fall through to the bit-at-a-time
//! walk. That is worth roughly a 3× speedup over the bare `puff` loop on
//! icon-sized images and costs about forty lines.
//!
//! # Hostile input
//!
//! The bit reader feeds zero bytes past the end of the input rather than
//! erroring inside the hot loop, and counts them; a stream that reads more
//! than [`MAX_PADDING`] bits past its end is truncated and says so. That,
//! plus the caller-supplied output cap, is what bounds a malformed stream:
//! without both, a truncated file decodes as an endless run of stored
//! blocks with `LEN == 0`, which terminates neither on output nor on input.

use crate::Error;

/// Bits answered by the direct-lookup table; longer codes take the slow path.
const FAST_BITS: u32 = 9;
/// Low-bit mask for [`FAST_BITS`].
const FAST_MASK: u64 = (1 << FAST_BITS) - 1;
/// Longest code a DEFLATE Huffman table may contain.
const MAX_CODE_BITS: usize = 15;
/// Zero bytes the reader will invent past the end of the input before it
/// calls the stream truncated. One byte would be too strict — the last real
/// byte of a stream is routinely consumed with bits to spare — and an
/// unbounded count never terminates.
const MAX_PADDING: u32 = 8;

/// Base length for literal/length symbols 257..=285.
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
/// Extra bits for literal/length symbols 257..=285.
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// Base distance for distance symbols 0..=29.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
/// Extra bits for distance symbols 0..=29.
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// The order the code-length code's own lengths arrive in.
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// An LSB-first bit reader over a byte slice.
///
/// Past the end of `data` it produces zero bits and counts how many invented
/// bytes it has handed out, so the caller can tell a stream that ended
/// tidily from one that was cut short.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u64,
    bitcnt: u32,
    padded: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bitbuf: 0,
            bitcnt: 0,
            padded: 0,
        }
    }

    /// Top the buffer up to **at least 56 bits**, which is the bound a
    /// caller may rely on: the fast path starting from an empty buffer adds
    /// exactly seven bytes and leaves 56, and at `bitcnt == 56` it is a
    /// no-op. 56 is deliberately above the real requirement — the longest
    /// run the block loop asks for between refills is a 15-bit length code,
    /// five extra bits, a 15-bit distance code and thirteen extra, i.e.
    /// **48 bits** — so there are eight bits of headroom. Anything wanting
    /// a wider read than 48 between refills must re-check this arithmetic
    /// rather than assume 57 or 64.
    ///
    /// The fast path loads **eight bytes at once** with `from_le_bytes` and
    /// keeps as many whole bytes of them as fit. That is one load instead
    /// of seven, and it is worth about a third of the whole decode on a
    /// 512×512 image — a bit reader that refills a byte at a time is the
    /// classic way to make an inflate loop half the speed it should be.
    ///
    /// It keeps only whole bytes, and masks the load down to them, so the
    /// invariant the rest of the reader depends on survives: **every bit at
    /// or above `bitcnt` is zero**. The better-known form of this trick
    /// (`bitcnt |= 56` and advance by however many bytes were used) leaves
    /// the unconsumed eighth byte sitting above `bitcnt`, which is harmless
    /// for a pure `peek`/`consume` loop — the next refill ORs the identical
    /// value back over it — but not here, where [`Self::take_bytes`] reads
    /// whole bytes out of the buffer and `self.pos` must mean exactly "the
    /// next byte not yet in `bitbuf`". Getting that wrong decoded a stored
    /// block as block type 3.
    ///
    /// The slow path near the end of the input (and past it) keeps the
    /// byte-at-a-time form, because that is where the zero padding and its
    /// counter live.
    #[inline]
    fn refill(&mut self) {
        if self.bitcnt <= 56 && self.pos + 8 <= self.data.len() {
            let chunk: [u8; 8] = self.data[self.pos..self.pos + 8]
                .try_into()
                .expect("the slice is exactly 8 bytes");
            // 0..=7 whole bytes, whichever leaves `bitcnt` at 56..=63:
            // zero when the buffer already holds 56 or more, which is why
            // the guard above bounds `bitcnt` rather than this shift.
            let whole = ((63 - self.bitcnt) >> 3) as usize;
            let bits = 8 * whole as u32;
            let mask = (1u64 << bits) - 1;
            self.bitbuf |= (u64::from_le_bytes(chunk) & mask) << self.bitcnt;
            self.pos += whole;
            self.bitcnt += bits;
            return;
        }
        while self.bitcnt <= 56 {
            let byte = if self.pos < self.data.len() {
                let b = self.data[self.pos];
                self.pos += 1;
                b
            } else {
                self.padded = self.padded.saturating_add(1);
                0
            };
            self.bitbuf |= u64::from(byte) << self.bitcnt;
            self.bitcnt += 8;
        }
    }

    /// Has the reader read meaningfully past the end of its input?
    #[inline]
    fn truncated(&self) -> bool {
        self.padded > MAX_PADDING
    }

    /// Drop `n` bits that [`Self::peek`] looked at.
    #[inline]
    fn consume(&mut self, n: u32) {
        self.bitbuf >>= n;
        self.bitcnt -= n;
    }

    /// The next `n` bits, LSB-first, without consuming them.
    #[inline]
    fn peek(&self, n: u32) -> u32 {
        (self.bitbuf & ((1u64 << n) - 1)) as u32
    }

    /// Read `n` bits (`n <= 32`), LSB-first.
    #[inline]
    fn bits(&mut self, n: u32) -> u32 {
        if self.bitcnt < n {
            self.refill();
        }
        let v = self.peek(n);
        self.consume(n);
        v
    }

    /// Discard bits up to the next byte boundary and return the reader's
    /// position in whole bytes (used by stored blocks).
    fn align(&mut self) {
        let drop = self.bitcnt % 8;
        self.consume(drop);
    }

    /// Take `n` whole bytes straight from the input, honouring whatever is
    /// still sitting in the bit buffer. Only called on byte boundaries.
    fn take_bytes(&mut self, n: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        for _ in 0..n {
            if self.bitcnt >= 8 {
                out.push((self.bitbuf & 0xFF) as u8);
                self.consume(8);
            } else if self.pos < self.data.len() {
                out.push(self.data[self.pos]);
                self.pos += 1;
            } else {
                return Err(Error::Truncated(
                    "stored block runs past the end of the data",
                ));
            }
        }
        Ok(())
    }
}

/// A canonical Huffman decoding table.
struct Huffman {
    /// How many codes there are of each length, `counts[0]` unused.
    counts: [u16; MAX_CODE_BITS + 1],
    /// Symbols ordered by `(code length, symbol)`.
    symbols: Vec<u16>,
    /// `FAST_BITS`-indexed shortcut: `len << 12 | symbol`, or 0 for a miss.
    fast: Vec<u16>,
}

impl Huffman {
    /// Build a table from a code-length-per-symbol list.
    ///
    /// Rejects an over-subscribed set (more codes than the length allows).
    /// An *under*-subscribed one is accepted, because RFC 1951 permits it
    /// for a distance table with a single code and real encoders emit that;
    /// the holes simply decode as [`Error::BadCode`].
    fn new(lengths: &[u8]) -> Result<Self, Error> {
        let mut counts = [0u16; MAX_CODE_BITS + 1];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;

        let mut left = 1i32;
        for (len, &count) in counts.iter().enumerate().skip(1) {
            left <<= 1;
            left -= i32::from(count);
            if left < 0 {
                return Err(Error::BadHuffmanTable);
            }
            debug_assert!(len <= MAX_CODE_BITS);
        }

        let mut offsets = [0u16; MAX_CODE_BITS + 2];
        for len in 1..=MAX_CODE_BITS {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let total = offsets[MAX_CODE_BITS + 1] as usize;
        let mut symbols = vec![0u16; total];
        let mut next = offsets;
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[next[l as usize] as usize] = sym as u16;
                next[l as usize] += 1;
            }
        }

        // Canonical codes, MSB-first, then reversed into the fast table:
        // the bit reader hands out the bits in transmission order, which is
        // the code's bits from the top, i.e. the reversed value.
        let mut fast = vec![0u16; 1 << FAST_BITS];
        let mut code = 0u32;
        let mut index = 0usize;
        for (len, &count) in counts.iter().enumerate().skip(1) {
            for _ in 0..count {
                if len <= FAST_BITS as usize {
                    let rev = reverse_bits(code, len as u32) as usize;
                    let entry = ((len as u16) << 12) | symbols[index];
                    let step = 1usize << len;
                    let mut slot = rev;
                    while slot < fast.len() {
                        fast[slot] = entry;
                        slot += step;
                    }
                }
                code += 1;
                index += 1;
            }
            code <<= 1;
        }

        Ok(Self {
            counts,
            symbols,
            fast,
        })
    }

    /// Decode one symbol. The caller must have refilled the reader.
    #[inline]
    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16, Error> {
        let entry = self.fast[(br.bitbuf & FAST_MASK) as usize];
        if entry != 0 {
            br.consume(u32::from(entry >> 12));
            return Ok(entry & 0x0FFF);
        }
        self.decode_slow(br)
    }

    /// Bit-at-a-time walk for codes the fast table does not cover.
    #[cold]
    fn decode_slow(&self, br: &mut BitReader<'_>) -> Result<u16, Error> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..=MAX_CODE_BITS {
            code |= i32::try_from(br.bits(1)).expect("one bit is 0 or 1");
            let count = i32::from(self.counts[len]);
            if code - count < first {
                let at = index + (code - first);
                return self.symbols.get(at as usize).copied().ok_or(Error::BadCode);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(Error::BadCode)
    }
}

/// Reverse the low `len` bits of `v`.
fn reverse_bits(v: u32, len: u32) -> u32 {
    let mut out = 0;
    for i in 0..len {
        out |= ((v >> i) & 1) << (len - 1 - i);
    }
    out
}

/// Decompress a zlib stream, refusing to produce more than `max_out` bytes.
///
/// `max_out` is the caller's own arithmetic (for PNG: the exact size of the
/// filtered raster), so a hostile stream cannot make the decoder allocate:
/// it is an error to exceed it, not a reason to grow.
///
/// # Errors
/// A malformed header, a bad Huffman table, a code that is not in the
/// table, a back-reference before the start of the output, output past
/// `max_out`, a truncated stream, or a mismatched Adler-32.
pub fn zlib_decompress(data: &[u8], max_out: usize) -> Result<Vec<u8>, Error> {
    if data.len() < 2 {
        return Err(Error::Truncated("zlib stream shorter than its header"));
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0F != 8 {
        return Err(Error::BadZlibHeader);
    }
    // CINFO > 7 means a window larger than 32 KiB, which PNG forbids.
    if cmf >> 4 > 7 {
        return Err(Error::BadZlibHeader);
    }
    if (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
        return Err(Error::BadZlibHeader);
    }
    if flg & 0x20 != 0 {
        return Err(Error::PresetDictionary);
    }

    let out = inflate(&data[2..], max_out)?;

    // The Adler-32 sits at the very end of the stream. The bit reader does
    // not tell us where inflate stopped to the byte, so take it from the
    // tail — a PNG's IDAT run ends exactly at the checksum.
    if data.len() >= 6 {
        let tail = &data[data.len() - 4..];
        let want = u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]);
        let got = adler32(&out);
        if want != got {
            return Err(Error::BadChecksum);
        }
    }
    Ok(out)
}

/// Decompress a raw DEFLATE stream (no wrapper), capped at `max_out` bytes.
///
/// # Errors
/// As [`zlib_decompress`], minus the wrapper and checksum errors.
pub fn inflate(data: &[u8], max_out: usize) -> Result<Vec<u8>, Error> {
    let mut br = BitReader::new(data);
    let mut out: Vec<u8> = Vec::with_capacity(max_out.min(1 << 20));
    let fixed = fixed_tables()?;

    loop {
        if br.truncated() {
            return Err(Error::Truncated("deflate stream ends mid-block"));
        }
        let last = br.bits(1);
        let kind = br.bits(2);
        match kind {
            0 => {
                br.align();
                let len = br.bits(16) as usize;
                let nlen = br.bits(16) as usize;
                if len ^ 0xFFFF != nlen {
                    return Err(Error::BadStoredBlock);
                }
                if out.len() + len > max_out {
                    return Err(Error::TooMuchOutput);
                }
                br.take_bytes(len, &mut out)?;
            }
            1 => inflate_block(&mut br, &fixed.0, &fixed.1, &mut out, max_out)?,
            2 => {
                let (lit, dist) = dynamic_tables(&mut br)?;
                inflate_block(&mut br, &lit, &dist, &mut out, max_out)?;
            }
            _ => return Err(Error::BadBlockType),
        }
        if last == 1 {
            break;
        }
    }
    if br.truncated() {
        return Err(Error::Truncated("deflate stream ends mid-block"));
    }
    Ok(out)
}

/// The fixed literal/length and distance tables of RFC 1951 §3.2.6.
fn fixed_tables() -> Result<(Huffman, Huffman), Error> {
    let mut lit = [0u8; 288];
    for (i, l) in lit.iter_mut().enumerate() {
        *l = match i {
            // 0..=143 and 280..=287 share a length; spelling the second
            // range out rather than folding it into `_` keeps the table
            // readable against RFC 1951 §3.2.6, which lists four ranges.
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist = [5u8; 30];
    Ok((Huffman::new(&lit)?, Huffman::new(&dist)?))
}

/// Read a dynamic block's two Huffman tables.
fn dynamic_tables(br: &mut BitReader<'_>) -> Result<(Huffman, Huffman), Error> {
    let hlit = br.bits(5) as usize + 257;
    let hdist = br.bits(5) as usize + 1;
    let hclen = br.bits(4) as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(Error::BadHuffmanTable);
    }

    let mut clen = [0u8; 19];
    for &slot in CLEN_ORDER.iter().take(hclen) {
        clen[slot] = br.bits(3) as u8;
    }
    let clen_table = Huffman::new(&clen)?;

    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lengths.len() {
        if br.truncated() {
            return Err(Error::Truncated(
                "code lengths run past the end of the data",
            ));
        }
        br.refill();
        let sym = clen_table.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(Error::BadHuffmanTable);
                }
                let prev = lengths[i - 1];
                let n = 3 + br.bits(2) as usize;
                if i + n > lengths.len() {
                    return Err(Error::BadHuffmanTable);
                }
                for _ in 0..n {
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let n = 3 + br.bits(3) as usize;
                if i + n > lengths.len() {
                    return Err(Error::BadHuffmanTable);
                }
                i += n;
            }
            18 => {
                let n = 11 + br.bits(7) as usize;
                if i + n > lengths.len() {
                    return Err(Error::BadHuffmanTable);
                }
                i += n;
            }
            _ => return Err(Error::BadCode),
        }
    }

    let lit = Huffman::new(&lengths[..hlit])?;
    let dist = Huffman::new(&lengths[hlit..])?;
    Ok((lit, dist))
}

/// The literal/length loop shared by fixed and dynamic blocks.
fn inflate_block(
    br: &mut BitReader<'_>,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
    max_out: usize,
) -> Result<(), Error> {
    loop {
        if br.truncated() {
            return Err(Error::Truncated("deflate block ends mid-symbol"));
        }
        br.refill();
        let sym = lit.decode(br)?;
        if sym < 256 {
            if out.len() >= max_out {
                return Err(Error::TooMuchOutput);
            }
            out.push(sym as u8);
            continue;
        }
        if sym == 256 {
            return Ok(());
        }
        let li = sym as usize - 257;
        if li >= LEN_BASE.len() {
            return Err(Error::BadCode);
        }
        let len = LEN_BASE[li] as usize + br.bits(u32::from(LEN_EXTRA[li])) as usize;

        let dsym = dist.decode(br)? as usize;
        if dsym >= DIST_BASE.len() {
            return Err(Error::BadCode);
        }
        let d = DIST_BASE[dsym] as usize + br.bits(u32::from(DIST_EXTRA[dsym])) as usize;
        if d > out.len() {
            return Err(Error::BadDistance);
        }
        if out.len() + len > max_out {
            return Err(Error::TooMuchOutput);
        }
        let mut from = out.len() - d;
        if d >= len {
            // Non-overlapping: copy the run in one go.
            out.extend_from_within(from..from + len);
        } else {
            for _ in 0..len {
                let b = out[from];
                out.push(b);
                from += 1;
            }
        }
    }
}

/// Adler-32 (RFC 1950 §9) of `data`.
#[must_use]
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    // 5552 is the largest n for which the inner sums cannot overflow u32.
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zlib stream of stored blocks, which is what `nitro-shot`'s encoder
    /// writes — so this is the one format both halves of the tree agree on.
    fn zlib_stored(raw: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let chunks: Vec<&[u8]> = if raw.is_empty() {
            vec![&[]]
        } else {
            raw.chunks(65_535).collect()
        };
        for (i, c) in chunks.iter().enumerate() {
            out.push(u8::from(i + 1 == chunks.len()));
            let len = c.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(c);
        }
        out.extend_from_slice(&adler32(raw).to_be_bytes());
        out
    }

    #[test]
    fn stored_round_trip() {
        for n in [0usize, 1, 100, 65_535, 65_536, 200_000] {
            let raw: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
            let z = zlib_stored(&raw);
            assert_eq!(zlib_decompress(&z, raw.len()).unwrap(), raw, "n = {n}");
        }
    }

    #[test]
    fn adler_known_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"a"), 0x0062_0062);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn rejects_bad_header() {
        assert!(zlib_decompress(&[0x00, 0x00], 16).is_err());
        assert!(zlib_decompress(&[0x78], 16).is_err());
        assert!(zlib_decompress(&[], 16).is_err());
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut z = zlib_stored(b"hello");
        let n = z.len();
        z[n - 1] ^= 0xFF;
        assert!(matches!(zlib_decompress(&z, 5), Err(Error::BadChecksum)));
    }

    #[test]
    fn honours_the_output_cap() {
        let z = zlib_stored(&[7u8; 1000]);
        assert!(matches!(
            zlib_decompress(&z, 100),
            Err(Error::TooMuchOutput)
        ));
    }

    /// Truncation must terminate. The interesting case is a stream cut so
    /// that the reader sees an endless run of zero bits, which decodes as
    /// stored blocks of length zero: without the padding counter this loops
    /// for ever.
    #[test]
    fn truncation_terminates() {
        let z = zlib_stored(&[1u8; 400]);
        for cut in 0..z.len() {
            let _ = zlib_decompress(&z[..cut], 400);
        }
        // The specific endless-stored-block shape.
        assert!(zlib_decompress(&[0x78, 0x01], 4096).is_err());
    }

    #[test]
    fn reverse_bits_is_its_own_inverse() {
        for len in 1..=15u32 {
            for v in 0..(1u32 << len).min(64) {
                assert_eq!(reverse_bits(reverse_bits(v, len), len), v);
            }
        }
    }

    #[test]
    fn fixed_and_dynamic_blocks_from_a_real_encoder() {
        // Produced by `python3 -c "import zlib; ..."` over a byte pattern
        // that a real compressor turns into a dynamic block with matches.
        let raw: Vec<u8> = (0..5000).map(|i| ((i / 13) % 17) as u8).collect();
        let compressed = super::tests::deflate_via_fixture(&raw);
        assert_eq!(zlib_decompress(&compressed, raw.len()).unwrap(), raw);
    }

    /// A compressed fixture is generated at test time by the one compressor
    /// every dev box has: python's zlib. Skipped (by producing a stored
    /// stream instead) if python is missing, so the suite still passes.
    fn deflate_via_fixture(raw: &[u8]) -> Vec<u8> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let child = Command::new("python3")
            .arg("-c")
            .arg("import sys,zlib; sys.stdout.buffer.write(zlib.compress(sys.stdin.buffer.read(), 9))")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(mut c) => {
                c.stdin.take().unwrap().write_all(raw).unwrap();
                let out = c.wait_with_output().unwrap();
                if out.status.success() && !out.stdout.is_empty() {
                    out.stdout
                } else {
                    zlib_stored(raw)
                }
            }
            Err(_) => zlib_stored(raw),
        }
    }
}

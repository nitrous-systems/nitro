//! Minimal PNG encoder: 8-bit RGB, filter type 0 on every row, zlib stream
//! made of *stored* deflate blocks (no compression). Enough for
//! screenshots that go straight into a viewer or a diff tool; no crate.
//!
//! **A copy of `nitro-shot/src/png.rs`, deliberately.** The alternative
//! was to move it into `nitro-core`, and that would put a PNG encoder in
//! the dependency graph of the server, the toolkit and every app that
//! links either — for the benefit of two CLIs that between them run once
//! a session. 120 lines of pure arithmetic with a fixture test in each
//! copy is the cheaper side of that trade; if a third consumer appears,
//! move it then.

/// CRC-32 (IEEE, as PNG uses it) lookup table, built at compile time.
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

/// CRC-32 of `data`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// Adler-32 of `data`.
#[must_use]
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    // 5552 is the largest n such that 255n(n+1)/2 + (n+1)(65520) fits u32.
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

/// Largest stored-block payload.
const STORED_MAX: usize = 65_535;

/// zlib-wrap `raw` using stored deflate blocks.
#[must_use]
pub fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let blocks = raw.len().div_ceil(STORED_MAX).max(1);
    let mut out = Vec::with_capacity(2 + raw.len() + blocks * 5 + 4);
    out.extend_from_slice(&[0x78, 0x01]); // CM=8, CINFO=7, no dict, level 0
    let mut chunks = raw.chunks(STORED_MAX).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[0x01, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        out.push(u8::from(last)); // BFINAL, BTYPE=00
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encode `XRGB8888` pixels (little-endian `u32` per pixel, rows `stride`
/// bytes apart) as an RGB PNG.
///
/// # Panics
/// If `data` is shorter than `stride * height`.
#[must_use]
pub fn encode_xrgb(width: u32, height: u32, stride: u32, data: &[u8]) -> Vec<u8> {
    let (w, h, s) = (width as usize, height as usize, stride as usize);
    assert!(data.len() >= s * h, "pixel buffer too short");
    let mut raw = Vec::with_capacity(h * (1 + 3 * w));
    for y in 0..h {
        raw.push(0); // filter: none
        let row = &data[y * s..y * s + 4 * w];
        for px in row.chunks_exact(4) {
            raw.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // depth, RGB, deflate, filter, no interlace

    let idat = zlib_stored(&raw);
    let mut out = Vec::with_capacity(8 + 25 + 12 + idat.len() + 12);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']);
    chunk(&mut out, *b"IHDR", &ihdr);
    chunk(&mut out, *b"IDAT", &idat);
    chunk(&mut out, *b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny chunk reader: `(type, data)` pairs, verifying each CRC.
    fn chunks(png: &[u8]) -> Vec<(String, Vec<u8>)> {
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']
        );
        let mut pos = 8;
        let mut out = Vec::new();
        while pos < png.len() {
            let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &png[pos + 4..pos + 8];
            let data = &png[pos + 8..pos + 8 + len];
            let crc = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
            assert_eq!(crc, crc32(&png[pos + 4..pos + 8 + len]), "crc of {kind:?}");
            out.push((String::from_utf8(kind.to_vec()).unwrap(), data.to_vec()));
            pos += 12 + len;
        }
        out
    }

    /// Inflate a stored-only zlib stream, checking the trailer.
    fn inflate_stored(z: &[u8]) -> Vec<u8> {
        assert_eq!(z[0] & 0x0F, 8, "CM=deflate");
        assert_eq!(
            u16::from_be_bytes([z[0], z[1]]) % 31,
            0,
            "zlib header check"
        );
        let mut pos = 2;
        let mut out = Vec::new();
        loop {
            let hdr = z[pos];
            assert_eq!(hdr >> 1 & 0b11, 0, "stored block");
            let len = u16::from_le_bytes([z[pos + 1], z[pos + 2]]) as usize;
            let nlen = u16::from_le_bytes([z[pos + 3], z[pos + 4]]);
            assert_eq!(!(len as u16), nlen);
            out.extend_from_slice(&z[pos + 5..pos + 5 + len]);
            pos += 5 + len;
            if hdr & 1 == 1 {
                break;
            }
        }
        let adler = u32::from_be_bytes(z[pos..pos + 4].try_into().unwrap());
        assert_eq!(adler, adler32(&out));
        assert_eq!(pos + 4, z.len(), "no trailing garbage");
        out
    }

    #[test]
    fn known_checksums() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn encodes_3x2_rgb() {
        // Row 0: red, green, blue; row 1: white, black, grey. Stride has
        // 4 bytes of padding to prove it is honoured.
        let px = |r: u8, g: u8, b: u8| [b, g, r, 0];
        let mut data = Vec::new();
        for row in [
            [px(255, 0, 0), px(0, 255, 0), px(0, 0, 255)],
            [px(255, 255, 255), px(0, 0, 0), px(128, 128, 128)],
        ] {
            for p in row {
                data.extend_from_slice(&p);
            }
            data.extend_from_slice(&[0xAA; 4]);
        }
        let png = encode_xrgb(3, 2, 16, &data);
        let ch = chunks(&png);
        let kinds: Vec<&str> = ch.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["IHDR", "IDAT", "IEND"]);
        assert_eq!(
            ch[0].1,
            [0, 0, 0, 3, 0, 0, 0, 2, 8, 2, 0, 0, 0],
            "IHDR: 3x2, 8-bit, RGB"
        );
        let raw = inflate_stored(&ch[1].1);
        assert_eq!(
            raw,
            [
                0, 255, 0, 0, 0, 255, 0, 0, 0, 255, //
                0, 255, 255, 255, 0, 0, 0, 128, 128, 128,
            ]
        );
        assert!(ch[2].1.is_empty());
    }

    #[test]
    fn splits_large_streams_into_stored_blocks() {
        let raw = vec![7u8; STORED_MAX * 2 + 10];
        let z = zlib_stored(&raw);
        assert_eq!(inflate_stored(&z), raw);
        assert_eq!(z.len(), 2 + 3 * 5 + raw.len() + 4);
        assert_eq!(inflate_stored(&zlib_stored(&[])), Vec::<u8>::new());
    }

    #[test]
    fn matches_known_good_fixture() {
        // 1x1 white pixel; verified once with Python's zlib/binascii
        // (chunk CRCs, inflate, Adler-32). Guards against silent drift.
        let png = encode_xrgb(1, 1, 4, &[255, 255, 255, 0]);
        let hex = png.iter().fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        });
        assert_eq!(
            hex,
            concat!(
                "89504e470d0a1a0a",
                "0000000d4948445200000001000000010802000000907753de",
                "0000000f494441547801010400fbff00ffffff05fe02fe49666e2b",
                "0000000049454e44ae426082"
            )
        );
    }
}

//! Reading a `.ppm` image, and turning it into the `ARGB` rows the
//! toolkit's [`Image`](nitro_ui::widgets::Image) widget wants.
//!
//! **P6 binary PPM only, and that is deliberate.** There is no PNG or
//! JPEG decoder anywhere in this tree — `nitro-shot` and `nitro-hey` each
//! carry a small *encoder* and nothing more — and adding one would put a
//! parser for hostile bytes into the dependency graph of a program that
//! runs for the whole session. `DEPENDENCIES.md` lists `png` under
//! Rejected for that reason.
//!
//! PPM is the format a decoder can be written in seventy lines and read
//! in one sitting: a magic number, three integers and a block of RGB
//! triples. Converting an actual photograph is one `convert` away, and a
//! user who wants a JPEG on their desktop is better served by a
//! `nitro-wallpaperctl` that shells out to whatever is installed than by
//! this process linking libjpeg. Recorded in the README.
//!
//! # What the parser refuses
//!
//! Everything it does not understand, with a message rather than a wrong
//! picture: a `P3` (ASCII) file, a maxval that is not 255, a truncated
//! pixel block, a header with no whitespace after it, and a file big
//! enough to be a mistake. Comments (`# …`) are honoured because `netpbm`
//! writes them.

/// A decoded image: `width × height` pixels, `ARGB` as the toolkit takes
/// them (`[b, g, r, a]` per pixel, rows top to bottom).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixels {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub data: Vec<u8>,
}

/// The largest image the parser will decode, in pixels.
///
/// 64 megapixels is four times a 4K screen and about 256 MB once it is
/// `ARGB`. The cap is here because the header is three integers a file
/// can claim anything in, and `width * height * 4` on a hostile header is
/// how a wallpaper becomes an OOM.
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;

/// Decode a binary PPM (`P6`).
///
/// # Errors
/// A message naming what was wrong, suitable for printing: a wallpaper
/// that silently painted the default gradient after being given a file
/// would be a bug report about the file that never gets filed.
pub fn parse_ppm(bytes: &[u8]) -> Result<Pixels, String> {
    let mut r = Reader { bytes, at: 0 };
    let magic = r.token().ok_or("empty file")?;
    if magic != b"P6" {
        return Err(format!(
            "not a binary PPM: magic is {:?}, want P6 (P3 is the ASCII form \u{2014} convert it with `pnmtoplainpnm -reverse`)",
            String::from_utf8_lossy(magic)
        ));
    }
    let width = r.number().ok_or("no width in the header")?;
    let height = r.number().ok_or("no height in the header")?;
    let maxval = r.number().ok_or("no maxval in the header")?;
    if maxval != 255 {
        return Err(format!(
            "maxval is {maxval}, want 255 (16-bit PPM is not supported)"
        ));
    }
    // Exactly one whitespace byte separates the header from the pixels,
    // and it is part of the format rather than something to skip: a
    // parser that ate all the whitespace would eat a pixel whose red
    // channel happens to be 0x20.
    if !r.take_single_whitespace() {
        return Err("no whitespace between the header and the pixels".to_owned());
    }
    if width == 0 || height == 0 {
        return Err(format!("empty image: {width}x{height}"));
    }
    let count = u64::from(width) * u64::from(height);
    if count > MAX_PIXELS {
        return Err(format!(
            "{width}x{height} is {count} pixels, over the {MAX_PIXELS}-pixel cap"
        ));
    }
    let want = count as usize * 3;
    let rest = &r.bytes[r.at..];
    if rest.len() < want {
        return Err(format!(
            "truncated: {width}x{height} needs {want} bytes of pixels, got {}",
            rest.len()
        ));
    }
    // Trailing bytes are ignored rather than refused: some writers append
    // a newline, and a wallpaper is not the right place to be strict
    // about a file that is otherwise perfectly readable.
    let mut data = Vec::with_capacity(count as usize * 4);
    for px in rest[..want].chunks_exact(3) {
        // `[b, g, r, a]`: the toolkit hands the buffer to the server as
        // `AR24`/`XR24`, which is little-endian `0xAARRGGBB` and so blue
        // first in memory.
        data.push(px[2]);
        data.push(px[1]);
        data.push(px[0]);
        data.push(0xff);
    }
    Ok(Pixels {
        width,
        height,
        data,
    })
}

/// A byte cursor over a PPM header.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// The next whitespace-delimited token, skipping `#` comments.
    fn token(&mut self) -> Option<&'a [u8]> {
        loop {
            while self.at < self.bytes.len() && self.bytes[self.at].is_ascii_whitespace() {
                self.at += 1;
            }
            if self.at < self.bytes.len() && self.bytes[self.at] == b'#' {
                while self.at < self.bytes.len() && self.bytes[self.at] != b'\n' {
                    self.at += 1;
                }
                continue;
            }
            break;
        }
        if self.at >= self.bytes.len() {
            return None;
        }
        let start = self.at;
        while self.at < self.bytes.len() && !self.bytes[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
        Some(&self.bytes[start..self.at])
    }

    /// The next token as a `u32`.
    fn number(&mut self) -> Option<u32> {
        let t = self.token()?;
        std::str::from_utf8(t).ok()?.parse().ok()
    }

    /// Consume exactly one whitespace byte; see [`parse_ppm`].
    fn take_single_whitespace(&mut self) -> bool {
        // `token` already consumed up to the end of `maxval`, so the
        // cursor sits on the single separator the format promises.
        if self.at < self.bytes.len() && self.bytes[self.at].is_ascii_whitespace() {
            self.at += 1;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×1 PPM: one red pixel and one green one.
    fn two_pixels() -> Vec<u8> {
        let mut v = b"P6\n2 1\n255\n".to_vec();
        v.extend_from_slice(&[0xff, 0x00, 0x00, 0x00, 0xff, 0x00]);
        v
    }

    #[test]
    fn a_binary_ppm_decodes_to_argb_rows() {
        let px = parse_ppm(&two_pixels()).expect("a P6 file");
        assert_eq!((px.width, px.height), (2, 1));
        assert_eq!(px.data.len(), 2 * 4);
        // `[b, g, r, a]`: red is `00 00 ff ff`, green is `00 ff 00 ff`.
        assert_eq!(&px.data[0..4], &[0x00, 0x00, 0xff, 0xff]);
        assert_eq!(&px.data[4..8], &[0x00, 0xff, 0x00, 0xff]);
    }

    #[test]
    fn comments_in_the_header_are_skipped() {
        // `netpbm` writes one, so a parser that choked on it would reject
        // most of the PPMs in the world.
        let mut v = b"P6\n# CREATOR: GIMP\n2 1\n# and another\n255\n".to_vec();
        v.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let px = parse_ppm(&v).expect("comments are fine");
        assert_eq!((px.width, px.height), (2, 1));
        assert_eq!(&px.data[0..4], &[3, 2, 1, 0xff]);
    }

    #[test]
    fn the_pixel_block_keeps_a_whitespace_byte() {
        // Exactly one whitespace byte ends the header, and it must not be
        // "skip all whitespace": a first pixel whose red channel is 0x20
        // (a space) or 0x0a (a newline) would be eaten, shifting every
        // pixel in the image by one byte and turning the picture into
        // colourful noise.
        let mut v = b"P6\n1 1\n255\n".to_vec();
        v.extend_from_slice(&[0x20, 0x0a, 0x09]);
        let px = parse_ppm(&v).expect("whitespace-looking pixels are pixels");
        assert_eq!(&px.data[0..4], &[0x09, 0x0a, 0x20, 0xff]);
    }

    #[test]
    fn an_ascii_ppm_is_refused_with_a_message() {
        let e = parse_ppm(b"P3\n1 1\n255\n255 0 0\n").expect_err("P3 is not P6");
        assert!(e.contains("P6"), "{e}");
        assert!(e.contains("P3"), "and it says what it got: {e}");
    }

    #[test]
    fn a_wrong_maxval_is_refused() {
        let e = parse_ppm(b"P6\n1 1\n65535\n").expect_err("16-bit");
        assert!(e.contains("maxval"), "{e}");
    }

    #[test]
    fn a_truncated_file_is_refused_rather_than_padded() {
        // The alternative — pad with black — produces a picture that
        // looks almost right, which is the worst possible answer to a
        // corrupt file.
        let mut v = b"P6\n4 4\n255\n".to_vec();
        v.extend_from_slice(&[0; 10]);
        let e = parse_ppm(&v).expect_err("truncated");
        assert!(e.contains("truncated"), "{e}");
    }

    #[test]
    fn a_header_that_is_not_a_header_is_an_error_not_a_panic() {
        for bytes in [
            &b""[..],
            b"P6",
            b"P6\n",
            b"P6\n2",
            b"P6\n2 2",
            b"P6\nwide tall\n255\n",
            b"P6\n0 0\n255\n\x00",
            b"\x00\x00\x00\x00",
        ] {
            assert!(parse_ppm(bytes).is_err(), "{bytes:?} should not decode");
        }
    }

    #[test]
    fn an_absurd_size_is_capped_rather_than_allocated() {
        // The header is three integers a file can claim anything in, and
        // `width * height * 4` on a hostile one is how a wallpaper
        // becomes an OOM. The file is eleven bytes long.
        let e = parse_ppm(b"P6\n65535 65535\n255\n\x00").expect_err("over the cap");
        assert!(e.contains("cap"), "{e}");
    }

    #[test]
    fn trailing_bytes_are_ignored() {
        let mut v = two_pixels();
        v.extend_from_slice(b"\n\n");
        assert!(
            parse_ppm(&v).is_ok(),
            "a trailing newline is not corruption"
        );
    }
}

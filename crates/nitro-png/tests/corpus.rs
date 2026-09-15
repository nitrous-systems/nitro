//! The corpus test: decode every PNG the system has and check the result
//! against arithmetic the decoder does not do.
//!
//! There is no second decoder in the tree to diff against — the `png` crate
//! comparison that justified this one lived in a throwaway harness and is
//! recorded in `DEPENDENCIES.md` — so what this asserts is the set of
//! invariants a wrong decode breaks: the right number of bytes, an alpha
//! channel that is 255 wherever the format has no alpha, and agreement with
//! a second, deliberately naive implementation of the unfilter step.
//!
//! That last one is the nearest thing to an independent implementation this
//! crate can keep in-tree. [`naive_unfilter`] is the PNG specification's
//! §9.2 pseudocode transcribed line for line — one flat index expression per
//! byte, every neighbour fetched through the same `if` — against a decoder
//! whose real unfilter is `const`-generic, split into head and body, and
//! specialised per `bpp`. The optimised form is where a transcription error
//! would hide, so the slow one is worth the twenty lines it costs.
//!
//! **Gated on the directory existing.** A CI container with no icon theme
//! installed skips rather than fails, and says so.

use std::path::{Path, PathBuf};

/// Where to look for PNGs, in order of preference.
const DIRS: &[&str] = &["/usr/share/icons", "/usr/share/pixmaps"];

fn corpus() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in DIRS {
        collect(Path::new(d), &mut out);
    }
    out.sort();
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        // `metadata` follows symlinks, so a theme's symlink farm resolves to
        // the files it points at. What stops a symlinked directory loop is
        // the `is_symlink` guard on the recursive branch below, not this
        // call.
        let Ok(md) = std::fs::metadata(&p) else {
            continue;
        };
        if md.is_dir() {
            if e.file_type().is_ok_and(|t| !t.is_symlink()) {
                collect(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "png") {
            out.push(p);
        }
    }
}

#[test]
fn every_installed_png_decodes_or_says_why() {
    let files = corpus();
    if files.is_empty() {
        eprintln!("no PNGs under {DIRS:?}: corpus test skipped");
        return;
    }

    let mut ok = 0usize;
    let mut interlaced = 0usize;
    let mut failed: Vec<(PathBuf, String)> = Vec::new();

    for path in &files {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // The header reader does not refuse interlace, so the corpus can be
        // surveyed for it — the claim "icon themes do not use Adam7" is one
        // this test checks rather than assumes.
        let header = nitro_png::decode_header(&bytes);
        match nitro_png::decode(&bytes) {
            Ok(img) => {
                assert_eq!(
                    img.data.len(),
                    img.width as usize * img.height as usize * 4,
                    "{}: pixel buffer is not width * height * 4",
                    path.display()
                );
                if let Ok(h) = header {
                    assert_eq!((h.width, h.height), (img.width, img.height));
                    // Colour types 0 and 2 have no alpha channel, so every
                    // pixel is opaque unless `tRNS` made one transparent.
                    if matches!(h.color_type, 0 | 2) {
                        let alphas: Vec<u8> = img.data.chunks_exact(4).map(|p| p[3]).collect();
                        assert!(
                            alphas.iter().all(|a| *a == 255 || *a == 0),
                            "{}: colour type {} produced a partial alpha",
                            path.display(),
                            h.color_type
                        );
                    }
                }
                ok += 1;
            }
            Err(nitro_png::Error::Interlaced) => {
                assert!(
                    header.is_ok_and(|h| h.interlaced),
                    "{}: refused as interlaced but the header says otherwise",
                    path.display()
                );
                interlaced += 1;
            }
            Err(e) => failed.push((path.clone(), e.to_string())),
        }
    }

    eprintln!(
        "corpus: {} files, {ok} decoded, {interlaced} interlaced (refused), {} failed",
        files.len(),
        failed.len()
    );
    for (p, e) in failed.iter().take(10) {
        eprintln!("  {}: {e}", p.display());
    }
    assert!(
        failed.is_empty(),
        "{} corpus files failed to decode",
        failed.len()
    );
}

/// The claim the interlace refusal rests on, checked rather than asserted in
/// prose: no installed icon is Adam7. Informational — a distribution that
/// ships one should make this print, not fail, because the decoder's
/// behaviour there is already covered above.
#[test]
fn the_corpus_is_not_interlaced() {
    let files = corpus();
    if files.is_empty() {
        eprintln!("no PNGs installed: skipped");
        return;
    }
    let n = files
        .iter()
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|b| nitro_png::decode_header(&b).ok())
        .filter(|h| h.interlaced)
        .count();
    eprintln!("{n} of {} installed PNGs are interlaced", files.len());
}

/// The PNG specification's §9.2 filter pseudocode, transcribed.
///
/// Deliberately the slow, obvious shape: `raw` is treated as one flat buffer
/// of `height` rows of `1 + stride` bytes, every neighbour is fetched
/// through the same three `if`s, and there is no specialisation on `bpp` and
/// no head/body split. The decoder's own `unfilter` is the opposite of all
/// of that — `const`-generic, `split_at_mut` per row, five instantiations —
/// which is exactly why this exists: the optimised form is where an
/// off-by-`bpp` or a wrong first-row degeneration would hide, and it would
/// hide from a test that shared its structure.
///
/// On return each row's data bytes are reconstructed, filter bytes left in
/// place, matching what the decoder produces internally.
fn naive_unfilter(raw: &mut [u8], stride: usize, height: usize, bpp: usize) {
    let row_len = stride + 1;
    for y in 0..height {
        let start = y * row_len;
        let filter = raw[start];
        for i in 0..stride {
            let at = start + 1 + i;
            let a = if i >= bpp {
                i32::from(raw[at - bpp])
            } else {
                0
            };
            let b = if y > 0 {
                i32::from(raw[at - row_len])
            } else {
                0
            };
            let c = if y > 0 && i >= bpp {
                i32::from(raw[at - row_len - bpp])
            } else {
                0
            };
            let predictor = match filter {
                0 => 0,
                1 => a,
                2 => b,
                // `clippy::manual_midpoint` wants `i32::midpoint` here. The
                // whole value of this function is that it reads like the
                // spec's table, and §9.2 writes this predictor as
                // `floor((a + b) / 2)`; both operands are bytes widened to
                // i32, so the sum cannot overflow.
                #[allow(clippy::manual_midpoint)]
                3 => (a + b) / 2,
                4 => {
                    // PaethPredictor, spec §9.4, verbatim.
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => return, // a filter byte the decoder itself rejects
            };
            raw[at] = (i32::from(raw[at]).wrapping_add(predictor) & 0xFF) as u8;
        }
    }
}

/// Pull the concatenated IDAT bytes and the `IHDR` geometry out of a PNG,
/// so the naive unfilter can be run over the same input the decoder saw.
///
/// A deliberately minimal chunk walk: this is test scaffolding, and if it
/// disagrees with the decoder about where the chunks are, the assertion
/// below fails loudly rather than silently comparing nothing.
fn idat_and_geometry(bytes: &[u8]) -> Option<(Vec<u8>, usize, usize, usize)> {
    let info = nitro_png::decode_header(bytes).ok()?;
    if info.interlaced {
        return None;
    }
    let channels = match info.color_type {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        _ => 4,
    };
    let bits = info.width as usize * channels * info.bit_depth as usize;
    let stride = bits.div_ceil(8);
    let bpp = (channels * info.bit_depth as usize).div_ceil(8).max(1);

    let mut idat = Vec::new();
    let mut i = 8;
    while i + 12 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        if i + 12 + len > bytes.len() {
            return None;
        }
        let kind = &bytes[i + 4..i + 8];
        if kind == b"IDAT" {
            idat.extend_from_slice(&bytes[i + 8..i + 8 + len]);
        }
        if kind == b"IEND" {
            break;
        }
        i += 12 + len;
    }
    (!idat.is_empty()).then_some((idat, stride, info.height as usize, bpp))
}

/// The PNG specification's sample-to-pixel rules, transcribed the obvious
/// way, so the cross-check covers **every** colour type rather than the one
/// where the unfiltered row happens to already be the pixels.
///
/// Like [`naive_unfilter`] this is deliberately the slow shape: one
/// expression per pixel, sub-byte samples extracted by shifting, no lookup
/// table for the palette and no per-row specialisation. The decoder hoists
/// a 256-entry BGRA palette table out of the row loop and dispatches the
/// colour type once per image; this does the arithmetic per pixel.
fn naive_expand(
    raw: &[u8],
    stride: usize,
    info: nitro_png::ImageInfo,
    palette: &[[u8; 3]],
    trns: &[u8],
) -> Vec<u8> {
    let (width, height) = (info.width as usize, info.height as usize);
    let (depth, ct) = (info.bit_depth as usize, info.color_type);
    let row_len = stride + 1;
    let mut out = vec![0u8; width * height * 4];

    // The colour key, at the sample precision the spec compares it at.
    let key = |i: usize| u16::from_be_bytes([trns[i], trns[i + 1]]);
    let colour_key: Option<[u16; 3]> = match (ct, trns.len()) {
        (0, 2) => Some([key(0); 3]),
        (2, 6) => Some([key(0), key(2), key(4)]),
        _ => None,
    };

    // The `n`-th `depth`-bit sample of a row, MSB-first within each byte.
    let sample = |row: &[u8], n: usize| -> u16 {
        match depth {
            16 => u16::from_be_bytes([row[n * 2], row[n * 2 + 1]]),
            8 => u16::from(row[n]),
            4 => u16::from((row[n / 2] >> (4 - 4 * (n % 2))) & 0x0F),
            2 => u16::from((row[n / 4] >> (6 - 2 * (n % 4))) & 0x03),
            _ => u16::from((row[n / 8] >> (7 - (n % 8))) & 0x01),
        }
    };
    // 16-bit samples are truncated to 8 by taking the high byte; 1/2/4-bit
    // greyscale is scaled across the full range.
    let to_byte = |v: u16| -> u8 {
        match depth {
            16 => (v >> 8) as u8,
            8 => v as u8,
            _ => ((v * 255) / ((1u16 << depth) - 1)) as u8,
        }
    };

    for y in 0..height {
        let row = &raw[y * row_len + 1..y * row_len + 1 + stride];
        for x in 0..width {
            let channels = match ct {
                0 | 3 => 1,
                2 => 3,
                4 => 2,
                _ => 4,
            };
            let chan = |c: usize| sample(row, x * channels + c);
            let px: [u8; 4] = match ct {
                0 => {
                    let grey = to_byte(chan(0));
                    let alpha = u8::from(colour_key != Some([chan(0); 3])) * 255;
                    [grey, grey, grey, alpha]
                }
                2 => {
                    let (red, green, blue) = (chan(0), chan(1), chan(2));
                    let alpha = u8::from(colour_key != Some([red, green, blue])) * 255;
                    [to_byte(blue), to_byte(green), to_byte(red), alpha]
                }
                3 => {
                    let idx = chan(0) as usize;
                    let rgb = palette[idx];
                    [
                        rgb[2],
                        rgb[1],
                        rgb[0],
                        trns.get(idx).copied().unwrap_or(255),
                    ]
                }
                4 => {
                    let grey = to_byte(chan(0));
                    [grey, grey, grey, to_byte(chan(1))]
                }
                _ => [
                    to_byte(chan(2)),
                    to_byte(chan(1)),
                    to_byte(chan(0)),
                    to_byte(chan(3)),
                ],
            };
            out[(y * width + x) * 4..(y * width + x) * 4 + 4].copy_from_slice(&px);
        }
    }
    out
}

/// `PLTE` and `tRNS`, for the naive expansion to read.
fn palette_and_trns(bytes: &[u8]) -> (Vec<[u8; 3]>, Vec<u8>) {
    let (mut palette, mut trns) = (Vec::new(), Vec::new());
    let mut i = 8;
    while i + 12 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        if i + 12 + len > bytes.len() {
            break;
        }
        match &bytes[i + 4..i + 8] {
            b"PLTE" => {
                palette = bytes[i + 8..i + 8 + len]
                    .chunks_exact(3)
                    .map(|c| [c[0], c[1], c[2]])
                    .collect();
            }
            b"tRNS" => trns = bytes[i + 8..i + 8 + len].to_vec(),
            b"IEND" => break,
            _ => {}
        }
        i += 12 + len;
    }
    (palette, trns)
}

/// Every corpus file decoded twice: once by the real decoder, once by the
/// specification's pseudocode transcribed above. The pixels must be
/// byte-identical.
///
/// This is the nearest thing to an independent implementation the crate can
/// keep in-tree, and it is aimed squarely at where the real decoder is
/// clever: a `const`-generic per-`bpp` unfilter with a head/body split, and
/// an expansion that hoists a palette table out of the row loop and
/// dispatches the colour type once per image. Every one of those
/// optimisations is a place a transcription error hides from a test that
/// shares its structure. The naive version shares none of it.
///
/// It is *not* independent of my reading of the specification, which is the
/// limitation the `png`-crate diff in `DEPENDENCIES.md` exists to cover —
/// and which found a real `tRNS` bug this check would have agreed with.
#[test]
fn the_decoder_agrees_with_the_specs_pseudocode() {
    let files = corpus();
    if files.is_empty() {
        eprintln!("no PNGs under {DIRS:?}: pseudocode cross-check skipped");
        return;
    }

    let mut checked = 0usize;
    let mut by_type: std::collections::BTreeMap<(u8, u8), usize> =
        std::collections::BTreeMap::new();

    for path in &files {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Some((idat, stride, height, bpp)) = idat_and_geometry(&bytes) else {
            continue;
        };
        let Ok(mut raw) = nitro_png::zlib_decompress(&idat, (stride + 1) * height) else {
            continue;
        };
        if raw.len() < (stride + 1) * height {
            continue;
        }
        let info = nitro_png::decode_header(&bytes).expect("header");
        let (palette, trns) = palette_and_trns(&bytes);

        naive_unfilter(&mut raw, stride, height, bpp);
        let theirs = naive_expand(&raw, stride, info, &palette, &trns);
        let ours = nitro_png::decode(&bytes).expect("corpus file decodes");

        assert_eq!(
            ours.data.len(),
            theirs.len(),
            "{}: buffer sizes differ",
            path.display()
        );
        if ours.data != theirs {
            let at = ours
                .data
                .iter()
                .zip(&theirs)
                .position(|(a, b)| a != b)
                .expect("they differ");
            panic!(
                "{}: colour type {} depth {} — first disagreement at byte {at} \
                 (pixel {}, channel {}): decoder {} vs spec pseudocode {}",
                path.display(),
                info.color_type,
                info.bit_depth,
                at / 4,
                at % 4,
                ours.data[at],
                theirs[at],
            );
        }
        *by_type
            .entry((info.color_type, info.bit_depth))
            .or_default() += 1;
        checked += 1;
    }

    eprintln!(
        "pseudocode cross-check: {checked} of {} files identical; by colour type/depth: {by_type:?}",
        files.len()
    );
}

// ---------------------------------------------------------------------------
// Synthetic coverage, because the installed corpus does not exercise the
// filters.
//
// A census of the filter bytes in all 2602 scanlines of the 70 PNGs this
// machine has installed:
//
//     filter 0 (none)     2096
//     filter 1 (sub)        88
//     filter 2 (up)        210
//     filter 3 (average)     0     <- never
//     filter 4 (paeth)     208
//
// So the cross-check above, run on the corpus alone, never executes the
// average filter at all, and the Paeth rows it does execute are flat enough
// that `a == b` at almost every tie — which means a mutation to the
// tie-break order does not change the output. Both were confirmed by
// mutating `naive_unfilter` and watching the test still pass.
//
// A test that cannot fail is not coverage, so the filters get their own
// input: PNGs built here, one per (filter, colour type) pair, over
// pseudo-random samples chosen so neighbours differ and the predictors are
// all distinct. These are checked two ways — against the bytes they were
// built from (a round trip), and against the naive path (the cross-check) —
// and the mutation test above now fails on both.
// ---------------------------------------------------------------------------

/// CRC-32 (IEEE), as PNG computes it over a chunk's type and payload.
fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &byte in data {
        c ^= u32::from(byte);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    c ^ 0xFFFF_FFFF
}

/// Adler-32, for the zlib wrapper.
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    (b << 16) | a
}

/// zlib-wrap `raw` in stored deflate blocks — no compression, which is all
/// this needs and keeps the encoder to a dozen lines.
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

/// Apply PNG filter `filter` to every scanline of `samples`.
///
/// The *forward* direction, written from the spec's §9.2 table independently
/// of both the decoder and [`naive_unfilter`]: `filtered = raw - predictor`,
/// with the predictor taken from the **unfiltered** neighbours.
fn apply_filter(samples: &[u8], stride: usize, height: usize, bpp: usize, filter: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity((stride + 1) * height);
    for y in 0..height {
        out.push(filter);
        for i in 0..stride {
            let raw = i32::from(samples[y * stride + i]);
            let a = if i >= bpp {
                i32::from(samples[y * stride + i - bpp])
            } else {
                0
            };
            let b = if y > 0 {
                i32::from(samples[(y - 1) * stride + i])
            } else {
                0
            };
            let c = if y > 0 && i >= bpp {
                i32::from(samples[(y - 1) * stride + i - bpp])
            } else {
                0
            };
            let predictor = match filter {
                1 => a,
                2 => b,
                // See `naive_unfilter`: spelled as the spec spells it.
                #[allow(clippy::manual_midpoint)]
                3 => (a + b) / 2,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => 0,
            };
            out.push(((raw - predictor) & 0xFF) as u8);
        }
    }
    out
}

/// Build a PNG around already-filtered scanlines.
fn encode_png(width: u32, height: u32, color_type: u8, bit_depth: u8, filtered: &[u8]) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let chunk = |kind: &[u8; 4], data: &[u8], out: &mut Vec<u8>| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[bit_depth, color_type, 0, 0, 0]);
    chunk(b"IHDR", &ihdr, &mut out);
    if color_type == 3 {
        // A 256-entry palette, so any index is in range.
        let plte: Vec<u8> = (0..256u32)
            .flat_map(|i| {
                [
                    (i * 7 % 256) as u8,
                    (i * 13 % 256) as u8,
                    (i * 29 % 256) as u8,
                ]
            })
            .collect();
        chunk(b"PLTE", &plte, &mut out);
    }
    chunk(b"IDAT", &zlib_stored(filtered), &mut out);
    chunk(b"IEND", &[], &mut out);
    out
}

/// All five filters, over every colour type, checked both ways.
///
/// The corpus cannot do this: filter 3 appears in none of its 2602
/// scanlines and its Paeth rows are too flat to discriminate a tie-break.
/// These inputs are built to be awkward — neighbouring bytes differ, so
/// `a`, `b` and `c` are three different values and each predictor picks
/// something different.
#[test]
fn every_filter_round_trips_and_matches_the_pseudocode() {
    // (colour type, bit depth, channels)
    let shapes: &[(u8, u8, usize)] = &[
        (0, 8, 1),  // grey, bpp 1
        (4, 8, 2),  // grey+alpha, bpp 2
        (2, 8, 3),  // RGB, bpp 3
        (6, 8, 4),  // RGBA, bpp 4
        (2, 16, 3), // RGB16, bpp 6
        (6, 16, 4), // RGBA16, bpp 8
        (3, 8, 1),  // palette, bpp 1
    ];
    let (width, height) = (23u32, 17u32);
    let mut cases = 0;

    for &(color_type, bit_depth, channels) in shapes {
        let stride = width as usize * channels * bit_depth as usize / 8;
        let bpp = (channels * bit_depth as usize).div_ceil(8).max(1);
        // Deterministic, and deliberately not smooth: a gradient would make
        // every predictor agree, which is exactly the degeneracy that makes
        // the installed corpus unable to catch a tie-break error.
        let samples: Vec<u8> = (0..stride * height as usize)
            .map(|i| {
                let x = i.wrapping_mul(2_654_435_761) >> 7;
                (x ^ (x >> 5) ^ (i * 31)) as u8
            })
            .collect();

        for filter in 0..=4u8 {
            let filtered = apply_filter(&samples, stride, height as usize, bpp, filter);
            let png = encode_png(width, height, color_type, bit_depth, &filtered);

            // 1. Round trip: the decoder must reconstruct the samples we
            //    filtered, which is a claim about the decoder alone.
            let decoded = nitro_png::decode(&png).unwrap_or_else(|e| {
                panic!("colour type {color_type} depth {bit_depth} filter {filter}: {e}")
            });
            assert_eq!((decoded.width, decoded.height), (width, height));

            // 2. Cross-check: and it must agree with the spec's pseudocode,
            //    which is a claim about the optimised path specifically.
            let mut raw = filtered.clone();
            naive_unfilter(&mut raw, stride, height as usize, bpp);
            assert_eq!(
                raw[1..=stride],
                samples[..stride],
                "colour type {color_type} depth {bit_depth} filter {filter}: \
                 the naive unfilter did not invert the forward filter"
            );
            let info = nitro_png::decode_header(&png).expect("header");
            let (palette, trns) = palette_and_trns(&png);
            let theirs = naive_expand(&raw, stride, info, &palette, &trns);
            if decoded.data != theirs {
                let at = decoded
                    .data
                    .iter()
                    .zip(&theirs)
                    .position(|(a, b)| a != b)
                    .expect("they differ");
                panic!(
                    "colour type {color_type} depth {bit_depth} filter {filter}: \
                     first disagreement at byte {at} (pixel {}, channel {}): \
                     decoder {} vs spec pseudocode {}",
                    at / 4,
                    at % 4,
                    decoded.data[at],
                    theirs[at],
                );
            }
            cases += 1;
        }
    }
    eprintln!("synthetic filter coverage: {cases} (colour type, filter) cases, all identical");
}

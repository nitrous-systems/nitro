//! A hostile file must return `Err`, never panic.
//!
//! Every file in the corpus (and the built-in fixture, so this still tests
//! something on a box with no icons) is put through two mutations: truncate
//! it at twenty pseudo-random offsets, and flip a byte at twenty more. The
//! only assertion is that `decode` returns — and that whatever it returns is
//! self-consistent, because a decoder that reports a size it did not
//! produce would hand a caller an out-of-bounds slice.
//!
//! The generator is a fixed-seed xorshift rather than a random one: a fuzz
//! failure that cannot be reproduced from the test name is not a test, it is
//! a rumour.

use std::path::PathBuf;

/// xorshift64*, seeded per file from its path so different files explore
/// different offsets while any one file is reproducible.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

fn hash(s: &str) -> u64 {
    // FNV-1a, so the seed is a pure function of the path.
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn corpus() -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>, budget: &mut usize) {
        if *budget == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            if *budget == 0 {
                return;
            }
            let p = e.path();
            let Ok(md) = std::fs::metadata(&p) else {
                continue;
            };
            if md.is_dir() {
                if e.file_type().is_ok_and(|t| !t.is_symlink()) {
                    walk(&p, out, budget);
                }
            } else if p.extension().is_some_and(|x| x == "png") {
                out.push(p);
                *budget -= 1;
            }
        }
    }
    let mut out = Vec::new();
    // Capped: forty mutations of every PNG on a developer's machine is a
    // minute of test time for no more coverage than a few hundred files give.
    let mut budget = 400usize;
    walk(
        std::path::Path::new("/usr/share/icons"),
        &mut out,
        &mut budget,
    );
    walk(
        std::path::Path::new("/usr/share/pixmaps"),
        &mut out,
        &mut budget,
    );
    out.sort();
    out
}

/// Whatever comes back must be internally consistent: a reported size that
/// does not match the buffer is the bug class this catches.
fn check(result: Result<nitro_png::Image, nitro_png::Error>) {
    if let Ok(img) = result {
        assert_eq!(
            img.data.len(),
            img.width as usize * img.height as usize * 4,
            "decoded {}x{} but produced {} bytes",
            img.width,
            img.height,
            img.data.len()
        );
        assert!(img.width > 0 && img.height > 0);
    }
}

#[test]
fn truncation_and_corruption_never_panic() {
    let mut files: Vec<(String, Vec<u8>)> = vec![(
        "builtin fixture".into(),
        nitro_png::tests_support::ONE_PIXEL_RGBA.to_vec(),
    )];
    for p in corpus() {
        if let Ok(b) = std::fs::read(&p) {
            files.push((p.display().to_string(), b));
        }
    }

    for (name, bytes) in &files {
        let mut rng = Rng::new(hash(name));

        // Twenty truncations, plus the two boundary cases by hand: the
        // empty file and one byte short, which are where an off-by-one in
        // the chunk walker lives.
        for cut in (0..20)
            .map(|_| rng.below(bytes.len() + 1))
            .chain([0, bytes.len().saturating_sub(1)])
        {
            check(nitro_png::decode(&bytes[..cut]));
        }

        // Twenty single-byte corruptions. Most will be caught by the CRC,
        // which is the point: the ones that are not have to be survived by
        // the decoder proper.
        for _ in 0..20 {
            let mut b = bytes.clone();
            if b.is_empty() {
                continue;
            }
            let at = rng.below(b.len());
            b[at] ^= 1 << (rng.below(8));
            check(nitro_png::decode(&b));
        }

        // And twenty corruptions with the CRCs deliberately not in the way:
        // a decoder whose only defence is the checksum is not bounded. The
        // IHDR fields are the interesting target — a claimed 4-billion-pixel
        // image, a zero dimension, an impossible bit depth.
        for _ in 0..20 {
            let mut b = bytes.clone();
            if b.len() < 34 {
                continue;
            }
            let at = 16 + rng.below(13); // inside IHDR's payload
            b[at] ^= 1 << (rng.below(8));
            fix_crc(&mut b);
            check(nitro_png::decode(&b));
        }
    }

    eprintln!("fuzzed {} files x 62 mutations", files.len());
}

/// Recompute every chunk CRC so a mutation is seen by the decoder rather
/// than rejected at the door.
fn fix_crc(b: &mut [u8]) {
    let mut i = 8;
    while i + 12 <= b.len() {
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        if i + 12 + len > b.len() {
            return;
        }
        let crc = crc32(&b[i + 4..i + 8 + len]);
        b[i + 8 + len..i + 12 + len].copy_from_slice(&crc.to_be_bytes());
        i += 12 + len;
    }
}

/// CRC-32 (IEEE), the same polynomial PNG uses. A copy, because the
/// decoder's own is private — and a test that shared the implementation it
/// checks would be checking nothing.
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

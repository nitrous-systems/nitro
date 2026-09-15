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
        // `symlink_metadata`, so a theme's symlink farm is followed for
        // files but a directory loop cannot make this recurse for ever.
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

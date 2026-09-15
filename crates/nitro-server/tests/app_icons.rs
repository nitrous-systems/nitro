//! The application-icon corpus test: point the real lookup at the real
//! icon theme on this machine and decode whatever it finds.
//!
//! Everything else about application icons is tested against fixture
//! trees this repository writes, which is right — a test whose result
//! depends on what a box happens to have installed is not a test. But
//! fixtures only ever contain what their author thought to put in them,
//! and the whole feature is about reading files somebody else wrote: an
//! `index.theme` with a group this parser does not expect, a directory
//! named `scalable` holding PNGs after all, a symlink farm, an icon
//! whose PNG is a palette image with `tRNS`. This file is the tripwire
//! for those.
//!
//! It asserts **invariants**, not contents, because the contents are
//! whatever the distribution shipped: every path the lookup returns must
//! exist, be a `.png`, sit under a search directory, and decode to a
//! square-able tile of exactly the requested size. Nothing here can fail
//! because the box has a different theme; it can only fail because the
//! lookup returned something it should not have, or the decoder choked
//! on a real file.
//!
//! **Gated on there being a theme at all.** A CI container with no
//! `/usr/share/icons` skips and says so, the way
//! `crates/nitro-png/tests/corpus.rs` does — that is a requirement of
//! this crate's tests, not politeness: the test box itself has only 17
//! PNGs and a container has none.

use nitro_server::icon_theme::IconTheme;
use nitro_server::icons::{AppIcon, IconEngine};

/// Icon names to look for, chosen to span the ways a theme is laid out
/// rather than to be present.
///
/// Absence is not a failure — most boxes have none of these — so the
/// list is generous on purpose: the more names, the more likely a real
/// file is exercised on any given machine, and `found == 0` skips.
const NAMES: &[&str] = &[
    // Applications a developer box tends to have.
    "firefox",
    "foot",
    "htop",
    "gvim",
    "vim",
    "apport",
    "python3",
    "nautilus",
    "gnome-terminal",
    "code",
    "thunderbird",
    "libreoffice-writer",
    // The freedesktop icon-naming spec's own names, which `hicolor` and
    // every desktop theme are supposed to carry.
    "application-x-executable",
    "text-x-generic",
    "folder",
    "user-home",
    "dialog-information",
    "dialog-error",
    "system-run",
    "preferences-system",
];

/// Sizes a desktop actually lays out at, plus one nobody ships so the
/// closest-match path is exercised on a real theme.
const SIZES: &[u32] = &[16, 22, 24, 32, 48, 37];

#[test]
fn every_icon_the_real_theme_answers_with_is_a_png_that_decodes() {
    let theme = IconTheme::load("hicolor");
    if theme.is_empty() {
        eprintln!(
            "skipping: no icon directories on this machine (looked in {:?})",
            theme.dirs()
        );
        return;
    }

    let mut found = 0usize;
    let mut decoded = 0usize;
    let mut bytes = 0usize;
    for name in NAMES {
        for &size in SIZES {
            for scale in [1u32, 2] {
                let Some(path) = theme.lookup(name, size, scale) else {
                    continue;
                };
                found += 1;

                // The four things the lookup promises, whatever the box
                // has installed.
                assert!(
                    path.is_file(),
                    "{name} at {size}x{scale} resolved to {} which is not a file",
                    path.display()
                );
                assert!(
                    path.extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("png")),
                    "{name} at {size}x{scale} resolved to {}, which is not a .png \
                     — only PNG is a candidate (docs/icons.md)",
                    path.display()
                );
                assert!(
                    theme.dirs().iter().any(|d| path.starts_with(d)),
                    "{name} at {size}x{scale} resolved to {}, which is outside \
                     every search directory {:?}",
                    path.display(),
                    theme.dirs()
                );

                // And it decodes. This is the half fixtures cannot
                // reach: a real theme's PNGs are palette images with
                // `tRNS`, 16-bit greyscale, APNGs, and files written by
                // every encoder of the last twenty years.
                let data = std::fs::read(&path).expect("a file the lookup said exists");
                let img = nitro_png::decode(&data).unwrap_or_else(|e| {
                    panic!("{} failed to decode: {e}", path.display());
                });
                assert!(img.width > 0 && img.height > 0);
                assert_eq!(
                    img.data.len(),
                    img.width as usize * img.height as usize * 4,
                    "{} decoded to the wrong buffer size",
                    path.display()
                );
                decoded += 1;
                bytes += img.data.len();
            }
        }
    }

    if found == 0 {
        eprintln!(
            "skipping: {} icon director{} but none of the {} probe names \
             resolved to a PNG (theme chain {:?})",
            theme.dirs().len(),
            if theme.dirs().len() == 1 { "y" } else { "ies" },
            NAMES.len(),
            theme.chain()
        );
        return;
    }
    eprintln!(
        "corpus: {found} lookups answered, {decoded} decoded, {bytes} bytes of \
         pixels, over theme chain {:?}",
        theme.chain()
    );
}

#[test]
fn the_engine_loads_a_real_application_icon_at_the_size_it_was_asked_for() {
    // The same corpus, through the engine rather than the lookup: the
    // decode, the resample and the cache accounting on files this
    // repository did not write.
    //
    // The invariant that matters is the one the paint path depends on:
    // whatever the source file's dimensions, the cached tile is exactly
    // `px * px * 4` bytes. A theme with a 22 px icon answering a 24 px
    // request is the normal case, not an error, and the tile must still
    // be 24 square or the blit's one-to-one fast path silently becomes a
    // scaled one.
    let mut engine = IconEngine::with_theme("hicolor");
    // No `.desktop` indirection here: this test is about the *theme*
    // half, and a box whose `/usr/share/applications` happens to carry an
    // entry for one of the probe names would silently change which file
    // is being decoded. The hop has its own tests, against fixtures.
    engine.set_desktop_dirs(Vec::new());
    if !engine.has_app_icons() {
        eprintln!("skipping: no icon theme installed");
        return;
    }

    let mut loaded = 0usize;
    for name in NAMES {
        let Some(icon) = engine.lookup_app(name) else {
            continue;
        };
        assert!(
            matches!(icon, AppIcon::Theme(_)),
            "{name} answered from the symbolic set with no .desktop index"
        );
        let handle = icon.handle();
        for px in [16u32, 24, 32] {
            if engine.app_tile_len(handle, px).is_none() {
                // A name that resolved but whose file will not decode is
                // a miss, not a panic — that is the non-fatal contract,
                // and `app_icon_misses` is where it shows up.
                continue;
            }
            assert_eq!(
                engine.app_tile_len(handle, px),
                Some(px as usize * px as usize * 4),
                "{name} at {px} px is not a {px}-square tile"
            );
            loaded += 1;
        }
    }

    if loaded == 0 {
        eprintln!("skipping: no probe name resolved to a decodable icon");
        return;
    }

    // The accounting agrees with itself: the counters the `stats` reply
    // publishes are the ones a reader will use to decide whether the
    // cache is behaving, so they have to add up on real data.
    let mut pairs = Vec::new();
    engine.write_pairs(&mut pairs);
    let get = |k: &str| pairs.iter().find(|(n, _)| *n == k).map_or(0, |(_, v)| *v);
    assert_eq!(get("app_icons_cached"), loaded as u64);
    assert_eq!(get("app_icon_loads"), loaded as u64);
    assert_eq!(get("app_icon_evictions"), 0, "far inside the 4 MiB cap");
    assert!(get("app_icon_bytes") > 0 && get("app_icon_bytes") < IconEngine::APP_MAX_BYTES as u64);
    assert!(
        get("app_icon_decode_us_max") > 0,
        "a decode happened, so it was timed"
    );
    eprintln!(
        "engine: {loaded} tiles, {} bytes, slowest decode {} µs",
        get("app_icon_bytes"),
        get("app_icon_decode_us_max")
    );
}

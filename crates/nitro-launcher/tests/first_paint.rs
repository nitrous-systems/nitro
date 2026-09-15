//! Time to first paint of the launcher's list, with 40 entries, icons
//! against no icons.
//!
//! The question #3714 has to answer: a row gained an icon, so does
//! opening the launcher get slower? The structural argument says no —
//! `SetIcon` is one-way, so a row costs no round trip, and the decode is
//! the server's and lazy — but "no round trip" is a claim about a
//! protocol and "it opens as fast" is a claim about a clock, and the
//! second does not follow from the first for free. A row's icon is still
//! a node, a mutation and a blit.
//!
//! So it is measured. Two arms, one variable: every entry names an icon,
//! or none does. Same 40 `.desktop` files otherwise, same tree, same
//! server.
//!
//! **What is timed** is the show: the bare-Super tap that makes the
//! overlay visible, through `settle`, which runs the app loop until the
//! tree is laid out, painted and flushed and the server has gone quiet.
//! That is the interval a user perceives as "the launcher opened", and
//! it includes the per-row `SetText` measurement round trips that
//! dominate it.
//!
//! **What it is not**: a benchmark of the server's compositing. The fake
//! backend paints into a heap buffer, so the absolute numbers are a dev
//! box's and are not the box's. The *comparison* is what this file is
//! for, which is why both arms run in the same process, alternating, and
//! the reported figure is a median of several shows rather than one.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nitro_launcher::{Launcher, build};
use nitro_ui::Size;
use nitro_ui::shell::Surface;
use nitro_ui::test::Harness;
use nitro_ui::widgets::Button;

/// How many entries. The spec's number, and a realistic full
/// `/usr/share/applications`.
const ENTRIES: usize = 40;

/// Shows per arm. The first is discarded (it pays for the tree's own
/// creation), and the rest are taken as a median.
const SHOWS: usize = 7;

/// `KEY_LEFTMETA`, the bare-Super tap the launcher opens on.
const KEY_SUPER: u32 = 125;

/// Write `ENTRIES` desktop files, every one naming an icon or none.
fn fixture(name: &str, icons: bool) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nitro-launcher-firstpaint-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    for i in 0..ENTRIES {
        // Names chosen so every entry matches the empty query and the
        // list is full: the launcher caps at `MAX_RESULTS` (20), so both
        // arms show the same 20 rows out of the same 40 entries.
        let icon = if icons {
            // A name no icon theme has, on purpose: this is the
            // *expensive* case, not the cheap one — the server walks its
            // whole search path, answers `BadIcon`, and the widget sends
            // its fallback. If icons cost time anywhere, it is here.
            format!("Icon=application-{i:02}\n")
        } else {
            String::new()
        };
        std::fs::write(
            dir.join(format!("app{i:02}.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName=Application {i:02}\n\
                 Exec=/bin/true\n{icon}"
            ),
        )
        .expect("fixture file");
    }
    dir
}

/// The median of `v`, which is the honest summary of a handful of timings
/// on a machine that is also doing other things: one scheduling hiccup
/// moves a mean and does not move this.
fn median(mut v: Vec<Duration>) -> Duration {
    v.sort_unstable();
    v[v.len() / 2]
}

/// Time `SHOWS` opens of a launcher over `dir`, discarding the first.
///
/// Returns the timings and how many of the visible rows asked the icon
/// *theme* for their artwork and had to fall back — the premise check,
/// because both arms send one `SetIcon` per row (a row with no `Icon=`
/// still shows the generic symbolic one) and a mutation count therefore
/// cannot tell them apart. What separates them is where the name is
/// looked up: the icon arm makes the server walk its whole icon search
/// path per row, answer `BadIcon`, and take the widget's fallback; the
/// control arm never leaves the compiled-in set.
fn time_shows(dir: &Path, label: &str) -> (Vec<Duration>, usize) {
    let mut h = Harness::shell(
        "nitro-launcher",
        Launcher::new()
            .with_dirs(vec![dir.to_path_buf()])
            .with_builtins(Vec::new()),
        Surface::overlay(),
        Some(Size::new(300.0, 220.0)),
        build,
    );
    h.settle();
    assert_eq!(
        h.state().entries().len(),
        ENTRIES,
        "{label}: the fixture did not load"
    );

    let mut times = Vec::new();
    for show in 0..SHOWS {
        let t0 = Instant::now();
        h.key(KEY_SUPER);
        h.settle();
        // Not a timeout loop: `settle` already runs until the tree is
        // painted and the server is quiet, so the clock stops here.
        let elapsed = t0.elapsed();
        assert!(h.state().is_visible(), "{label}: the launcher did not show");
        if show > 0 {
            // The first is discarded: it is the one that pays for
            // whatever the build left unpainted, and both arms pay it.
            times.push(elapsed);
        }
        h.key(KEY_SUPER);
        h.settle();
        assert!(!h.state().is_visible(), "{label}: it did not hide again");
    }

    // Counted at the end, off the live widgets, because this is a
    // property of the rows rather than of any one frame.
    h.key(KEY_SUPER);
    h.settle();
    let mut fell_back = 0usize;
    for n in 0..20 {
        let Some(id) = nitro_ui::introspect::resolve(h.ui(), &format!("window/results/{n}")) else {
            break;
        };
        if h.widget::<Button<Launcher>>(id).icon_fell_back() {
            fell_back += 1;
        }
    }
    h.quit();
    (times, fell_back)
}

#[test]
fn forty_entries_open_as_fast_with_icons_as_without() {
    let with = fixture("icons", true);
    let without = fixture("plain", false);

    // Alternating, not one arm then the other: a machine that gets busy
    // halfway through would otherwise put all of its noise in one arm and
    // read as an effect.
    let (a1, icon_rows) = time_shows(&with, "icons");
    let (b1, plain_rows) = time_shows(&without, "plain");
    let (a2, _) = time_shows(&with, "icons");
    let (b2, _) = time_shows(&without, "plain");

    let icons = median([a1, a2].concat());
    let plain = median([b1, b2].concat());

    // The premise: the icon arm's rows really did ask the icon theme and
    // really did fall back, and the control arm's never left the
    // compiled-in set. Without this the test could pass by measuring the
    // same thing twice.
    assert!(
        icon_rows > 0,
        "no row asked the icon theme, so this is not an icon arm"
    );
    assert_eq!(
        plain_rows, 0,
        "{plain_rows} control rows fell back, so the control is not a control"
    );

    let ratio = icons.as_secs_f64() / plain.as_secs_f64();
    eprintln!(
        "first paint, {ENTRIES} entries / 20 rows: icons {icons:?}, none \
         {plain:?} (x{ratio:.2}); {icon_rows} rows went to the theme"
    );

    // The assertion is deliberately loose, and the looseness is the
    // point rather than a hedge. This runs on a shared machine with no
    // quiescence guarantee, so a tight bound would be a flaky test that
    // future readers learn to re-run — which is worse than no test. What
    // is worth catching is a *structural* regression: a synchronous round
    // trip per row would put 20 of them on this path and show up as a
    // multiple, not as a few per cent. Anything under 2x is noise on this
    // measurement and the numbers printed above are the real result.
    assert!(
        ratio < 2.0,
        "40 entries with icons opened {ratio:.2}x slower than without \
         ({icons:?} vs {plain:?}) — a round trip per row is the thing to \
         look for"
    );

    let _ = std::fs::remove_dir_all(&with);
    let _ = std::fs::remove_dir_all(&without);
}

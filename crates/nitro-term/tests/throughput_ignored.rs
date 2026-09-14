//! A throughput measurement, run by hand rather than by CI.
//!
//! `cargo test -p nitro-term --test throughput_ignored -- --ignored --nocapture`
//!
//! It is `#[ignore]`d because it is a *measurement*, not an assertion:
//! the number depends on the machine, and a throughput figure that
//! failed the build on a loaded CI box would be noise with a red cross
//! next to it. The cost *claims* are asserted in `term.rs`, which is the
//! file that must stay green.

use std::time::Instant;

use nitro_term::TermApp;
use nitro_term::pty::Pty;
use nitro_term::widget::TermGrid;
use nitro_ui::Size;
use nitro_ui::test::Harness;

#[test]
#[ignore = "a measurement, not an assertion; see the module docs"]
fn seq_one_million() {
    report(
        "seq 1 1000000",
        &["/bin/sh", "-c", "seq 1 1000000"],
        1_000_000,
    );
}

#[test]
#[ignore = "a measurement, not an assertion; see the module docs"]
fn cat_five_megabytes() {
    // Generated rather than read from the filesystem, so the number does
    // not depend on what this machine happens to have in /usr/share.
    report(
        "cat 5 MB",
        &[
            "/bin/sh",
            "-c",
            "yes 'the quick brown fox jumps over the lazy dog 0123456789' | head -c 5000000",
        ],
        0,
    );
}

/// Run `argv` in a terminal, pacing the scene at one frame per drain,
/// and print lines/s, MB/s and the commit count.
fn report(what: &str, argv: &[&str], want_lines: usize) {
    let pty = Pty::spawn_command(argv, 80, 24).expect("pty");
    let mut h = Harness::sized(
        "nitro-term",
        TermApp::new(pty),
        Size::new(640.0, 400.0),
        nitro_term::build,
    );
    let grid = nitro_term::grid_of(h.ui()).expect("grid");
    {
        let (ui, state) = h.parts();
        nitro_term::install(ui, state, grid).expect("install");
    }
    h.settle();

    let commits_before = h.commits();
    let start = Instant::now();
    let mut frames = 0u64;
    loop {
        {
            let (ui, state) = h.parts();
            nitro_term::drain_pty(state, ui);
        }
        h.frame();
        frames += 1;
        let done = h.state_mut().child_exited()
            || (want_lines > 0
                && h.widget::<TermGrid>(grid)
                    .term()
                    .grid()
                    .text()
                    .contains(&want_lines.to_string()));
        if done {
            break;
        }
        assert!(
            start.elapsed().as_secs() <= 120,
            "{what}: gave up after 120 s"
        );
    }
    let secs = start.elapsed().as_secs_f64();
    let bytes = h.state().bytes_read();
    let commits = h.commits() - commits_before;
    println!(
        "{what}: {secs:.2} s, {} bytes, {:.1} MB/s, {frames} drains, {commits} commits \
         ({:.1} commits/s)",
        bytes,
        bytes as f64 / secs / 1e6,
        f64::from(commits) / secs,
    );
}

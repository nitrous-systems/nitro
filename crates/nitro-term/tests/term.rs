//! The terminal, driven through a real server and a real `/bin/sh`.
//!
//! Every test here builds the tree the binary builds
//! ([`nitro_term::build`]), installs the same hooks ([`nitro_term::install`])
//! and drives it the way a user or a script would. The pty is a real
//! pseudoterminal with a real shell on the far end, so "the shell echoed
//! this" means the shell really did.
//!
//! Half of these are **cost** tests rather than behaviour tests, and
//! they are the reason the file exists. The claims in `docs/term.md` —
//! a commit carries a screenful rather than a line, one keystroke costs
//! two mutations, an idle terminal sends nothing — are all assertions
//! about what does *not* happen, and the mutation tap is the only honest
//! way to check them from outside.

use std::time::{Duration, Instant};

use nitro_term::pty::Pty;
use nitro_term::widget::{TermGrid, TermGridMut as _};
use nitro_term::{GRID_NAME, TermApp};
use nitro_ui::event::key;
use nitro_ui::test::Harness;
use nitro_ui::{ColorRole, Palette, Size, WidgetId};

/// How long a test will wait for a shell to say something before giving
/// up. Generous: a loaded CI box forks slowly, and a flaky timeout in a
/// test that is *about* throughput would be read as a throughput bug.
const DEADLINE: Duration = Duration::from_secs(10);

/// A harness running the real tree over a pty running `argv`.
///
/// The window is 640×400, which is comfortably more than the harness's
/// 320×240 output — the grid is sized from the window, not the output,
/// and a test about cell counts should not be a test about clipping.
fn harness_running(argv: &[&str]) -> (Harness<TermApp>, WidgetId) {
    let pty = Pty::spawn_command(argv, 80, 24).expect("pty");
    // `term_theme()`, the same one `run()` passes: the window backdrop
    // *is* the terminal's default background, and the grid paints no
    // rect for a default-background run because of it. A harness on the
    // toolkit's default light theme would be a different terminal, and
    // the one class of bug it could not see is precisely the one that
    // shipped — a light backdrop under a dark palette.
    let mut h = Harness::with(
        "nitro-term",
        TermApp::new(pty),
        Some(Size::new(640.0, 400.0)),
        nitro_term::term_theme(&Palette::default()),
        nitro_term::build,
    );
    let grid = nitro_term::grid_of(h.ui()).expect("the grid");
    // The same `install` the binary calls, on the same tree `build`
    // produced: a test that installed its own hooks would be testing a
    // different terminal.
    let (ui, state) = h.parts();
    nitro_term::install(ui, state, grid).expect("install");
    h.settle();
    (h, grid)
}

/// A harness on a plain interactive shell, with the prompt suppressed so
/// the screen holds only what a test put there.
fn harness_shell() -> (Harness<TermApp>, WidgetId) {
    harness_running(&["/bin/sh", "-c", "PS1= ; export PS1; exec /bin/sh -i"])
}

/// Pump the harness until `f` is true, draining the pty as the app loop
/// would.
///
/// The descriptor hook only runs when `epoll` says so, and the harness
/// has no `epoll` — so a test drives the drain directly. It is the same
/// function the hook calls, which is what keeps this a test of the app
/// rather than of the test.
fn pump_until(
    h: &mut Harness<TermApp>,
    what: &str,
    mut f: impl FnMut(&mut Harness<TermApp>) -> bool,
) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain(h);
        h.settle();
        if f(h) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One drain of the pty into the grid, plus the frame the app would take.
fn drain(h: &mut Harness<TermApp>) {
    drain_only(h);
    h.frame();
}

/// One turn of the **real** app loop that carried pty data: drain the
/// descriptor, then flush — because `App`'s `event_loop_with` calls
/// `ui.flush()` at the end of every wakeup, unconditionally.
///
/// That trailing flush is the whole reason this helper exists in this
/// shape. An earlier version drained and did not flush, which meant a
/// commit could only happen at `h.frame()` — so the cost test asserted
/// frame pacing that the harness was providing rather than the app.
/// Leaving it out hides every commit that happens between frames, which
/// is exactly what `a_commit_carries_a_screenful_not_a_line` measures.
fn drain_only(h: &mut Harness<TermApp>) {
    {
        let (ui, state) = h.parts();
        nitro_term::drain_pty(state, ui);
    }
    h.flush();
}

/// The screen, as text.
fn screen(h: &mut Harness<TermApp>, grid: WidgetId) -> String {
    h.widget::<TermGrid>(grid).term().grid().text()
}

/// Type `text` into the pty, as `hey … set grid value` does.
fn type_text(h: &mut Harness<TermApp>, grid: WidgetId, text: &str) {
    // Straight to the pty: the widget holds its own `dup` of the
    // master, so there is no queue for a test (or the app) to remember
    // to flush.
    h.ui()
        .widget_mut::<TermGrid>(grid)
        .expect("grid")
        .send_bytes(text.as_bytes());
}

// ---------------------------------------------------------------------
// behaviour
// ---------------------------------------------------------------------

#[test]
fn echo_hello_shows_hello_in_the_grid() {
    let (mut h, grid) = harness_running(&["/bin/sh", "-c", "echo hello"]);
    pump_until(&mut h, "hello", |h| screen(h, grid).contains("hello"));
    assert!(screen(&mut h, grid).contains("hello"));
    h.quit();
}

#[test]
fn what_the_shell_colours_becomes_styled_runs() {
    // `printf` with SGR is what `ls --color` does, minus the directory
    // listing: red text, then a default-coloured tail. Two runs, and the
    // first carries the colour.
    let (mut h, grid) =
        harness_running(&["/bin/sh", "-c", "printf '\\033[31mred\\033[0m plain\\n'"]);
    pump_until(&mut h, "the coloured line", |h| {
        screen(h, grid).contains("red plain")
    });
    let g = h.widget::<TermGrid>(grid);
    let mut runs = Vec::new();
    g.term().grid().row_runs(0, &mut runs);
    assert!(
        runs.len() >= 2,
        "expected a coloured run and a plain one: {runs:?}"
    );
    assert_eq!(runs[0].text, "red");
    assert_eq!(
        runs[0].style.fg,
        nitro_term::grid::CellColor::Indexed(1),
        "SGR 31 is palette colour 1"
    );
    assert_eq!(
        runs[1].style.fg,
        nitro_term::grid::CellColor::Default,
        "SGR 0 put it back"
    );
    h.quit();
}

#[test]
fn the_osc_title_reaches_the_window() {
    let (mut h, grid) = harness_running(&[
        "/bin/sh",
        "-c",
        "printf '\\033]0;hello from osc\\007'; sleep 5",
    ]);
    pump_until(&mut h, "the title", |h| {
        h.ui().window_title() == "hello from osc"
    });
    assert_eq!(h.ui().window_title(), "hello from osc");
    let _ = grid;
    h.quit();
}

#[test]
fn the_alt_screen_is_entered_and_left_without_losing_the_primary() {
    // What `vim` does on start-up and on `:q`, reduced to its two escape
    // sequences. The primary screen has to come back exactly.
    let (mut h, grid) = harness_running(&[
        "/bin/sh",
        "-c",
        "printf 'primary\\n'; sleep 0.2; printf '\\033[?1049h\\033[Halt screen'; \
         sleep 0.4; printf '\\033[?1049l'; sleep 5",
    ]);
    pump_until(&mut h, "the alt screen", |h| {
        h.widget::<TermGrid>(grid).term().alt_screen()
    });
    assert!(screen(&mut h, grid).contains("alt screen"));
    assert!(
        !screen(&mut h, grid).contains("primary"),
        "the alt screen must not show the primary one"
    );
    pump_until(&mut h, "the primary screen back", |h| {
        !h.widget::<TermGrid>(grid).term().alt_screen()
    });
    assert!(
        screen(&mut h, grid).contains("primary"),
        "leaving 1049 restores the primary screen exactly"
    );
    h.quit();
}

#[test]
fn a_resize_reaches_the_shell() {
    // The resize path end to end, **through the app's own hook**: a
    // `Configure` arrives, `Ui` fires `on_resize`, the terminal turns
    // the new pixel size into a cell count, reflows, and `TIOCSWINSZ`
    // tells the child — which then reports what it was told.
    //
    // The `h.configure(...)` below is the *only* thing this test does to
    // cause all of that, and that is the point. An earlier version
    // called `nitro_term::sync_size` by hand right after it, which meant
    // it asserted that `sync_size` works rather than that a resize
    // works — and the binary, which had no resize hook at all, kept its
    // start-up geometry for ever while this test passed.
    let (mut h, grid) = harness_shell();
    let cell = h.widget::<TermGrid>(grid).cell_size();
    assert!(
        cell.w > 0.0,
        "the cell metric comes from a real measurement"
    );

    // A window exactly 40 cells wide, whatever the font turned out to be.
    let want_cols = 40usize;
    let want_rows = 10usize;
    h.configure(Size::new(
        cell.w * want_cols as f32,
        cell.h * want_rows as f32,
    ));
    assert_eq!(h.widget::<TermGrid>(grid).term().grid().cols(), want_cols);
    assert_eq!(h.widget::<TermGrid>(grid).term().grid().rows(), want_rows);

    // `stty size` rather than `$COLUMNS`: the variable is a bash-ism
    // that `dash` — which is `/bin/sh` on Debian and Ubuntu — does not
    // maintain, so asking for it would test the shell rather than the
    // `TIOCSWINSZ` this test is about. `stty` asks the kernel, which is
    // exactly who we told.
    type_text(&mut h, grid, "stty size\n");
    pump_until(&mut h, "the shell's window size", |h| {
        screen(h, grid).contains(&format!("{want_rows} {want_cols}"))
    });
    h.quit();
}

#[test]
fn hey_addresses_the_grid_by_name() {
    // The two `hey` recipes in the README, through the same code path
    // the socket uses: `get grid text` reads the screen, `set grid value`
    // types into the pty.
    let (mut h, grid) = harness_shell();
    let resolved = nitro_ui::introspect::resolve(h.ui(), &format!("window/{GRID_NAME}"));
    assert_eq!(resolved, Some(grid), "the grid answers to its name");

    let (ui, state) = h.parts();
    nitro_ui::introspect::set(
        ui,
        state,
        &format!("window/{GRID_NAME}"),
        "value",
        // The literal two-character `\n` a shell would pass, not a real
        // newline: interpreting it is the widget's job, and asserting on
        // the already-interpreted form would skip the half of the path
        // the box run found broken.
        "echo scripted\\n",
    )
    .expect("set value");

    // The bug the box run found, and the reason this assertion is here
    // rather than folded into the loop below: `set value` used to fill a
    // queue that only the *descriptor hook* emptied, so a scripted
    // command sat unsent until the child happened to say something on
    // its own. Nothing here drains anything — the widget writes to its
    // own `dup` of the master — so the only way `scripted` reaches the
    // screen is if `set` really wrote it.
    pump_until(&mut h, "the scripted command", |h| {
        screen(h, grid).contains("scripted")
    });

    // And reading it back is a screen dump, with no font and no
    // screenshot involved. Both spellings: `value` is what every widget
    // answers, and `text` is what a caller asks when it wants "what does
    // this read" without knowing the role — the box run found that one
    // returning nothing, because `Role::Terminal` was not in the set the
    // `text` property is derived for.
    for prop in ["value", "text"] {
        let dump = nitro_ui::introspect::get_prop(h.ui(), &format!("window/{GRID_NAME}"), prop)
            .expect("get");
        assert!(
            dump.contains("scripted"),
            "get grid {prop} should dump the screen, got {dump:?}"
        );
    }
    h.quit();
}

// ---------------------------------------------------------------------
// cost
// ---------------------------------------------------------------------

#[test]
fn a_scripted_send_types_rather_than_pastes() {
    // The box run's second finding, and the subtlest of the three.
    //
    // `send` used to wrap its bytes in the bracketed-paste markers when
    // the program had asked for them (DECSET 2004). bash 5.1 and later
    // turn that on by default — and the whole *purpose* of the markers
    // is to tell readline that what follows is data, not keystrokes, so
    // the newline was inserted as a literal character and every scripted
    // command sat on the prompt unrun. The screen looked perfect; the
    // command never executed.
    //
    // A script driving a terminal is a keyboard, not a clipboard. The
    // assertion is on the bytes the pty received, because "it typed it"
    // and "it pasted it" produce the same screen and differ only there.
    let (mut h, grid) = harness_running(&[
        "/bin/sh",
        "-c",
        // `cat` echoes its input back, so what the child received is
        // observable from the screen without a second channel.
        "printf '\\033[?2004h'; cat",
    ]);
    pump_until(&mut h, "the terminal to accept 2004", |h| {
        h.widget::<TermGrid>(grid).term().bracketed_paste()
    });

    let (ui, state) = h.parts();
    nitro_ui::introspect::invoke(
        ui,
        state,
        &format!("window/{GRID_NAME}"),
        "send",
        Some("hello\\n"),
    )
    .expect("send");
    pump_until(&mut h, "the echoed text", |h| {
        screen(h, grid).contains("hello")
    });

    let text = screen(&mut h, grid);
    assert!(
        !text.contains("200~") && !text.contains("201~"),
        "a scripted send must not be bracketed; the child echoed {text:?}"
    );
    h.quit();
}

#[test]
fn the_window_backdrop_is_the_terminals_own_background() {
    // A pixel test, and the only kind that could have caught this.
    //
    // `TermGrid::bg_of` answers `None` for a run whose background is the
    // default, so no rect is painted for it — which is most of the
    // screen, and the reason ordinary text costs one node per run rather
    // than two. That optimisation is only correct if the window's
    // backdrop is already the terminal's background colour, and nothing
    // made it so: the app took the toolkit's default light theme, and a
    // bare shell prompt on the box was dark text on `#f2f2f2` with the
    // light-on-dark ANSI palette illegible over it.
    //
    // Every existing test passed, because they all assert on the grid
    // model or on mutation counts. The screenshots were of `vim` and
    // `htop`, which set their own background and so hid it.
    let (mut h, grid) = harness_running(&["/bin/sh", "-c", "sleep 30"]);
    h.ui()
        .widget_mut::<TermGrid>(grid)
        .expect("grid")
        .feed(b"x");
    h.frame();
    h.settle();

    // `Color::to_u32` packs RGBA, but a screenshot pixel is 0xAARRGGBB,
    // so the comparison is built from the components rather than by
    // masking one into the other.
    let bg = Palette::default().get(ColorRole::TerminalBackground);
    let want = (u32::from(bg.r) << 16) | (u32::from(bg.g) << 8) | u32::from(bg.b);
    let shot = h.shot();
    // A point well away from the one glyph, in the middle of the window.
    let (x, y) = (shot.width / 2, shot.height / 2);
    let got = shot.pixel(x, y) & 0x00ff_ffff;
    assert_eq!(
        got, want,
        "the backdrop at ({x},{y}) is #{got:06x}, not the palette's #{want:06x}; \
         default-background runs paint no rect and rely on this"
    );
    h.quit();
}

#[test]
fn a_commit_carries_a_screenful_not_a_line() {
    // What actually bounds the commit rate, asserted on the real loop
    // sequence (drain, then flush — `event_loop_with` flushes after every
    // wakeup).
    //
    // The claim is about **bytes per commit**, not frames per commit, and
    // that is a correction the box forced. The tidier design — mark paint
    // only from the frame callback, so the scene is touched once per
    // `Frame` — froze the screen: a `RequestFrame` is one-in-flight, so
    // painting became dependent on an answer that a server coalescing
    // flips under load does not send. Four frames in twelve seconds, with
    // consecutive framebuffer readbacks byte-identical while output
    // flowed.
    //
    // So `DRAIN_CHUNK` is the bound: at most 256 KiB of pty output per
    // commit. A megabyte of `seq` is a few dozen commits rather than a
    // million, and the server's flip coalescing keeps the *glass* at one
    // change per refresh however many commits arrive.
    let (mut h, grid) = harness_running(&["/bin/sh", "-c", "seq 1 200000"]);
    h.tap();
    h.clear_tap();
    let before = h.commits();

    let deadline = Instant::now() + DEADLINE;
    while !screen(&mut h, grid).contains("200000") {
        drain_only(&mut h);
        assert!(Instant::now() < deadline, "timed out waiting for seq");
    }
    let commits = h.commits() - before;
    let bytes = h.state().bytes_read();

    // 200 000 lines is ~1.3 MB: six 256 KiB chunks, plus slack for the
    // partial reads a pipe delivers. The number that must not appear is
    // anything approaching one per line.
    let floor = bytes as usize / (256 * 1024);
    assert!(
        commits < 200,
        "{commits} commits for {bytes} bytes ({} lines); chunking should \
         give roughly {floor}, and one-per-line would be 200 000",
        200_000
    );
    assert!(
        commits >= 2,
        "{commits} commits for {bytes} bytes — the screen cannot be \
         keeping up with a burst it only drew once"
    );
    h.quit();
}

#[test]
fn a_fast_writer_does_not_starve_the_screen() {
    // The bug this test exists for: `drain_pty` used to read until
    // `WouldBlock`, which never comes while the writer is faster than
    // the reader. `cat` of a 5 MB file was consumed in **one** drain and
    // produced **one** commit — a perfect score by any bytes-per-commit
    // measure, describing a terminal that showed nothing for two and a
    // half seconds and then jumped to the end.
    //
    // So the assertion is that a big stream takes *several* drains, which
    // is the opposite direction from every other cost test here and is
    // deliberate. Every other bound in this file is an upper one; this is
    // the lower bound that keeps them honest, because "very few commits"
    // is also what a frozen screen produces.
    let (mut h, grid) = harness_running(&[
        "/bin/sh",
        "-c",
        "yes 'the quick brown fox jumps over the lazy dog' | head -c 3000000",
    ]);
    let mut drains = 0u32;
    let deadline = Instant::now() + DEADLINE;
    loop {
        drain_only(&mut h);
        h.frame();
        drains += 1;
        if h.state().bytes_read() >= 3_000_000 {
            break;
        }
        assert!(Instant::now() < deadline, "timed out reading 3 MB");
    }
    assert!(
        drains > 4,
        "3 MB arrived in {drains} drain(s); a screen that only updates \
         when the writer stops is not keeping up"
    );
    let _ = grid;
    h.quit();
}

#[test]
fn one_keystroke_costs_two_mutations() {
    // The steady-state cost claim, and the word *steady* is doing real
    // work. A key echoed by an idle shell changes two things: the run of
    // text on the cursor's row, and the cursor rect that moved one cell
    // right. Anything more means a row that did not change was re-sent.
    //
    // The **first** character on a row costs two more — a `CreateNode`
    // for the text node that row did not have, and the `SetBounds` that
    // places it. That is not waste, it is the node coming into
    // existence, and it happens once per row rather than once per key.
    // So the test measures both: the first keystroke on a fresh row, and
    // then the one after it, which is the number a person typing feels.
    let (mut h, grid) = harness_shell();
    settle_shell(&mut h, grid);

    h.tap();
    h.clear_tap();
    type_text(&mut h, grid, "x");
    pump_until(&mut h, "the echoed x", |h| screen(h, grid).contains('x'));
    let first: Vec<&str> = tapped_ops(&h);
    assert!(
        first.len() <= 4,
        "the first key on a row cost {} mutations ({first:?}); \
         a node, its bounds, its text and the cursor is the budget",
        first.len()
    );
    assert!(
        first.contains(&"CreateNode"),
        "the first key on a row is where the text node is created: {first:?}"
    );

    // Now the steady state: the node exists, its box spans the row, and
    // only the string and the cursor move.
    h.clear_tap();
    type_text(&mut h, grid, "y");
    pump_until(&mut h, "the echoed y", |h| screen(h, grid).contains("xy"));
    let second: Vec<&str> = tapped_ops(&h);
    assert_eq!(
        second,
        ["SetText", "SetBounds"],
        "a keystroke into an existing row is one SetText for the run and \
         one SetBounds for the cursor rect"
    );
    h.quit();
}

/// The mutations the tap holds, minus the commits that carry them.
fn tapped_ops(h: &Harness<TermApp>) -> Vec<&'static str> {
    h.mutations()
        .iter()
        .filter(|m| m.op != "Commit")
        .map(|m| m.op)
        .collect()
}

/// Pump until the shell has finished starting up and the grid is clean.
fn settle_shell(h: &mut Harness<TermApp>, grid: WidgetId) {
    pump_until(h, "a quiet shell", |h| {
        drain_only(h);
        h.frame();
        !h.widget::<TermGrid>(grid).term().grid().dirty()
    });
}

#[test]
fn nothing_is_sent_while_it_sits_there() {
    // The idle contract, from outside: a shell at a prompt, no timer, no
    // frame request, no bytes.
    let (mut h, grid) = harness_shell();
    settle_shell(&mut h, grid);
    // Drain once more so anything still in flight has landed.
    drain(&mut h);
    h.settle();

    assert_eq!(
        h.next_timeout(),
        None,
        "an idle terminal schedules no timer"
    );
    assert!(
        !h.ui().frame_pending(),
        "an idle terminal has no frame callback outstanding"
    );
    h.assert_idle(300);
    h.quit();
}

#[test]
fn a_clean_row_sends_nothing_when_another_row_changes() {
    // The per-row damage claim: writing to the bottom row must not
    // re-send the top one. Driven through the grid directly, because the
    // point is about which rows `paint` touches and a shell would decide
    // that for us.
    let (mut h, grid) = harness_running(&["/bin/sh", "-c", "sleep 30"]);
    h.ui()
        .widget_mut::<TermGrid>(grid)
        .expect("grid")
        .feed(b"top row\r\n\r\n\r\nbottom");
    h.frame();
    h.settle();

    h.tap();
    h.clear_tap();
    // One more character on the bottom row; the top row is untouched.
    h.ui()
        .widget_mut::<TermGrid>(grid)
        .expect("grid")
        .feed(b"!");
    h.frame();
    h.settle();

    let set_texts = h.mutations().iter().filter(|m| m.op == "SetText").count();
    assert_eq!(
        set_texts,
        1,
        "one changed row is one SetText, not one per row on screen: {:?}",
        h.mutations()
    );
    h.quit();
}

#[test]
fn scrolling_back_and_typing_snaps_to_the_bottom() {
    let (mut h, grid) = harness_running(&["/bin/sh", "-c", "sleep 30"]);
    h.ui()
        .widget_mut::<TermGrid>(grid)
        .expect("grid")
        .feed(b"one\r\ntwo\r\nthree\r\n");
    h.frame();
    h.settle();

    // Wheel up: the view moves into the scrollback only if there is any,
    // which there is not on a 24-row screen with three lines — so the
    // assertion is about the snap-back, which holds either way.
    h.ui().widget_mut::<TermGrid>(grid).expect("grid").feed(&[]);
    for _ in 0..40 {
        h.ui()
            .widget_mut::<TermGrid>(grid)
            .expect("grid")
            .feed(b"filler\r\n");
    }
    h.frame();
    h.settle();
    h.widget::<TermGrid>(grid);
    // Now there is scrollback. Scroll into it.
    {
        let mut g = h.ui().widget_mut::<TermGrid>(grid).expect("grid");
        g.term_mut().grid_mut().scroll_up(5);
    }
    assert!(
        h.widget::<TermGrid>(grid).term().grid().scroll_offset() > 0,
        "the view really is scrolled back"
    );

    h.key(key::A);
    assert_eq!(
        h.widget::<TermGrid>(grid).term().grid().scroll_offset(),
        0,
        "any keypress snaps the view back to the bottom"
    );
    h.quit();
}

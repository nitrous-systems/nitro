//! `nitro-term` — a terminal emulator, and the app that makes the box
//! usable.
//!
//! It is the application that stresses this stack hardest. A calculator
//! changes one label per keypress; a terminal can change every cell on
//! screen twenty times a second, and the thing generating the change
//! does not know or care that a display server exists. So it is where
//! `DESIGN.md`'s first goal — **work proportional to change, idle means
//! zero** — either holds or is revealed to be a claim about toy
//! workloads.
//!
//! ```text
//!   ┌──────────────────────────────────────────┐
//!   │ $ ls --color                             │   the grid, named `grid`
//!   │ Cargo.toml  crates/  docs/               │
//!   │ $ █                                      │
//!   └──────────────────────────────────────────┘
//!            ▲                    │
//!      pty master fd         keys, encoded
//!            │                    ▼
//!        setsid --ctty $SHELL  (its own session, our pty as its tty)
//! ```
//!
//! # The four pieces
//!
//! **[`pty`]** opens the pseudoterminal and starts the shell. The child
//! needs its own session and the pty as its controlling terminal, and
//! this tree denies `unsafe` — so `pre_exec` is out and the job is given
//! to util-linux's `setsid --ctty`, which does exactly those two
//! syscalls between the fork and the exec. Without it there is no job
//! control and Ctrl-C reaches nothing; with it a shell is a shell.
//!
//! **[`vt`]** parses what the child writes. The state machine is the
//! `vte` crate's (Paul Williams' DEC ANSI parser, which assigns no
//! meaning); every escape sequence's *meaning* is [`vt::Term`]'s, and
//! every one of them ends up as a call on a [`grid::Grid`].
//!
//! **[`grid`]** is the model: rows of cells, a scrollback ring, an
//! alternate screen, and **damage per row and per column span**. The
//! damage is not an optimisation bolted on afterwards — it is what the
//! widget reads to decide what to send, so a write to one cell is
//! traceable all the way to one `SetText` on the wire.
//!
//! **[`widget::TermGrid`]** draws it: one `Text` node per same-style run
//! per row, each in its own paint slot, so the toolkit's per-slot diff
//! drops everything that did not change. `docs/term.md` has the numbers.
//!
//! # One commit per screenful, not one per line
//!
//! The loop below is the reason `seq 1 1000000` does not melt the
//! compositor. Bytes are drained from the pty the instant they arrive —
//! the child must never block on a full pipe — but they go only into the
//! grid, and a commit carries **at most [`DRAIN_CHUNK`] of pty output**
//! — 256 KiB, about four screenfuls — rather than one line.
//!
//! That bound, not the frame callback, is what paces the scene, and the
//! distinction was settled on hardware rather than argued: making the
//! frame callback the only thing that could paint froze the screen,
//! because a `RequestFrame` is one-in-flight and a server coalescing
//! flips under load is precisely when its answer does not come. The
//! server's own flip coalescing supplies the upper bound a frame
//! callback was meant to: however many commits arrive, the glass
//! changes at most once per refresh. See `docs/term.md`.
//!
//! And when nothing is happening, nothing is asked for: no frame is
//! requested when the grid has no damage, so the app sits in
//! `epoll_wait` with no timer and no pending callback. That is the
//! property `nothing_is_sent_while_it_sits_there` checks from outside.
//!
//! # Driving it from a shell
//!
//! ```console
//! $ hey nitro-term get grid text          # the whole screen, as text
//! $ hey nitro-term set grid value 'ls\n'  # type it into the pty
//! ```
//!
//! Both go through the widget's own `action`, so a script and a keyboard
//! reach the pty by the same path — which is what makes the tests and
//! the box acceptance drivable without a keyboard at all.

pub mod grid;
pub mod keys;
pub mod pty;
pub mod theme;
pub mod vt;
pub mod widget;

use nitro_ui::build::StyleBuilder as _;
use nitro_ui::event::{Handled, KeyEvent, mods};
use nitro_ui::{App, Error, Size, Ui, WidgetId};

use crate::pty::Pty;
use crate::widget::{TermGrid, TermGridMut as _};

/// The name the app registers under, and so the first argument to `hey`.
pub const APP_NAME: &str = "nitro-term";

/// The addressing name of the grid widget.
///
/// One name for two jobs, deliberately: `get grid text` reads the screen
/// and `set grid value` types into it. A second widget for input would
/// be a second thing to keep in step with the first, and there is
/// nothing for it to be — the pty is the input.
pub const GRID_NAME: &str = "grid";

/// The app's state: the pty, and the id of the widget showing it.
///
/// This is the `S` of `Ui<S>`. The *terminal* lives in the widget rather
/// than here, because every widget method needs it; what lives here is
/// the descriptor, which only the loop touches.
pub struct TermApp {
    pty: Pty,
    /// The widget showing the pty, once the tree has been built.
    ///
    /// `Option` because the state is constructed before the tree is: the
    /// pty has to exist first (the child starts while the window is
    /// being opened), and a placeholder id would be a lie the type
    /// system could not catch. Every helper below simply does nothing
    /// until [`install`] has filled it in.
    grid: Option<WidgetId>,
    /// Read buffer, reused. 64 KiB because that is the pipe capacity a
    /// fast writer fills: a smaller buffer only means more syscalls per
    /// megabyte.
    buf: Vec<u8>,
    /// Bytes read since start-up, for the tests and for `hey`.
    bytes_read: u64,
    /// Frame callbacks served, ditto. The ratio of this to `bytes_read`
    /// is the claim the throughput number rests on.
    frames: u64,
}

impl TermApp {
    /// A terminal on `pty`, showing the widget that will be built next.
    #[must_use]
    pub fn new(pty: Pty) -> Self {
        Self {
            pty,
            grid: None,
            buf: vec![0; 64 * 1024],
            bytes_read: 0,
            frames: 0,
        }
    }

    /// Bytes read from the pty since start-up.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Frame callbacks served since start-up.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The grid widget, once [`install`] has named it.
    #[must_use]
    pub fn grid(&self) -> Option<WidgetId> {
        self.grid
    }

    /// Whether the child has gone.
    pub fn child_exited(&mut self) -> bool {
        self.pty.child_exited()
    }
}

/// The most a single drain will read before handing the loop back, and
/// so **the bound on how much output one commit carries**.
///
/// Public because it is the pacing mechanism rather than an
/// implementation detail: a commit is at most this many bytes of pty
/// output — about four screens of dense text at 80×24 — and the upper
/// bound on what the *display* shows is the server's flip coalescing,
/// not anything this client does.
///
/// The cap is not a tuning knob. Without it the drain loop is unbounded:
/// a writer faster than we are — `cat` of a large file is exactly that —
/// refills the pty as fast as we empty it, so "read until `WouldBlock`"
/// never comes back. Measured, before the cap existed: `cat` of a 5 MB
/// file was consumed in **one** drain and produced **one** commit, which
/// is a perfect score by the letter of the pacing claim and describes a
/// terminal that showed nothing for two and a half seconds and then
/// jumped to the end.
///
/// The loop is level-triggered, so whatever is left over wakes us again
/// immediately and the screen keeps up with the stream rather than
/// waiting for it to end.
pub const DRAIN_CHUNK: usize = 256 * 1024;

/// Drain what the pty has, feed it to the grid, and write back whatever
/// the grid owes.
///
/// Called from the descriptor hook, so it runs whenever `epoll` says the
/// master is readable. It reads until `WouldBlock` **or [`DRAIN_CHUNK`]
/// bytes**, whichever comes first — see that constant for why the second
/// half of that sentence is load-bearing. It deliberately does not touch
/// the scene; see the module docs.
///
/// Returns whether the child is gone, which is what ends the app.
pub fn drain_pty(state: &mut TermApp, ui: &mut Ui<TermApp>) -> bool {
    let Some(grid) = state.grid else { return false };
    let mut buf = std::mem::take(&mut state.buf);
    let mut closed = false;
    let mut drained = 0usize;
    loop {
        match state.pty.read(&mut buf) {
            Ok(0) => {
                closed = true;
                break;
            }
            Ok(n) => {
                state.bytes_read += n as u64;
                drained += n;
                if let Ok(mut g) = ui.widget_mut::<TermGrid>(grid) {
                    g.feed(&buf[..n]);
                }
                if drained >= DRAIN_CHUNK {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // An I/O error on a pty master means the far end is gone;
            // on Linux the last close gives `EIO` rather than `EOF`,
            // which is why this is a normal ending and not a panic.
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    state.buf = buf;
    // The title and the frame request ride the same drain: an OSC that
    // arrived in this batch should reach the window in the commit the
    // batch produces, not the one after.
    sync_title(state, ui);
    let _ = crate::widget::request_frame_if_dirty(ui, grid);
    closed
}

/// Push an OSC title through to the window, if one arrived.
///
/// `set_window_title` drops an unchanged title, which matters more here
/// than anywhere else: a shell whose prompt carries an OSC sets the same
/// string on every command.
pub fn sync_title(state: &mut TermApp, ui: &mut Ui<TermApp>) {
    let Some(grid) = state.grid else { return };
    let title = ui
        .widget_mut::<TermGrid>(grid)
        .ok()
        .and_then(|mut g| g.take_title());
    if let Some(t) = title {
        let _ = ui.set_window_title(t);
    }
}

/// Bring the grid, the window and the child's `winsize` into line with
/// the window's current size.
///
/// The order is the point. The cell count comes from the *measured* cell
/// metric, so it cannot be computed before the first layout; the grid is
/// resized first (so the widget's own reflow happens inside the toolkit's
/// passes), and `TIOCSWINSZ` goes last — because that is what sends the
/// child `SIGWINCH`, and a child that repainted before the grid had the
/// new size would paint into the old one.
pub fn sync_size(state: &mut TermApp, ui: &mut Ui<TermApp>) {
    let Some(grid) = state.grid else { return };
    let window = ui.window_size();
    let (cols, rows) = match ui.widget::<TermGrid>(grid) {
        Ok(g) => g.cells_for(window),
        Err(_) => return,
    };
    if cols == 0 || rows == 0 {
        return;
    }
    let changed = ui
        .widget::<TermGrid>(grid)
        .is_ok_and(|g| g.term().grid().cols() != cols || g.term().grid().rows() != rows);
    if !changed {
        return;
    }
    if let Ok(mut g) = ui.widget_mut::<TermGrid>(grid) {
        g.resize_grid(cols, rows);
    }
    let _ = state.pty.resize(cols as u16, rows as u16);
}

/// Build the tree: a container holding the grid, which fills it.
///
/// The container is not decoration. `hey nitro-term get grid text` has
/// to resolve `window/grid`, and a path segment names a *child* of the
/// root — so a grid that was itself the root would answer to `window`
/// and to nothing else, and every recipe in the README would have to
/// spell the root instead of the widget. One `Flex` is the price of the
/// grid keeping its own name.
///
/// # Panics
/// Never in practice: the only `attach` names an id built one line
/// above, and a fresh id cannot be stale.
pub fn build(ui: &mut Ui<TermApp>) -> WidgetId {
    let grid = ui.build(
        crate::widget::term_grid()
            .name(GRID_NAME)
            .cells(80, 24)
            .scrollback(default_scrollback())
            // The grid takes whatever size the server gives; the cell
            // count follows from that rather than the other way round.
            .grow(1.0)
            .width_percent(1.0)
            .height_percent(1.0),
    );
    let root = ui.build(nitro_ui::widgets::column());
    ui.attach(root, grid).expect("attach the grid");
    root
}

/// The grid widget in a tree [`build`] made.
///
/// A helper rather than a constant, because the id is the arena's to
/// hand out; every caller wants the same first child.
#[must_use]
pub fn grid_of(ui: &Ui<TermApp>) -> Option<WidgetId> {
    ui.root().and_then(|r| ui.children(r).first().copied())
}

/// How many lines of scrollback, from `--scrollback N` or the default.
///
/// Read from the command line rather than a config file because there is
/// no config system yet and inventing one for a single integer would be
/// the wrong order to do things in.
#[must_use]
pub fn default_scrollback() -> usize {
    scrollback_from(std::env::args().skip(1))
}

/// The `--scrollback N` argument, or 10 000.
///
/// Split out so it is testable without touching the process's own
/// arguments, which are global state a test cannot set without racing
/// every other test in the binary.
pub fn scrollback_from(args: impl IntoIterator<Item = String>) -> usize {
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if let Some(rest) = a.strip_prefix("--scrollback=") {
            return rest.parse().unwrap_or(DEFAULT_SCROLLBACK);
        }
        if a == "--scrollback" {
            return it
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_SCROLLBACK);
        }
    }
    DEFAULT_SCROLLBACK
}

/// Lines of scrollback kept by default.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Install the hooks that make the app a terminal: the pty's descriptor,
/// the frame callback, and the quit shortcut.
///
/// Separate from [`run`] so the tests install the same three on a
/// harness-built tree rather than a copy of them.
///
/// # Errors
/// If the pty's descriptor cannot be registered.
pub fn install(ui: &mut Ui<TermApp>, state: &mut TermApp, grid: WidgetId) -> Result<(), Error> {
    state.grid = Some(grid);
    // The widget writes to the pty itself, through its own `dup` of the
    // master. Handing it the descriptor rather than a queue for the app
    // to drain is what makes a key, a paste and a scripted
    // `hey set grid value` all take the same path: the bytes are written
    // in the turn they were produced, with no entry point left to
    // remember a flush. See `TermGrid::pty`.
    if let Ok(fd) = state.pty.dup_master()
        && let Ok(mut g) = ui.widget_mut::<TermGrid>(grid)
    {
        g.set_pty_fd(fd);
    }
    let fd = state.pty.as_fd();
    ui.add_fd(fd, |s: &mut TermApp, ui: &mut Ui<TermApp>| {
        if drain_pty(s, ui) {
            ui.quit();
        }
    })?;
    // The frame callback counts frames and re-arms; it is deliberately
    // *not* the only thing that can paint.
    //
    // Making it so was the tidier design and it froze the screen: a
    // `RequestFrame` is one-in-flight, so painting became dependent on
    // the answer arriving — and during a sustained burst the server is
    // coalescing flips, which is exactly when it does not. Four frames
    // in twelve seconds on the box, with consecutive framebuffer
    // readbacks byte-identical while output flowed. See
    // `TermGridMut::feed` for the full reasoning and `docs/term.md` for
    // what bounds the commit rate instead.
    //
    // Re-arming here keeps one request outstanding while output is
    // flowing, which is what makes `frames` a usable measure of how
    // often the screen actually changed.
    ui.on_frame(move |s: &mut TermApp, ui: &mut Ui<TermApp>, _f| {
        s.frames += 1;
        let _ = crate::widget::request_frame_if_dirty(ui, grid);
    });
    // The window's size is the grid's size, and nothing else will tell
    // us it changed. This is the hook that makes a dragged window a
    // reflowed grid and a `SIGWINCH` for the child; without it the
    // terminal keeps its start-up geometry for ever and `stty size`
    // stays wrong, which is exactly the state this app was in until a
    // review caught that only the *test* called `sync_size`.
    ui.on_resize(|s: &mut TermApp, ui: &mut Ui<TermApp>, _size| sync_size(s, ui));
    // Ctrl-Shift-Q, not Ctrl-Q: a terminal must not steal a chord the
    // program inside it might want, and Ctrl-Q is XON.
    ui.set_shortcut(
        mods::CTRL | mods::SHIFT,
        nitro_ui::event::key::Q,
        |_s: &mut TermApp, ui: &mut Ui<TermApp>| ui.quit(),
    );
    // Keys the grid did not take still belong to the pty: the widget
    // only has focus once something has clicked or tabbed into it, and a
    // terminal whose first keystroke went nowhere would look broken.
    ui.on_key(
        move |_s: &mut TermApp, ui: &mut Ui<TermApp>, ev: &KeyEvent| {
            let sent = ui
                .widget_mut::<TermGrid>(grid)
                .is_ok_and(|mut g| g.type_key(ev));
            if sent {
                let _ = crate::widget::request_frame_if_dirty(ui, grid);
                Handled::Yes
            } else {
                Handled::No
            }
        },
    );
    Ok(())
}

/// Connect, open the window, start the shell and run until it exits.
///
/// # Errors
/// Any connection, wire, `epoll` or pty failure. They are all fatal: a
/// terminal without a pty is not a terminal, and the honest failure is
/// to say so rather than to open an empty window.
pub fn run() -> Result<(), Error> {
    // 80×24 before the first measurement, because the child is started
    // before the server has told us how big a cell is — and a child that
    // started at 0×0 would render its first prompt into a terminal with
    // no size. The first `Configure` corrects it.
    let pty = Pty::spawn(80, 24).map_err(|e| io_error(&e))?;
    if !pty.has_job_control() {
        eprintln!(
            "nitro-term: `setsid --ctty` not found; running without job control \
             (Ctrl-C will not reach the foreground program). Install util-linux."
        );
    }
    let mut state = TermApp::new(pty);
    let mut ui = App::new(APP_NAME)?
        .title(APP_NAME)
        .size(Size::new(720.0, 420.0))
        // The window's backdrop has to be the terminal's *own* default
        // background, and this line is what makes that true. The widget
        // paints no rect for a run whose background is the default
        // (`TermGrid::bg_of` answers `None`) precisely because the
        // backdrop is already that colour — which is most of the screen,
        // and the reason ordinary text costs one node per run instead of
        // two. Without this the toolkit's light `#f2f2f2` shows through
        // and the palette's light-on-dark ANSI colours are illegible on
        // it, which is exactly what a screenshot of a bare shell prompt
        // showed on the box.
        .theme(term_theme())
        .build(build)?;
    let grid = grid_of(&ui).ok_or(Error::NoRoot)?;
    install(&mut ui, &mut state, grid)?;
    // The limits and the first size sync ride the first commit: a
    // terminal the user can drag down to four columns is not one the
    // server should have allowed.
    apply_limits(&mut ui, grid)?;
    sync_size(&mut state, &mut ui);
    let socket = nitro_ui::introspect::Socket::bind(APP_NAME).ok();
    nitro_ui::app::event_loop_with(&mut ui, &mut state, socket)
}

/// The toolkit theme a terminal window wants: the default one, with its
/// background replaced by the palette's.
///
/// Only `background` matters — it is what [`Ui`] paints the window
/// backdrop with, and the grid's default-background runs rely on it
/// being their colour. The rest of the theme describes buttons and
/// fields, of which a terminal has none.
#[must_use]
pub fn term_theme() -> nitro_ui::Theme {
    let palette = crate::theme::Palette::default();
    nitro_ui::Theme {
        background: palette.background,
        text: palette.foreground,
        ..nitro_ui::Theme::default()
    }
}

/// Tell the server the smallest useful window: [`MIN_CELLS`] at the
/// measured cell size.
///
/// [`MIN_CELLS`]: crate::widget::MIN_CELLS
///
/// # Errors
/// A wire failure.
pub fn apply_limits(ui: &mut Ui<TermApp>, grid: WidgetId) -> Result<(), Error> {
    let cell = ui
        .widget::<TermGrid>(grid)
        .map(TermGrid::cell_size)
        .unwrap_or_default();
    if cell.w <= 0.0 || cell.h <= 0.0 {
        return Ok(());
    }
    let min = Size::new(
        cell.w * crate::widget::MIN_CELLS.0 as f32,
        cell.h * crate::widget::MIN_CELLS.1 as f32,
    );
    // Zero max: no upper limit. A terminal is happy at any size the
    // screen can hold.
    ui.set_window_limits(min, Size::ZERO)
}

/// Turn a pty failure into the toolkit's error type.
///
/// The toolkit has no variant for "the machine is out of pseudoterminals"
/// and should not grow one for this app, so the `Errno` is carried
/// through the `Io` variant it already has.
fn io_error(e: &std::io::Error) -> Error {
    Error::Io(
        e.raw_os_error()
            .map_or(rustix::io::Errno::IO, rustix::io::Errno::from_raw_os_error),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scrollback_argument_is_read_in_both_spellings() {
        let of = |v: &[&str]| scrollback_from(v.iter().map(|s| (*s).to_owned()));
        assert_eq!(of(&["--scrollback", "500"]), 500);
        assert_eq!(of(&["--scrollback=500"]), 500);
        assert_eq!(of(&[]), DEFAULT_SCROLLBACK);
        // A value that is not a number is the default rather than a
        // panic: a terminal that refuses to start because of a typo in
        // an optional flag is worse than one that ignores it.
        assert_eq!(of(&["--scrollback", "lots"]), DEFAULT_SCROLLBACK);
        assert_eq!(of(&["--scrollback"]), DEFAULT_SCROLLBACK);
        // A flag elsewhere in the line is still found.
        assert_eq!(of(&["--other", "--scrollback", "7"]), 7);
    }
}

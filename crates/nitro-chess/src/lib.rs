//! `nitro-chess` — a chess GUI on `nitro-ui` for the Salewski chess
//! engine.
//!
//! The third front end for one engine. Dr. Stefan Salewski wrote the
//! engine and two GUIs for it, [`tiny-chess`] on `egui` and
//! [`xilem-chess`] on Xilem, and this app is the same program a third
//! time: the same engine file, the same controls, the same
//! click-a-piece-then-a-square play. What differs is the toolkit, which is
//! the point — three GUIs over one engine make the toolkits comparable
//! (the crate's README has the line counts).
//!
//! ```text
//! ┌─────────────────────┬──────────────────────────┐
//! │ status              │ ♜ ♞ ♝ ♛ ♚ ♝ ♞ ♜          │
//! │ White 00:12  Black… │ ♟ ♟ ♟ ♟ ♟ ♟ ♟ ♟          │
//! │ 1.5 s per move      │                          │
//! │ ────●────────────   │         board            │
//! │ ☐ Engine plays white│                          │
//! │ ☑ Engine plays black│ ♙ ♙ ♙ ♙ ♙ ♙ ♙ ♙          │
//! │ [Rotate] [New game] │ ♖ ♘ ♗ ♕ ♔ ♗ ♘ ♖          │
//! │ 1. e2-e4  e7-e5 …   │                          │
//! └─────────────────────┴──────────────────────────┘
//! ```
//!
//! [`engine`] is upstream's file, vendored — see its header and
//! `LICENSE.salewski-chess`. [`board`] is the one custom widget. This
//! file is the rest: a state struct, the tree, and the engine thread.
//!
//! # The engine thread
//!
//! `engine::reply` thinks for seconds, so it runs on a thread, and the
//! [`engine::Game`] is **moved** onto it and moved back with the answer —
//! no `Arc<Mutex<_>>`, and no way for the UI to touch the game while the
//! engine owns it. The thread wakes the loop the way `nitro-files`' scan
//! does (`docs/ui.md`, "Long work off the loop"): the answer goes into a
//! channel, then one byte into a pipe registered with [`Ui::add_fd`].
//!
//! # Scripting
//!
//! Every control has a name, and the board takes `click` and `move`:
//!
//! ```text
//! hey nitro-chess do window/board move e2e4
//! hey nitro-chess get window/board value      # the position, rank 8 first
//! hey nitro-chess get window/status value
//! hey nitro-chess do window/new_game click
//! ```

/// The Salewski chess engine, vendored from `xilem-chess`.
///
/// Upstream's code, not ours, so it is held to upstream's lints rather
/// than the workspace's: the allow-list is the one its own header asks
/// `clippy` to run with, plus the workspace's `pedantic`/`missing_docs`.
/// Keeping it byte-for-byte upstream's (bar one import, see its header)
/// is worth more than a lint-clean copy nobody can diff.
#[allow(
    missing_docs,
    dead_code,
    unreachable_code,
    unused_imports,
    clippy::all,
    clippy::pedantic
)]
#[rustfmt::skip]
pub mod engine;

pub mod board;

use std::fmt::Write as _;
use std::io::{PipeReader, Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use board::{BoardMut as _, View};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::{key, mods};
use nitro_ui::widgets::{
    Checkbox, Label, Slider, button, checkbox, column, label, row, scroll, slider,
};
use nitro_ui::{App, ColorRole, Error, FdToken, Size, TimerId, Ui, WidgetId};

/// The name the app registers under, and so the first argument to `hey`.
pub const APP_NAME: &str = "nitro-chess";

/// Seconds per engine move to start with; `xilem-chess`'s default.
const SECS_PER_MOVE: f32 = 1.5;
/// The slider's range. The engine asserts `0.1 <= secs < 18`.
const SECS_RANGE: (f32, f32) = (0.1, 5.0);
/// Width of the control panel.
const PANEL: f32 = 220.0;
/// Gap between controls; the window's padding is twice it.
const GAP: f32 = 6.0;

/// A game's end, as the engine judged it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The side to move is mated.
    Checkmate,
    /// The side to move has no legal move and is not in check.
    Stalemate,
}

/// The engine at work: the answer's channel and doorbell.
struct Thinking {
    /// The game comes back with the move.
    rx: Receiver<(Box<engine::Game>, engine::Move)>,
    /// The doorbell's read end; one byte per answer.
    wake: PipeReader,
    /// The doorbell's hook in the loop.
    token: FdToken,
    /// [`Chess::generation`] when it was asked; a New Game since makes
    /// the answer stale.
    generation: u64,
    /// Only asked to judge a position with no legal move, not to play.
    verdict: bool,
}

/// The app's state.
pub struct Chess {
    /// The engine's game. `None` while the engine thread has it.
    game: Option<Box<engine::Game>>,
    /// What the board shows. Kept here too, so it can be drawn while
    /// the engine has the game.
    view: View,
    /// The starting position, for a New Game while the engine is busy.
    setup: [i8; 64],
    /// Whether the engine plays white, black.
    engine_plays: [bool; 2],
    /// The engine's time per move.
    secs_per_move: f32,
    /// The moves so far, in the engine's notation.
    moves: Vec<String>,
    /// The status line.
    status: String,
    /// Set when the game has ended.
    outcome: Option<Outcome>,
    /// Time used by white, black, not counting the current turn.
    spent: [Duration; 2],
    /// When the current turn began.
    turn_started: Instant,
    /// The once-a-second clock timer, while armed.
    ticking: Option<TimerId>,
    /// The engine at work, if it is.
    thinking: Option<Thinking>,
    /// Bumped by New Game, to tell a stale answer from a current one.
    generation: u64,
}

impl Chess {
    /// A new game, human (white) against the engine (black).
    #[must_use]
    pub fn new() -> Self {
        let game = Box::new(engine::new_game());
        let setup = engine::get_board(&game);
        Self {
            game: Some(game),
            view: View::new(setup),
            setup,
            engine_plays: [false, true],
            secs_per_move: SECS_PER_MOVE,
            moves: Vec::new(),
            status: String::from("White to move"),
            outcome: None,
            spent: [Duration::ZERO; 2],
            turn_started: Instant::now(),
            ticking: None,
            thinking: None,
            generation: 0,
        }
    }

    /// The same, with the engine playing the sides `engine_plays` says
    /// and thinking `secs_per_move` a move. For tests, which want a
    /// fast engine or none.
    #[must_use]
    pub fn with(engine_plays: [bool; 2], secs_per_move: f32) -> Self {
        Self {
            engine_plays,
            secs_per_move: secs_per_move.clamp(SECS_RANGE.0, SECS_RANGE.1),
            ..Self::new()
        }
    }

    /// The moves so far.
    #[must_use]
    pub fn moves(&self) -> &[String] {
        &self.moves
    }

    /// How the game ended, if it has.
    #[must_use]
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome
    }

    /// The engine thread's doorbell and its hook, while it is thinking:
    /// what a test polls and then hands to [`Ui::run_fd`], in place of
    /// the app loop's `epoll`.
    #[must_use]
    pub fn engine_hook(&self) -> Option<(std::os::fd::BorrowedFd<'_>, FdToken)> {
        self.thinking.as_ref().map(|t| (t.wake.as_fd(), t.token))
    }

    /// Set the status line.
    fn say(&mut self, text: impl Into<String>) {
        self.status = text.into();
    }

    /// `0` when white is to move, `1` for black.
    fn side(&self) -> usize {
        self.moves.len() % 2
    }

    /// The side to move's sign on the engine's board.
    fn sign(&self) -> i8 {
        if self.side() == 0 { 1 } else { -1 }
    }

    /// The clock of `side`, including the turn in progress.
    fn clock(&self, side: usize) -> Duration {
        let running = self.outcome.is_none() && side == self.side() && !self.moves.is_empty();
        let now = if running {
            self.turn_started.elapsed()
        } else {
            Duration::ZERO
        };
        self.spent[side] + now
    }

    /// Book the turn that just ended to the side that played it.
    fn end_turn(&mut self) {
        // White's first move starts the clocks, as over the board.
        if !self.moves.is_empty() {
            let side = self.side();
            self.spent[side] += self.turn_started.elapsed();
        }
        self.turn_started = Instant::now();
    }
}

impl Default for Chess {
    fn default() -> Self {
        Self::new()
    }
}

/// `MM:SS`.
fn mm_ss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// The move list, one numbered line per full move.
fn move_list(moves: &[String]) -> String {
    let mut out = String::new();
    for (n, pair) in moves.chunks(2).enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let black = pair.get(1).map_or("", String::as_str);
        let line = format!("{:>3}. {:<8} {black}", n + 1, pair[0]);
        out.push_str(line.trim_end());
    }
    out
}

/// The widgets the app writes to. `Copy`, so every callback carries it,
/// as `nitro-calc`'s `Screen` is carried.
#[derive(Debug, Clone, Copy)]
struct Ids {
    board: WidgetId,
    status: WidgetId,
    white: WidgetId,
    black: WidgetId,
    speed: WidgetId,
    moves: WidgetId,
}

impl Ids {
    /// Push the state into the tree. Every setter drops an unchanged
    /// value, so this is what a change costs and no more.
    fn refresh(self, s: &Chess, ui: &mut Ui<Chess>) {
        if let Ok(mut b) = ui.widget_mut::<board::Board<Chess>>(self.board) {
            b.set_view(s.view);
        }
        for (id, text) in [
            (self.status, s.status.clone()),
            (self.white, format!("White {}", mm_ss(s.clock(0)))),
            (self.black, format!("Black {}", mm_ss(s.clock(1)))),
            (self.speed, format!("{:.1} s per move", s.secs_per_move)),
            (self.moves, move_list(&s.moves)),
        ] {
            if let Ok(mut l) = ui.widget_mut::<Label>(id) {
                l.set_text(text);
            }
        }
    }

    /// A square was clicked: pick a piece up, put it down, or neither.
    fn clicked(self, s: &mut Chess, ui: &mut Ui<Chess>, sq: u8) {
        if s.outcome.is_some() || s.thinking.is_some() || s.engine_plays[s.side()] {
            return;
        }
        let sign = s.sign();
        let Some(game) = s.game.as_mut() else {
            return;
        };
        let own = engine::get_board(game)[usize::from(sq)].signum() == sign;
        match s.view.selected {
            Some(from) if from == sq => {
                s.view.selected = None;
                s.view.targets = 0;
            }
            Some(from) if !own => {
                s.view.selected = None;
                s.view.targets = 0;
                if engine::move_is_valid2(game, i64::from(from), i64::from(sq)) {
                    self.play(s, ui, from, sq);
                    return;
                }
                s.say("Invalid move");
            }
            _ if own => {
                s.view.selected = Some(sq);
                s.view.targets = engine::tag(game, i64::from(sq))
                    .iter()
                    .fold(0u64, |acc, m| acc | 1 << m.di);
            }
            _ => {}
        }
        self.refresh(s, ui);
    }

    /// Make the move `from`–`to`, already known to be legal.
    fn play(self, s: &mut Chess, ui: &mut Ui<Chess>, from: u8, to: u8) {
        let Some(game) = s.game.as_mut() else {
            return;
        };
        let (a, b) = (from.cast_signed(), to.cast_signed());
        let flag = engine::do_move(game, a, b, false);
        // Upstream pads a pawn move to a piece move's width ("  E2-E4").
        let notation = engine::move_to_str(game, a, b, flag)
            .trim_start()
            .to_owned();
        s.view.squares = engine::get_board(game);
        s.view.last = Some((from, to));
        s.view.selected = None;
        s.view.targets = 0;
        s.status.clone_from(&notation);
        s.end_turn();
        s.moves.push(notation);
        self.next_turn(s, ui);
    }

    /// Whose move is it, and is there one: start the engine if it is
    /// the engine's turn or the game may be over, and the clock if not.
    fn next_turn(self, s: &mut Chess, ui: &mut Ui<Chess>) {
        if s.outcome.is_none() && s.thinking.is_none() {
            let sign = s.sign();
            if let Some(game) = s.game.as_mut() {
                let squares = engine::get_board(game);
                let any = (0..64i64)
                    .filter(|&sq| squares[sq as usize].signum() == sign)
                    .any(|sq| !engine::tag(game, sq).is_empty());
                // No legal move is checkmate or stalemate, and telling
                // them apart is the engine's call: `reply` on such a
                // position says which, and plays nothing.
                if !any || s.engine_plays[s.side()] {
                    self.think(s, ui, !any);
                }
            }
        }
        if s.outcome.is_none() {
            self.arm_clock(s, ui);
        }
        self.refresh(s, ui);
    }

    /// Hand the game to the engine thread.
    fn think(self, s: &mut Chess, ui: &mut Ui<Chess>, verdict: bool) {
        let (wake, mut ring) = match std::io::pipe() {
            Ok(pipe) => pipe,
            Err(e) => return s.say(format!("cannot start the engine: {e}")),
        };
        let Some(mut game) = s.game.take() else {
            return;
        };
        // A verdict needs no search, and `reply` searches until its time
        // is up whether or not there is anything to find.
        game.secs_per_move = if verdict {
            SECS_RANGE.0
        } else {
            s.secs_per_move
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mv = engine::reply(&mut game);
            // Send *then* ring, so a wakeup never arrives ahead of its
            // answer. A closed pipe means the app has gone.
            let _ = tx.send((game, mv));
            let _ = ring.write_all(&[1]);
        });
        let hook = move |s: &mut Chess, ui: &mut Ui<Chess>| self.answered(s, ui);
        match ui.add_fd(wake.as_fd(), hook) {
            Ok(token) => {
                s.thinking = Some(Thinking {
                    rx,
                    wake,
                    token,
                    generation: s.generation,
                    verdict,
                });
            }
            Err(e) => {
                // Only a failed `dup`. The thread has the game, and with
                // no hook its answer would never be read: wait for it.
                if let Ok((game, _)) = rx.recv() {
                    s.game = Some(game);
                }
                s.say(format!("cannot watch the engine: {e}"));
            }
        }
    }

    /// The doorbell rang: take the engine's answer.
    fn answered(self, s: &mut Chess, ui: &mut Ui<Chess>) {
        let Some(t) = s.thinking.as_mut() else {
            return;
        };
        let mut byte = [0u8; 1];
        let _ = t.wake.read(&mut byte);
        let (mut game, mv) = match t.rx.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                // The thread died with the game; start afresh rather
                // than hang.
                let t = s.thinking.take().expect("checked above");
                ui.remove_fd(t.token);
                s.game = Some(Box::new(engine::new_game()));
                s.say("the engine failed");
                new_game(self, s, ui);
                return;
            }
        };
        let t = s.thinking.take().expect("checked above");
        ui.remove_fd(t.token);

        if t.generation != s.generation {
            // New Game was pressed while it thought: the answer is to a
            // question nobody is asking any more.
            engine::reset_game(&mut game);
            s.game = Some(game);
            self.next_turn(s, ui);
            return;
        }
        if t.verdict {
            s.game = Some(game);
            let outcome = if mv.state == engine::STATE_CHECKMATE {
                Outcome::Checkmate
            } else {
                Outcome::Stalemate
            };
            self.finish(s, ui, outcome);
            return;
        }
        let side = s.side();
        s.game = Some(game);
        if !s.engine_plays[side] {
            // The box was unticked while it thought: the move is the
            // human's to make now.
            s.say("Your move");
            self.next_turn(s, ui);
            return;
        }
        let (from, to) = (mv.src as u8, mv.dst as u8);
        self.play(s, ui, from, to);
        let mut status = format!(
            "{} (score {})",
            s.moves.last().map_or("", String::as_str),
            mv.score
        );
        if mv.score.abs() > i64::from(engine::KING_VALUE_DIV_2) {
            // `xilem-chess`'s arithmetic, from the engine's own depth.
            let turns = i64::from(mv.checkmate_in) / 2 + if mv.score > 0 { -1 } else { 1 };
            let _ = write!(status, ", checkmate in {turns}");
        }
        s.status = status;
        self.refresh(s, ui);
    }

    /// The game is over.
    fn finish(self, s: &mut Chess, ui: &mut Ui<Chess>, outcome: Outcome) {
        Self::stop_clock(s, ui);
        s.end_turn();
        s.outcome = Some(outcome);
        s.status = match outcome {
            Outcome::Checkmate if s.side() == 0 => "Checkmate, black wins".to_owned(),
            Outcome::Checkmate => "Checkmate, white wins".to_owned(),
            Outcome::Stalemate => "Stalemate, a draw".to_owned(),
        };
        self.refresh(s, ui);
    }

    /// Tick the clocks once a second while a game is running.
    fn arm_clock(self, s: &mut Chess, ui: &mut Ui<Chess>) {
        if s.ticking.is_some() || s.moves.is_empty() {
            return;
        }
        let timer = ui.set_timer(1000, move |s: &mut Chess, ui: &mut Ui<Chess>| {
            s.ticking = None;
            self.arm_clock(s, ui);
            self.refresh(s, ui);
        });
        s.ticking = Some(timer);
    }

    /// Stop the clocks: a finished game, or a new one, costs nothing
    /// until somebody moves.
    fn stop_clock(s: &mut Chess, ui: &mut Ui<Chess>) {
        if let Some(timer) = s.ticking.take() {
            ui.cancel_timer(&timer);
        }
    }
}

/// Back to the starting position, clocks and move list cleared.
fn new_game(ids: Ids, s: &mut Chess, ui: &mut Ui<Chess>) {
    Ids::stop_clock(s, ui);
    s.generation += 1;
    if let Some(game) = s.game.as_mut() {
        engine::reset_game(game);
    }
    let flipped = s.view.flipped;
    s.view = View::new(s.setup);
    s.view.flipped = flipped;
    s.moves.clear();
    s.outcome = None;
    s.spent = [Duration::ZERO; 2];
    s.turn_started = Instant::now();
    s.say("White to move");
    ids.next_turn(s, ui);
}

/// Build the whole tree and return its root.
///
/// Public because the tests build the tree the binary builds.
///
/// # Panics
/// Never in practice: every `attach` names an id this function has just
/// created.
pub fn build(ui: &mut Ui<Chess>) -> WidgetId {
    let status = ui.build(label("").name("status").width_percent(1.0));
    let white = ui.build(label("").name("white_clock").family("mono"));
    let black = ui.build(label("").name("black_clock").family("mono"));
    let speed = ui.build(label("").name("speed").color_role(ColorRole::TextDim));
    let moves = ui.build(label("").name("moves").family("mono").width_percent(1.0));
    let board = ui.build(
        board::board(View::new([0; 64]))
            .name("board")
            // Elastic both ways: it takes what the window has, and a
            // smaller window is honestly a smaller board (the square
            // side follows), down to `MIN_SQUARE`.
            .grow(1.0)
            .shrink_to_zero()
            .height_percent(1.0)
            .min_width(board::MIN_SQUARE * 8.0)
            .min_height(board::MIN_SQUARE * 8.0),
    );
    let clocks = ui.build(row().gap(GAP * 2.0));
    ui.attach(clocks, white).unwrap();
    ui.attach(clocks, black).unwrap();
    let list = ui.build(scroll().name("move_list").grow(1.0).width_percent(1.0));
    ui.attach(list, moves).unwrap();

    let ids = Ids {
        board,
        status,
        white,
        black,
        speed,
        moves,
    };
    // The board's callback needs the board's own id, so it is set once
    // the board exists.
    //
    // The board is out of its slot while its own callback runs (the
    // take-out dispatch of `docs/ui.md`), so the repaint that callback
    // asks for is deferred until it is back.
    if let Ok(mut b) = ui.widget_mut::<board::Board<Chess>>(board) {
        b.set_on_square(move |s, ui, sq| {
            ids.clicked(s, ui, sq);
            ui.defer(move |s: &mut Chess, ui: &mut Ui<Chess>| ids.refresh(s, ui));
        });
    }
    let controls = controls(ui, ids);

    let panel = ui.build(column().gap(GAP).width(PANEL));
    for id in [status, clocks, speed] {
        ui.attach(panel, id).unwrap();
    }
    for id in controls {
        ui.attach(panel, id).unwrap();
    }
    let (seconds, engine) = (controls[0], [controls[1], controls[2]]);
    ui.attach(panel, list).unwrap();

    let root = ui.build(row().gap(GAP * 2.0).padding(GAP * 2.0));
    ui.attach(root, panel).unwrap();
    ui.attach(root, board).unwrap();

    let min = Size::new(
        PANEL + board::MIN_SQUARE * 8.0 + GAP * 6.0,
        board::MIN_SQUARE * 8.0 + GAP * 4.0,
    );
    let _ = ui.set_window_limits(min, Size::ZERO);
    install_keyboard(ui, ids);

    // The state was built before the tree: show what it says — the
    // controls included, which were built at their defaults — and start
    // the first turn, which may be the engine's. Neither setter runs its
    // callback.
    ui.defer(move |s: &mut Chess, ui: &mut Ui<Chess>| {
        if let Ok(mut sl) = ui.widget_mut::<Slider<Chess>>(seconds) {
            sl.set_value(s.secs_per_move);
        }
        for (&id, on) in engine.iter().zip(s.engine_plays) {
            if let Ok(mut c) = ui.widget_mut::<Checkbox<Chess>>(id) {
                c.set_checked(on);
            }
        }
        ids.next_turn(s, ui);
    });
    root
}

/// The panel's controls, top to bottom: the speed slider, the two
/// engine checkboxes, and the three buttons.
fn controls(ui: &mut Ui<Chess>, ids: Ids) -> [WidgetId; 6] {
    let (lo, hi) = SECS_RANGE;
    let seconds = ui.build(
        slider(SECS_PER_MOVE)
            .range(lo, hi)
            .step(0.1)
            .name("seconds")
            .width_percent(1.0)
            .on_change(move |s: &mut Chess, ui: &mut Ui<Chess>, v| {
                s.secs_per_move = v.clamp(lo, hi);
                ids.refresh(s, ui);
            }),
    );
    let engine = |ui: &mut Ui<Chess>, side: usize, text: &str, name: &str| {
        ui.build(checkbox(text).name(name).on_toggle(
            move |s: &mut Chess, ui: &mut Ui<Chess>, on| {
                s.engine_plays[side] = on;
                ids.next_turn(s, ui);
            },
        ))
    };
    [
        seconds,
        engine(ui, 0, "Engine plays white", "engine_white"),
        engine(ui, 1, "Engine plays black", "engine_black"),
        ui.build(button("Rotate").name("rotate").width_percent(1.0).on_click(
            move |s: &mut Chess, ui: &mut Ui<Chess>| {
                s.view.flipped = !s.view.flipped;
                ids.refresh(s, ui);
            },
        )),
        ui.build(
            button("New game")
                .name("new_game")
                .width_percent(1.0)
                .on_click(move |s: &mut Chess, ui: &mut Ui<Chess>| new_game(ids, s, ui)),
        ),
        ui.build(
            button("Print move list")
                .name("print")
                .width_percent(1.0)
                .on_click(|s: &mut Chess, _ui: &mut Ui<Chess>| {
                    // Upstream's own listing, to the terminal, as in
                    // both of its GUIs.
                    if let Some(game) = &s.game {
                        engine::print_move_list(game);
                    }
                }),
        ),
    ]
}

/// `Ctrl+Q` quits and `Ctrl+N` starts a new game — both modified, so a
/// stray key cannot throw a game away — and `Escape` puts a picked-up
/// piece back.
fn install_keyboard(ui: &mut Ui<Chess>, ids: Ids) {
    ui.set_shortcut(mods::CTRL, key::Q, |_s: &mut Chess, ui: &mut Ui<Chess>| {
        ui.quit();
    });
    ui.set_shortcut(
        mods::CTRL,
        key::N,
        move |s: &mut Chess, ui: &mut Ui<Chess>| {
            new_game(ids, s, ui);
        },
    );
    ui.set_shortcut(
        mods::NONE,
        key::ESC,
        move |s: &mut Chess, ui: &mut Ui<Chess>| {
            s.view.selected = None;
            s.view.targets = 0;
            ids.refresh(s, ui);
        },
    );
}

/// Connect, open the window and run until the app quits.
///
/// # Errors
/// Any connection, wire or `epoll` failure.
pub fn run() -> Result<(), Error> {
    App::new(APP_NAME)?.title("Chess").run(Chess::new(), build)
}

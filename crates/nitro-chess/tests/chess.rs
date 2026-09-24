//! The chess app, driven through a real server.
//!
//! Every test builds the tree the binary builds ([`nitro_chess::build`])
//! and drives it from outside: through the board's `click` and `move`
//! actions — the introspection socket's `do`, which runs the same
//! callback a pointer click runs — and, once, through a real click. The
//! engine thread is real too; the one thing faked is the app loop's
//! `epoll` wakeup for it, which [`answer`] stands in for.

use nitro_chess::board::{Board, parse_square};
use nitro_chess::{Chess, Outcome, build};
use nitro_ui::test::Harness;
use nitro_ui::{Point, Size, introspect};

/// The window's declared minimum, which is what the harness's small
/// output can best hold.
const WINDOW: Size = Size::new(512.0, 280.0);

/// Human against human: nothing thinks unless a test asks it to.
fn two_humans() -> Harness<Chess> {
    Harness::sized("chess", Chess::with([false, false], 0.1), WINDOW, build)
}

/// Human (white) against a fast engine (black).
fn against_engine() -> Harness<Chess> {
    Harness::sized("chess", Chess::with([false, true], 0.1), WINDOW, build)
}

fn act(h: &mut Harness<Chess>, path: &str, action: &str, arg: Option<&str>) {
    let (ui, state) = h.parts();
    introspect::invoke(ui, state, &format!("window/{path}"), action, arg)
        .unwrap_or_else(|e| panic!("{path} {action}: {e}"));
    h.settle();
}

fn get(h: &mut Harness<Chess>, path: &str) -> String {
    // The socket escapes a value's newlines to keep it one line.
    introspect::unescape(
        &introspect::get_prop(h.ui(), &format!("window/{path}"), "value").expect("get"),
    )
}

/// Play `moves`, each like `e2e4`, through the board's `move` action.
fn play(h: &mut Harness<Chess>, moves: &[&str]) {
    for m in moves {
        act(h, "board", "move", Some(m));
    }
}

/// What the app loop's `epoll` would do for the engine thread: run its
/// hook. The hook's read of the doorbell blocks until the engine rings,
/// so this waits for the answer, and it repeats for as long as the app
/// keeps the engine busy (a verdict after a move, the engine playing
/// both sides).
fn answer(h: &mut Harness<Chess>, at_most: usize) {
    for _ in 0..at_most {
        let Some((_, token)) = h.state().engine_hook() else {
            return;
        };
        let (ui, state) = h.parts();
        ui.run_fd(state, token);
        h.settle();
    }
}

fn board_id(h: &mut Harness<Chess>) -> nitro_ui::WidgetId {
    introspect::resolve(h.ui(), "window/board").expect("a board")
}

#[test]
fn the_board_starts_in_the_opening_position() {
    let mut h = two_humans();
    assert_eq!(
        get(&mut h, "board"),
        "r n b q k b n r\n\
         p p p p p p p p\n\
         . . . . . . . .\n\
         . . . . . . . .\n\
         . . . . . . . .\n\
         . . . . . . . .\n\
         P P P P P P P P\n\
         R N B Q K B N R"
    );
    assert_eq!(get(&mut h, "status"), "White to move");
}

#[test]
fn picking_up_a_piece_shows_where_it_can_go() {
    let mut h = two_humans();
    act(&mut h, "board", "click", Some("g1"));
    let id = board_id(&mut h);
    let view = *h.widget::<Board<Chess>>(id).view();
    assert_eq!(view.selected, parse_square("g1"));
    let expect = ["f3", "h3"]
        .iter()
        .fold(0u64, |acc, s| acc | 1 << parse_square(s).unwrap());
    assert_eq!(view.targets, expect);

    // The same square again puts it back.
    act(&mut h, "board", "click", Some("g1"));
    let view = *h.widget::<Board<Chess>>(id).view();
    assert_eq!((view.selected, view.targets), (None, 0));
}

#[test]
fn a_move_moves_and_is_listed() {
    let mut h = two_humans();
    play(&mut h, &["e2e4", "e7e5", "g1f3"]);
    let board = get(&mut h, "board");
    let rows: Vec<&str> = board.lines().collect();
    assert_eq!(rows[3], ". . . . p . . .");
    assert_eq!(rows[4], ". . . . P . . .");
    assert_eq!(rows[5], ". . . . . N . .");
    assert_eq!(h.state().moves().len(), 3);
    let listing = get(&mut h, "moves");
    assert_eq!(listing, "  1. E2-E4    E7-E5\n  2. N_G1-F3", "{listing}");
    let id = board_id(&mut h);
    let last = h.widget::<Board<Chess>>(id).view().last;
    assert_eq!(
        last,
        Some((parse_square("g1").unwrap(), parse_square("f3").unwrap()))
    );
}

#[test]
fn an_illegal_move_is_refused_and_so_is_the_wrong_colour() {
    let mut h = two_humans();
    let before = get(&mut h, "board");
    play(&mut h, &["e2e5"]);
    assert_eq!(get(&mut h, "board"), before);
    assert_eq!(get(&mut h, "status"), "Invalid move");
    // Black's pawn, with white to move, cannot even be picked up.
    act(&mut h, "board", "click", Some("e7"));
    let id = board_id(&mut h);
    assert_eq!(h.widget::<Board<Chess>>(id).view().selected, None);
    assert!(h.state().moves().is_empty());
}

#[test]
fn the_engine_answers_a_move() {
    let mut h = against_engine();
    play(&mut h, &["e2e4"]);
    // It is the engine's turn, so the board ignores the human.
    play(&mut h, &["d2d4"]);
    assert_eq!(h.state().moves().len(), 1);
    answer(&mut h, 4);
    assert_eq!(h.state().moves().len(), 2, "{:?}", h.state().moves());
    assert!(
        get(&mut h, "status").contains("score"),
        "{}",
        get(&mut h, "status")
    );
    // And it is white's turn again.
    play(&mut h, &["d2d4"]);
    assert_eq!(h.state().moves().len(), 3);
}

#[test]
fn the_engine_plays_itself_when_it_has_both_sides() {
    let mut h = two_humans();
    act(&mut h, "engine_white", "toggle", None);
    act(&mut h, "engine_black", "toggle", None);
    answer(&mut h, 4);
    assert!(h.state().moves().len() >= 4, "{:?}", h.state().moves());
    // Handing white back to the human stops it at white's turn.
    act(&mut h, "engine_white", "toggle", None);
    answer(&mut h, 4);
    assert_eq!(h.state().moves().len() % 2, 0);
    assert!(h.state().engine_hook().is_none());
}

#[test]
fn fools_mate_is_checkmate_by_the_engines_verdict() {
    let mut h = two_humans();
    play(&mut h, &["f2f3", "e7e5", "g2g4", "d8h4"]);
    // White has no legal move; whether that is mate is the engine's call.
    answer(&mut h, 2);
    assert_eq!(h.state().outcome(), Some(Outcome::Checkmate));
    assert_eq!(get(&mut h, "status"), "Checkmate, black wins");
    // Nothing moves after the end.
    play(&mut h, &["a2a3"]);
    assert_eq!(h.state().moves().len(), 4);
    // And a finished game is silent: no clock, no engine.
    assert_eq!(h.next_timeout(), None);
    h.assert_idle(150);
}

#[test]
fn a_stalemate_is_a_draw() {
    // The shortest known stalemate (Sam Loyd, ten moves).
    let mut h = two_humans();
    play(
        &mut h,
        &[
            "e2e3", "a7a5", "d1h5", "a8a6", "h5a5", "h7h5", "h2h4", "a6h6", "a5c7", "f7f6", "c7d7",
            "e8f7", "d7b7", "d8d3", "b7b8", "d3h7", "b8c8", "f7g6", "c8e6",
        ],
    );
    assert_eq!(h.state().moves().len(), 19, "{:?}", h.state().moves());
    answer(&mut h, 2);
    assert_eq!(h.state().outcome(), Some(Outcome::Stalemate));
    assert_eq!(get(&mut h, "status"), "Stalemate, a draw");
}

#[test]
fn new_game_while_the_engine_thinks_discards_its_answer() {
    let mut h = against_engine();
    play(&mut h, &["e2e4"]);
    assert!(h.state().engine_hook().is_some());
    act(&mut h, "new_game", "click", None);
    assert!(h.state().moves().is_empty());
    answer(&mut h, 4);
    assert!(h.state().moves().is_empty(), "{:?}", h.state().moves());
    assert!(get(&mut h, "board").ends_with("R N B Q K B N R"));
    // And the new game plays.
    play(&mut h, &["d2d4"]);
    assert_eq!(h.state().moves().len(), 1);
}

#[test]
fn rotate_turns_the_board_and_a_real_click_follows_it() {
    let mut h = two_humans();
    let id = board_id(&mut h);
    let b = h.bounds(id);
    // The top-left square of the board widget's centred 8×8.
    let cell = (b.w.min(b.h) / 8.0).floor();
    let top_left = Point::new(
        b.x + ((b.w - cell * 8.0) / 2.0).floor() + cell / 2.0,
        b.y + ((b.h - cell * 8.0) / 2.0).floor() + cell / 2.0,
    );
    h.click_at(top_left);
    h.settle();
    // a8 holds a black rook, and white is to move: nothing picked up.
    assert_eq!(h.widget::<Board<Chess>>(id).view().selected, None);

    act(&mut h, "rotate", "click", None);
    assert!(h.widget::<Board<Chess>>(id).view().flipped);
    h.click_at(top_left);
    h.settle();
    // Upside down, the top-left square is h1: white's rook.
    assert_eq!(
        h.widget::<Board<Chess>>(id).view().selected,
        parse_square("h1")
    );
}

#[test]
fn the_pieces_are_on_the_screen() {
    let mut h = two_humans();
    if !h.has_text() {
        return;
    }
    let id = board_id(&mut h);
    let b = h.bounds(id);
    let cell = (b.w.min(b.h) / 8.0).floor();
    let left = b.x + ((b.w - cell * 8.0) / 2.0).floor();
    let top = b.y + ((b.h - cell * 8.0) / 2.0).floor();
    // The harness's output is 320×240, so only the board's left columns
    // are on it; a8 and a5 are.
    let shot = h.shot();
    // An empty square is one flat colour; a square with a piece is not.
    let distinct = |col: f32, row: f32| {
        let mut seen = std::collections::HashSet::new();
        for y in 0..cell as u32 {
            for x in 0..cell as u32 {
                let px = (left + col * cell) as u32 + x;
                let py = (top + row * cell) as u32 + y;
                seen.insert(shot.pixel(px, py));
            }
        }
        seen.len()
    };
    assert!(distinct(0.0, 0.0) > 2, "a8's rook left no ink");
    assert_eq!(distinct(0.0, 3.0), 1, "a5 should be an empty square");
}

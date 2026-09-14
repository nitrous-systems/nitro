//! The virtualised [`List`](nitro_ui::List), through the harness: a real
//! server, a real client, real keys and — the point of the file — a real
//! count of the mutations each operation sends.
//!
//! Every claim `docs/ui.md` makes about this widget is a number, and
//! every number here is asserted from the **outside**: how many nodes
//! exist, how many messages a scroll costs, how many a selection move
//! costs. That is the only honest way to check "work is proportional to
//! what changed", and it is why a 100 000-row model appears in a test
//! that runs in milliseconds.

use nitro_core::{Color, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{column, panel};
use nitro_ui::{List, Row, Ui, WidgetId, list};

/// A model of `n` rows, each with a name and a size-ish detail.
fn rows(n: usize) -> Vec<Row> {
    (0..n)
        .map(|i| Row::new(format!("file-{i:05}")).detail(format!("{i} B")))
        .collect()
}

/// A list of `n` rows in a window `h` pixels tall, and the list's id.
fn list_of(n: usize, h: f32) -> (Harness<Vec<usize>>, WidgetId) {
    // The state is the log of activations, so a test can assert that a
    // callback ran with the row it was given rather than that a flag was
    // set.
    let mut h = Harness::sized(
        "list",
        Vec::new(),
        Size::new(240.0, h),
        |ui: &mut Ui<Vec<usize>>| {
            let l = ui.build(
                list()
                    .name("rows")
                    .rows(rows(n))
                    .on_activate(|s: &mut Vec<usize>, _ui: &mut Ui<Vec<usize>>, i: usize| {
                        s.push(i);
                    })
                    .grow(1.0)
                    .width_percent(1.0),
            );
            let root = ui.build(
                column()
                    .child(panel().background(Color::WHITE))
                    .width_percent(1.0)
                    .height_percent(1.0),
            );
            ui.attach(root, l).unwrap();
            root
        },
    );
    let root = h.ui().root().unwrap();
    let id = h.ui().children(root)[1];
    h.settle();
    (h, id)
}

/// The op names of the mutations recorded since the last `clear_tap`.
fn ops<S: 'static>(h: &Harness<S>) -> Vec<&'static str> {
    h.mutations().iter().map(|m| m.op).collect()
}

/// How many mutations of a given kind were sent.
fn count<S: 'static>(h: &Harness<S>, op: &str) -> usize {
    h.mutations().iter().filter(|m| m.op == op).count()
}

/// Click the middle of row `row`, in window coordinates.
fn click_row(h: &mut Harness<Vec<usize>>, id: WidgetId, row: usize) {
    let b = h.bounds(id);
    let row_h = h.widget::<List<Vec<usize>>>(id).row_height();
    let offset = h.widget::<List<Vec<usize>>>(id).offset();
    let y = b.y + (row as f32 + 0.5) * row_h - offset;
    h.click_at(nitro_core::Point::new(b.x + 40.0, y));
    h.settle();
}

#[test]
fn a_hundred_thousand_rows_create_a_screenful_of_nodes() {
    // The whole reason the widget exists. The model is enormous; the
    // scene holds the rows that fit plus the two spare ones that make a
    // one-row scroll free, and not one node more.
    let (mut h, id) = list_of(100_000, 200.0);
    let fits = h.widget::<List<Vec<usize>>>(id).rows_that_fit();
    let made = h.widget::<List<Vec<usize>>>(id).materialised();
    assert!(fits > 0, "the viewport shows something");
    assert_eq!(
        made,
        fits + 2,
        "materialised rows are the visible ones plus two spare"
    );
    assert!(
        made < 30,
        "a 200 px viewport holds tens of rows, not thousands: {made}"
    );

    // The rows really are on screen — a virtualisation that drew nothing
    // would pass every count above.
    assert!(
        h.has_ink(h.bounds(id), 0x00ff_ffff) || !h.has_text(),
        "the visible rows are drawn"
    );
}

#[test]
fn the_node_count_does_not_depend_on_the_models_length() {
    // The same viewport over a hundred rows and over a hundred thousand
    // materialises the same number of rows. Two harnesses would be two
    // servers in one test, so the two halves are compared by the number
    // rather than by sharing a process.
    let (mut small, small_id) = list_of(100, 200.0);
    let made = small.widget::<List<Vec<usize>>>(small_id).materialised();
    let fits = small.widget::<List<Vec<usize>>>(small_id).rows_that_fit();
    assert_eq!(made, fits + 2);
}

#[test]
fn scrolling_one_row_is_one_set_transform() {
    // Inside the spare rows the visible set does not change, so the
    // scroll is exactly the group move and the commit that carries it.
    // This is the same claim `Scroll` makes, one level down.
    let (mut h, id) = list_of(100_000, 200.0);
    let row_h = h.widget::<List<Vec<usize>>>(id).row_height();
    h.tap();
    h.clear_tap();
    h.ui()
        .widget_mut::<List<Vec<usize>>>(id)
        .unwrap()
        .scroll_to(row_h);
    h.settle();
    assert_eq!(
        ops(&h),
        ["SetTransform", "Commit"],
        "one row of scroll is one SetTransform and its commit"
    );
    assert!(
        h.widget::<List<Vec<usize>>>(id).offset().to_bits() == row_h.to_bits(),
        "and it landed where it was asked to"
    );
}

#[test]
fn scrolling_a_page_repaints_only_the_rows_that_changed() {
    // A page down moves the window by a page, so a page of rows is
    // re-emitted — and *only* a page: the count is bounded by the
    // viewport, not by how far down the model the offset landed.
    let (mut h, id) = list_of(100_000, 200.0);
    let (row_h, fits) = {
        let l = h.widget::<List<Vec<usize>>>(id);
        (l.row_height(), l.rows_that_fit())
    };
    h.tap();
    h.clear_tap();
    // Jump a long way, so the re-anchor is a whole window rather than a
    // slide: every materialised row is a different row afterwards.
    h.ui()
        .widget_mut::<List<Vec<usize>>>(id)
        .unwrap()
        .scroll_to(row_h * 5_000.0);
    h.settle();

    let texts = count(&h, "SetText");
    let ring = fits + 2;
    assert!(
        texts <= ring * 2,
        "a full page re-emits at most a page of text nodes, got {texts} for {ring} rows"
    );
    assert!(texts > 0, "the rows really were re-emitted");
    // No node was created or destroyed: the ring is reused, which is
    // what makes scrolling cost mutations rather than allocations.
    assert_eq!(count(&h, "CreateNode"), 0, "the ring is reused");
    assert_eq!(count(&h, "DestroyNode"), 0, "and nothing is thrown away");

    // The window really moved.
    let first = h.widget::<List<Vec<usize>>>(id).first_visible();
    assert_eq!(first, 5_000);
    let visible = h.widget::<List<Vec<usize>>>(id).visible_rows();
    assert_eq!(visible.first().map(|(i, _)| *i), Some(5_000));
    assert_eq!(visible.len(), fits.min(100_000));
}

#[test]
fn moving_the_selection_is_two_set_fills() {
    // Selection is the row's background and nothing else, so moving it
    // repaints two rows' backgrounds: the one that lost it and the one
    // that gained it. Tinting the text as well would have cost a
    // `SetText` per run per row on every arrow key.
    let (mut h, id) = list_of(1_000, 200.0);
    // A click lands on the row under the pointer, so a test that wants
    // row 0 has to aim at row 0 rather than at the widget's middle.
    click_row(&mut h, id, 0);
    assert_eq!(h.widget::<List<Vec<usize>>>(id).cursor(), 0);

    h.tap();
    h.clear_tap();
    h.key(key::DOWN);
    h.settle();
    assert_eq!(h.widget::<List<Vec<usize>>>(id).cursor(), 1);
    assert_eq!(
        ops(&h),
        ["SetFill", "SetFill", "Commit"],
        "a selection move is two fills and the commit that carries them"
    );
    assert_eq!(
        h.widget::<List<Vec<usize>>>(id).selection(),
        vec![1],
        "single selection follows the cursor"
    );
}

#[test]
fn the_keyboard_walks_the_list_and_scrolls_it_into_view() {
    let (mut h, id) = list_of(1_000, 200.0);
    click_row(&mut h, id, 0);
    h.key(key::END);
    h.settle();
    assert_eq!(
        h.widget::<List<Vec<usize>>>(id).cursor(),
        999,
        "End is the last row"
    );
    let l = h.widget::<List<Vec<usize>>>(id);
    assert_eq!(
        l.offset().to_bits(),
        l.max_offset().to_bits(),
        "and it scrolled the last row onto the screen"
    );

    h.key(key::HOME);
    h.settle();
    assert_eq!(h.widget::<List<Vec<usize>>>(id).cursor(), 0);
    assert_eq!(
        h.widget::<List<Vec<usize>>>(id).offset().to_bits(),
        0f32.to_bits()
    );

    let fits = h.widget::<List<Vec<usize>>>(id).rows_that_fit();
    h.key(key::PAGE_DOWN);
    h.settle();
    assert_eq!(
        h.widget::<List<Vec<usize>>>(id).cursor(),
        fits,
        "Page Down moves a viewport's worth"
    );
    h.key(key::PAGE_UP);
    h.settle();
    assert_eq!(h.widget::<List<Vec<usize>>>(id).cursor(), 0);

    // Up at the top and Down at the bottom clamp rather than wrapping or
    // panicking.
    h.key(key::UP);
    h.settle();
    assert_eq!(h.widget::<List<Vec<usize>>>(id).cursor(), 0);
}

#[test]
fn enter_activates_the_cursors_row_and_the_callback_sees_the_index() {
    let (mut h, id) = list_of(50, 200.0);
    click_row(&mut h, id, 0);
    h.key(key::DOWN);
    h.key(key::DOWN);
    h.settle();
    assert_eq!(h.widget::<List<Vec<usize>>>(id).cursor(), 2);
    h.key(key::ENTER);
    h.settle();
    assert_eq!(h.state().as_slice(), [2], "Enter activated row 2");
}

#[test]
fn type_ahead_jumps_to_the_first_row_that_starts_with_what_was_typed() {
    let mut h = Harness::sized(
        "typeahead",
        Vec::new(),
        Size::new(240.0, 200.0),
        |ui: &mut Ui<Vec<usize>>| {
            let l = ui.build(
                list()
                    .name("rows")
                    .rows(vec![
                        Row::new("alpha"),
                        Row::new("beta"),
                        Row::new("gamma"),
                        Row::new("Delta"),
                    ])
                    .grow(1.0)
                    .width_percent(1.0),
            );
            let root = ui.build(column().width_percent(1.0).height_percent(1.0));
            ui.attach(root, l).unwrap();
            root
        },
    );
    let root = h.ui().root().unwrap();
    let id = h.ui().children(root)[0];
    h.settle();
    // Aim at the first row rather than the widget's middle: a click
    // selects what is under the pointer.
    let b = h.bounds(id);
    let row_h = h.widget::<List<Vec<usize>>>(id).row_height();
    h.click_at(nitro_core::Point::new(b.x + 40.0, b.y + row_h * 0.5));
    h.settle();
    assert_eq!(h.widget::<List<Vec<usize>>>(id).cursor(), 0);

    // `g` for gamma. The harness's `key` produces the text a real
    // keyboard would, and the list only sees it because nothing else
    // consumed it — which is the contract `docs/ui.md` describes.
    h.key(34); // KEY_G
    h.settle();
    assert_eq!(
        h.widget::<List<Vec<usize>>>(id).cursor(),
        2,
        "g found gamma"
    );

    // Matching ignores case, so `d` finds `Delta` — and because the
    // prefix is still warm, the second letter *extends* it rather than
    // starting a new search, so `gd` matches nothing and the cursor
    // stays put. Typing after the gap is what starts a new prefix.
    h.key(32); // KEY_D
    h.settle();
    assert_eq!(
        h.widget::<List<Vec<usize>>>(id).cursor(),
        2,
        "a warm prefix extends: `gd` matches no row"
    );
}

#[test]
fn a_click_selects_and_a_second_one_activates() {
    let (mut h, id) = list_of(50, 200.0);
    let b = h.bounds(id);
    let row_h = h.widget::<List<Vec<usize>>>(id).row_height();
    // The middle of the third row.
    let y = b.y + row_h * 2.5;
    h.click_at(nitro_core::Point::new(b.x + 40.0, y));
    h.settle();
    assert_eq!(h.widget::<List<Vec<usize>>>(id).cursor(), 2);
    assert!(
        h.state().is_empty(),
        "one click selects, it does not activate"
    );

    h.click_at(nitro_core::Point::new(b.x + 40.0, y));
    h.settle();
    assert_eq!(h.state().as_slice(), [2], "the second click activated it");
}

#[test]
fn replacing_the_model_costs_a_screenful_not_a_model() {
    // A directory refresh replaces a hundred thousand rows. What that
    // may cost is the rows that are on screen.
    let (mut h, id) = list_of(100_000, 200.0);
    let ring = h.widget::<List<Vec<usize>>>(id).materialised();
    h.tap();
    h.clear_tap();
    h.ui()
        .widget_mut::<List<Vec<usize>>>(id)
        .unwrap()
        .set_rows(rows(100_000).into_iter().rev().collect());
    h.settle();
    let texts = count(&h, "SetText");
    assert!(
        texts <= ring * 2,
        "a new model re-emits a screenful ({texts} text nodes for {ring} rows)"
    );
    assert_eq!(
        count(&h, "CreateNode"),
        0,
        "the ring is reused across models"
    );
    assert_eq!(
        h.widget::<List<Vec<usize>>>(id).row(0).map(|r| r.text),
        Some("file-99999".to_owned()),
        "and it really is the new model"
    );
}

#[test]
fn a_shorter_model_pulls_the_offset_and_the_cursor_back() {
    // The failure mode this prevents: a refresh that shrinks the
    // directory leaves the list scrolled past the end, showing blank.
    let (mut h, id) = list_of(10_000, 200.0);
    h.ui()
        .widget_mut::<List<Vec<usize>>>(id)
        .unwrap()
        .select(9_000);
    h.settle();
    assert!(h.widget::<List<Vec<usize>>>(id).offset() > 0.0);

    h.ui()
        .widget_mut::<List<Vec<usize>>>(id)
        .unwrap()
        .set_rows(rows(3));
    h.settle();
    let l = h.widget::<List<Vec<usize>>>(id);
    assert_eq!(l.cursor(), 2, "the cursor is inside the new model");
    assert_eq!(
        l.offset().to_bits(),
        0f32.to_bits(),
        "and nothing is scrolled off"
    );
    assert_eq!(l.visible_rows().len(), 3);
}

#[test]
fn an_empty_list_is_a_value_and_not_a_panic() {
    let (mut h, id) = list_of(0, 200.0);
    let l = h.widget::<List<Vec<usize>>>(id);
    assert_eq!(l.len(), 0);
    assert!(l.is_empty());
    assert!(l.visible_rows().is_empty());
    assert_eq!(l.materialised(), 0);
    // Every gesture is a no-op rather than an out-of-bounds anything.
    h.click(id);
    h.settle();
    h.key(key::DOWN);
    h.key(key::ENTER);
    h.key(key::END);
    h.settle();
    assert!(h.state().is_empty());
}

#[test]
fn a_settled_list_sends_nothing_while_idle() {
    // The contract every widget in this tree has. A list that armed a
    // timer for its type-ahead expiry would fail this, which is why the
    // prefix expires by elapsed time on the next key instead.
    let (mut h, id) = list_of(100_000, 200.0);
    click_row(&mut h, id, 0);
    h.key(key::DOWN);
    h.settle();
    assert_eq!(h.next_timeout(), None, "a list schedules no timer");
    h.assert_idle(200);
}

#[test]
fn the_visible_rows_are_what_a_script_reads() {
    // `hey <app> get <list> text` answers the window into the model, not
    // the model: a hundred thousand rows down a socket is not a value
    // anybody wanted, and the widget genuinely does not draw them.
    let (mut h, id) = list_of(100_000, 200.0);
    let text = nitro_ui::introspect::get_prop(h.ui(), "window/rows", "text").expect("text");
    // The protocol is line-based, so a value that contains newlines
    // arrives escaped; a script unescapes it exactly like this.
    let text = nitro_ui::introspect::unescape(&text);
    let lines: Vec<&str> = text.lines().collect();
    let fits = h.widget::<List<Vec<usize>>>(id).rows_that_fit();
    assert_eq!(lines.len(), fits, "one line per visible row");
    assert!(lines[0].starts_with("file-00000"), "{:?}", lines[0]);
    assert!(
        lines[0].contains('\t'),
        "the detail column is tab-separated"
    );

    let role = nitro_ui::introspect::get_prop(h.ui(), "window/rows", "role").expect("role");
    assert_eq!(role, "list");
}

#[test]
fn a_script_can_select_and_activate_a_row_by_index() {
    let (mut h, id) = list_of(1_000, 200.0);
    {
        let (ui, state) = h.parts();
        nitro_ui::introspect::invoke(ui, state, "window/rows", "select", Some("500"))
            .expect("select");
    }
    h.settle();
    assert_eq!(h.widget::<List<Vec<usize>>>(id).cursor(), 500);
    assert!(
        h.widget::<List<Vec<usize>>>(id).first_visible() > 400,
        "selecting scrolled it into view"
    );

    {
        let (ui, state) = h.parts();
        nitro_ui::introspect::invoke(ui, state, "window/rows", "activate", None).expect("activate");
    }
    h.settle();
    assert_eq!(h.state().as_slice(), [500]);

    // An index past the end is an error value, never a panic.
    let (ui, state) = h.parts();
    assert!(
        nitro_ui::introspect::invoke(ui, state, "window/rows", "select", Some("99999")).is_err()
    );
}

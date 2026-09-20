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
    let (h, id) = list_of(100_000, 200.0);
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
    let (small, small_id) = list_of(100, 200.0);
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

#[test]
fn a_palette_change_repaints_every_row() {
    // Found on the box, and invisible to every other test in this file.
    //
    // `List`'s per-slot cache answers "unchanged" from the row index,
    // the model generation and the selection — all three of which are
    // still true after a desktop-wide scheme switch, when every colour
    // has moved. So a settled list answered `cx.keep` for every row and
    // went on painting the *old* scheme's text: on screen, a file list
    // in dark-scheme grey on a light window, with `hey get rows text`
    // reporting perfectly correct content. The instrument agreeing with
    // the code because it was measuring the layer below the broken one,
    // for the fourth time in this milestone.
    let (mut h, id) = list_of(1_000, 200.0);
    let visible = h.widget::<List<Vec<usize>>>(id).visible_rows().len();
    assert!(visible > 1, "the list materialises rows to repaint");

    // Settled: the next flush sends nothing at all.
    h.tap();
    h.clear_tap();
    h.settle();
    assert_eq!(ops(&h), Vec::<&str>::new(), "the list really is settled");

    h.ui().set_palette(nitro_core::Palette::dark());
    h.settle();

    // Every materialised row re-emitted its text, not just the ones a
    // selection touched. Two runs per row (label and detail), so the
    // count is a lower bound rather than an equality — what matters is
    // that it scales with the rows on screen instead of being zero.
    let texts = count(&h, "SetText");
    assert!(
        texts >= visible,
        "a scheme switch repaints every materialised row: {texts} SetTexts for {visible} rows"
    );

    // And it is still one commit, because the whole point is that a
    // switch costs one transaction per client.
    assert_eq!(count(&h, "Commit"), 1, "one commit for the whole switch");

    // Idle again afterwards: a widget that marked itself dirty and never
    // cleared the flag would repaint for ever.
    h.clear_tap();
    h.settle();
    assert_eq!(
        ops(&h),
        Vec::<&str>::new(),
        "settled again after the switch"
    );

    // The same palette twice is not a change.
    h.ui().set_palette(nitro_core::Palette::dark());
    h.settle();
    assert_eq!(
        ops(&h),
        Vec::<&str>::new(),
        "an unchanged palette is silence"
    );
}

// ---------------------------------------------------------------------
// Row icons
// ---------------------------------------------------------------------
//
// A row's icon is a **name**, and `SetIcon` is the mutation it costs. So
// every claim about it is a count of that mutation, taken from the
// outside — the same instrument the rest of this file uses for text, and
// for the same reason: "work proportional to change" is a number.
//
// The lesson #3714's review paid for is the one these tests are shaped
// by: a diff must compare against what was **requested**, not against
// what is displayed. A slot that cached the displayed name would re-send
// the row's icon on every repaint the moment a fallback or a capability
// mask put something else on screen.

/// The two names [`icon_rows`] cycles through, named once so the
/// "two cache entries, not one per row" arithmetic below has something to
/// count rather than a repeated literal.
const TWO_ICONS: [&str; 2] = ["file-earmark", "folder-fill"];

/// A model of `n` rows cycling through `names` as their icons.
fn icon_rows_cycling(n: usize, names: &[&str]) -> Vec<Row> {
    (0..n)
        .map(|i| {
            Row::new(format!("file-{i:05}"))
                .icon(names[i % names.len()])
                .detail(format!("{i} B"))
        })
        .collect()
}

/// A model of `n` rows alternating between two icon names.
fn icon_rows(n: usize) -> Vec<Row> {
    icon_rows_cycling(n, &TWO_ICONS)
}

/// A list of `n` icon-carrying rows in a window `h` pixels tall.
fn icon_list_of(n: usize, h: f32) -> (Harness<Vec<usize>>, WidgetId) {
    icon_list_cycling(n, h, &TWO_ICONS)
}

/// As [`icon_list_of`], with the icon names the rows cycle through
/// spelled out: a test that cares how the cycle lines up with the ring
/// picks its own period.
fn icon_list_cycling(
    n: usize,
    h: f32,
    names: &'static [&'static str],
) -> (Harness<Vec<usize>>, WidgetId) {
    let mut h = Harness::sized(
        "list-icons",
        Vec::new(),
        Size::new(240.0, h),
        move |ui: &mut Ui<Vec<usize>>| {
            let l = ui.build(
                list()
                    .name("rows")
                    .rows(icon_rows_cycling(n, names))
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

#[test]
fn a_row_icon_is_a_named_icon_node_and_the_server_rasterises_it() {
    // The baseline the frame itself caches, measured rather than
    // written down: since #3715 a decorated window's title bar carries
    // an application icon and three symbolic button glyphs, so
    // `icons_cached` counts those too. A hard-coded 4 would go stale
    // the day the frame changes, and go stale silently.
    let base = {
        let h = Harness::sized(
            "list-icon-baseline",
            Vec::<usize>::new(),
            Size::new(200.0, 200.0),
            |ui: &mut Ui<Vec<usize>>| ui.build(nitro_ui::widgets::label("no icons here")),
        );
        let n = h.server().stat("icons_cached");
        h.quit();
        n
    };
    let (h, id) = icon_list_of(40, 200.0);
    let made = h.widget::<List<Vec<usize>>>(id).materialised();
    assert!(made > 1, "the list materialised rows");

    // The server really drew artwork for them — the claim a count of
    // client-side mutations cannot make on its own.
    assert!(
        h.server().stat("icon_renders") > base,
        "the server rasterised no icon for the rows at all"
    );
    // Two distinct names, one size: the cache is keyed on
    // `(icon, device px)`, so a screenful of alternating icons is two
    // entries however many rows show them. That is the arithmetic that
    // separates "the rows share artwork" from "each row has its own".
    assert_eq!(
        h.server().stat("icons_cached"),
        base + TWO_ICONS.len() as u64,
        "two names at one size are two cache entries, not one per row"
    );
    assert_eq!(h.server().stat("icon_refusals"), 0, "both names exist");
    h.quit();
}

#[test]
fn a_row_that_gained_an_icon_is_the_same_height() {
    // The layout contract: the icon column is width, never height. A
    // 16 px icon in a row whose text line is ~13 px would otherwise make
    // every list in the tree taller, which is a change to every app.
    let (plain, plain_id) = list_of(40, 200.0);
    let (icons, icons_id) = icon_list_of(40, 200.0);
    let a = plain.widget::<List<Vec<usize>>>(plain_id).row_height();
    let b = icons.widget::<List<Vec<usize>>>(icons_id).row_height();
    assert_eq!(
        a.to_bits(),
        b.to_bits(),
        "an icon column changed the row height: {a} vs {b}"
    );
    // And the same number of rows fit, which is the visible consequence.
    assert_eq!(
        plain.widget::<List<Vec<usize>>>(plain_id).rows_that_fit(),
        icons.widget::<List<Vec<usize>>>(icons_id).rows_that_fit()
    );
    plain.quit();
    icons.quit();
}

#[test]
fn set_rows_with_identical_rows_sends_no_set_icon_and_one_changed_row_sends_one() {
    // The headline, and the review lesson from #3714 made into a number.
    //
    // A `List` diff reuses its ring slots, so a refresh that produced
    // the same listing must cost nothing — and a refresh that changed
    // one row's *type* must cost exactly the one `SetIcon` that row
    // needs. The comparison happens inside the paint slot, against the
    // last `SetIcon` it **requested**, which is what makes both halves
    // true at once.
    let (mut h, id) = icon_list_of(40, 200.0);
    h.tap();
    h.clear_tap();

    // Identical rows. The generation bump invalidates every cached slot,
    // so the widget re-derives and re-emits every row — and the wire
    // layer drops every message whose content did not change.
    h.ui()
        .widget_mut::<List<Vec<usize>>>(id)
        .unwrap()
        .set_rows(icon_rows(40));
    h.settle();
    assert_eq!(
        count(&h, "SetIcon"),
        0,
        "a set_rows over an unchanged listing re-sent icons: {:?}",
        h.mutations()
    );
    // The control that makes the zero mean something: the text did not
    // move either, so this is a genuinely unchanged model rather than an
    // icon path that stopped working.
    assert_eq!(count(&h, "SetText"), 0, "nor any text");

    // One row's icon changed. Row 3 is inside the materialised window.
    let mut changed = icon_rows(40);
    changed[3] = Row::new("file-00003").icon("hdd").detail("3 B");
    h.clear_tap();
    h.ui()
        .widget_mut::<List<Vec<usize>>>(id)
        .unwrap()
        .set_rows(changed);
    h.settle();
    assert_eq!(
        count(&h, "SetIcon"),
        1,
        "one changed row is one SetIcon, got {:?}",
        h.mutations()
    );
    assert_eq!(
        count(&h, "CreateNode"),
        0,
        "and the icon node is reused rather than re-created"
    );
    h.quit();
}

#[test]
fn scrolling_does_not_re_send_an_icon_for_a_row_that_merely_moved() {
    // The recycling question. Slots are addressed `row % ring`, so a
    // scroll that re-anchors the window hands slot *k* a different row —
    // and if that row's icon name is the same, the slot's cached
    // `SetIcon` matches and nothing is sent. That is the case worth
    // pinning: the rows all changed, the icons did not.
    //
    // Three names, and each scroll moves the model by a whole multiple of
    // `3 * ring` rows, so every slot is handed a row exactly three names
    // further along: same name, different row. The alignment is
    // **engineered rather than lucky** — with two names it fell out of
    // the ring happening to be even (10 here), so the expectation was
    // parity-dependent and went vacuous the day a viewport or row height
    // moved the ring to an odd number (#566). Three names cannot line up
    // with a one-ring scroll at all unless `3 | ring`, so if the step
    // below is ever weakened the test fails loudly instead of quietly
    // asserting nothing.
    const NAMES: &[&str] = &["file-earmark", "folder-fill", "hdd"];
    let (mut h, id) = icon_list_cycling(1_000, 200.0, NAMES);
    let row_h = h.widget::<List<Vec<usize>>>(id).row_height();
    let ring = h.widget::<List<Vec<usize>>>(id).materialised();

    // Inside the spare rows first: one `SetTransform`, no row work at
    // all, which is the existing contract and the floor for this one.
    h.tap();
    h.clear_tap();
    h.ui()
        .widget_mut::<List<Vec<usize>>>(id)
        .unwrap()
        .scroll_to(row_h);
    h.settle();
    assert_eq!(
        count(&h, "SetIcon"),
        0,
        "a one-row scroll re-sent an icon: {:?}",
        h.mutations()
    );

    // Now twenty scrolls of `3 * ring` rows each, which re-anchors every
    // time and re-emits every row's text while every slot keeps its icon
    // name. Model is 1 000 rows, so 20 * 3 * ring rows of travel stays
    // well inside it and none of the scrolls clamps.
    let step = NAMES.len() * ring;
    assert!(
        20 * step + ring < 1_000,
        "the scrolls must not clamp: {step}-row steps in a 1 000-row model"
    );
    h.clear_tap();
    for k in 1..=20 {
        h.ui()
            .widget_mut::<List<Vec<usize>>>(id)
            .unwrap()
            .scroll_to(row_h * (k * step) as f32);
        h.settle();
    }
    let icons = count(&h, "SetIcon");
    let texts = count(&h, "SetText");
    assert!(
        texts > 0,
        "the rows really were re-emitted, so the icon count means something"
    );
    // The honest expectation, and it is flat: **0 SetIcons against 400
    // SetTexts** as measured here. Every slot was handed a different row
    // and every row re-derived its paint, so a diff that compared
    // anything other than the last *requested* icon name would show one
    // `SetIcon` per row per scroll instead.
    assert_eq!(
        icons,
        0,
        "scrolling re-sent icons for rows that only moved: {icons} SetIcons \
         against {texts} SetTexts over 20 scrolls of {step} rows each \
         ({ring}-slot ring, {} icon names)",
        NAMES.len()
    );
    h.quit();
}

#[test]
fn without_the_icons_capability_a_row_is_its_label_and_no_icon_node() {
    // The guard, and it is not cosmetic: painting an icon creates a node
    // of `NodeKind::Icon`, which a server predating the icon set rejects
    // as a *decode error* and closes the connection on. So a list that
    // emitted its rows' icons unguarded would not lose a column, it
    // would lose the application. Masked before the first paint, because
    // once the node exists the damage is done.
    let mut h = Harness::sized(
        "list-icons-off",
        Vec::new(),
        Size::new(240.0, 200.0),
        |ui: &mut Ui<Vec<usize>>| {
            let l = ui.build(
                list()
                    .name("rows")
                    .rows(icon_rows(40))
                    .grow(1.0)
                    .width_percent(1.0),
            );
            let root = ui.build(column().width_percent(1.0).height_percent(1.0));
            ui.attach(root, l).unwrap();
            root
        },
    );
    h.ui().hide_icons(true);
    assert!(!h.ui().has_icons());
    h.settle();
    let root = h.ui().root().unwrap();
    let id = h.ui().children(root)[0];

    assert_eq!(
        count(&h, "SetIcon"),
        0,
        "an icon-less server was sent a SetIcon: {:?}",
        h.mutations()
    );
    // The app is alive and the pass succeeded, which is the claim that
    // matters: a missing column, never a broken tree.
    h.ui().flush().expect("the paint pass must not fail");
    assert_eq!(h.server().stat("clients"), 1);
    // The rows are still there and still readable, and the row height did
    // not move either — the label simply starts where the icon would have
    // been, exactly as a leading-icon `Button` collapses to its label.
    assert!(h.widget::<List<Vec<usize>>>(id).materialised() > 1);
    h.quit();
}

#[test]
fn the_capability_going_away_moves_the_labels_and_the_cache_notices() {
    // The failure the `icons` field of `RowPaint` exists for, and it is
    // exactly the shape of the palette bug above: the per-slot cache
    // answers "same row, same generation, same selection" — all three
    // still true — while the labels have to move left by the width of a
    // column that no longer exists. Without the field a settled list
    // would keep the indent and draw nothing in it.
    let (mut h, id) = icon_list_of(40, 200.0);
    h.tap();
    h.clear_tap();
    h.settle();
    assert_eq!(ops(&h), Vec::<&str>::new(), "the list really is settled");

    h.ui().hide_icons(true);
    let visible = h.widget::<List<Vec<usize>>>(id).visible_rows().len();
    h.ui().mark(id, nitro_ui::Dirty::PAINT);
    h.settle();

    // Every materialised row's text box moved, so every row re-emitted.
    assert!(
        count(&h, "SetBounds") >= visible,
        "the labels did not move when the icon column went away: {:?}",
        h.mutations()
    );
    // And the icon nodes are gone rather than left drawing stale artwork.
    assert!(
        count(&h, "DestroyNode") >= visible,
        "the icon nodes outlived the capability: {:?}",
        h.mutations()
    );
    // Settled again: a widget that marked itself and never cleared the
    // flag would repaint for ever.
    h.clear_tap();
    h.settle();
    assert_eq!(ops(&h), Vec::<&str>::new(), "settled again");
    h.quit();
}

#[test]
fn a_row_icon_survives_a_scheme_flip_with_no_set_icon_at_all() {
    // The property the whole by-name design rests on, from the client's
    // side: the node holds a *role index*, so the server re-resolves the
    // colour at paint time. A scheme switch therefore costs the rows'
    // text (which carries its colour in `SetText`) and **nothing** for
    // their icons.
    let (mut h, _id) = icon_list_of(40, 200.0);
    let before = h.server().stat("icon_renders");
    h.tap();
    h.clear_tap();
    h.ui().set_palette(nitro_core::Palette::dark());
    h.settle();
    assert_eq!(
        count(&h, "SetIcon"),
        0,
        "a scheme switch re-sent a row's icon: {:?}",
        h.mutations()
    );
    assert!(count(&h, "SetText") > 0, "the text did change colour");
    assert_eq!(
        h.server().stat("icon_renders"),
        before,
        "and the server re-rasterised an icon it already had as coverage"
    );
    h.quit();
}

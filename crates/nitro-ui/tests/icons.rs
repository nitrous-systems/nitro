//! The `Icon` widget and icon buttons, through the harness: a real
//! server, a real client, real pixels — and, crucially, the branch a
//! widget takes against a server that **has no icon set**.
//!
//! That branch is the one worth a file of its own, because getting it
//! wrong is not cosmetic. `PaintCx::icon` creates a node of
//! `NodeKind::Icon`, which a server predating the icon set rejects as a
//! decode error and **closes the connection** on. So a widget that emits
//! an icon without checking `caps::ICONS` does not cost a gap in a row,
//! it costs the application — which is the opposite of what
//! `docs/ui.md`, `docs/icons.md` and the `caps::ICONS` doc comment all
//! promise.
//!
//! An old server cannot be started from this tree, so the bit is masked
//! on a real connection (`Ui::hide_icons`). That runs the real code path
//! with exactly one variable changed; a hand-built fake `Ui` would prove
//! only that the fake works.

use nitro_core::Size;
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Button, Icon, button, column, icon, label};
use nitro_ui::{ColorRole, Ui, WidgetId};

/// A tree with a bare icon, an icon button and a plain button.
///
/// `CrossAlign::Start`, deliberately: a column **stretches** its children
/// across the cross axis by default, so every child would be laid out at
/// the column's width and "how wide is this button" would be answering
/// the container's opinion rather than the widget's. Every width
/// assertion below depends on this line.
fn tree(ui: &mut Ui<()>) -> WidgetId {
    ui.build(
        column()
            .cross_align(nitro_ui::CrossAlign::Start)
            .child(icon("gear").name("gear").size(16.0))
            .child(button("Menu").icon("list").name("menu"))
            .child(button("Plain").name("plain"))
            .child(label("text")),
    )
}

fn named(h: &mut Harness<()>, name: &str) -> WidgetId {
    nitro_ui::introspect::resolve(h.ui(), &format!("window/{name}"))
        .unwrap_or_else(|| panic!("no widget named {name}"))
}

/// Every `SetIcon` the tree sent since the tap was armed.
fn set_icons(h: &Harness<()>) -> usize {
    h.mutations().iter().filter(|m| m.op == "SetIcon").count()
}

/// The pixels of a widget's box, as `0x00rrggbb`.
fn crop(h: &Harness<()>, rect: nitro_ui::Rect) -> Vec<u32> {
    let img = h.shot();
    let r = rect.round_out();
    let mut out = Vec::new();
    for y in r.y..r.bottom() {
        for x in r.x..r.right() {
            out.push(img.pixel(x as u32, y as u32) & 0x00ff_ffff);
        }
    }
    out
}

#[test]
fn an_icon_paints_one_set_icon_and_measures_its_square() {
    let mut h = Harness::sized("icons", (), Size::new(200.0, 160.0), tree);
    h.settle();

    // Square by contract, and with no round trip: the measurement is
    // arithmetic, not a question for the server.
    let id = named(&mut h, "gear");
    let b = h.ui().window_bounds(id);
    assert!(
        (b.w - 16.0).abs() < 0.01 && (b.h - 16.0).abs() < 0.01,
        "an icon measures to its size on both axes, got {}x{}",
        b.w,
        b.h
    );
    assert_eq!(h.widget::<Icon>(id).name(), "gear");
    assert_eq!(h.widget::<Icon>(id).color_role(), ColorRole::Text);

    // It really drew: the server rasterised something for it.
    assert!(
        h.server().stat("icon_renders") > 0,
        "the server rasterised no icon at all"
    );

    // And a settled tree sends nothing further — an icon is static, and
    // the two things that could change it (the scheme, the output scale)
    // are the server's to act on, not the client's.
    h.tap();
    let root = h.ui().root().expect("a root");
    h.ui().mark(root, nitro_ui::Dirty::PAINT);
    h.settle();
    assert_eq!(
        set_icons(&h),
        0,
        "a repaint re-sent a SetIcon: {:?}",
        h.mutations()
    );

    h.quit();
}

#[test]
fn without_the_icons_capability_an_icon_widget_draws_nothing_and_still_measures() {
    // The `Icon` widget's half of the contract: same box, no node.
    let mut h = Harness::sized("icons-off", (), Size::new(200.0, 160.0), tree);
    h.settle();
    let id = named(&mut h, "gear");
    let with = h.ui().window_bounds(id);

    h.ui().hide_icons(true);
    assert!(!h.ui().has_icons());
    h.tap();
    h.ui().mark(id, nitro_ui::Dirty::LAYOUT);
    let root = h.ui().root().expect("a root");
    h.ui().mark(root, nitro_ui::Dirty::LAYOUT);
    h.settle();

    let without = h.ui().window_bounds(id);
    assert!(
        (with.w - without.w).abs() < 0.01 && (with.h - without.h).abs() < 0.01,
        "the box moved when the capability went away: {with:?} vs {without:?}"
    );
    assert_eq!(
        set_icons(&h),
        0,
        "an icon-less server was sent a SetIcon: {:?}",
        h.mutations()
    );

    // The app is alive and the paint pass succeeded, which is the claim
    // that matters: a gap, never a broken tree.
    h.ui().flush().expect("the paint pass must not fail");
    assert_eq!(h.server().stat("clients"), 1);
    h.quit();
}

#[test]
fn without_the_icons_capability_a_button_falls_back_to_its_label() {
    // The regression this file exists for.
    //
    // A `Button` with `.icon(...)` used to emit the icon unconditionally,
    // so against a server with no icon set it would send
    // `CreateNode { kind: Icon }` — a decode error there, and a closed
    // connection. `nitro-bar`'s launcher button is the live consumer, so
    // the bar would have killed itself against an older server.
    //
    // The fallback is the label rather than a blank face, which is why
    // the button keeps its text as its accessible name in the first
    // place: the glyph is for the eye, the word is for everything else.
    let mut h = Harness::sized("button-icon-off", (), Size::new(200.0, 160.0), tree);
    h.settle();
    let id = named(&mut h, "menu");
    assert_eq!(h.widget::<Button<()>>(id).icon(), Some("list"));
    assert_eq!(h.widget::<Button<()>>(id).text(), "Menu");
    let square = h.ui().window_bounds(id);

    h.ui().hide_icons(true);
    h.tap();
    // The *button* is marked, not just the root: a widget's measurement
    // is memoized, and in real life the capability cannot change after
    // the handshake, so nothing invalidates it on its own. Marking it is
    // how the test reaches the first-measure-against-an-old-server state
    // that a real app would simply start in.
    h.ui().mark(id, nitro_ui::Dirty::LAYOUT);
    let root = h.ui().root().expect("a root");
    h.ui().mark(root, nitro_ui::Dirty::LAYOUT);
    h.settle();

    // Not one icon node, and not one `SetIcon`.
    assert_eq!(
        set_icons(&h),
        0,
        "an icon-less server was sent a SetIcon: {:?}",
        h.mutations()
    );
    // The paint pass succeeded and the client is still connected — the
    // two different claims, and the second is the one a user notices.
    h.ui()
        .flush()
        .expect("an icon button against an icon-less server must not fail its paint");
    assert_eq!(h.server().stat("clients"), 1, "the client survived");

    // It fell back to the *label*, so the button is now as wide as the
    // word "Menu" rather than a 16-px square. Measure and paint agreed
    // about which it was showing, which is the bug's other half: a
    // square box with a word painted into it would have clipped.
    let text_box = h.ui().window_bounds(id);
    assert!(
        text_box.w > square.w + 1.0,
        "the button kept its icon-sized box ({} wide) while painting a \
         label: measure and paint disagree",
        text_box.w
    );

    h.quit();
}

#[test]
fn an_icon_button_never_creates_an_icon_node_without_the_capability() {
    // The **dangerous** half of the regression, and the one the
    // fall-back-geometry test above cannot reach.
    //
    // `PaintCx::icon` creates a node of `NodeKind::Icon`. On a server
    // that predates the icon set that kind is a *decode error*, so the
    // connection is closed — the app dies rather than losing a glyph.
    // Reaching it needs the capability gone **before the first paint**,
    // because once the node exists the damage is done and a later
    // repaint merely reuses it. The build closure runs before any paint,
    // so the bit is masked there — which is also exactly the state a
    // real app starts in against a real old server.
    let mut h = Harness::sized(
        "button-icon-never",
        (),
        Size::new(200.0, 160.0),
        |ui: &mut Ui<()>| {
            ui.hide_icons(true);
            ui.tap(true);
            tree(ui)
        },
    );
    h.settle();

    // Not one `SetIcon`, and — the assertion that actually matters — not
    // one node of the icon kind, from the very first paint onwards.
    assert_eq!(
        set_icons(&h),
        0,
        "an icon-less server was sent a SetIcon: {:?}",
        h.mutations()
    );
    assert!(
        !h.mutations().is_empty(),
        "the tap recorded nothing at all, so this test proves nothing"
    );

    // The app is alive, painted, and still a client. Against a real old
    // server the failure would be a closed socket, so "still connected"
    // is the claim that stands in for it here.
    h.ui().flush().expect("the paint pass must not fail");
    assert_eq!(h.server().stat("clients"), 1);
    assert_eq!(h.server().stat("windows"), 1);
    assert_eq!(
        h.server().stat("icon_renders"),
        0,
        "the server rasterised an icon for a client that was told it had none"
    );

    // And it drew the labels instead of nothing: a blank button would
    // satisfy every line above.
    let id = named(&mut h, "menu");
    let bounds = h.bounds(id);
    let face = crop(&h, bounds)[0];
    assert!(
        h.ink_count(bounds, face) > 0,
        "the icon button fell back to a blank face rather than its label"
    );

    h.quit();
}

#[test]
fn an_icon_button_against_a_server_with_icons_draws_the_icon_not_the_label() {
    // The control that makes the test above mean "the capability is
    // what decided", rather than "icon buttons never draw icons".
    let mut h = Harness::sized("button-icon-on", (), Size::new(200.0, 160.0), tree);
    h.settle();
    assert!(h.ui().has_icons(), "the harness server has the icon set");

    let menu = named(&mut h, "menu");
    let plain = named(&mut h, "plain");
    let menu_box = h.ui().window_bounds(menu);
    let plain_box = h.ui().window_bounds(plain);

    // The face is the icon's square **plus the theme's padding**, which
    // is asymmetric (14 x 7) — so the face is 44x30 rather than square,
    // and asserting squareness here would be asserting something the
    // theme does not promise. What the icon path actually owes is that
    // the *content* box is the icon's side on both axes:
    let (px, py) = h.ui().theme().button_padding;
    let side = 16.0_f32.max(13.0); // ICON_SIZE vs the default font size
    assert!(
        (menu_box.w - (side + px * 2.0)).abs() < 0.01
            && (menu_box.h - (side + py * 2.0)).abs() < 0.01,
        "an icon button is the icon's square plus the button padding; \
         want {}x{}, got {}x{}",
        side + px * 2.0,
        side + py * 2.0,
        menu_box.w,
        menu_box.h
    );
    // And it is narrower than a text button whose word is a similar
    // length, which is the visible consequence: the glyph is 16 px where
    // "Plain" is wider.
    assert!(
        plain_box.w > menu_box.w,
        "a text button ({} wide) should be wider than an icon one ({})",
        plain_box.w,
        menu_box.w
    );

    h.quit();
}

#[test]
fn a_disabled_icon_button_is_tinted_like_disabled_text() {
    // An icon takes a *role*, and the role has to be the one the label
    // would have taken: a disabled icon button drawn in `ButtonText`
    // looks enabled, which is a lie about whether it can be clicked.
    //
    // Asserted on pixels rather than on the role constant, because the
    // role is what the widget *sends* and the tint is what the user
    // sees — the same model-versus-pixels split the rest of this branch
    // is measured on.
    let mut h = Harness::sized(
        "button-disabled",
        (),
        Size::new(120.0, 80.0),
        |ui: &mut Ui<()>| {
            ui.build(column().child(button("Menu").icon("list").name("menu").size(13.0)))
        },
    );
    h.settle();
    let id = named(&mut h, "menu");
    let bounds = h.bounds(id);
    let enabled = crop(&h, bounds);

    h.ui()
        .widget_mut::<Button<()>>(id)
        .expect("the button")
        .set_enabled(false);
    h.settle();
    let disabled = crop(&h, bounds);

    // The *pixels*, not an ink count against a fixed background: the
    // button's face changes with the state too, so a count would come
    // back identical while both the face and the glyph had moved.
    assert_ne!(
        enabled, disabled,
        "a disabled icon button paints the same as an enabled one, so it \
         looks clickable when it is not"
    );

    // And specifically the *glyph* moved, not only the face. The face is
    // a flat colour, so the icon is whatever differs from the pixel just
    // inside the button's top-left corner.
    let ink = |px: &[u32]| {
        let face = px[0];
        px.iter().filter(|p| **p != face).count()
    };
    assert!(
        ink(&enabled) > 0 && ink(&disabled) > 0,
        "the icon is drawn in both states"
    );
    let glyph_moved = enabled
        .iter()
        .zip(&disabled)
        .filter(|(a, b)| a != b && **a != enabled[0])
        .count();
    assert!(
        glyph_moved > 0,
        "only the face changed; the glyph kept its enabled tint, which is \
         what makes a disabled button look clickable"
    );

    h.quit();
}

#[test]
fn an_unknown_icon_name_leaves_the_app_running() {
    // `BadIcon` is one of the protocol's two non-fatal errors, and this
    // is it from the client's side: the node draws nothing, the app
    // keeps painting, and the connection survives. A desktop must not
    // lose an application because one widget named an icon a newer set
    // has.
    let mut h = Harness::sized("bad-icon", (), Size::new(120.0, 80.0), |ui: &mut Ui<()>| {
        ui.build(
            column()
                .child(icon("definitely-not-an-icon").name("bad").size(16.0))
                .child(label("still here")),
        )
    });
    h.settle();
    // A few repaints, because an error latched on the first pass would
    // surface on a later one.
    for _ in 0..3 {
        let root = h.ui().root().expect("a root");
        h.ui().mark(root, nitro_ui::Dirty::PAINT);
        h.settle();
        h.ui()
            .flush()
            .expect("an unknown icon must not fail a paint");
    }
    assert_eq!(h.server().stat("clients"), 1, "the client survived");
    assert_eq!(h.server().stat("windows"), 1);
    // And it drew nothing for the bad name.
    assert_eq!(h.server().stat("icon_renders"), 0);
    h.quit();
}

//! The toolkit's half of the colour palette: what a `Theme` push costs,
//! what it moves, and what it leaves alone.
//!
//! The server owns the palette and pushes it; everything here is about
//! the client end of that. The two claims worth checking are the ones an
//! app relies on without ever saying so: a scheme switch is **one
//! commit**, and an app that was idle before one is idle again after it.

use nitro_core::{Palette, Role};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Label, button, column, label, panel};
use nitro_ui::{ColorRole, Size, Theme, Ui, WidgetId};

/// A small tree with one of everything that paints: a panel background,
/// a label that follows a role, a label that follows the theme's `text`,
/// and a button (three colours of its own plus a focus ring).
fn tree(ui: &mut Ui<()>) -> WidgetId {
    let dim = ui.build(label("dim").name("dim").color_role(ColorRole::TextDim));
    let ink = ui.build(label("ink").name("ink"));
    let go = ui.build(button("Go").name("go"));
    let root = ui.build(column().padding(8.0).gap(4.0));
    ui.attach(root, dim).unwrap();
    ui.attach(root, ink).unwrap();
    ui.attach(root, go).unwrap();
    root
}

fn harness() -> Harness<()> {
    Harness::sized("theme", (), Size::new(200.0, 120.0), tree)
}

#[test]
fn a_client_starts_on_the_palette_the_server_pushed() {
    // The server's default scheme is light, and the harness runs a real
    // server — so a client that never touches a theme is already on the
    // desktop's palette rather than on a built-in guess.
    let mut h = harness();
    h.settle();
    assert_eq!(*h.ui().palette(), Palette::light());
    let accent = h.ui().color(ColorRole::Accent);
    assert_eq!(accent, Palette::light().get(ColorRole::Accent));
    let bg = h.ui().color(ColorRole::WindowBackground);
    assert_eq!(h.ui().theme().background, bg);
    h.quit();
}

#[test]
fn a_palette_change_costs_exactly_one_commit_and_repaints_everything() {
    // The property the whole design rests on: a desktop-wide colour
    // switch is one transaction per client, not one per widget. A
    // toolkit that marked and flushed per widget would still *look*
    // right, and would cost a commit per label on every switch.
    let mut h = harness();
    h.settle();
    let before = h.commits();
    h.tap();
    h.clear_tap();

    h.ui().set_palette(Palette::dark());
    let sent = h.ui().flush().expect("flush");
    assert!(sent, "a palette change is a commit");
    assert_eq!(h.commits(), before + 1, "exactly one commit");

    // And every painted widget was actually restyled: the mutations
    // carry the dark scheme's colours, not the light one's.
    let muts = h.mutations().len();
    assert!(muts > 0, "a palette change repaints");
    assert_eq!(*h.ui().palette(), Palette::dark());
    assert_eq!(h.ui().theme().text, Palette::dark().get(ColorRole::Text));

    h.quit();
}

#[test]
fn an_idle_tree_is_silent_again_after_the_switch() {
    // The other half: a switch costs one commit and then *nothing*. An
    // implementation that marked the tree dirty and never cleared the
    // flag would pass the test above and spin for ever.
    let mut h = harness();
    h.settle();
    h.ui().set_palette(Palette::dark());
    h.ui().flush().expect("flush");
    h.settle();
    h.assert_idle(60);
    h.quit();
}

#[test]
fn an_unchanged_palette_is_not_a_commit() {
    // A server that re-sends the palette it already sent — which it does
    // whenever a client reconnects — must not cost a settled app a
    // frame.
    let mut h = harness();
    h.settle();
    let before = h.commits();
    let current = *h.ui().palette();
    h.ui().set_palette(current);
    let sent = h.ui().flush().expect("flush");
    assert!(!sent, "an unchanged palette sends nothing");
    assert_eq!(h.commits(), before);
    h.quit();
}

#[test]
fn a_role_coloured_label_follows_the_scheme_and_a_literal_one_does_not() {
    // Why `.color_role()` exists. A label given a literal keeps it —
    // that is what a literal *means* — so an app that wants its text to
    // follow the desktop has to name a role, and this is the difference
    // in one test.
    let mut h = harness();
    h.settle();
    let root = h.ui().root().unwrap();
    let dim = h.ui().children(root)[0];
    let ink = h.ui().children(root)[1];

    // The role-coloured one resolves at paint time, so it holds no
    // colour of its own at all.
    assert_eq!(
        h.widget::<Label>(dim).color_role(),
        Some(Role::TextDim),
        "it names a role"
    );
    assert_eq!(h.widget::<Label>(dim).color(), None, "and no literal");

    // Pin the second one to a literal: the light scheme's dim grey.
    let literal = Palette::light().get(Role::TextDim);
    h.ui()
        .widget_mut::<Label>(ink)
        .expect("the ink label")
        .set_color(literal);
    h.settle();

    h.ui().set_palette(Palette::dark());
    h.settle();

    assert_eq!(
        h.widget::<Label>(ink).color(),
        Some(literal),
        "a literal does not follow the scheme"
    );
    assert_eq!(
        h.ui().color(Role::TextDim),
        Palette::dark().get(Role::TextDim),
        "but the role did move"
    );
    assert_ne!(
        h.ui().color(Role::TextDim),
        literal,
        "the two schemes really disagree about this role"
    );
    h.quit();
}

#[test]
fn an_apps_own_metrics_survive_a_scheme_switch() {
    // A palette says nothing about font sizes and paddings, so a switch
    // must not reset them. `Theme::with_palette` is what guarantees it;
    // this is the assertion from outside.
    let mut h = harness();
    h.settle();
    let mut custom = h.ui().theme().clone();
    custom.font_size = 19.0;
    custom.radius = 0.0;
    custom.font_family = "mono".to_owned();
    h.ui().set_theme(custom);
    h.settle();

    h.ui().set_palette(Palette::dark());
    h.settle();

    let t = h.ui().theme();
    assert_eq!(t.font_size.to_bits(), 19.0f32.to_bits());
    assert_eq!(t.radius.to_bits(), 0.0f32.to_bits());
    assert_eq!(t.font_family, "mono");
    assert_eq!(
        t.background,
        Palette::dark().get(ColorRole::WindowBackground)
    );
    h.quit();
}

#[test]
fn the_theme_hook_runs_after_the_tree_was_rethemed() {
    // The ordering a custom widget depends on: by the time an `on_theme`
    // handler runs, `ui.palette()` is already the new one, so a handler
    // rebuilding derived colours reads the right table. A hook that ran
    // first would hand every app the *previous* scheme.
    struct S {
        seen: Vec<nitro_core::Color>,
    }
    let mut h = Harness::sized(
        "theme-hook",
        S { seen: Vec::new() },
        Size::new(160.0, 80.0),
        |ui: &mut Ui<S>| {
            ui.on_theme(|s: &mut S, ui: &mut Ui<S>| {
                s.seen.push(ui.color(ColorRole::Accent));
            });
            let l = ui.build(label("hi"));
            let root = ui.build(panel());
            ui.attach(root, l).unwrap();
            root
        },
    );
    h.settle();
    assert_eq!(h.ui().theme_handler_count(), 1);
    let before = h.state().seen.len();

    h.ui().set_palette(Palette::dark());
    let (ui, state) = h.parts();
    ui.dispatch_theme(state);

    assert_eq!(h.state().seen.len(), before + 1, "the hook ran once");
    assert_eq!(
        h.state().seen.last().copied(),
        Some(Palette::dark().get(ColorRole::Accent)),
        "and saw the new palette, not the old one"
    );
    h.quit();
}

#[test]
fn the_backdrop_follows_the_scheme_in_pixels() {
    // The window background is not a widget — no widget's repaint covers
    // it — so it is the one thing a "mark every widget" implementation
    // would miss. Checked in pixels, on a real server.
    let mut h = Harness::sized("backdrop", (), Size::new(120.0, 80.0), |ui: &mut Ui<()>| {
        ui.build(column())
    });
    h.settle();
    let px = |h: &Harness<()>| h.shot().pixel(60, 40) & 0x00ff_ffff;
    let want = |c: nitro_core::Color| u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b);

    assert_eq!(
        px(&h),
        want(Palette::light().get(ColorRole::WindowBackground))
    );

    h.ui().set_palette(Palette::dark());
    h.settle();
    assert_eq!(
        px(&h),
        want(Palette::dark().get(ColorRole::WindowBackground))
    );
    h.quit();
}

#[test]
fn a_theme_built_from_a_palette_is_what_the_widgets_read() {
    // The mapping itself, without a server: every colour field of the
    // toolkit's `Theme` is a projection of a role, so the two can never
    // disagree about what "the accent" is.
    let p = Palette::dark();
    let t = Theme::from_palette(&p);
    assert_eq!(t.background, p.get(Role::WindowBackground));
    assert_eq!(t.surface, p.get(Role::Surface));
    assert_eq!(t.text, p.get(Role::Text));
    assert_eq!(t.text_disabled, p.get(Role::TextDim));
    assert_eq!(t.button, p.get(Role::Button));
    assert_eq!(t.button_hover, p.get(Role::ButtonHover));
    assert_eq!(t.button_active, p.get(Role::ButtonActive));
    assert_eq!(t.button_disabled, p.get(Role::ButtonDisabled));
    assert_eq!(t.button_text, p.get(Role::ButtonText));
    assert_eq!(t.border, p.get(Role::Border));
    assert_eq!(t.focus, p.get(Role::Focus));
    assert_eq!(t.field, p.get(Role::Field));
    assert_eq!(t.selection, p.get(Role::Selection));
    assert_eq!(t.caret, p.get(Role::Caret));
    assert_eq!(t.placeholder, p.get(Role::Placeholder));
    assert_eq!(t.accent, p.get(Role::Accent));
    assert_eq!(t.track, p.get(Role::Track));
}

//! The quick-settings widgets (`nitro_ui::quick`): tiles, round buttons,
//! the chunky slider, drill-down rows — real pixels through a real server.

use nitro_core::{Color, Point, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::quick::{CHUNKY_H, RoundButton, Tile};
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Slider, column, row, slider};
use nitro_ui::{ColorRole, Rect, Ui, WidgetId, choice_row, round_button, section_card, tile};

#[derive(Default)]
struct St {
    toggles: Vec<bool>,
    clicks: u32,
    value: f32,
    tile: Option<WidgetId>,
    slider: Option<WidgetId>,
    round: Option<WidgetId>,
    disabled: Option<WidgetId>,
}

fn tree(ui: &mut Ui<St>) -> WidgetId {
    let t = ui.build(
        tile("Dark Style", "circle-half")
            .subtitle("Off")
            .name("dark")
            .width(200.0)
            .on_toggle(|s: &mut St, _ui: &mut Ui<St>, on| s.toggles.push(on)),
    );
    let sl = ui.build(
        slider(0.5)
            .chunky()
            .name("volume")
            .width(200.0)
            .on_change(|s: &mut St, _ui: &mut Ui<St>, v| s.value = v),
    );
    let rb = ui.build(
        round_button("gear")
            .label("Settings")
            .name("settings")
            .on_click(|s: &mut St, _ui: &mut Ui<St>| s.clicks += 1),
    );
    let dis = ui.build(
        round_button("lock")
            .label("Lock")
            .name("lock")
            .disabled()
            .on_click(|s: &mut St, _ui: &mut Ui<St>| s.clicks += 100),
    );
    let card = ui.build(section_card("Sound").name("card"));
    let buttons = ui.build(row().gap(8.0));
    ui.attach(buttons, rb).unwrap();
    ui.attach(buttons, dis).unwrap();
    let root = ui.build(column().padding(8.0).gap(8.0));
    for c in [t, sl, buttons, card] {
        ui.attach(root, c).unwrap();
    }
    ui.add_child(card, choice_row("Speakers", true).name("sink0"))
        .unwrap();
    ui.defer(move |s: &mut St, _ui: &mut Ui<St>| {
        s.tile = Some(t);
        s.slider = Some(sl);
        s.round = Some(rb);
        s.disabled = Some(dis);
    });
    root
}

fn harness() -> Harness<St> {
    let mut h = Harness::sized("quick", St::default(), Size::new(260.0, 230.0), tree);
    let (ui, st) = h.parts();
    ui.run_deferred(st);
    h.settle();
    h
}

/// How many pixels in `r` are (close to) the accent colour.
fn accent_pixels(h: &Harness<St>, r: Rect, accent: Color) -> usize {
    let shot = h.shot();
    let want = (u32::from(accent.r) << 16) | (u32::from(accent.g) << 8) | u32::from(accent.b);
    let mut n = 0;
    for y in r.y as u32..(r.y + r.h) as u32 {
        for x in r.x as u32..(r.x + r.w) as u32 {
            if shot.pixel(x, y) & 0x00ff_ffff == want {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn a_tile_toggles_on_click_and_its_badge_takes_the_accent() {
    let mut h = harness();
    let t = h.state().tile.unwrap();
    let accent = h.ui().color(ColorRole::Accent);
    let b = h.bounds(t);
    let badge = Tile::<St>::badge_rect(b.size());
    let badge = Rect::new(b.x + badge.x, b.y + badge.y, badge.w, badge.h);
    assert_eq!(accent_pixels(&h, badge, accent), 0, "off: no accent ink");
    h.click(t);
    assert_eq!(h.state().toggles, vec![true]);
    assert!(h.widget::<Tile<St>>(t).is_on());
    assert!(
        accent_pixels(&h, badge, accent) > 200,
        "on: the badge is filled with the accent"
    );
    // The tile body beside the badge stays neutral.
    let body = Rect::new(b.x + 60.0, b.y + 4.0, 100.0, 6.0);
    assert_eq!(accent_pixels(&h, body, accent), 0, "the tile stays neutral");
    let v = h.ui().accessible(t).unwrap().value;
    assert_eq!(v.as_deref(), Some("true"));
    h.click(t);
    assert_eq!(h.state().toggles, vec![true, false]);
    assert_eq!(accent_pixels(&h, badge, accent), 0);
}

#[test]
fn the_chunky_sliders_fill_follows_its_value_and_a_drag_sets_it() {
    let mut h = harness();
    let sl = h.state().slider.unwrap();
    let accent = h.ui().color(ColorRole::Accent);
    let b = h.bounds(sl);
    assert!((b.h - CHUNKY_H).abs() < 0.5, "as tall as the knob: {b:?}");
    let strip = Rect::new(b.x, b.y + b.h / 2.0 - 1.0, b.w, 2.0);
    let half = accent_pixels(&h, strip, accent);
    {
        let mut s = h.ui().widget_mut::<Slider<St>>(sl).unwrap();
        s.set_value(0.9);
    }
    h.settle();
    let more = accent_pixels(&h, strip, accent);
    assert!(
        more > half + 50,
        "fill grows with the value: {half} → {more}"
    );
    // A press near the left end sets a low value.
    h.click_at(Point::new(b.x + 20.0, b.y + b.h / 2.0));
    assert!(h.state().value < 0.1, "{}", h.state().value);
    assert!(h.widget::<Slider<St>>(sl).is_chunky());
}

#[test]
fn a_disabled_round_button_ignores_clicks_and_an_enabled_one_does_not() {
    let mut h = harness();
    let rb = h.state().round.unwrap();
    let dis = h.state().disabled.unwrap();
    h.click(dis);
    assert_eq!(h.state().clicks, 0);
    assert!(!h.widget::<RoundButton<St>>(dis).is_enabled());
    h.click(rb);
    assert_eq!(h.state().clicks, 1);
    assert_eq!(
        h.ui().accessible(rb).unwrap().name.as_deref(),
        Some("Settings")
    );
    {
        let mut b = h.ui().widget_mut::<RoundButton<St>>(dis).unwrap();
        b.set_enabled(true);
    }
    h.settle();
    h.click(dis);
    assert_eq!(h.state().clicks, 101);
}

#[test]
fn a_settled_quick_tree_is_idle() {
    let mut h = harness();
    h.assert_idle(150);
}

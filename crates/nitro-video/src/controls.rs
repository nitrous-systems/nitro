//! The controls overlay: a bar of nitro-ui widgets over the video.
//!
//! Play/pause, a seek slider, the time, a repeat toggle and a
//! fullscreen button, in one row pinned to the bottom of the
//! [`SurfaceView`](nitro_ui::SurfaceView). The widgets are addressed by
//! name (`play`, `seek`, `time`, `repeat`, `fullscreen`, `controls`,
//! `video`), which is also what `hey` sees. The repeat button shows its
//! state in its label (`Repeat on` / `Repeat off`) and, when on, in the
//! accent colour.
//! The bar repaints at most once a second (the time and the slider);
//! video frames never touch the tree.

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::introspect;
use nitro_ui::layout::CrossAlign;
use nitro_ui::surface::{OverlayAlign, SurfacePointer, surface_view};
use nitro_ui::widgets::{button, label, panel, row, slider};
use nitro_ui::{ColorRole, Ui, WidgetId};

use crate::player::Player;

/// The icon the play button shows while playing.
pub const ICON_PAUSE: &str = "pause-fill";
/// The icon the play button shows while paused or ended.
pub const ICON_PLAY: &str = "play-fill";
/// The repeat toggle's icon.
pub const ICON_REPEAT: &str = "repeat";
/// The fullscreen button's icon.
pub const ICON_FULLSCREEN: &str = "arrows-angle-expand";

/// The widgets the player writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    /// The surface view (the video).
    pub view: WidgetId,
    /// The controls bar, shown and hidden as a whole.
    pub bar: WidgetId,
    /// Play/pause.
    pub play: WidgetId,
    /// The seek slider, in seconds.
    pub seek: WidgetId,
    /// `m:ss / m:ss`.
    pub time: WidgetId,
    /// Repeat (loop) toggle.
    pub repeat: WidgetId,
    /// Fullscreen toggle.
    pub fullscreen: WidgetId,
}

impl Ids {
    /// Find the widgets [`build`] made, by name.
    #[must_use]
    pub fn resolve(ui: &Ui<Player>) -> Option<Self> {
        let f = |n: &str| introspect::resolve(ui, n);
        Some(Self {
            view: ui.root()?,
            bar: f("controls")?,
            play: f("play")?,
            seek: f("seek")?,
            time: f("time")?,
            repeat: f("repeat")?,
            fullscreen: f("fullscreen")?,
        })
    }
}

/// Build the video view and its controls; returns the root.
///
/// `aspect` is the video's width / height, `duration` its length in
/// seconds.
pub fn build(ui: &mut Ui<Player>, aspect: f32, duration: f64) -> WidgetId {
    let bar = panel()
        .name("controls")
        .background_role(ColorRole::Surface)
        .radius(0.0)
        .padding_xy(8.0, 4.0)
        .child(
            row()
                .gap(8.0)
                .cross_align(CrossAlign::Center)
                .child(
                    button("Pause")
                        .name("play")
                        .icon(ICON_PAUSE)
                        .on_click(|p: &mut Player, ui: &mut Ui<Player>| p.toggle_play(ui)),
                )
                .child(
                    slider(0.0)
                        .name("seek")
                        .range(0.0, duration.max(0.001) as f32)
                        .on_change(|p: &mut Player, ui: &mut Ui<Player>, v| {
                            p.request_seek(ui, f64::from(v));
                        })
                        .grow(1.0),
                )
                .child(label(time_text(0.0, duration)).name("time"))
                .child(
                    button("Repeat off")
                        .name("repeat")
                        .icon(ICON_REPEAT)
                        .on_click(|p: &mut Player, ui: &mut Ui<Player>| p.toggle_repeat(ui)),
                )
                .child(
                    button("Fullscreen")
                        .name("fullscreen")
                        .icon(ICON_FULLSCREEN)
                        .on_click(|p: &mut Player, ui: &mut Ui<Player>| {
                            p.toggle_fullscreen(ui);
                        }),
                ),
        );
    ui.build(
        surface_view()
            .name("video")
            .aspect(aspect)
            .on_pointer(|p: &mut Player, ui: &mut Ui<Player>, ev: SurfacePointer| {
                p.on_pointer(ui, ev);
            })
            .overlay(bar, OverlayAlign::Bottom),
    )
}

/// The time label's text.
#[must_use]
pub fn time_text(pos: f64, duration: f64) -> String {
    format!(
        "{} / {}",
        crate::pacing::format_time(pos),
        crate::pacing::format_time(duration)
    )
}

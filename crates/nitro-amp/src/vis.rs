//! The visualiser: Winamp's spectrum analyser and oscilloscope, as a
//! custom widget.
//!
//! # Drawn by covering, so a frame is a handful of `SetBounds`
//!
//! The obvious way to draw a bar with a green-to-red gradient is a rect
//! per bar whose fill runs the height of the display — and that is a
//! `SetFill` on every bar every frame, because a gradient is in the
//! node's own coordinates and moves when the bar's height does.
//!
//! So it is drawn the other way round. One gradient rect fills the whole
//! bar area and never changes; over it, each bar has a **cover** in the
//! background colour that hangs from the top down to where the bar
//! stops, and fixed stripes in the same colour make the gaps between
//! bars. A bar rising is its cover shortening: one `SetBounds`, the
//! colour already right at every height, and exactly the look of the
//! original, where a bar reveals the gradient behind it rather than
//! carrying one of its own. A frame of the analyser costs at most two
//! `SetBounds` per bar (cover and peak cap), and a bar that did not
//! move costs nothing, because the paint diff sends only what changed.
//!
//! Colours come from palette roles — `Success` at the foot through
//! `Danger` at the top, like a level meter — so the display follows the
//! desktop's scheme with no colour of its own (`deploy/lint-colors.sh`).

use nitro_ui::build::{Built, IntoWidget, StyleBuilder};
use nitro_ui::event::button;
use nitro_ui::layout::Constraints;
use nitro_ui::widget::{Access, EventCx, MeasureCx, PaintCx, Role as WidgetRole, Widget};
use nitro_ui::{ColorRole, Event, Fill, Handled, Point, Rect, Size, WidgetMut};

use crate::dsp::{self, Analyzer, BARS};

/// Points across the oscilloscope.
pub const SCOPE_POINTS: usize = 64;

/// What the display shows. Clicking it steps through these in order,
/// as clicking Winamp's does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The 19-bar analyser with falling peak caps.
    Spectrum,
    /// The waveform.
    Scope,
    /// Nothing, and no work done to draw it.
    Off,
}

impl Mode {
    /// The mode after this one.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Spectrum => Self::Scope,
            Self::Scope => Self::Off,
            Self::Off => Self::Spectrum,
        }
    }

    /// Its name, as `hey` reads and sets it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Spectrum => "spectrum",
            Self::Scope => "scope",
            Self::Off => "off",
        }
    }

    /// The mode a name names.
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim() {
            "spectrum" => Some(Self::Spectrum),
            "scope" | "oscilloscope" => Some(Self::Scope),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// The visualiser widget.
#[derive(Debug, Clone)]
pub struct Vis {
    mode: Mode,
    analyzer: Analyzer,
    /// The waveform, `-1.0..=1.0` per point.
    scope: [f32; SCOPE_POINTS],
}

impl Vis {
    /// What it is showing.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The analyser's current bars and peaks.
    #[must_use]
    pub fn analyzer(&self) -> &Analyzer {
        &self.analyzer
    }

    /// Whether there is nothing left moving — every bar and cap at rest
    /// and the waveform flat — so a stopped player may stop ticking.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        match self.mode {
            Mode::Spectrum => self.analyzer.is_settled(),
            Mode::Scope => self.scope.iter().all(|v| *v == 0.0),
            Mode::Off => true,
        }
    }
}

/// Setters for a live [`Vis`].
pub trait VisMut {
    /// Take one tick's worth of heard samples (mono, at `rate`).
    ///
    /// An empty slice is silence — what a stopped player feeds so the
    /// bars fall rather than freeze.
    fn feed(&mut self, mono: &[f32], rate: u32);

    /// Show `mode`.
    fn set_mode(&mut self, mode: Mode);
}

impl<S: 'static> VisMut for WidgetMut<'_, Vis, S> {
    fn feed(&mut self, mono: &[f32], rate: u32) {
        let before = (self.analyzer.clone(), self.scope);
        match self.mode {
            Mode::Spectrum => {
                let levels = if mono.is_empty() {
                    [0.0; BARS]
                } else {
                    dsp::spectrum(mono, rate)
                };
                self.analyzer.update(&levels);
            }
            Mode::Scope => self.scope = scope_points(mono),
            Mode::Off => return,
        }
        if before != (self.analyzer.clone(), self.scope) {
            self.request_paint();
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode;
            self.analyzer = Analyzer::default();
            self.scope = [0.0; SCOPE_POINTS];
            self.request_paint();
        }
    }
}

/// Reduce `mono` to [`SCOPE_POINTS`] by taking each span's sample of
/// largest magnitude, sign kept — the peak, so a transient narrower than
/// a span still shows.
fn scope_points(mono: &[f32]) -> [f32; SCOPE_POINTS] {
    let mut out = [0.0; SCOPE_POINTS];
    if mono.is_empty() {
        return out;
    }
    for (i, o) in out.iter_mut().enumerate() {
        let a = i * mono.len() / SCOPE_POINTS;
        let b = ((i + 1) * mono.len() / SCOPE_POINTS)
            .max(a + 1)
            .min(mono.len());
        *o = mono[a..b]
            .iter()
            .copied()
            .fold(0.0f32, |m, v| if v.abs() > m.abs() { v } else { m })
            .clamp(-1.0, 1.0);
    }
    out
}

/// Inset of the drawing from the widget's edge.
const INSET: f32 = 3.0;
/// Width of a gap between bars.
const GAP: f32 = 1.0;
/// Height of a peak cap.
const CAP: f32 = 1.5;

impl<S: 'static> Widget<S> for Vis {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        // Twice the original's 76×16, which at 96 dpi is the smallest
        // that still reads as bars rather than a texture.
        constraints.constrain(Size::new(152.0, 40.0))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let b = cx.bounds;
        let bg = cx.color(ColorRole::Field);
        let border = cx.color(ColorRole::Border);
        cx.rect(
            0,
            Rect::new(0.0, 0.0, b.w, b.h),
            Fill::Solid(bg),
            3.0,
            (1.0, border),
        );
        let area = Rect::new(
            INSET,
            INSET,
            (b.w - 2.0 * INSET).max(0.0),
            (b.h - 2.0 * INSET).max(0.0),
        );
        match self.mode {
            Mode::Off => {}
            Mode::Spectrum => {
                let top = cx.color(ColorRole::Danger);
                let foot = cx.color(ColorRole::Success);
                let cap = cx.color(ColorRole::TextDim);
                cx.rect(
                    1,
                    area,
                    Fill::Linear {
                        start: Point::new(0.0, 0.0),
                        end: Point::new(0.0, area.h),
                        c0: top,
                        c1: foot,
                    },
                    0.0,
                    (0.0, nitro_ui::Color::TRANSPARENT),
                );
                let n = BARS as f32;
                let bar_w = ((area.w - GAP * (n - 1.0)) / n).max(1.0);
                let mut slot: u16 = 2;
                for i in 0..BARS {
                    let x = area.x + i as f32 * (bar_w + GAP);
                    let lit = (self.analyzer.bars[i].clamp(0.0, 1.0) * area.h).round();
                    cx.fill_rect(slot, Rect::new(x, area.y, bar_w, area.h - lit), bg);
                    slot += 1;
                    let peak = (self.analyzer.peaks[i].clamp(0.0, 1.0) * area.h).round();
                    let py = (area.y + area.h - peak - CAP).max(area.y);
                    cx.fill_rect(slot, Rect::new(x, py, bar_w, CAP), cap);
                    slot += 1;
                    if i + 1 < BARS {
                        cx.fill_rect(slot, Rect::new(x + bar_w, area.y, GAP, area.h), bg);
                        slot += 1;
                    }
                }
            }
            Mode::Scope => {
                let ink = cx.color(ColorRole::Success);
                let w = area.w / SCOPE_POINTS as f32;
                let mid = area.y + area.h / 2.0;
                for (i, v) in self.scope.iter().enumerate() {
                    let y = (mid - v * area.h / 2.0).clamp(area.y, area.y + area.h - 2.0);
                    cx.fill_rect(
                        1 + i as u16,
                        Rect::new(area.x + i as f32 * w, y.round(), w.max(1.0), 2.0),
                        ink,
                    );
                }
            }
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        match ev {
            Event::PointerDown { button, .. } if *button == button::LEFT => {
                self.mode = self.mode.next();
                self.analyzer = Analyzer::default();
                self.scope = [0.0; SCOPE_POINTS];
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> WidgetRole {
        WidgetRole::Image
    }

    fn accessible(&self) -> Access {
        Access {
            name: Some("visualiser".to_owned()),
            value: Some(self.mode.name().to_owned()),
            actions: vec!["set_value"],
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        if action != "set_value" {
            return Handled::No;
        }
        let Some(mode) = arg.and_then(Mode::from_name) else {
            return Handled::No;
        };
        if mode != self.mode {
            self.mode = mode;
            self.analyzer = Analyzer::default();
            self.scope = [0.0; SCOPE_POINTS];
            cx.request_paint();
        }
        Handled::Yes
    }
}

/// Builder for a [`Vis`].
pub struct VisBuilder<S> {
    built: Built<S>,
}

impl<S: 'static> StyleBuilder<S> for VisBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for VisBuilder<S> {
    fn into_widget(self) -> Built<S> {
        self.built
    }
}

/// A visualiser, starting in spectrum mode.
#[must_use]
pub fn vis<S: 'static>() -> VisBuilder<S> {
    VisBuilder {
        built: Built::new(Vis {
            mode: Mode::Spectrum,
            analyzer: Analyzer::default(),
            scope: [0.0; SCOPE_POINTS],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_cycle_and_name_themselves() {
        let mut m = Mode::Spectrum;
        for _ in 0..3 {
            assert_eq!(Mode::from_name(m.name()), Some(m));
            m = m.next();
        }
        assert_eq!(m, Mode::Spectrum);
        assert_eq!(Mode::from_name("disco"), None);
    }

    #[test]
    fn the_scope_keeps_each_spans_peak() {
        let mut mono = vec![0.0; SCOPE_POINTS * 4];
        mono[5] = -0.9;
        mono[6] = 0.3;
        let p = scope_points(&mono);
        assert!((p[1] + 0.9).abs() < f32::EPSILON);
        assert!(p[0].abs() < f32::EPSILON);
        assert!(scope_points(&[]).iter().all(|v| *v == 0.0));
        // Fewer samples than points: every point still reads one.
        let p = scope_points(&[0.5; 3]);
        assert!(p.iter().all(|v| (*v - 0.5).abs() < f32::EPSILON));
    }
}

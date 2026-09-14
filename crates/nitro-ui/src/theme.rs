//! Colours, sizes and spacing an app can override in one place.
//!
//! [`Theme`] is a **view on a [`Palette`]**, not a source of colours.
//! The server owns the palette (`theme.scheme` in `server.conf`), pushes
//! it over the wire, and [`Theme::from_palette`] maps its roles onto the
//! fields the built-in widgets read. An app that wants a colour asks for
//! a role — [`Ui::color`](crate::Ui::color) — rather than writing one
//! down; `deploy/lint-colors.sh` fails the build for a literal.

use nitro_core::{Color, Palette, Role};

/// The look of the built-in widgets.
///
/// One value, owned by the [`Ui`](crate::Ui) and readable from every
/// paint: a widget asks the theme for its colours rather than hard-coding
/// them. It is deliberately a plain struct of values, not a trait or a
/// lookup table — there is no styling language here, and the day there
/// needs to be one, this is what it will be built out of.
///
/// Its *colours* are no longer its own, though: they are a projection of
/// the server's [`Palette`], refreshed whenever the server pushes a new
/// one. The non-colour fields — the font, the radius, the paddings —
/// stay app state, because they are not something a desktop-wide colour
/// scheme has an opinion about.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// Window and panel background.
    pub background: Color,
    /// Raised surface (a panel on top of the background).
    pub surface: Color,
    /// Default text colour.
    pub text: Color,
    /// Text on a disabled widget.
    pub text_disabled: Color,
    /// Button face.
    pub button: Color,
    /// Button face under the pointer.
    pub button_hover: Color,
    /// Button face while pressed.
    pub button_active: Color,
    /// Button face when disabled.
    pub button_disabled: Color,
    /// Text on a button.
    pub button_text: Color,
    /// Border of a panel or button.
    pub border: Color,
    /// Focus ring.
    pub focus: Color,
    /// Background of an editable field.
    pub field: Color,
    /// Text selection highlight.
    pub selection: Color,
    /// The caret in a text field.
    pub caret: Color,
    /// Placeholder text in an empty field.
    pub placeholder: Color,
    /// Filled part of a slider track, and a checked checkbox.
    pub accent: Color,
    /// Unfilled part of a slider track, and a separator line.
    pub track: Color,
    /// Default font family: a name, or one of `sans`, `serif`, `mono`.
    pub font_family: String,
    /// Default font size in logical pixels.
    pub font_size: f32,
    /// Corner radius of buttons and panels.
    pub radius: f32,
    /// Border width of buttons and panels.
    pub border_width: f32,
    /// Padding inside a button, horizontal then vertical.
    pub button_padding: (f32, f32),
    /// Padding inside a panel.
    pub panel_padding: f32,
    /// Default gap between the children of a flex container.
    pub gap: f32,
    /// Edge length of a checkbox's box.
    pub checkbox_size: f32,
    /// Height of a slider's track.
    pub slider_track: f32,
    /// Diameter of a slider's knob.
    pub slider_knob: f32,
    /// Thickness of a separator line.
    pub separator_width: f32,
}

impl Default for Theme {
    /// The default [`Palette`] (light) at the default metrics.
    ///
    /// It is the theme an app gets before the server's `Theme` message
    /// arrives, and the one the tests run on.
    fn default() -> Self {
        Self::from_palette(&Palette::default())
    }
}

impl Theme {
    /// Map a palette's roles onto the widget colours, keeping the
    /// non-colour metrics at their defaults.
    ///
    /// This is the *only* place the two vocabularies meet. A widget that
    /// needs a colour with no field here reads the role directly through
    /// [`Ui::color`](crate::Ui::color) rather than growing the struct —
    /// the fields that exist are the ones the built-in widgets use.
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            background: p.get(Role::WindowBackground),
            surface: p.get(Role::Surface),
            text: p.get(Role::Text),
            text_disabled: p.get(Role::TextDim),
            button: p.get(Role::Button),
            button_hover: p.get(Role::ButtonHover),
            button_active: p.get(Role::ButtonActive),
            button_disabled: p.get(Role::ButtonDisabled),
            button_text: p.get(Role::ButtonText),
            border: p.get(Role::Border),
            focus: p.get(Role::Focus),
            field: p.get(Role::Field),
            selection: p.get(Role::Selection),
            caret: p.get(Role::Caret),
            placeholder: p.get(Role::Placeholder),
            accent: p.get(Role::Accent),
            track: p.get(Role::Track),
            font_family: "sans".to_owned(),
            font_size: 14.0,
            radius: 4.0,
            border_width: 1.0,
            button_padding: (14.0, 7.0),
            panel_padding: 8.0,
            gap: 8.0,
            checkbox_size: 16.0,
            slider_track: 4.0,
            slider_knob: 14.0,
            separator_width: 1.0,
        }
    }

    /// The same theme with `p`'s colours and this one's metrics.
    ///
    /// What [`Ui::set_palette`](crate::Ui::set_palette) applies when the
    /// server pushes a palette: an app that customised its font size or
    /// its paddings keeps them across a scheme switch, because those are
    /// not the desktop's business.
    #[must_use]
    pub fn with_palette(&self, p: &Palette) -> Self {
        Self {
            font_family: self.font_family.clone(),
            font_size: self.font_size,
            radius: self.radius,
            border_width: self.border_width,
            button_padding: self.button_padding,
            panel_padding: self.panel_padding,
            gap: self.gap,
            checkbox_size: self.checkbox_size,
            slider_track: self.slider_track,
            slider_knob: self.slider_knob,
            separator_width: self.separator_width,
            ..Self::from_palette(p)
        }
    }
}

/// How a run of text is drawn: family, size, weight and slant.
///
/// The colour is *not* part of it, because a colour change does not need
/// a reshape and the two travel separately on the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    /// Family name, or one of `sans`, `serif`, `mono`.
    pub family: String,
    /// Size in logical pixels.
    pub size_px: f32,
    /// CSS-style weight (400 regular, 700 bold).
    pub weight: u16,
    /// Whether to select an italic face.
    pub italic: bool,
}

impl TextStyle {
    /// A style in the theme's family at `size_px`, regular and upright.
    #[must_use]
    pub fn new(family: impl Into<String>, size_px: f32) -> Self {
        Self {
            family: family.into(),
            size_px,
            weight: 400,
            italic: false,
        }
    }

    /// The theme's default text style.
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        Self::new(theme.font_family.clone(), theme.font_size)
    }
}

impl Default for TextStyle {
    fn default() -> Self {
        Self::new("sans", 14.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_theme_is_legible() {
        let t = Theme::default();
        assert!(t.font_size > 0.0);
        assert_ne!(t.text, t.background);
        // Every widget's own colours have to be distinguishable from the
        // surface they sit on, or the widget is invisible.
        assert_ne!(t.field, t.border);
        assert_ne!(t.accent, t.track);
        assert_ne!(t.caret, t.field);
        assert!(t.checkbox_size > 0.0 && t.slider_knob > t.slider_track);
        assert_eq!(
            TextStyle::from_theme(&t).size_px.to_bits(),
            t.font_size.to_bits()
        );
        assert_eq!(TextStyle::from_theme(&t).family, t.font_family);
    }

    #[test]
    fn every_colour_field_comes_from_a_role() {
        // The mapping is exhaustive in the direction that matters: no
        // field keeps a light-scheme value when the palette went dark.
        // A field `from_palette` forgot would show up here as a colour
        // that did not move.
        let light = Theme::from_palette(&Palette::light());
        let dark = Theme::from_palette(&Palette::dark());
        for (name, l, d) in [
            ("background", light.background, dark.background),
            ("surface", light.surface, dark.surface),
            ("text", light.text, dark.text),
            ("text_disabled", light.text_disabled, dark.text_disabled),
            ("button", light.button, dark.button),
            ("button_hover", light.button_hover, dark.button_hover),
            ("button_active", light.button_active, dark.button_active),
            (
                "button_disabled",
                light.button_disabled,
                dark.button_disabled,
            ),
            ("button_text", light.button_text, dark.button_text),
            ("border", light.border, dark.border),
            ("focus", light.focus, dark.focus),
            ("field", light.field, dark.field),
            ("selection", light.selection, dark.selection),
            ("caret", light.caret, dark.caret),
            ("placeholder", light.placeholder, dark.placeholder),
            ("accent", light.accent, dark.accent),
            ("track", light.track, dark.track),
        ] {
            assert_ne!(l, d, "{name} did not follow the scheme");
        }
        // And the metrics did *not* move, because they are not colours.
        assert_eq!(light.font_size.to_bits(), dark.font_size.to_bits());
        assert_eq!(light.radius.to_bits(), dark.radius.to_bits());
    }

    #[test]
    fn with_palette_keeps_the_apps_metrics() {
        let mut t = Theme::default();
        t.font_size = 22.0;
        t.font_family = "mono".to_owned();
        t.radius = 0.0;
        let dark = t.with_palette(&Palette::dark());
        assert_eq!(dark.font_size.to_bits(), 22.0f32.to_bits());
        assert_eq!(dark.font_family, "mono");
        assert_eq!(dark.radius.to_bits(), 0.0f32.to_bits());
        assert_eq!(dark.background, Palette::dark().get(Role::WindowBackground));
        // Idempotent for the palette it already has.
        assert_eq!(dark.with_palette(&Palette::dark()), dark);
    }

    #[test]
    fn the_default_theme_is_the_default_palette() {
        assert_eq!(Theme::default(), Theme::from_palette(&Palette::default()));
        assert_eq!(
            Theme::default().background,
            Palette::light().get(Role::WindowBackground)
        );
    }
}

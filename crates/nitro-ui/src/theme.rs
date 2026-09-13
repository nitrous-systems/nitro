//! Colours, sizes and spacing an app can override in one place.

use nitro_core::Color;

/// The look of the built-in widgets.
///
/// One value, owned by the [`Ui`](crate::Ui) and readable from every
/// paint: a widget asks the theme for its colours rather than hard-coding
/// them, so an app changes the whole look by handing [`App`](crate::App) a
/// different `Theme`. It is deliberately a plain struct of values, not a
/// trait or a lookup table — there is no styling language here, and the
/// day there needs to be one, this is what it will be built out of.
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
}

impl Default for Theme {
    /// A restrained light theme: it is the one an app gets if it never
    /// thinks about theming, so it should be legible before it is pretty.
    fn default() -> Self {
        Self {
            background: Color::rgb(0xf2, 0xf2, 0xf2),
            surface: Color::rgb(0xff, 0xff, 0xff),
            text: Color::rgb(0x1a, 0x1a, 0x1a),
            text_disabled: Color::rgb(0x9a, 0x9a, 0x9a),
            button: Color::rgb(0xe4, 0xe4, 0xe8),
            button_hover: Color::rgb(0xd6, 0xd9, 0xe4),
            button_active: Color::rgb(0xc0, 0xc6, 0xd8),
            button_disabled: Color::rgb(0xec, 0xec, 0xee),
            button_text: Color::rgb(0x12, 0x12, 0x16),
            border: Color::rgb(0xc2, 0xc2, 0xc8),
            focus: Color::rgb(0x33, 0x88, 0xff),
            font_family: "sans".to_owned(),
            font_size: 14.0,
            radius: 4.0,
            border_width: 1.0,
            button_padding: (14.0, 7.0),
            panel_padding: 8.0,
            gap: 8.0,
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
        assert_eq!(
            TextStyle::from_theme(&t).size_px.to_bits(),
            t.font_size.to_bits()
        );
        assert_eq!(TextStyle::from_theme(&t).family, t.font_family);
    }
}

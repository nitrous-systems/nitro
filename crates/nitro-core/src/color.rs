//! Colours.

/// An sRGB colour with straight (non-premultiplied) alpha, 8 bits per channel.
///
/// This is the wire and API representation. Rasterizers convert to whatever
/// internal form they need (typically premultiplied).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha (255 = opaque).
    pub a: u8,
}

impl Color {
    /// Fully transparent black.
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
    /// Opaque black.
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    /// Opaque white.
    pub const WHITE: Self = Self::rgb(255, 255, 255);

    /// Opaque colour.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Colour with alpha.
    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// From `0xRRGGBBAA`.
    #[must_use]
    pub const fn from_u32(v: u32) -> Self {
        Self::rgba((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// To `0xRRGGBBAA`.
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        (self.r as u32) << 24 | (self.g as u32) << 16 | (self.b as u32) << 8 | self.a as u32
    }

    /// Whether the colour is fully opaque.
    #[must_use]
    pub const fn is_opaque(self) -> bool {
        self.a == 255
    }

    /// Whether the colour is fully transparent.
    #[must_use]
    pub const fn is_transparent(self) -> bool {
        self.a == 0
    }

    /// Same colour with a different alpha.
    #[must_use]
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::Color;

    #[test]
    fn u32_round_trip() {
        let c = Color::rgba(0x12, 0x34, 0x56, 0x78);
        assert_eq!(c.to_u32(), 0x1234_5678);
        assert_eq!(Color::from_u32(0x1234_5678), c);
        assert!(Color::WHITE.is_opaque());
        assert!(Color::TRANSPARENT.is_transparent());
        assert_eq!(Color::WHITE.with_alpha(0).a, 0);
    }
}

//! The byte layout: `repr(C)` zerocopy twins of the value types, and the
//! [`Plain`] trait that maps an API type to its fixed-size wire form.
//!
//! Everything here is little-endian by construction (`zerocopy`'s
//! `little_endian::{U16, U32, …}` types) and `Unaligned`, so a payload can
//! be decoded in place from an arbitrary `&[u8]` with no copy and no
//! alignment dance.

use nitro_core::{Color, IRect, Point, Rect, Size, Transform};
use zerocopy::byteorder::little_endian::{F32, I32, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::error::DecodeError;
use crate::types::{
    Align, AxisSource, BufferId, ButtonState, CursorPos, Edge, ErrorCode, Layer, NodeId, NodeKind,
    TouchPhase, WindowRef, WindowState,
};

/// A type with a fixed-size, little-endian wire representation.
///
/// The blanket requirement on `Wire` is what makes a message body a single
/// `repr(C)` struct: concatenating the wire forms of the fields *is* the
/// payload layout.
pub trait Plain: Sized + Copy {
    /// The `repr(C)`, unaligned, little-endian twin.
    type Wire: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned + Copy;

    /// Convert to the wire form. Infallible.
    fn to_wire(self) -> Self::Wire;

    /// Convert from the wire form.
    ///
    /// # Errors
    /// [`DecodeError::BadValue`] when the bytes are a valid `Wire` but not
    /// a valid value of `Self` (an out-of-range tag, a bool that is not
    /// 0 or 1).
    fn from_wire(w: Self::Wire) -> Result<Self, DecodeError>;
}

/// `impl Plain` for a scalar whose wire form is a byteorder wrapper.
macro_rules! plain_scalar {
    ($t:ty, $w:ty) => {
        impl Plain for $t {
            type Wire = $w;
            fn to_wire(self) -> $w {
                <$w>::new(self)
            }
            fn from_wire(w: $w) -> Result<Self, DecodeError> {
                Ok(w.get())
            }
        }
    };
}

plain_scalar!(u16, U16);
plain_scalar!(u32, U32);
plain_scalar!(i32, I32);
plain_scalar!(u64, U64);
plain_scalar!(f32, F32);

impl Plain for u8 {
    type Wire = u8;
    fn to_wire(self) -> u8 {
        self
    }
    fn from_wire(w: u8) -> Result<Self, DecodeError> {
        Ok(w)
    }
}

/// `bool` travels as a `u8`; anything but 0 or 1 is a decode error rather
/// than a silently-accepted truthy byte.
impl Plain for bool {
    type Wire = u8;
    fn to_wire(self) -> u8 {
        u8::from(self)
    }
    fn from_wire(w: u8) -> Result<Self, DecodeError> {
        match w {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::BadValue),
        }
    }
}

/// `impl Plain` for the `u8`/`u16`-tagged enums from [`crate::types`].
macro_rules! plain_tag {
    ($t:ty, $repr:ty, $w:ty) => {
        impl Plain for $t {
            type Wire = $w;
            fn to_wire(self) -> $w {
                <$repr as Plain>::to_wire(self.raw())
            }
            fn from_wire(w: $w) -> Result<Self, DecodeError> {
                Self::from_raw(<$repr as Plain>::from_wire(w)?)
            }
        }
    };
}

plain_tag!(Align, u8, u8);
plain_tag!(Layer, u8, u8);
plain_tag!(NodeKind, u8, u8);
plain_tag!(ButtonState, u8, u8);
plain_tag!(AxisSource, u8, u8);
plain_tag!(TouchPhase, u8, u8);
plain_tag!(WindowState, u8, u8);
plain_tag!(Edge, u8, u8);
plain_tag!(ErrorCode, u16, U16);

/// `impl Plain` for the id newtypes.
macro_rules! plain_id {
    ($t:ty) => {
        impl Plain for $t {
            type Wire = U32;
            fn to_wire(self) -> U32 {
                U32::new(self.0)
            }
            fn from_wire(w: U32) -> Result<Self, DecodeError> {
                Ok(Self(w.get()))
            }
        }
    };
}

plain_id!(NodeId);
plain_id!(BufferId);
plain_id!(WindowRef);

/// Wire twin of [`Point`]: `x, y` as `f32`.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct WirePoint {
    /// Horizontal coordinate.
    pub x: F32,
    /// Vertical coordinate.
    pub y: F32,
}

/// Wire twin of [`Size`]: `w, h` as `f32`.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct WireSize {
    /// Width.
    pub w: F32,
    /// Height.
    pub h: F32,
}

/// Wire twin of [`Rect`]: `x, y, w, h` as `f32`.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct WireRect {
    /// Left edge.
    pub x: F32,
    /// Top edge.
    pub y: F32,
    /// Width.
    pub w: F32,
    /// Height.
    pub h: F32,
}

/// Wire twin of [`IRect`]: `x, y, w, h` as `i32`.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct WireIRect {
    /// Left edge.
    pub x: I32,
    /// Top edge.
    pub y: I32,
    /// Width.
    pub w: I32,
    /// Height.
    pub h: I32,
}

/// Wire twin of [`Color`]: four bytes `r, g, b, a`.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct WireColor {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha.
    pub a: u8,
}

/// Wire twin of [`CursorPos`]: a `u32` byte offset then an `f32` x.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct WireCursorPos {
    /// Byte offset into the measured string.
    pub offset: U32,
    /// Horizontal position in logical pixels.
    pub x: F32,
}

/// Wire twin of [`Transform`]: `a, b, c, d, e, f` as `f32`.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct WireTransform {
    /// x-scale / cos.
    pub a: F32,
    /// y-shear / sin.
    pub b: F32,
    /// x-shear / -sin.
    pub c: F32,
    /// y-scale / cos.
    pub d: F32,
    /// x-translation.
    pub e: F32,
    /// y-translation.
    pub f: F32,
}

impl Plain for Point {
    type Wire = WirePoint;
    fn to_wire(self) -> WirePoint {
        WirePoint {
            x: F32::new(self.x),
            y: F32::new(self.y),
        }
    }
    fn from_wire(w: WirePoint) -> Result<Self, DecodeError> {
        Ok(Self::new(w.x.get(), w.y.get()))
    }
}

impl Plain for Size {
    type Wire = WireSize;
    fn to_wire(self) -> WireSize {
        WireSize {
            w: F32::new(self.w),
            h: F32::new(self.h),
        }
    }
    fn from_wire(w: WireSize) -> Result<Self, DecodeError> {
        Ok(Self::new(w.w.get(), w.h.get()))
    }
}

impl Plain for Rect {
    type Wire = WireRect;
    fn to_wire(self) -> WireRect {
        WireRect {
            x: F32::new(self.x),
            y: F32::new(self.y),
            w: F32::new(self.w),
            h: F32::new(self.h),
        }
    }
    fn from_wire(w: WireRect) -> Result<Self, DecodeError> {
        Ok(Self::new(w.x.get(), w.y.get(), w.w.get(), w.h.get()))
    }
}

impl Plain for IRect {
    type Wire = WireIRect;
    fn to_wire(self) -> WireIRect {
        WireIRect {
            x: I32::new(self.x),
            y: I32::new(self.y),
            w: I32::new(self.w),
            h: I32::new(self.h),
        }
    }
    fn from_wire(w: WireIRect) -> Result<Self, DecodeError> {
        Ok(Self::new(w.x.get(), w.y.get(), w.w.get(), w.h.get()))
    }
}

impl Plain for Color {
    type Wire = WireColor;
    fn to_wire(self) -> WireColor {
        WireColor {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a,
        }
    }
    fn from_wire(w: WireColor) -> Result<Self, DecodeError> {
        Ok(Self::rgba(w.r, w.g, w.b, w.a))
    }
}

impl Plain for CursorPos {
    type Wire = WireCursorPos;
    fn to_wire(self) -> WireCursorPos {
        WireCursorPos {
            offset: U32::new(self.offset),
            x: F32::new(self.x),
        }
    }
    fn from_wire(w: WireCursorPos) -> Result<Self, DecodeError> {
        Ok(Self::new(w.offset.get(), w.x.get()))
    }
}

impl Plain for Transform {
    type Wire = WireTransform;
    fn to_wire(self) -> WireTransform {
        WireTransform {
            a: F32::new(self.a),
            b: F32::new(self.b),
            c: F32::new(self.c),
            d: F32::new(self.d),
            e: F32::new(self.e),
            f: F32::new(self.f),
        }
    }
    fn from_wire(w: WireTransform) -> Result<Self, DecodeError> {
        Ok(Self {
            a: w.a.get(),
            b: w.b.get(),
            c: w.c.get(),
            d: w.d.get(),
            e: w.e.get(),
            f: w.f.get(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_sizes_are_the_documented_ones() {
        assert_eq!(size_of::<WirePoint>(), 8);
        assert_eq!(size_of::<WireSize>(), 8);
        assert_eq!(size_of::<WireRect>(), 16);
        assert_eq!(size_of::<WireIRect>(), 16);
        assert_eq!(size_of::<WireColor>(), 4);
        assert_eq!(size_of::<WireTransform>(), 24);
        assert_eq!(size_of::<WireCursorPos>(), 8);
    }

    #[test]
    fn values_are_little_endian() {
        assert_eq!(Plain::to_wire(1.0f32).as_bytes(), &[0x00, 0x00, 0x80, 0x3f]);
        assert_eq!(
            Plain::to_wire(0x1234_5678u32).as_bytes(),
            &[0x78, 0x56, 0x34, 0x12]
        );
        assert_eq!(
            Plain::to_wire(Color::rgba(1, 2, 3, 4)).as_bytes(),
            &[1, 2, 3, 4]
        );
    }

    #[test]
    fn bools_reject_other_bytes() {
        assert_eq!(<bool as Plain>::from_wire(0), Ok(false));
        assert_eq!(<bool as Plain>::from_wire(1), Ok(true));
        assert_eq!(<bool as Plain>::from_wire(2), Err(DecodeError::BadValue));
    }

    #[test]
    fn align_tags_round_trip() {
        for a in [Align::Left, Align::Center, Align::Right] {
            assert_eq!(<Align as Plain>::from_wire(Plain::to_wire(a)), Ok(a));
        }
        assert_eq!(<Align as Plain>::from_wire(3), Err(DecodeError::BadValue));
    }
}

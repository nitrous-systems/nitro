//! 2-D affine transforms.

use crate::{Point, Rect};

/// A 2-D affine transform `[a c e; b d f; 0 0 1]` mapping `(x, y)` to
/// `(a*x + c*y + e, b*x + d*y + f)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// x-scale / cos.
    pub a: f32,
    /// y-shear / sin.
    pub b: f32,
    /// x-shear / -sin.
    pub c: f32,
    /// y-scale / cos.
    pub d: f32,
    /// x-translation.
    pub e: f32,
    /// y-translation.
    pub f: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    /// The identity transform.
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// Pure translation.
    #[must_use]
    pub const fn translate(x: f32, y: f32) -> Self {
        Self {
            e: x,
            f: y,
            ..Self::IDENTITY
        }
    }

    /// Pure scale about the origin.
    #[must_use]
    pub const fn scale(sx: f32, sy: f32) -> Self {
        Self {
            a: sx,
            d: sy,
            ..Self::IDENTITY
        }
    }

    /// Whether this is exactly the identity.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    /// Whether this is a pure translation (no scale, shear or rotation).
    #[must_use]
    pub fn is_translation(&self) -> bool {
        self.a.to_bits() == 1.0f32.to_bits()
            && self.b.to_bits() == 0.0f32.to_bits()
            && self.c.to_bits() == 0.0f32.to_bits()
            && self.d.to_bits() == 1.0f32.to_bits()
    }

    /// Whether the transform maps axis-aligned rects to axis-aligned rects
    /// (no rotation or shear).
    #[must_use]
    pub fn is_axis_aligned(&self) -> bool {
        self.b.to_bits() == 0.0f32.to_bits() && self.c.to_bits() == 0.0f32.to_bits()
    }

    /// `self` applied after `inner`: `(self ∘ inner)(p) = self(inner(p))`.
    #[must_use]
    pub fn then(&self, inner: &Self) -> Self {
        Self {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            e: self.a * inner.e + self.c * inner.f + self.e,
            f: self.b * inner.e + self.d * inner.f + self.f,
        }
    }

    /// Apply to a point.
    #[must_use]
    pub fn apply(&self, p: Point) -> Point {
        Point::new(
            self.a * p.x + self.c * p.y + self.e,
            self.b * p.x + self.d * p.y + self.f,
        )
    }

    /// Axis-aligned bounding box of the transformed rect.
    #[must_use]
    pub fn apply_rect(&self, r: &Rect) -> Rect {
        if r.is_empty() {
            return Rect::EMPTY;
        }
        let p0 = self.apply(Point::new(r.x, r.y));
        let p1 = self.apply(Point::new(r.right(), r.y));
        let p2 = self.apply(Point::new(r.x, r.bottom()));
        let p3 = self.apply(Point::new(r.right(), r.bottom()));
        let x0 = p0.x.min(p1.x).min(p2.x).min(p3.x);
        let y0 = p0.y.min(p1.y).min(p2.y).min(p3.y);
        let x1 = p0.x.max(p1.x).max(p2.x).max(p3.x);
        let y1 = p0.y.max(p1.y).max(p2.y).max(p3.y);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// Inverse, or `None` if singular.
    #[must_use]
    pub fn invert(&self) -> Option<Self> {
        let det = self.a * self.d - self.b * self.c;
        if det.to_bits() == 0.0f32.to_bits() || !det.is_finite() {
            return None;
        }
        let id = 1.0 / det;
        Some(Self {
            a: self.d * id,
            b: -self.b * id,
            c: -self.c * id,
            d: self.a * id,
            e: (self.c * self.f - self.d * self.e) * id,
            f: (self.b * self.e - self.a * self.f) * id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_and_apply() {
        let t = Transform::translate(10.0, 20.0);
        let s = Transform::scale(2.0, 3.0);
        // scale first, then translate
        let ts = t.then(&s);
        assert_eq!(ts.apply(Point::new(1.0, 1.0)), Point::new(12.0, 23.0));
        // translate first, then scale
        let st = s.then(&t);
        assert_eq!(st.apply(Point::new(1.0, 1.0)), Point::new(22.0, 63.0));
        assert!(t.is_translation());
        assert!(!s.is_translation());
        assert!(ts.is_axis_aligned());
    }

    #[test]
    fn rect_bounds_and_inverse() {
        let t = Transform::translate(5.0, 5.0).then(&Transform::scale(2.0, 2.0));
        let r = t.apply_rect(&Rect::new(0.0, 0.0, 1.0, 1.0));
        assert_eq!(r, Rect::new(5.0, 5.0, 2.0, 2.0));
        let inv = t.invert().unwrap();
        let p = inv.apply(Point::new(7.0, 7.0));
        assert!((p.x - 1.0).abs() < 1e-6 && (p.y - 1.0).abs() < 1e-6);
        assert!(Transform::scale(0.0, 1.0).invert().is_none());
        assert_eq!(t.apply_rect(&Rect::EMPTY), Rect::EMPTY);
    }
}

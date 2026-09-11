//! Points, sizes and rectangles.

/// A point in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
}

impl Point {
    /// Origin.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    /// Construct a point.
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A size in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Size {
    /// Zero size.
    pub const ZERO: Self = Self { w: 0.0, h: 0.0 };

    /// Construct a size.
    #[must_use]
    pub const fn new(w: f32, h: f32) -> Self {
        Self { w, h }
    }
}

/// An axis-aligned rectangle in logical coordinates (`f32`).
///
/// A rect with `w <= 0` or `h <= 0` is *empty*; empty rects intersect with
/// nothing and are ignored by unions.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// The empty rect at the origin.
    pub const EMPTY: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    /// Construct from origin and size.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Construct from two corners (any order).
    #[must_use]
    pub fn from_corners(a: Point, b: Point) -> Self {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        Self::new(x, y, a.x.max(b.x) - x, a.y.max(b.y) - y)
    }

    /// Whether the rect has no area.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// Right edge (`x + w`).
    #[must_use]
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    /// Bottom edge (`y + h`).
    #[must_use]
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// Top-left corner.
    #[must_use]
    pub fn origin(&self) -> Point {
        Point::new(self.x, self.y)
    }

    /// Size.
    #[must_use]
    pub fn size(&self) -> Size {
        Size::new(self.w, self.h)
    }

    /// Whether `p` lies inside (left/top inclusive, right/bottom exclusive).
    #[must_use]
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.y >= self.y && p.x < self.right() && p.y < self.bottom()
    }

    /// Intersection; empty (`EMPTY`) if the rects do not overlap.
    #[must_use]
    pub fn intersect(&self, o: &Self) -> Self {
        if self.is_empty() || o.is_empty() {
            return Self::EMPTY;
        }
        let left = self.x.max(o.x);
        let top = self.y.max(o.y);
        let right = self.right().min(o.right());
        let bottom = self.bottom().min(o.bottom());
        if right <= left || bottom <= top {
            Self::EMPTY
        } else {
            Self::new(left, top, right - left, bottom - top)
        }
    }

    /// Smallest rect containing both; empty rects are ignored.
    #[must_use]
    pub fn union(&self, o: &Self) -> Self {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let left = self.x.min(o.x);
        let top = self.y.min(o.y);
        Self::new(
            left,
            top,
            self.right().max(o.right()) - left,
            self.bottom().max(o.bottom()) - top,
        )
    }

    /// Whether the two rects overlap with positive area.
    #[must_use]
    pub fn intersects(&self, o: &Self) -> bool {
        !self.intersect(o).is_empty()
    }

    /// Translate by `(dx, dy)`.
    #[must_use]
    pub fn translate(&self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// Grow by `d` on every side (negative shrinks).
    #[must_use]
    pub fn inflate(&self, d: f32) -> Self {
        Self::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    /// Smallest integer rect that covers this rect.
    #[must_use]
    pub fn round_out(&self) -> IRect {
        if self.is_empty() {
            return IRect::EMPTY;
        }
        let x0 = self.x.floor();
        let y0 = self.y.floor();
        let x1 = self.right().ceil();
        let y1 = self.bottom().ceil();
        IRect::from_edges(x0 as i32, y0 as i32, x1 as i32, y1 as i32)
    }
}

/// An axis-aligned rectangle in integer pixel coordinates.
///
/// Empty when `w <= 0` or `h <= 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct IRect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub w: i32,
    /// Height.
    pub h: i32,
}

impl IRect {
    /// The empty rect at the origin.
    pub const EMPTY: Self = Self {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    /// Construct from origin and size.
    #[must_use]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// Construct from edges (`x1`/`y1` exclusive).
    #[must_use]
    pub const fn from_edges(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// Whether the rect has no area.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Right edge (exclusive).
    #[must_use]
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    /// Bottom edge (exclusive).
    #[must_use]
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// Pixel count.
    #[must_use]
    pub fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            i64::from(self.w) * i64::from(self.h)
        }
    }

    /// Whether the pixel `(x, y)` lies inside.
    #[must_use]
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    /// Intersection; `EMPTY` if disjoint.
    #[must_use]
    pub fn intersect(&self, o: &Self) -> Self {
        if self.is_empty() || o.is_empty() {
            return Self::EMPTY;
        }
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        if x1 <= x0 || y1 <= y0 {
            Self::EMPTY
        } else {
            Self::from_edges(x0, y0, x1, y1)
        }
    }

    /// Smallest rect containing both; empty rects are ignored.
    #[must_use]
    pub fn union(&self, o: &Self) -> Self {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Self::from_edges(
            self.x.min(o.x),
            self.y.min(o.y),
            self.right().max(o.right()),
            self.bottom().max(o.bottom()),
        )
    }

    /// Whether the two rects overlap with positive area.
    #[must_use]
    pub fn intersects(&self, o: &Self) -> bool {
        !self.intersect(o).is_empty()
    }

    /// Whether `o` lies entirely inside `self`.
    #[must_use]
    pub fn contains_rect(&self, o: &Self) -> bool {
        !o.is_empty()
            && o.x >= self.x
            && o.y >= self.y
            && o.right() <= self.right()
            && o.bottom() <= self.bottom()
    }

    /// Translate by `(dx, dy)`.
    #[must_use]
    pub const fn translate(&self, dx: i32, dy: i32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// Convert to a logical rect.
    #[must_use]
    pub fn to_rect(&self) -> Rect {
        Rect::new(self.x as f32, self.y as f32, self.w as f32, self.h as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_intersect_union() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersect(&b), Rect::new(5.0, 5.0, 5.0, 5.0));
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 15.0, 15.0));
        let c = Rect::new(20.0, 20.0, 1.0, 1.0);
        assert!(a.intersect(&c).is_empty());
        assert!(!a.intersects(&c));
        assert_eq!(a.union(&Rect::EMPTY), a);
        assert_eq!(Rect::EMPTY.union(&a), a);
    }

    #[test]
    fn rect_round_out() {
        let r = Rect::new(0.5, 0.2, 2.0, 2.0);
        assert_eq!(r.round_out(), IRect::new(0, 0, 3, 3));
        assert_eq!(Rect::EMPTY.round_out(), IRect::EMPTY);
        let n = Rect::new(-1.5, -0.5, 1.0, 1.0);
        assert_eq!(n.round_out(), IRect::new(-2, -1, 2, 2));
    }

    #[test]
    fn rect_contains_is_half_open() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(r.contains(Point::new(9.999, 9.999)));
        assert!(!r.contains(Point::new(10.0, 5.0)));
    }

    #[test]
    fn irect_ops() {
        let a = IRect::new(0, 0, 10, 10);
        let b = IRect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), IRect::new(5, 5, 5, 5));
        assert_eq!(a.union(&b), IRect::new(0, 0, 15, 15));
        assert_eq!(a.area(), 100);
        assert!(a.contains_rect(&IRect::new(1, 1, 2, 2)));
        assert!(!a.contains_rect(&b));
        assert!(!a.contains_rect(&IRect::EMPTY));
        assert!(a.contains(9, 9));
        assert!(!a.contains(10, 9));
        assert_eq!(IRect::new(-2, -2, 1, 1).intersect(&a), IRect::EMPTY);
    }
}

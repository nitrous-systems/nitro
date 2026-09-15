//! The flex layout model: pure functions over styles and measured sizes.
//!
//! Nothing here knows about widgets, the arena or the scene. A container
//! measures its children, hands the results to [`solve`] as [`FlexItem`]s
//! and gets one [`Rect`] per child back, in the container's own coordinate
//! space. That is what makes the whole layout model unit-testable without
//! a server, a connection or a widget tree — see the tests at the bottom
//! of this file.
//!
//! The subset is the useful half of CSS flexbox: one axis
//! ([`Direction`]), alignment on both axes ([`MainAlign`],
//! [`CrossAlign`]), `gap`, `padding`, `margin`, explicit or automatic
//! sizes ([`Length`]) with min/max clamps, and `flex_grow`/`flex_shrink`
//! to divide what is left over. No wrapping, no `order`, no baselines.

use nitro_core::{Rect, Size};

/// Which way a [`Flex`](crate::widgets::Flex) container stacks children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Left to right; the main axis is horizontal.
    Row,
    /// Top to bottom; the main axis is vertical.
    #[default]
    Column,
}

impl Direction {
    /// The main-axis component of a size.
    #[must_use]
    pub fn main(self, s: Size) -> f32 {
        match self {
            Direction::Row => s.w,
            Direction::Column => s.h,
        }
    }

    /// The cross-axis component of a size.
    #[must_use]
    pub fn cross(self, s: Size) -> f32 {
        match self {
            Direction::Row => s.h,
            Direction::Column => s.w,
        }
    }

    /// Build a size from main and cross components.
    #[must_use]
    pub fn size(self, main: f32, cross: f32) -> Size {
        match self {
            Direction::Row => Size::new(main, cross),
            Direction::Column => Size::new(cross, main),
        }
    }

    /// Build a rect from main/cross positions and sizes.
    #[must_use]
    pub fn rect(self, main: f32, cross: f32, main_size: f32, cross_size: f32) -> Rect {
        match self {
            Direction::Row => Rect::new(main, cross, main_size, cross_size),
            Direction::Column => Rect::new(cross, main, cross_size, main_size),
        }
    }
}

/// How leftover main-axis space is distributed between children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MainAlign {
    /// Pack at the start.
    #[default]
    Start,
    /// Pack in the middle.
    Center,
    /// Pack at the end.
    End,
    /// First child at the start, last at the end, the rest evenly spread.
    SpaceBetween,
}

/// How a child is placed and sized on the cross axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CrossAlign {
    /// Align to the start edge, at the child's own cross size.
    #[default]
    Start,
    /// Centre, at the child's own cross size.
    Center,
    /// Align to the end edge, at the child's own cross size.
    End,
    /// Fill the container's cross extent.
    Stretch,
}

/// How small a child may be laid out along the main axis.
///
/// The default is [`ShrinkFloor::Content`]: a child never ends up
/// smaller than the size it measured to. That is CSS's `min-size: auto`
/// on a flex item, and it exists because most widgets have no smaller
/// honest version of themselves — a label laid out below its measured
/// height is painted with the full glyphs and loses its descenders,
/// and a container laid out below its children's total renders them
/// outside itself, on top of whatever comes next.
///
/// A widget that *is* honestly smaller than its content — a viewport
/// over a scrolled or virtualised child, like
/// [`List`](crate::List) and [`Scroll`](crate::widgets::Scroll) — says
/// so with [`ShrinkFloor::Zero`], and then `flex_shrink` means what it
/// means in CSS.
///
/// # The flag is one field, but the floor is per-axis
///
/// There is no `shrink_floor_x`/`_y`: the floor applies to whichever
/// axis is the **parent's main axis**, so the same widget is floored on
/// its height in a `Column` and on its width in a `Row`. That matters
/// for the two opt-outs whose justification is horizontal:
/// [`text_field`](crate::widgets::text_field) is a viewport over its own
/// string and [`slider`](crate::widgets::slider) has no content at all,
/// but both arguments are about *width*. Put either in a `Column` and
/// `Zero` also lets it be laid out shorter than it measured, which is
/// #561 again wearing a different widget — a field squeezed below its
/// line height clips the text inside it.
///
/// In practice neither is a column's flexible child (a field's height is
/// its font's, a slider's is its knob's, and both are normally rows'
/// children), so this is a sharp edge rather than a live bug. The
/// defence if you hit it is the ordinary one: `min_height`, which the
/// floor is a `max` against. Splitting the flag per axis would be the
/// thorough fix and is deliberately not done here — one field is what
/// every call site actually needs, and two would have to be explained at
/// each of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShrinkFloor {
    /// Never smaller than the measured size (`basis`). The default.
    #[default]
    Content,
    /// May be shrunk to nothing; only an explicit `min_*` holds it up.
    Zero,
}

/// An explicit, relative or automatic extent.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Length {
    /// Whatever the widget measures to.
    #[default]
    Auto,
    /// A fixed number of logical pixels.
    Px(f32),
    /// A fraction of the parent's content extent, `0.0..=1.0`-ish (100.0
    /// is *not* full width; use `0.5` for half).
    Percent(f32),
}

impl Length {
    /// Resolve against an available extent, or `None` for [`Length::Auto`]
    /// (and for a percentage of an unbounded parent).
    #[must_use]
    pub fn resolve(self, available: f32) -> Option<f32> {
        match self {
            Length::Px(v) => Some(v),
            Length::Percent(f) if available.is_finite() => Some(available * f),
            // `Auto`, and a percentage of an unbounded parent, which has
            // no finite extent to take a fraction of.
            Length::Auto | Length::Percent(_) => None,
        }
    }
}

/// Space on the four sides of a box.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Edges {
    /// Left.
    pub left: f32,
    /// Top.
    pub top: f32,
    /// Right.
    pub right: f32,
    /// Bottom.
    pub bottom: f32,
}

impl Edges {
    /// The same value on all four sides.
    #[must_use]
    pub const fn all(v: f32) -> Self {
        Self {
            left: v,
            top: v,
            right: v,
            bottom: v,
        }
    }

    /// `h` left and right, `v` top and bottom.
    #[must_use]
    pub const fn symmetric(h: f32, v: f32) -> Self {
        Self {
            left: h,
            top: v,
            right: h,
            bottom: v,
        }
    }

    /// Total horizontal space.
    #[must_use]
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    /// Total vertical space.
    #[must_use]
    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }

    /// Total space along `dir`'s main axis.
    #[must_use]
    pub fn main(&self, dir: Direction) -> f32 {
        match dir {
            Direction::Row => self.horizontal(),
            Direction::Column => self.vertical(),
        }
    }

    /// Total space along `dir`'s cross axis.
    #[must_use]
    pub fn cross(&self, dir: Direction) -> f32 {
        match dir {
            Direction::Row => self.vertical(),
            Direction::Column => self.horizontal(),
        }
    }

    /// Offset of the content box's origin along the main axis.
    #[must_use]
    pub fn main_start(&self, dir: Direction) -> f32 {
        match dir {
            Direction::Row => self.left,
            Direction::Column => self.top,
        }
    }

    /// Offset of the content box's origin along the cross axis.
    #[must_use]
    pub fn cross_start(&self, dir: Direction) -> f32 {
        match dir {
            Direction::Row => self.top,
            Direction::Column => self.left,
        }
    }
}

/// Everything the layout pass needs to know about one widget.
///
/// It lives in the widget's arena slot rather than in the widget itself,
/// because the framework reads it for every widget on every pass and a
/// widget that forgot to expose it would simply not lay out.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutStyle {
    /// Stacking direction; only meaningful on a container.
    pub direction: Direction,
    /// Main-axis distribution of leftover space.
    pub main_align: MainAlign,
    /// Cross-axis placement of children.
    pub cross_align: CrossAlign,
    /// Space between adjacent children.
    pub gap: f32,
    /// Space inside this widget's box, around its children.
    pub padding: Edges,
    /// Space outside this widget's box, reserved by its parent.
    pub margin: Edges,
    /// Explicit width.
    pub width: Length,
    /// Explicit height.
    pub height: Length,
    /// Lower clamp on the resolved width.
    pub min_width: Option<f32>,
    /// Upper clamp on the resolved width.
    pub max_width: Option<f32>,
    /// Lower clamp on the resolved height.
    pub min_height: Option<f32>,
    /// Upper clamp on the resolved height.
    pub max_height: Option<f32>,
    /// Share of leftover main-axis space this widget takes.
    pub flex_grow: f32,
    /// Share of a main-axis overflow this widget gives back, weighted by
    /// its measured main size (as CSS does it).
    ///
    /// It only divides the overflow between the children that *can*
    /// shrink — the ones whose [`shrink_floor`](Self::shrink_floor) is
    /// [`ShrinkFloor::Zero`], plus any whose explicit `min_*` still
    /// leaves them room. `0.0` still means "never shrink at all".
    pub flex_shrink: f32,
    /// How far below its measured size this widget may be laid out.
    pub shrink_floor: ShrinkFloor,
}

impl Default for LayoutStyle {
    fn default() -> Self {
        Self {
            direction: Direction::Column,
            main_align: MainAlign::Start,
            cross_align: CrossAlign::Start,
            gap: 0.0,
            padding: Edges::default(),
            margin: Edges::default(),
            width: Length::Auto,
            height: Length::Auto,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            shrink_floor: ShrinkFloor::Content,
        }
    }
}

impl LayoutStyle {
    /// Clamp `size` to this style's min/max, ignoring `width`/`height`.
    #[must_use]
    pub fn clamp(&self, size: Size) -> Size {
        Size::new(
            clamp_opt(size.w, self.min_width, self.max_width),
            clamp_opt(size.h, self.min_height, self.max_height),
        )
    }
}

fn clamp_opt(v: f32, min: Option<f32>, max: Option<f32>) -> f32 {
    let mut v = v;
    if let Some(m) = max {
        v = v.min(m);
    }
    if let Some(m) = min {
        v = v.max(m);
    }
    v
}

/// The size range a parent offers a child.
///
/// `max` components may be [`f32::INFINITY`] for "as much as you like",
/// which is what an intrinsic measurement asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Constraints {
    /// Smallest acceptable size.
    pub min: Size,
    /// Largest acceptable size; components may be infinite.
    pub max: Size,
}

impl Constraints {
    /// Anything from zero to `max`.
    #[must_use]
    pub fn loose(max: Size) -> Self {
        Self {
            min: Size::ZERO,
            max,
        }
    }

    /// Exactly `size`.
    #[must_use]
    pub fn tight(size: Size) -> Self {
        Self {
            min: size,
            max: size,
        }
    }

    /// Unbounded in both axes: "what would you like to be?".
    #[must_use]
    pub fn unbounded() -> Self {
        Self {
            min: Size::ZERO,
            max: Size::new(f32::INFINITY, f32::INFINITY),
        }
    }

    /// Clamp `size` into the range.
    #[must_use]
    pub fn constrain(&self, size: Size) -> Size {
        Size::new(
            size.w.clamp(self.min.w, self.max.w.max(self.min.w)),
            size.h.clamp(self.min.h, self.max.h.max(self.min.h)),
        )
    }

    /// The same range with `min` dropped to zero.
    #[must_use]
    pub fn loosen(&self) -> Self {
        Self {
            min: Size::ZERO,
            max: self.max,
        }
    }

    /// Shrink the range by `edges` on both axes, never below zero.
    #[must_use]
    pub fn deflate(&self, edges: Edges) -> Self {
        let (h, v) = (edges.horizontal(), edges.vertical());
        Self {
            min: Size::new((self.min.w - h).max(0.0), (self.min.h - v).max(0.0)),
            max: Size::new(sub_max(self.max.w, h), sub_max(self.max.h, v)),
        }
    }
}

fn sub_max(v: f32, d: f32) -> f32 {
    if v.is_finite() { (v - d).max(0.0) } else { v }
}

/// One child, as the flex solver sees it.
#[derive(Debug, Clone)]
pub struct FlexItem {
    /// The child's margin, `flex_grow`, `flex_shrink` and cross clamps.
    pub style: LayoutStyle,
    /// The child's measured size, *excluding* its margin.
    pub basis: Size,
}

impl FlexItem {
    /// A child with the default style at a measured size.
    #[must_use]
    pub fn new(style: LayoutStyle, basis: Size) -> Self {
        Self { style, basis }
    }
}

/// The main-axis extent a set of children needs with no free space at
/// all: every child at its measured size, plus margins and gaps.
#[must_use]
pub fn intrinsic_main(container: &LayoutStyle, items: &[FlexItem]) -> f32 {
    let dir = container.direction;
    let mut total = 0.0;
    for it in items {
        total += dir.main(it.basis) + it.style.margin.main(dir);
    }
    total + gap_total(container.gap, items.len())
}

/// The cross-axis extent a set of children needs: the widest child plus
/// its margins.
#[must_use]
pub fn intrinsic_cross(container: &LayoutStyle, items: &[FlexItem]) -> f32 {
    let dir = container.direction;
    items.iter().fold(0.0f32, |acc, it| {
        acc.max(dir.cross(it.basis) + it.style.margin.cross(dir))
    })
}

fn gap_total(gap: f32, n: usize) -> f32 {
    if n > 1 { gap * (n - 1) as f32 } else { 0.0 }
}

/// Take a main-axis overflow back out of the children that can give it.
///
/// Returns the deficit still outstanding afterwards: zero when something
/// absorbed it, the full amount when nothing could. Split out of
/// [`solve`] because it is the one step with its own arithmetic worth
/// reading on its own.
///
/// Only the children that *can* give something back are weighted, so a
/// column of labels next to one scrollable child hands the whole deficit
/// to the child that can honestly absorb it instead of taking a
/// proportional bite out of every label first and clamping it back
/// afterwards — which would divide by a total that includes items
/// destined to give nothing.
///
/// The returned remainder is currently *not* load-bearing: `solve`
/// clamps the leftover at zero before positioning, so an unabsorbed
/// deficit is discarded either way and a version of this that lied and
/// returned zero would pass every test. It is returned honestly anyway,
/// because the alternative is a function whose signature says the
/// overflow was consumed when it was not.
fn shrink_to_fit(items: &[FlexItem], dir: Direction, main: &mut [f32], deficit: f32) -> f32 {
    let floors: Vec<f32> = items.iter().map(|it| floor_main(it, dir)).collect();
    let weights: Vec<f32> = items
        .iter()
        .zip(&*main)
        .zip(&floors)
        .map(|((it, m), floor)| {
            if *m > *floor {
                it.style.flex_shrink.max(0.0) * m
            } else {
                0.0
            }
        })
        .collect();
    let total: f32 = weights.iter().sum();
    if total <= 0.0 {
        return deficit;
    }
    for ((m, w), floor) in main.iter_mut().zip(&weights).zip(&floors) {
        *m = (*m - deficit * w / total).max(*floor);
    }
    0.0
}

/// The smallest main-axis size `it` may be laid out at.
///
/// The content floor is the item's own measured size — which, for a
/// container, already sums its children, so "a column is never shorter
/// than the rows inside it" needs no separate rule. An explicit `max_*`
/// caps it (an author who asked for a cap meant it), and an explicit
/// `min_*` larger than the content still wins, exactly as the final
/// clamp has always made it.
fn floor_main(it: &FlexItem, dir: Direction) -> f32 {
    let (min, max) = match dir {
        Direction::Row => (it.style.min_width, it.style.max_width),
        Direction::Column => (it.style.min_height, it.style.max_height),
    };
    let content = match it.style.shrink_floor {
        ShrinkFloor::Content => dir.main(it.basis),
        ShrinkFloor::Zero => 0.0,
    };
    let content = match max {
        Some(m) => content.min(m),
        None => content,
    };
    match min {
        Some(m) => m.max(content),
        None => content,
    }
}

/// Place `items` inside a container of `inner` **content-box** size.
///
/// `inner` is what is left after the container's own padding, and the
/// rects written to `out` are relative to the container's own origin —
/// padding is added back in here, so a caller never adds it twice. `out`
/// is cleared first and ends up with exactly one rect per item, which is
/// what lets the caller reuse one scratch vector for every container in
/// the tree.
///
/// The distribution is the usual two-pass one: every child starts at its
/// measured size, positive free space is divided by `flex_grow`, negative
/// free space is taken back weighted by `flex_shrink * basis`, then
/// [`MainAlign`] places whatever is still left over and [`CrossAlign`]
/// sizes and positions each child on the other axis.
///
/// The one place this departs from CSS's *defaults* rather than its
/// arithmetic is the shrink floor: a child is never laid out below its
/// own [`basis`](FlexItem::basis) unless it says it can
/// ([`ShrinkFloor::Zero`]), which is CSS's `min-size: auto` by another
/// name. An overflow no child will absorb is left as overflow — the
/// children run past the container's end and the parent (or the window)
/// clips them — rather than being squashed into text nobody can read.
pub fn solve(container: &LayoutStyle, inner: Size, items: &[FlexItem], out: &mut Vec<Rect>) {
    out.clear();
    if items.is_empty() {
        return;
    }
    let dir = container.direction;
    let inner_main = dir.main(inner);
    let inner_cross = dir.cross(inner);

    // 1. Every child at its measured main size.
    let mut main: Vec<f32> = items.iter().map(|it| dir.main(it.basis)).collect();
    let used: f32 = main
        .iter()
        .zip(items)
        .map(|(m, it)| m + it.style.margin.main(dir))
        .sum();
    let mut free = inner_main - used - gap_total(container.gap, items.len());

    // 2. Divide the free space.
    if free > 0.0 {
        let total: f32 = items.iter().map(|it| it.style.flex_grow.max(0.0)).sum();
        if total > 0.0 {
            for (m, it) in main.iter_mut().zip(items) {
                *m += free * it.style.flex_grow.max(0.0) / total;
            }
            free = 0.0;
        }
    } else if free < 0.0 {
        free = -shrink_to_fit(items, dir, &mut main, -free);
    }

    // Clamp to each child's own main-axis min/max, which can put free
    // space back on the table; that is a second-order effect CSS iterates
    // on and we do not.
    for (m, it) in main.iter_mut().zip(items) {
        *m = match dir {
            Direction::Row => clamp_opt(*m, it.style.min_width, it.style.max_width),
            Direction::Column => clamp_opt(*m, it.style.min_height, it.style.max_height),
        };
    }

    // 3. Position on the main axis.
    //
    // A negative leftover is overflow, not something to distribute. With
    // the content floor as the default it is also the *ordinary* case:
    // when nothing can shrink, every weight above is zero, `free` is
    // never zeroed and arrives here still negative. Spending it would
    // undo the floor's whole point — `Center` would halve it into a
    // cursor before the container's own origin, `End` would use it
    // whole, and `SpaceBetween` would divide it into a *negative gap*
    // that draws each child on top of the one before it. So the leftover
    // is clamped: whatever does not fit runs past the container's end
    // for the parent to clip, and `SpaceBetween` degenerates to `Start`,
    // which is what CSS does with negative free space too.
    let free = free.max(0.0);
    let n = items.len();
    let (mut cursor, between) = match container.main_align {
        MainAlign::Center => (free / 2.0, container.gap),
        MainAlign::End => (free, container.gap),
        MainAlign::SpaceBetween if n > 1 => (0.0, container.gap + free / (n - 1) as f32),
        // A single child has no gaps to spread the free space into, so
        // `SpaceBetween` degenerates to `Start`.
        MainAlign::Start | MainAlign::SpaceBetween => (0.0, container.gap),
    };
    cursor += container.padding.main_start(dir);

    let cross_origin = container.padding.cross_start(dir);
    for (i, it) in items.iter().enumerate() {
        let m_margin_start = match dir {
            Direction::Row => it.style.margin.left,
            Direction::Column => it.style.margin.top,
        };
        let c_margin_start = match dir {
            Direction::Row => it.style.margin.top,
            Direction::Column => it.style.margin.left,
        };
        let avail_cross = (inner_cross - it.style.margin.cross(dir)).max(0.0);
        let own_cross = dir.cross(it.basis);
        let cross_size = match container.cross_align {
            CrossAlign::Stretch => match dir {
                Direction::Row => clamp_opt(avail_cross, it.style.min_height, it.style.max_height),
                Direction::Column => clamp_opt(avail_cross, it.style.min_width, it.style.max_width),
            },
            _ => own_cross,
        };
        let cross_free = (avail_cross - cross_size).max(0.0);
        let cross_off = match container.cross_align {
            CrossAlign::Start | CrossAlign::Stretch => 0.0,
            CrossAlign::Center => cross_free / 2.0,
            CrossAlign::End => cross_free,
        };
        out.push(dir.rect(
            cursor + m_margin_start,
            cross_origin + c_margin_start + cross_off,
            main[i],
            cross_size,
        ));
        cursor += m_margin_start
            + main[i]
            + match dir {
                Direction::Row => it.style.margin.right,
                Direction::Column => it.style.margin.bottom,
            };
        if i + 1 < n {
            cursor += between;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout arithmetic is exact in binary (halves, integers), so an
    /// equality check is the honest one; `close` exists only because
    /// clippy's `float_cmp` cannot tell that from an accumulated sum.
    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    /// `assert_eq!` for floats, reporting both values on failure.
    macro_rules! assert_close {
        ($a:expr, $b:expr) => {
            assert!(close($a, $b), "{} != {}", $a, $b)
        };
        ($a:expr, $b:expr, $($rest:tt)*) => {
            assert!(close($a, $b), "{} != {}: {}", $a, $b, format_args!($($rest)*))
        };
    }

    fn item(basis: Size) -> FlexItem {
        FlexItem::new(LayoutStyle::default(), basis)
    }

    /// An item that may be laid out smaller than it measured — what a
    /// `List` or a `Scroll` is, and what every item was by default
    /// before the content floor landed.
    fn squashable(basis: Size) -> FlexItem {
        FlexItem::new(
            LayoutStyle {
                shrink_floor: ShrinkFloor::Zero,
                ..LayoutStyle::default()
            },
            basis,
        )
    }

    fn flexible(basis: Size, grow: f32) -> FlexItem {
        FlexItem::new(
            LayoutStyle {
                flex_grow: grow,
                ..LayoutStyle::default()
            },
            basis,
        )
    }

    fn column(gap: f32) -> LayoutStyle {
        LayoutStyle {
            direction: Direction::Column,
            gap,
            ..LayoutStyle::default()
        }
    }

    #[test]
    fn a_column_stacks_with_the_gap() {
        let style = column(8.0);
        let items = [item(Size::new(50.0, 20.0)), item(Size::new(30.0, 10.0))];
        let mut out = Vec::new();
        solve(&style, Size::new(100.0, 100.0), &items, &mut out);
        assert_eq!(out[0], Rect::new(0.0, 0.0, 50.0, 20.0));
        assert_eq!(out[1], Rect::new(0.0, 28.0, 30.0, 10.0));
        assert_close!(intrinsic_main(&style, &items), 38.0);
        assert_close!(intrinsic_cross(&style, &items), 50.0);
    }

    #[test]
    fn padding_offsets_the_content_box() {
        let style = LayoutStyle {
            padding: Edges::all(6.0),
            ..column(4.0)
        };
        let items = [item(Size::new(10.0, 10.0)), item(Size::new(10.0, 10.0))];
        let mut out = Vec::new();
        // `inner` is the content box: the caller has already subtracted
        // the padding from the container's own size.
        solve(&style, Size::new(88.0, 88.0), &items, &mut out);
        assert_eq!(out[0], Rect::new(6.0, 6.0, 10.0, 10.0));
        assert_eq!(out[1], Rect::new(6.0, 20.0, 10.0, 10.0));
    }

    #[test]
    fn grow_divides_the_free_space_in_proportion() {
        let style = column(0.0);
        let items = [
            flexible(Size::new(10.0, 10.0), 1.0),
            flexible(Size::new(10.0, 10.0), 3.0),
            item(Size::new(10.0, 10.0)),
        ];
        let mut out = Vec::new();
        solve(&style, Size::new(50.0, 110.0), &items, &mut out);
        // 110 - 30 = 80 free, split 1:3 → 20 and 60.
        assert_close!(out[0].h, 30.0);
        assert_close!(out[1].h, 70.0);
        assert_close!(out[2].h, 10.0);
        assert_close!(out[1].y, 30.0);
        assert_close!(out[2].y, 100.0);
    }

    #[test]
    fn shrink_is_weighted_by_the_measured_size() {
        let style = column(0.0);
        // Both items opt out of the content floor: this is the CSS
        // arithmetic, and the default floor is what stops it applying.
        let items = [
            squashable(Size::new(10.0, 40.0)),
            squashable(Size::new(10.0, 20.0)),
        ];
        let mut out = Vec::new();
        solve(&style, Size::new(50.0, 30.0), &items, &mut out);
        // 30 of overflow, weights 40:20 → 20 and 10 taken back.
        assert_close!(out[0].h, 20.0);
        assert_close!(out[1].h, 10.0);
        assert_close!(out[1].y, 20.0);
    }

    #[test]
    fn a_zero_shrink_child_keeps_its_size() {
        let fixed = LayoutStyle {
            flex_shrink: 0.0,
            shrink_floor: ShrinkFloor::Zero,
            ..LayoutStyle::default()
        };
        let items = [
            FlexItem::new(fixed, Size::new(10.0, 40.0)),
            squashable(Size::new(10.0, 20.0)),
        ];
        let mut out = Vec::new();
        solve(&column(0.0), Size::new(50.0, 50.0), &items, &mut out);
        assert_close!(out[0].h, 40.0);
        assert_close!(out[1].h, 10.0);
    }

    #[test]
    fn the_content_floor_holds_an_item_at_its_basis() {
        // A label beside a list, in a column 30 px short. The label
        // keeps every pixel it measured and the list — which is
        // honestly smaller when it is given less — takes the whole
        // deficit, even though by CSS weights the label would have
        // given back two thirds of it.
        let label = item(Size::new(10.0, 40.0));
        let list = squashable(Size::new(10.0, 60.0));
        let items = [label, list];
        let mut out = Vec::new();
        solve(&column(0.0), Size::new(50.0, 70.0), &items, &mut out);
        assert_close!(out[0].h, 40.0, "the label is not squashed");
        assert_close!(out[1].h, 30.0, "the list absorbs all 30 of it");
        assert_close!(out[1].y, 40.0);
    }

    #[test]
    fn an_explicit_min_larger_than_the_basis_still_wins() {
        // `min_height` above the measured size raises the floor; below
        // it, the content floor is the higher of the two and holds.
        let tall = LayoutStyle {
            min_height: Some(50.0),
            shrink_floor: ShrinkFloor::Zero,
            ..LayoutStyle::default()
        };
        let small_min = LayoutStyle {
            min_height: Some(5.0),
            ..LayoutStyle::default()
        };
        let items = [
            FlexItem::new(tall, Size::new(10.0, 20.0)),
            FlexItem::new(small_min, Size::new(10.0, 20.0)),
        ];
        let mut out = Vec::new();
        solve(&column(0.0), Size::new(50.0, 10.0), &items, &mut out);
        assert_close!(out[0].h, 50.0, "the explicit min beats the basis");
        assert_close!(out[1].h, 20.0, "the basis beats the smaller min");
    }

    #[test]
    fn a_max_below_the_basis_caps_the_content_floor() {
        // The third clamp in `floor_main` (`max(min, min(basis, max))`),
        // pinned through the one place it is observable.
        //
        // It is deliberately *not* asserted on the capped item's own
        // size: the final `clamp_opt` caps that to `max_height` whether
        // or not `floor_main` does, so asserting it would pass with the
        // cap deleted — coverage that only looks like coverage. What the
        // cap really decides is whether the item counts as *able to
        // shrink*, and that is visible in its **sibling**.
        //
        // A column 10 px short: A has a `Zero` floor and a basis of 40,
        // B measured 20 but is capped at 12. With the cap, B is above
        // its floor, so it is weighted and takes part of the deficit and
        // A keeps 33.33; without it, B's floor would be its full basis
        // of 20, B would be weightless, and A alone would absorb all 10
        // and come out at 30.
        let capped = LayoutStyle {
            max_height: Some(12.0),
            ..LayoutStyle::default()
        };
        let items = [
            squashable(Size::new(10.0, 40.0)),
            FlexItem::new(capped, Size::new(10.0, 20.0)),
        ];
        let mut out = Vec::new();
        solve(&column(0.0), Size::new(50.0, 50.0), &items, &mut out);
        assert_close!(out[1].h, 12.0, "the cap holds on the capped item");
        assert_close!(
            out[0].h,
            100.0 / 3.0,
            "the capped item was weighted as shrinkable, so its sibling \
             kept a share of the deficit rather than absorbing all of it"
        );
    }

    #[test]
    fn an_overflowing_column_runs_past_its_end_rather_than_squashing() {
        // Three rows of 26 in a 60 px column: 78 wanted, 18 short. Every
        // row keeps its height and the last one ends at 78 — outside the
        // container, for the parent (or the window) to clip. Squashing
        // would have made them 20 each and every glyph would have been
        // painted into a box too small for it.
        let items = [
            item(Size::new(10.0, 26.0)),
            item(Size::new(10.0, 26.0)),
            item(Size::new(10.0, 26.0)),
        ];
        let mut out = Vec::new();
        solve(&column(0.0), Size::new(50.0, 60.0), &items, &mut out);
        for (i, r) in out.iter().enumerate() {
            assert_close!(r.h, 26.0, "row {i} keeps its measured height");
        }
        assert_close!(out[2].y, 52.0);
        assert_close!(out[2].bottom(), 78.0);
        assert!(
            out[2].bottom() > 60.0,
            "the surplus is overflow, not a smaller row"
        );
        // And the rows do not overlap: each starts where the last ended.
        assert_close!(out[1].y, out[0].bottom());
        assert_close!(out[2].y, out[1].bottom());
    }

    #[test]
    fn an_overflowing_container_never_places_a_child_before_its_start() {
        // The leftover is spent from the start edge or not at all.
        //
        // With the content floor as the default, the ordinary overflow
        // case leaves *nothing* that can shrink — every item sits at its
        // floor, every weight is zero — so `free` stays negative and
        // reaches the alignment arithmetic, which before this clamp
        // turned it into position: `Center` halved it into a negative
        // cursor, `End` used it whole, and `SpaceBetween` divided it into
        // a *negative gap* that drew each child `|free| / (n - 1)` on top
        // of the one before it.
        //
        // That is the exact failure this floor exists to remove — a row
        // written through a sentence the user is still reading — and the
        // floor is what made the path reachable: with the old default
        // shrink of 1.0 something always absorbed the deficit and `free`
        // was zeroed before it got here. `main_align_places_the_leftover`
        // only ever offered positive free space, so nothing caught it.
        //
        // Three rows of 26 in a 60 px column: 78 wanted, 18 over.
        let items = [
            item(Size::new(10.0, 26.0)),
            item(Size::new(10.0, 26.0)),
            item(Size::new(10.0, 26.0)),
        ];
        let mut out = Vec::new();
        for align in [
            MainAlign::Start,
            MainAlign::Center,
            MainAlign::End,
            MainAlign::SpaceBetween,
        ] {
            let style = LayoutStyle {
                main_align: align,
                ..column(0.0)
            };
            solve(&style, Size::new(50.0, 60.0), &items, &mut out);
            assert!(
                out[0].y >= -1e-4,
                "{align:?} placed the first child at {} — before the \
                 container's own origin, i.e. on top of whatever \
                 precedes it",
                out[0].y,
            );
            for i in 1..out.len() {
                assert!(
                    out[i].y >= out[i - 1].bottom() - 1e-4,
                    "{align:?} overlapped row {i} at {} with row {} \
                     ending at {}",
                    out[i].y,
                    i - 1,
                    out[i - 1].bottom(),
                );
            }
            // Overflow is still overflow: clamping the leftover must not
            // have quietly shrunk anything to make it fit.
            for (i, r) in out.iter().enumerate() {
                assert_close!(r.h, 26.0, "{align:?} row {i}");
            }
            assert!(
                out[2].bottom() > 60.0,
                "{align:?} still runs past the container's end: {}",
                out[2].bottom(),
            );
        }
    }

    #[test]
    fn a_container_never_ends_before_the_children_it_holds() {
        // The rule #561 asked to be confirmed rather than assumed. A
        // container's basis is its own intrinsic size, which already sums
        // its children — so giving it the content floor is all it takes
        // for its children to stay inside it. Here: an inner column of
        // three 26 px rows (intrinsic 78) as the second child of a root
        // that is 40 px short. The inner column is laid out at 78 and
        // the rows it re-solves at that height all fit inside.
        let inner_style = column(0.0);
        let rows = [
            item(Size::new(10.0, 26.0)),
            item(Size::new(10.0, 26.0)),
            item(Size::new(10.0, 26.0)),
        ];
        let inner_basis = Size::new(
            intrinsic_cross(&inner_style, &rows),
            intrinsic_main(&inner_style, &rows),
        );
        assert_close!(inner_basis.h, 78.0);

        let outer = [
            item(Size::new(10.0, 20.0)),
            FlexItem::new(inner_style.clone(), inner_basis),
        ];
        let mut out = Vec::new();
        solve(&column(0.0), Size::new(50.0, 58.0), &outer, &mut out);
        let column_rect = out[1];
        assert_close!(column_rect.h, 78.0, "the column keeps its intrinsic height");

        // Now solve the column's own children in the height it was given.
        let mut inner_out = Vec::new();
        solve(
            &inner_style,
            Size::new(column_rect.w, column_rect.h),
            &rows,
            &mut inner_out,
        );
        let last = inner_out[2];
        assert_close!(last.bottom(), 78.0);
        assert!(
            last.bottom() <= column_rect.h + 1e-4,
            "the last row ends at {} inside a column of {}",
            last.bottom(),
            column_rect.h
        );
    }

    #[test]
    fn main_align_places_the_leftover() {
        let items = [item(Size::new(10.0, 10.0)), item(Size::new(10.0, 10.0))];
        let mut out = Vec::new();
        let cases = [
            (MainAlign::Start, 0.0, 10.0),
            (MainAlign::Center, 40.0, 50.0),
            (MainAlign::End, 80.0, 90.0),
            (MainAlign::SpaceBetween, 0.0, 90.0),
        ];
        for (align, first, second) in cases {
            let style = LayoutStyle {
                main_align: align,
                ..column(0.0)
            };
            solve(&style, Size::new(50.0, 100.0), &items, &mut out);
            assert_close!(out[0].y, first, "{align:?}");
            assert_close!(out[1].y, second, "{align:?}");
        }
    }

    #[test]
    fn cross_align_sizes_and_positions() {
        let items = [item(Size::new(20.0, 10.0))];
        let mut out = Vec::new();
        let cases = [
            (CrossAlign::Start, 0.0, 20.0),
            (CrossAlign::Center, 40.0, 20.0),
            (CrossAlign::End, 80.0, 20.0),
            (CrossAlign::Stretch, 0.0, 100.0),
        ];
        for (align, x, w) in cases {
            let style = LayoutStyle {
                cross_align: align,
                ..column(0.0)
            };
            solve(&style, Size::new(100.0, 100.0), &items, &mut out);
            assert_close!(out[0].x, x, "{align:?}");
            assert_close!(out[0].w, w, "{align:?}");
        }
    }

    #[test]
    fn a_row_is_a_column_with_the_axes_swapped() {
        let style = LayoutStyle {
            direction: Direction::Row,
            gap: 5.0,
            ..LayoutStyle::default()
        };
        let items = [item(Size::new(20.0, 10.0)), item(Size::new(30.0, 40.0))];
        let mut out = Vec::new();
        solve(&style, Size::new(100.0, 50.0), &items, &mut out);
        assert_eq!(out[0], Rect::new(0.0, 0.0, 20.0, 10.0));
        assert_eq!(out[1], Rect::new(25.0, 0.0, 30.0, 40.0));
        assert_close!(intrinsic_main(&style, &items), 55.0);
        assert_close!(intrinsic_cross(&style, &items), 40.0);
    }

    #[test]
    fn margins_are_reserved_outside_the_child() {
        let style = LayoutStyle {
            margin: Edges::all(4.0),
            ..LayoutStyle::default()
        };
        let items = [
            FlexItem::new(style, Size::new(10.0, 10.0)),
            item(Size::new(10.0, 10.0)),
        ];
        let mut out = Vec::new();
        solve(&column(0.0), Size::new(50.0, 100.0), &items, &mut out);
        assert_eq!(out[0], Rect::new(4.0, 4.0, 10.0, 10.0));
        assert_close!(out[1].y, 18.0);
        assert_close!(intrinsic_main(&column(0.0), &items), 28.0);
    }

    #[test]
    fn constraints_deflate_and_constrain() {
        let c = Constraints::loose(Size::new(100.0, 50.0));
        assert_eq!(c.constrain(Size::new(200.0, 10.0)), Size::new(100.0, 10.0));
        let d = c.deflate(Edges::all(10.0));
        assert_eq!(d.max, Size::new(80.0, 30.0));
        let u = Constraints::unbounded().deflate(Edges::all(10.0));
        assert!(u.max.w.is_infinite());
        assert_eq!(
            Constraints::tight(Size::new(5.0, 5.0)).min,
            Size::new(5.0, 5.0)
        );
    }

    #[test]
    fn lengths_resolve_against_the_parent() {
        assert_eq!(Length::Auto.resolve(100.0), None);
        assert_eq!(Length::Px(12.0).resolve(100.0), Some(12.0));
        assert_eq!(Length::Percent(0.25).resolve(100.0), Some(25.0));
        assert_eq!(Length::Percent(0.25).resolve(f32::INFINITY), None);
    }

    #[test]
    fn min_and_max_clamp_the_resolved_size() {
        let style = LayoutStyle {
            min_width: Some(20.0),
            max_height: Some(5.0),
            ..LayoutStyle::default()
        };
        assert_eq!(style.clamp(Size::new(10.0, 10.0)), Size::new(20.0, 5.0));
    }

    #[test]
    fn edges_report_their_axes() {
        let e = Edges::symmetric(3.0, 5.0);
        assert_close!(e.horizontal(), 6.0);
        assert_close!(e.vertical(), 10.0);
        assert_close!(e.main(Direction::Row), 6.0);
        assert_close!(e.cross(Direction::Row), 10.0);
        assert_close!(e.main(Direction::Column), 10.0);
        assert_close!(e.cross(Direction::Column), 6.0);
        assert_close!(e.main_start(Direction::Row), 3.0);
        assert_close!(e.cross_start(Direction::Row), 5.0);
        assert_close!(e.main_start(Direction::Column), 5.0);
        assert_close!(e.cross_start(Direction::Column), 3.0);
    }

    #[test]
    fn directions_project_and_rebuild() {
        let s = Size::new(10.0, 20.0);
        assert_close!(Direction::Row.main(s), 10.0);
        assert_close!(Direction::Row.cross(s), 20.0);
        assert_close!(Direction::Column.main(s), 20.0);
        assert_close!(Direction::Column.cross(s), 10.0);
        assert_eq!(Direction::Row.size(1.0, 2.0), Size::new(1.0, 2.0));
        assert_eq!(Direction::Column.size(1.0, 2.0), Size::new(2.0, 1.0));
        assert_eq!(
            Direction::Column.rect(1.0, 2.0, 3.0, 4.0),
            Rect::new(2.0, 1.0, 4.0, 3.0)
        );
    }

    #[test]
    fn no_children_is_no_rects() {
        let mut out = vec![Rect::new(1.0, 1.0, 1.0, 1.0)];
        solve(&column(4.0), Size::new(10.0, 10.0), &[], &mut out);
        assert!(out.is_empty(), "the output is always cleared first");
        assert_close!(intrinsic_main(&column(4.0), &[]), 0.0);
        assert_close!(intrinsic_cross(&column(4.0), &[]), 0.0);
    }
}

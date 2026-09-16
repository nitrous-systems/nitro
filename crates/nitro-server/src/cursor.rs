//! The software cursor: a set of 24×24 ARGB shapes the server paints over
//! the composited scene, last, every frame that shows one.
//!
//! M1 has no hardware cursor plane. That is a deliberate simplification —
//! a KMS cursor plane is a second scanout surface with its own format,
//! size and position constraints per driver, and it buys latency we do not
//! yet measure. It also has a property that makes it actively unhelpful
//! during bring-up: the plane is composited by the display engine, not
//! into the framebuffer, so it does **not** appear in a screenshot taken
//! by dumping the back buffer (see `docs/testbox.md`). Every visual test
//! run over SSH would lose the pointer exactly when it matters — when
//! checking that the pointer is where the input stack thinks it is.
//!
//! So the cursor is just another paint: an image blitted after the scene,
//! inside the frame's damage clip, with the same source-over blend as any
//! other surface. The cost is one blit per frame plus the damage of the
//! previous and current cursor rects; the benefit is that what a
//! screenshot shows is what the display shows, and that there is one
//! composition path rather than two.
//!
//! The bitmaps are compiled in rather than loaded: a cursor theme is a
//! file format, a search path and a fallback policy, none of which belong
//! in the server's first milestones. Each [`Shape`]'s mask is ASCII art —
//! `#` outline, `.` interior, space transparent — converted once, in
//! [`Cursor::new`], into the straight-alpha `[B, G, R, A]` bytes that
//! [`nitro_raster::PixelFormat::Argb8888`] expects. Editing a cursor means
//! editing the art, which is the point.
//!
//! # The shapes, and who chooses them
//!
//! [`Shape::Arrow`] is the X11 `left_ptr`: tip at the top-left corner, a
//! vertical left edge, a diagonal right edge down to the shoulder, and a
//! two-pixel tail running down and to the right at exactly 45°. The
//! transcription was checked against `/usr/share/icons/Adwaita/cursors/
//! left_ptr` (X11's own cursor, public domain in shape if not in any one
//! file) by decoding its 24 px frame and comparing the silhouette; nothing
//! is vendored, the art here is drawn from the geometry.
//!
//! The other five are **resize and move** shapes the *server* picks, from
//! the same `frame_hit` per motion event that already drives the resize
//! hint and the button hover: a band resolves to the shape for the edges
//! it pulls, a title drag in flight to [`Shape::Move`], and everything
//! else to the arrow. Clients cannot ask for a shape — that is a
//! `SetCursor` wire message, deferred to M5 (`docs/wire.md`).
//!
//! # Not themed, and deliberately
//!
//! A cursor is black-outlined white in **both** schemes. Everything else
//! on the desktop takes its colour from a palette role, so the exception
//! wants an argument: the pointer is the one thing that must stay legible
//! over content the desktop does not control — a photo, a terminal, a
//! client's own black window — and a dark-scheme cursor inverted to
//! white-on-black would vanish against exactly the dark content the dark
//! scheme exists for. White fill with a black outline reads on both, which
//! is why every desktop since the Lisa has shipped it and why X11 has no
//! themed-by-scheme cursor either. So there is no role for it, and
//! `deploy/lint-colors.sh` sees no colour construction here: the mask is
//! bytes, and the two values it maps to are `BLACK` and `WHITE`.
//!
//! # Scale
//!
//! The cursor is painted in **device** pixels, so on a 2× output a 24 px
//! arrow would be physically half the size it is at 1×. It is therefore
//! painted at `round(scale)`× with nearest-neighbour sampling — crisp and
//! period-correct, every source pixel becoming an exact n×n block — so a
//! 2× output gets a 48-device-pixel cursor and the same physical size.
//! [`Cursor::rect_scaled`] is what the damage follows.

use nitro_core::{Color, IRect, Rect};
use nitro_raster::{Canvas, Image, PixelFormat};

/// Width and height of a cursor image in **logical** pixels: the side of
/// the ASCII art, and the device side at scale 1.
pub const CURSOR_SIZE: i32 = 24;

/// Bytes per ARGB8888 pixel.
const BPP: usize = 4;

/// [`BPP`] as the `u32` the image descriptor's stride wants. A separate
/// constant rather than a conversion, so the one arithmetic fact is
/// stated once and nothing on the paint path can fail.
const BPP_U32: u32 = 4;

/// One cursor's art: [`CURSOR_SIZE`] rows of [`CURSOR_SIZE`] bytes.
type Mask = [&'static [u8; 24]; 24];

/// Which cursor the pointer is showing.
///
/// The server chooses, from the frame region under the pointer; there is
/// no client-facing request for one yet. The order is the order of
/// [`Shape::ALL`], which is the order the masks are stored in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Shape {
    /// The ordinary pointer: `left_ptr`, hotspot at its tip.
    #[default]
    Arrow,
    /// Left or right edge: a horizontal double arrow.
    SizeHor,
    /// Top or bottom edge: a vertical double arrow.
    SizeVer,
    /// Top-left / bottom-right corner: a `╲` double arrow.
    SizeFDiag,
    /// Top-right / bottom-left corner: a `╱` double arrow.
    SizeBDiag,
    /// A title-bar drag in flight: the four-way move cross.
    Move,
}

impl Shape {
    /// Every shape, in storage order. `Cursor` holds one converted mask
    /// per entry.
    pub const ALL: [Self; 6] = [
        Self::Arrow,
        Self::SizeHor,
        Self::SizeVer,
        Self::SizeFDiag,
        Self::SizeBDiag,
        Self::Move,
    ];

    /// Index into [`Shape::ALL`] and into `Cursor`'s mask array.
    #[must_use]
    pub fn index(self) -> usize {
        self as usize
    }

    /// The shape for a resize band pulling these edges.
    ///
    /// A corner names two edges and takes the diagonal that runs through
    /// it; a lone edge takes the axis it moves along. `Edges::NONE` is not
    /// a resize at all and answers [`Shape::Arrow`], which is what a caller
    /// that filtered nothing out would want anyway.
    #[must_use]
    pub fn for_edges(edges: crate::wm::Edges) -> Self {
        match (edges.left, edges.right, edges.top, edges.bottom) {
            // The two diagonals, named for the direction they point:
            // `fdiag` is `╲` (top-left ↔ bottom-right), `bdiag` is `╱`.
            (true, _, true, _) | (_, true, _, true) => Self::SizeFDiag,
            (true, _, _, true) | (_, true, true, _) => Self::SizeBDiag,
            (true, _, _, _) | (_, true, _, _) => Self::SizeHor,
            (_, _, true, _) | (_, _, _, true) => Self::SizeVer,
            _ => Self::Arrow,
        }
    }

    /// Where this shape's hotspot sits inside its image, in **logical**
    /// pixels of the art.
    ///
    /// The arrow's is its tip, at `(0, 0)`, which is what makes its
    /// covered rect simply the square at the pointer position. Every other
    /// shape is symmetric about its middle and is centred, because a
    /// double arrow that resized from its corner would point at one edge
    /// while grabbing another.
    #[must_use]
    pub fn hotspot(self) -> (i32, i32) {
        match self {
            Self::Arrow => (0, 0),
            _ => (CURSOR_SIZE / 2, CURSOR_SIZE / 2),
        }
    }

    /// The art for this shape.
    fn mask(self) -> &'static Mask {
        match self {
            Self::Arrow => &ARROW,
            Self::SizeHor => &SIZE_HOR,
            Self::SizeVer => &SIZE_VER,
            Self::SizeFDiag => &SIZE_FDIAG,
            Self::SizeBDiag => &SIZE_BDIAG,
            Self::Move => &MOVE,
        }
    }
}

/// Where the arrow's tip sits inside the image (hotspot), in pixels.
///
/// Kept as a constant because it is `Shape::Arrow`'s hotspot and the
/// arrow is the default; every other shape asks [`Shape::hotspot`].
pub const HOTSPOT: (i32, i32) = (0, 0);

/// The ordinary pointer: X11's `left_ptr`.
///
/// Tip at `(0, 0)`, a vertical left edge sixteen pixels long, a diagonal
/// right edge down to the shoulder, and a **two-pixel tail at exactly
/// 45°** — each row of the tail is shifted one column from the row above,
/// which `the_tails_run_is_a_forty_five_degree_diagonal` pins. The old art
/// had a four-pixel-wide tail leaving the notch three columns too far
/// right, which read as a check mark rather than an arrow; that is what
/// the box reported as "the mouse pointer tail is weird (off angle)".
const ARROW: Mask = [
    b"#                       ",
    b"##                      ",
    b"#.#                     ",
    b"#..#                    ",
    b"#...#                   ",
    b"#....#                  ",
    b"#.....#                 ",
    b"#......#                ",
    b"#.......#               ",
    b"#........#              ",
    b"#.........#             ",
    b"#..........#            ",
    b"#......#####            ",
    b"#..#..#                 ",
    b"#.# #..#                ",
    b"##   #..#               ",
    b"#     #..#              ",
    b"       #..#             ",
    b"        #..#            ",
    b"         #..#           ",
    b"          #..#          ",
    b"           ####         ",
    b"                        ",
    b"                        ",
];

/// A left/right edge: a horizontal double arrow, centred hotspot.
const SIZE_HOR: Mask = [
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"     ##          ##     ",
    b"    ###          ###    ",
    b"   ##.############.##   ",
    b"  ##................##  ",
    b"  ##................##  ",
    b"   ##.############.##   ",
    b"    ###          ###    ",
    b"     ##          ##     ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
];

/// A top/bottom edge: a vertical double arrow, centred hotspot.
const SIZE_VER: Mask = [
    b"                        ",
    b"                        ",
    b"           ##           ",
    b"          ####          ",
    b"         ##..##         ",
    b"        ##....##        ",
    b"        ###..###        ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"        ###..###        ",
    b"        ##....##        ",
    b"         ##..##         ",
    b"          ####          ",
    b"           ##           ",
    b"                        ",
    b"                        ",
];

/// A top-left / bottom-right corner: the `╲` double arrow.
const SIZE_FDIAG: Mask = [
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"     #####              ",
    b"     #...#              ",
    b"     #...#              ",
    b"     #...##             ",
    b"     ####.##            ",
    b"        ##.##           ",
    b"         ##.##          ",
    b"          ##.##         ",
    b"           ##.##        ",
    b"            ##.####     ",
    b"             ##...#     ",
    b"              #...#     ",
    b"              #...#     ",
    b"              #####     ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
];

/// A top-right / bottom-left corner: the `╱` double arrow.
const SIZE_BDIAG: Mask = [
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"              #####     ",
    b"              #...#     ",
    b"              #...#     ",
    b"             ##...#     ",
    b"            ##.####     ",
    b"           ##.##        ",
    b"          ##.##         ",
    b"         ##.##          ",
    b"        ##.##           ",
    b"     ####.##            ",
    b"     #...##             ",
    b"     #...#              ",
    b"     #...#              ",
    b"     #####              ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
];

/// A title-bar drag in flight: the four-way move cross.
const MOVE: Mask = [
    b"                        ",
    b"                        ",
    b"           ##           ",
    b"          ####          ",
    b"         ##..##         ",
    b"         ######         ",
    b"           ##           ",
    b"           ##           ",
    b"           ##           ",
    b"    ##     ##     ##    ",
    b"   ###     ##     ###   ",
    b"  ##.##############.##  ",
    b"  ##.##############.##  ",
    b"   ###     ##     ###   ",
    b"    ##     ##     ##    ",
    b"           ##           ",
    b"           ##           ",
    b"           ##           ",
    b"         ######         ",
    b"         ##..##         ",
    b"          ####          ",
    b"           ##           ",
    b"                        ",
    b"                        ",
];

/// Every cursor's ARGB8888 pixels, built once.
///
/// One allocation per shape, at startup: `6 × 24 × 24 × 4` = **13 824
/// bytes**, noted in `docs/budget.md`. The value is immutable afterwards,
/// so painting never allocates and never touches a mask again — which is
/// what makes a shape change cost the two damage rects and nothing else.
#[derive(Debug, Clone)]
pub struct Cursor {
    /// `[B, G, R, A]` per pixel, straight alpha, row-major, tightly
    /// packed; one entry per [`Shape::ALL`], in that order.
    shapes: Vec<Vec<u8>>,
}

impl Cursor {
    /// Build every cursor bitmap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shapes: Shape::ALL.iter().map(|s| convert(s.mask())).collect(),
        }
    }

    /// The device-pixel rectangle [`Shape::Arrow`] covers at scale 1 with
    /// its hotspot at `(x, y)` device pixels.
    ///
    /// The frame loop damages this rect at the old and the new pointer
    /// position, which is why it is a free function of the position rather
    /// than a method: the old rect outlives no `Cursor` borrow. See
    /// [`Cursor::rect_scaled`] for the shape- and scale-aware form; this
    /// is it with the two defaults, kept because "the arrow at 1×" is what
    /// most call sites and every existing test mean.
    #[must_use]
    pub fn rect(x: i32, y: i32) -> IRect {
        Self::rect_scaled(x, y, Shape::Arrow, 1)
    }

    /// The device-pixel rectangle `shape` covers at `scale`× with its
    /// hotspot at `(x, y)` device pixels.
    ///
    /// `scale` is the integer factor the cursor is magnified by — the
    /// output's scale rounded, at least 1 — so the side is `24 * scale`
    /// and the hotspot offset scales with it. Both the paint and the
    /// damage go through here, so they cannot disagree about which pixels
    /// a cursor owns.
    #[must_use]
    pub fn rect_scaled(x: i32, y: i32, shape: Shape, scale: i32) -> IRect {
        let scale = scale.max(1);
        let side = CURSOR_SIZE * scale;
        let (hx, hy) = shape.hotspot();
        IRect::new(x - hx * scale, y - hy * scale, side, side)
    }

    /// The integer magnification a cursor is painted at on an output of
    /// logical-to-device `scale`.
    ///
    /// Rounded, and never below 1: the blit is nearest-neighbour, so a
    /// whole factor is the only one that puts every source pixel on an
    /// exact block of device pixels. A fractional factor would resample
    /// the outline into grey mush — the one thing a cursor cannot afford,
    /// since its legibility *is* the black outline.
    #[must_use]
    pub fn paint_scale(scale: f32) -> i32 {
        if scale.is_finite() && scale >= 1.0 {
            // `round` on a finite f32 in this range is exact.
            #[allow(clippy::cast_possible_truncation)]
            let s = scale.round() as i32;
            s.max(1)
        } else {
            1
        }
    }

    /// Paint `shape` at `scale`× with its hotspot at `(x, y)`, clipped to
    /// `clip`.
    ///
    /// The destination is the rect from [`Cursor::rect_scaled`], at
    /// integer coordinates.
    ///
    /// At `scale == 1` the mapping is 1:1 and integer-aligned, so
    /// [`Canvas::blit`] takes its nearest-neighbour path: one source pixel
    /// per destination pixel, no filtering, and the fast loop.
    ///
    /// Above that it does **not** use `blit`, and that is the whole point.
    /// `Canvas::blit`'s scaled path is *bilinear*, which on a cursor is
    /// exactly wrong: a 1-px black outline resampled with its transparent
    /// neighbours comes out as a 2-px grey smear, and the outline is the
    /// entire reason a white arrow is legible on a white background. So a
    /// magnified cursor is painted as **blocks** — each source pixel a
    /// solid `scale × scale` rectangle, which is nearest-neighbour
    /// sampling written out. It is affordable because the art is two
    /// opaque colours and nothing else: equal-coloured pixels within a row
    /// coalesce into one fill, so a 24-row arrow is a few dozen
    /// [`Canvas::fill_irect`] calls rather than 576.
    pub fn paint(
        &self,
        canvas: &mut Canvas<'_>,
        clip: &IRect,
        x: i32,
        y: i32,
        shape: Shape,
        scale: i32,
    ) {
        let scale = scale.max(1);
        let r = Self::rect_scaled(x, y, shape, scale);
        if scale == 1 {
            #[allow(clippy::cast_precision_loss)] // device pixels, far inside f32's exact range
            let dst = Rect::new(r.x as f32, r.y as f32, r.w as f32, r.h as f32);
            let side = CURSOR_SIZE.cast_unsigned();
            let image = Image {
                data: &self.shapes[shape.index()],
                width: side,
                height: side,
                stride: side * BPP_U32,
                format: PixelFormat::Argb8888,
            };
            let src = IRect::new(0, 0, CURSOR_SIZE, CURSOR_SIZE);
            canvas.blit(clip, &dst, &image, &src, 1.0);
            return;
        }
        self.paint_blocks(canvas, clip, &r, shape, scale);
    }

    /// Nearest-neighbour magnification: every source pixel as a solid
    /// `scale × scale` block, runs of one colour coalesced per row.
    fn paint_blocks(
        &self,
        canvas: &mut Canvas<'_>,
        clip: &IRect,
        dst: &IRect,
        shape: Shape,
        scale: i32,
    ) {
        let pixels = &self.shapes[shape.index()];
        // Indexed as `i32` throughout: the art is 24 × 24, so every index
        // and every run length is far inside the range, and the only
        // arithmetic here is the block rectangle's, which is `i32`.
        for row in 0..CURSOR_SIZE {
            let mut col = 0;
            while col < CURSOR_SIZE {
                let at = |c: i32| -> &[u8] {
                    let off = (row * CURSOR_SIZE + c) as usize * BPP;
                    &pixels[off..off + BPP]
                };
                let px = at(col);
                // Transparent pixels are skipped rather than blended: the
                // art has no partial alpha at all, so "skip" and "blend a
                // zero" are the same pixel and one of them is free.
                let mut run = 1;
                while col + run < CURSOR_SIZE && at(col + run) == px {
                    run += 1;
                }
                if px[3] != 0 {
                    let block =
                        IRect::new(dst.x + col * scale, dst.y + row * scale, run * scale, scale);
                    // Reading a colour back out of the converted mask,
                    // which is stored `[B, G, R, A]` with straight alpha.
                    // Not a choice of colour: the two the art can hold
                    // were chosen in `convert`, from `Color::BLACK` and
                    // `Color::WHITE`, and the module docs say why a
                    // cursor has no palette role.
                    //
                    // lint-colors: allow — reconstructs a stored pixel, it does not pick one
                    canvas.fill_irect(clip, &block, Color::rgba(px[2], px[1], px[0], px[3]));
                }
                col += run;
            }
        }
    }
}

/// Turn one mask into straight-alpha `[B, G, R, A]` bytes.
fn convert(mask: &Mask) -> Vec<u8> {
    let side = CURSOR_SIZE as usize;
    debug_assert_eq!(mask.len(), side, "mask must have CURSOR_SIZE rows");
    let mut pixels = vec![0u8; side * side * BPP];
    for (y, row) in mask.iter().enumerate() {
        for (x, cell) in row.iter().enumerate() {
            // Outline is opaque black, interior opaque white, and anything
            // else fully transparent. Straight alpha: the colour of a
            // transparent pixel is irrelevant, but zero keeps the buffer
            // boring to look at in a hex dump.
            //
            // `Color::BLACK`/`WHITE` rather than a role: see the module
            // docs on why a cursor is not themed.
            let argb = match cell {
                b'#' => bgra(Color::BLACK),
                b'.' => bgra(Color::WHITE),
                _ => [0x00, 0x00, 0x00, 0x00],
            };
            let off = (y * side + x) * BPP;
            pixels[off..off + BPP].copy_from_slice(&argb);
        }
    }
    pixels
}

/// One opaque colour as the `[B, G, R, A]` bytes `Argb8888` wants.
fn bgra(c: Color) -> [u8; 4] {
    [c.b, c.g, c.r, 0xFF]
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{CURSOR_SIZE, Cursor, HOTSPOT, Shape};
    use crate::wm::Edges;
    use nitro_core::IRect;
    use nitro_raster::Canvas;

    /// Colour every pixel of a canvas buffer with, so that "did anything
    /// outside the clip change?" is a byte comparison.
    const SENTINEL: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];

    fn pixel(c: &Cursor, shape: Shape, x: usize, y: usize) -> [u8; 4] {
        let off = (y * CURSOR_SIZE as usize + x) * 4;
        c.shapes[shape.index()][off..off + 4]
            .try_into()
            .expect("4 bytes")
    }

    /// The art for a shape, as rows of `#`/`.`/space, read back out of the
    /// converted pixels — so a test reasons about what will be *painted*
    /// rather than about the source text.
    fn rows(c: &Cursor, shape: Shape) -> Vec<String> {
        (0..CURSOR_SIZE as usize)
            .map(|y| {
                (0..CURSOR_SIZE as usize)
                    .map(|x| match pixel(c, shape, x, y) {
                        [_, _, _, 0] => ' ',
                        [0, 0, 0, _] => '#',
                        _ => '.',
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn every_shape_has_the_documented_size() {
        let c = Cursor::new();
        assert_eq!(c.shapes.len(), Shape::ALL.len());
        for s in Shape::ALL {
            assert_eq!(
                c.shapes[s.index()].len(),
                (CURSOR_SIZE * CURSOR_SIZE * 4) as usize,
                "{s:?}"
            );
        }
        // The figure `docs/budget.md` carries.
        let bytes: usize = c.shapes.iter().map(Vec::len).sum();
        assert_eq!(bytes, 13_824);
    }

    #[test]
    fn tip_pixel_is_opaque() {
        let c = Cursor::new();
        let (hx, hy) = HOTSPOT;
        assert_eq!(pixel(&c, Shape::Arrow, hx as usize, hy as usize)[3], 0xFF);
        // The tip is outline, so black.
        assert_eq!(pixel(&c, Shape::Arrow, 0, 0), [0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn outside_the_arrow_is_transparent() {
        let c = Cursor::new();
        // Top-right corner: as far from the arrow as the image gets.
        assert_eq!(pixel(&c, Shape::Arrow, 23, 0)[3], 0x00);
        // Bottom-left corner, below where the left edge ends.
        assert_eq!(pixel(&c, Shape::Arrow, 0, 23)[3], 0x00);
    }

    #[test]
    fn interior_is_white() {
        let c = Cursor::new();
        // Row 6 is `#.....#`: column 3 is interior.
        assert_eq!(pixel(&c, Shape::Arrow, 3, 6), [0xFF, 0xFF, 0xFF, 0xFF]);
    }

    /// #3724's first report: "the mouse pointer tail is weird (off
    /// angle)". The old art ran a four-pixel-wide tail out of the notch
    /// three columns too far right, which reads as a check mark.
    ///
    /// A `left_ptr`'s tail is **two pixels wide at exactly 45°**, so each
    /// row of it starts one column further right than the row above. That
    /// is a property of the art a test can hold, and it is the one that
    /// was wrong.
    #[test]
    fn the_tails_run_is_a_forty_five_degree_diagonal() {
        let c = Cursor::new();
        let art = rows(&c, Shape::Arrow);
        // The tail is everything below the shoulder that is right of the
        // arrow's left edge: rows where the leftmost ink is not column 0.
        let starts: Vec<(usize, usize)> = art
            .iter()
            .enumerate()
            .filter_map(|(y, row)| row.find(|ch| ch != ' ').map(|x| (y, x)))
            .filter(|(_, x)| *x > 0)
            .collect();
        assert!(starts.len() >= 5, "no tail rows found in {art:#?}");
        for w in starts.windows(2) {
            let ((y0, x0), (y1, x1)) = (w[0], w[1]);
            assert_eq!(y1, y0 + 1, "the tail's rows are contiguous");
            assert_eq!(
                x1,
                x0 + 1,
                "row {y1} of the tail starts at column {x1}, not {}",
                x0 + 1
            );
        }
        // And it is two pixels wide, not four: every tail row but the last
        // (the closing `####`) has a four-character run — two outline, two
        // interior — and none is wider.
        for (y, x) in &starts[..starts.len() - 1] {
            let run = art[*y][*x..].trim_end().len();
            assert_eq!(run, 4, "tail row {y} is {run} px across, not 4");
        }
    }

    /// A double arrow points both ways, so it is symmetric about its
    /// hotspot — which is what makes a centred hotspot the honest one.
    #[test]
    fn the_resize_shapes_are_symmetric_about_their_centre() {
        let c = Cursor::new();
        let n = CURSOR_SIZE as usize;
        for shape in [
            Shape::SizeHor,
            Shape::SizeVer,
            Shape::SizeFDiag,
            Shape::SizeBDiag,
            Shape::Move,
        ] {
            let art = rows(&c, shape);
            for y in 0..n {
                for x in 0..n {
                    let here = art[y].as_bytes()[x];
                    let there = art[n - 1 - y].as_bytes()[n - 1 - x];
                    assert_eq!(here, there, "{shape:?} is not symmetric at ({x}, {y})");
                }
            }
            assert_eq!(
                shape.hotspot(),
                (CURSOR_SIZE / 2, CURSOR_SIZE / 2),
                "{shape:?} must be grabbed by its middle"
            );
        }
    }

    /// The bands and the shapes are one mapping: a corner takes the
    /// diagonal that runs through it, an edge the axis it moves along.
    #[test]
    fn a_band_picks_the_shape_that_points_along_it() {
        let case = |edges: Edges, want: Shape| {
            assert_eq!(Shape::for_edges(edges), want, "{edges:?}");
        };
        case(Edges::corner(false, false), Shape::SizeFDiag); // top-left
        case(Edges::corner(true, true), Shape::SizeFDiag); // bottom-right
        case(Edges::corner(true, false), Shape::SizeBDiag); // top-right
        case(Edges::corner(false, true), Shape::SizeBDiag); // bottom-left
        case(
            Edges {
                left: true,
                ..Edges::NONE
            },
            Shape::SizeHor,
        );
        case(
            Edges {
                right: true,
                ..Edges::NONE
            },
            Shape::SizeHor,
        );
        case(
            Edges {
                top: true,
                ..Edges::NONE
            },
            Shape::SizeVer,
        );
        case(
            Edges {
                bottom: true,
                ..Edges::NONE
            },
            Shape::SizeVer,
        );
        case(Edges::NONE, Shape::Arrow);
    }

    #[test]
    fn rect_is_the_image_square_at_the_hotspot() {
        let r = Cursor::rect(100, 50);
        assert_eq!(r, IRect::new(100 - HOTSPOT.0, 50 - HOTSPOT.1, 24, 24));
        assert_eq!(r.right(), 124 - HOTSPOT.0);
        assert_eq!(r.bottom(), 74 - HOTSPOT.1);
        // Negative positions stay well-formed: the blit clips.
        assert_eq!(Cursor::rect(-5, -7), IRect::new(-5, -7, 24, 24));
    }

    /// A 2× output gets a 48-device-pixel cursor, and a centred hotspot
    /// scales with it — or the pointer would sit in the arrow's corner.
    #[test]
    fn a_scaled_cursor_covers_a_scaled_square() {
        assert_eq!(
            Cursor::rect_scaled(100, 50, Shape::Arrow, 2),
            IRect::new(100, 50, 48, 48)
        );
        assert_eq!(
            Cursor::rect_scaled(100, 50, Shape::SizeHor, 1),
            IRect::new(88, 38, 24, 24)
        );
        assert_eq!(
            Cursor::rect_scaled(100, 50, Shape::SizeHor, 2),
            IRect::new(76, 26, 48, 48)
        );
        // Nonsense scales clamp rather than producing an empty rect.
        assert_eq!(Cursor::rect_scaled(0, 0, Shape::Arrow, 0).w, 24);
    }

    /// The magnification is a *whole* factor, so the nearest-neighbour
    /// blit puts every source pixel on an exact block.
    #[test]
    fn the_paint_scale_is_a_rounded_whole_factor() {
        assert_eq!(Cursor::paint_scale(1.0), 1);
        assert_eq!(Cursor::paint_scale(2.0), 2);
        assert_eq!(Cursor::paint_scale(1.4), 1);
        assert_eq!(Cursor::paint_scale(1.6), 2);
        // Below 1, and not-a-number: the cursor is never smaller than its
        // art.
        assert_eq!(Cursor::paint_scale(0.5), 1);
        assert_eq!(Cursor::paint_scale(f32::NAN), 1);
    }

    #[test]
    fn paint_writes_inside_the_clip_and_nowhere_else() {
        let (w, h) = (64usize, 64usize);
        let mut buf: Vec<u8> = SENTINEL.iter().copied().cycle().take(w * h * 4).collect();
        let before = buf.clone();
        let clip = IRect::new(10, 10, 12, 12);
        {
            let mut canvas = Canvas::new(&mut buf, w as u32, h as u32, (w * 4) as u32);
            Cursor::new().paint(&mut canvas, &clip, 8, 8, Shape::Arrow, 1);
        }

        let mut changed_inside = 0u32;
        for y in 0..h {
            for x in 0..w {
                let off = (y * w + x) * 4;
                let now = &buf[off..off + 4];
                let was = &before[off..off + 4];
                let inside = clip.contains(i32::try_from(x).unwrap(), i32::try_from(y).unwrap());
                if inside {
                    changed_inside += u32::from(now != was);
                } else {
                    assert_eq!(now, was, "wrote outside the clip at ({x}, {y})");
                }
            }
        }
        assert!(changed_inside > 0, "painted nothing inside the clip");
    }

    #[test]
    fn paint_far_from_the_clip_is_a_no_op() {
        let (w, h) = (64usize, 64usize);
        let mut buf: Vec<u8> = SENTINEL.iter().copied().cycle().take(w * h * 4).collect();
        let before = buf.clone();
        let clip = IRect::new(0, 0, 8, 8);
        {
            let mut canvas = Canvas::new(&mut buf, w as u32, h as u32, (w * 4) as u32);
            Cursor::new().paint(&mut canvas, &clip, 40, 40, Shape::Arrow, 1);
        }
        assert_eq!(buf, before);
    }

    /// At 2× the arrow's tip is a 2×2 block of the same pixel, not a
    /// resampled blur: that is the whole reason the factor is whole.
    #[test]
    fn a_scaled_paint_is_blocky_and_not_filtered() {
        let (w, h) = (64usize, 64usize);
        let mut buf: Vec<u8> = SENTINEL.iter().copied().cycle().take(w * h * 4).collect();
        let clip = IRect::new(0, 0, 64, 64);
        {
            let mut canvas = Canvas::new(&mut buf, w as u32, h as u32, (w * 4) as u32);
            Cursor::new().paint(&mut canvas, &clip, 4, 4, Shape::Arrow, 2);
        }
        let at = |x: usize, y: usize| -> [u8; 4] {
            let off = (y * w + x) * 4;
            buf[off..off + 4].try_into().expect("4 bytes")
        };
        // The tip is one black source pixel, so at 2× it is the 2×2 block
        // at the hotspot — all four the same, and black.
        let black = [0x00, 0x00, 0x00, 0x00];
        for (x, y) in [(4, 4), (5, 4), (4, 5), (5, 5)] {
            assert_eq!(at(x, y), black, "({x}, {y}) is not the tip's block");
        }
        // And the arrow reaches twice as far: row 21 of the art is the
        // tail's close, which at 2× lands at device y = 4 + 42.
        assert_ne!(at(4 + 22, 4 + 42), SENTINEL, "the 2x arrow is 48 px tall");
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(Cursor::default().shapes, Cursor::new().shapes);
    }
}

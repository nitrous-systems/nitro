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
//! two-pixel stem running down and to the right from the notch, one
//! column every two rows (about 63°) — steeper than the 45° body edge, as
//! in `left_ptr` and the classic Windows arrow. The
//! transcription was checked against `/usr/share/icons/Adwaita/cursors/
//! left_ptr` (X11's own cursor, public domain in shape if not in any one
//! file) by decoding its 24 px frame and comparing the silhouette; nothing
//! is vendored, the art here is drawn from the geometry.
//!
//! Five more are **resize and move** shapes the *server* picks, from
//! the same `frame_hit` per motion event that already drives the resize
//! hint and the button hover: a band resolves to the shape for the edges
//! it pulls, a title drag in flight to [`Shape::Move`], and everything
//! else to the arrow.
//!
//! Until M5-E (#3771) this paragraph ended "clients cannot ask for a
//! shape — deferred to M5". That is no longer true. A client that holds
//! pointer focus may name one with `SetCursor` (`CursorShape`, which is
//! `wp_cursor_shape_device_v1`'s list), and the eleven shapes after
//! [`Shape::Move`] exist for it: the I-beam, the hand, the hourglass and
//! the rest a browser needs. [`Shape::from_wire`] maps the wire's 35
//! values onto these 17 masks, and says on each arm why an alias is one.
//! Still **named shapes only**: `CursorType::kCustom` (CSS
//! `cursor: url(…)`) has no bitmap path, deliberately — Chromium's own
//! Wayland backend prefers compositor-drawn shapes for the same reason
//! nitro draws its cursor at all.
//!
//! Who wins, in order (`Server::cursor_choice`): a drag in flight owns
//! the shape; then the server's own chrome — a band that would really
//! resize takes its double arrow, and the title bar and buttons take the
//! arrow — whatever the client asked for; then, over the client's own
//! content only, the client's request; then the arrow. A request lasts
//! one continuous period of pointer focus: leave the window and the
//! client must ask again, `wl_pointer`'s rule.
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
use nitro_wire::types::CursorShape;

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
/// The server chooses from the frame region under the pointer, and a
/// client with pointer focus may choose over its own content
/// ([`Shape::from_wire`]). The order is the order of [`Shape::ALL`],
/// which is the order the masks are stored in — so new variants are
/// **appended**, never inserted, or [`Shape::index`] would mispaint.
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
    /// Selectable text: the I-beam.
    Text,
    /// Selectable vertical text: the I-beam on its side.
    VerticalText,
    /// A link or button: the pointing hand.
    Hand,
    /// Something can be grabbed: the open hand.
    Grab,
    /// Something is being dragged: the closed hand.
    Grabbing,
    /// Busy: the hourglass.
    Wait,
    /// Help is available: the arrow with a question mark.
    Help,
    /// Precise selection: a thin cross.
    Crosshair,
    /// Not allowed here: the slashed ring.
    NotAllowed,
    /// Zoom in: the magnifier with a plus.
    ZoomIn,
    /// Zoom out: the magnifier with a minus.
    ZoomOut,
}

impl Shape {
    /// Every shape, in storage order. `Cursor` holds one converted mask
    /// per entry.
    pub const ALL: [Self; 17] = [
        Self::Arrow,
        Self::SizeHor,
        Self::SizeVer,
        Self::SizeFDiag,
        Self::SizeBDiag,
        Self::Move,
        Self::Text,
        Self::VerticalText,
        Self::Hand,
        Self::Grab,
        Self::Grabbing,
        Self::Wait,
        Self::Help,
        Self::Crosshair,
        Self::NotAllowed,
        Self::ZoomIn,
        Self::ZoomOut,
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
    /// The rule: **the hotspot is the pixel the glyph points at.** The
    /// arrow's is its tip, at `(0, 0)`, which is what makes its covered
    /// rect simply the square at the pointer position, and [`Shape::Help`]
    /// shares it because its arrow half is the same art. The hand's is
    /// its fingertip; a magnifier's is the centre of its lens, the point
    /// a zoom is about. Everything else points at nothing in particular —
    /// double arrows, the I-beam, the hourglass, the open hand — and is
    /// symmetric about (or simply centred on) its middle, because a double
    /// arrow that resized from its corner would point at one edge while
    /// grabbing another.
    ///
    /// An explicit match with no `_`, so a new shape has to say where it
    /// points.
    #[must_use]
    pub fn hotspot(self) -> (i32, i32) {
        const CENTRE: (i32, i32) = (CURSOR_SIZE / 2, CURSOR_SIZE / 2);
        match self {
            Self::Arrow | Self::Help => (0, 0),
            // The outline pixel capping the index finger (`HAND` row 0).
            Self::Hand => (6, 0),
            // The lens centre (`ZOOM_IN` rows 2–18 are the lens).
            Self::ZoomIn | Self::ZoomOut => (10, 10),
            Self::SizeHor
            | Self::SizeVer
            | Self::SizeFDiag
            | Self::SizeBDiag
            | Self::Move
            | Self::Text
            | Self::VerticalText
            | Self::Grab
            | Self::Grabbing
            | Self::Wait
            | Self::Crosshair
            | Self::NotAllowed => CENTRE,
        }
    }

    /// The shape a client's `SetCursor` names, or `None` for
    /// [`CursorShape::None`] — "hide the cursor".
    ///
    /// Every one of the wire's 35 values answers. Seventeen masks cover
    /// them: where a wire shape has a glyph of its own it gets one, and
    /// where its glyph would be **identical** to one nitro already draws it
    /// is an alias, said so on the arm. Three aliases are *not* identical
    /// glyphs and carry a one-line argument instead: `Progress`, `Cell`,
    /// and the arrow-plus-badge family (`ContextMenu`, `Alias`, `Copy`).
    ///
    /// Exhaustive, with **no `_` arm**: a value added to the wire enum
    /// fails to compile here rather than silently painting an arrow.
    #[must_use]
    pub fn from_wire(shape: CursorShape) -> Option<Self> {
        Some(match shape {
            CursorShape::None => return None,
            // `Default` is the arrow itself. The other three are arrow +
            // badge (a menu, a link arrow, a plus). nitro draws no
            // badges today; the arrow is the honest part of the glyph. The
            // drag-and-drop tasks (#3773/#3774) are where badges would be
            // revisited — they are not drawn now.
            CursorShape::Default
            | CursorShape::ContextMenu
            | CursorShape::Alias
            | CursorShape::Copy => Self::Arrow,
            CursorShape::Help => Self::Help,
            CursorShape::Pointer => Self::Hand,
            // `Progress` is "busy but still interactive": elsewhere an
            // animated or arrow-composited hourglass. nitro animates
            // nothing, and a second static glyph would claim a difference
            // in interactivity it cannot show — so the hourglass.
            CursorShape::Progress | CursorShape::Wait => Self::Wait,
            // `Cell` is a fat hollow plus where `Crosshair` is a thin
            // cross; the affordance — aim at a point — is the same and the
            // art would differ only in stroke weight.
            CursorShape::Cell | CursorShape::Crosshair => Self::Crosshair,
            CursorShape::Text => Self::Text,
            CursorShape::VerticalText => Self::VerticalText,
            // Identical glyph: the four-way cross, for moving and for
            // panning in every direction alike.
            CursorShape::Move | CursorShape::AllScroll => Self::Move,
            // `NoDrop` is the one of the drag family with a real glyph of
            // its own elsewhere, and it is this one.
            CursorShape::NoDrop | CursorShape::NotAllowed => Self::NotAllowed,
            CursorShape::Grab => Self::Grab,
            CursorShape::Grabbing => Self::Grabbing,
            // Identical glyph: a one-way resize shows the double arrow of
            // its axis in every mainstream theme, and a column/row resize
            // is the same arrow over a divider nitro does not draw.
            CursorShape::EResize
            | CursorShape::WResize
            | CursorShape::EwResize
            | CursorShape::ColResize => Self::SizeHor,
            CursorShape::NResize
            | CursorShape::SResize
            | CursorShape::NsResize
            | CursorShape::RowResize => Self::SizeVer,
            CursorShape::NwResize | CursorShape::SeResize | CursorShape::NwseResize => {
                Self::SizeFDiag
            }
            CursorShape::NeResize | CursorShape::SwResize | CursorShape::NeswResize => {
                Self::SizeBDiag
            }
            CursorShape::ZoomIn => Self::ZoomIn,
            CursorShape::ZoomOut => Self::ZoomOut,
        })
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
            Self::Text => &TEXT,
            Self::VerticalText => &VERTICAL_TEXT,
            Self::Hand => &HAND,
            Self::Grab => &GRAB,
            Self::Grabbing => &GRABBING,
            Self::Wait => &WAIT,
            Self::Help => &HELP,
            Self::Crosshair => &CROSSHAIR,
            Self::NotAllowed => &NOT_ALLOWED,
            Self::ZoomIn => &ZOOM_IN,
            Self::ZoomOut => &ZOOM_OUT,
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
/// Tip at `(0, 0)`, a vertical left edge eighteen pixels long, a diagonal
/// right edge down to the shoulder, and a **two-pixel stem that steps one
/// column right every two rows** (about 63°), closed by a rounded cap —
/// which `the_tail_is_steeper_than_the_body_edge` pins. The first art had
/// a four-pixel-wide tail leaving the notch too far right (#3724); its
/// replacement ran the stem at exactly 45°, parallel to the body edge,
/// and still read as a check mark (#3820). `left_ptr` at 24 px and the
/// Windows arrow both advance the stem one column per two rows.
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
    b"#...#..#                ",
    b"#..# #..#               ",
    b"#.#  #..#               ",
    b"##    #..#              ",
    b"#     #..#              ",
    b"       #..#             ",
    b"       #..#             ",
    b"        #..#            ",
    b"        #..#            ",
    b"         ##             ",
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

/// Selectable text: the I-beam — a two-pixel stem with serif bars top and
/// bottom, the shape every toolkit's `xterm`/`text` cursor has. Centred
/// hotspot: it is symmetric, and the caret lands where its middle is.
const TEXT: Mask = [
    b"                        ",
    b"                        ",
    b"                        ",
    b"      ############      ",
    b"      #..........#      ",
    b"      #####..#####      ",
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
    b"          #..#          ",
    b"          #..#          ",
    b"      #####..#####      ",
    b"      #..........#      ",
    b"      ############      ",
    b"                        ",
    b"                        ",
    b"                        ",
];

/// Selectable vertical text: [`TEXT`] turned a quarter, drawn out rather
/// than rotated at startup so the art stays what is painted.
const VERTICAL_TEXT: Mask = [
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"   ###            ###   ",
    b"   #.#            #.#   ",
    b"   #.#            #.#   ",
    b"   #.#            #.#   ",
    b"   #.##############.#   ",
    b"   #................#   ",
    b"   #................#   ",
    b"   #.##############.#   ",
    b"   #.#            #.#   ",
    b"   #.#            #.#   ",
    b"   #.#            #.#   ",
    b"   ###            ###   ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
];

/// A link or a button: the pointing hand, index finger up, three fingers
/// curled beside it and the thumb out to the left. Its hotspot is the
/// **fingertip** — the pixel the glyph points at — not the middle.
const HAND: Mask = [
    b"     ####               ",
    b"     #..#               ",
    b"     #..#               ",
    b"     #..#               ",
    b"     #..####            ",
    b"     #..#..####         ",
    b"     #..#..#..####      ",
    b"     #..#..#..#..#      ",
    b" #####..#..#..#..#      ",
    b" #..##..#..#..#..#      ",
    b" #...#..#..#..#..#      ",
    b" ##.............##      ",
    b"  #.............#       ",
    b"  ##............#       ",
    b"   #............#       ",
    b"   ##...........#       ",
    b"    #..........##       ",
    b"    ##.........#        ",
    b"     #.........#        ",
    b"     #.........#        ",
    b"     ###########        ",
    b"                        ",
    b"                        ",
    b"                        ",
];

/// Something can be grabbed: the open hand, four fingers up and the thumb
/// out. Centred hotspot: an open hand points at nothing in particular.
const GRAB: Mask = [
    b"                        ",
    b"                        ",
    b"                        ",
    b"         ####           ",
    b"      ####..####        ",
    b"      #..#..#..#        ",
    b"      #..#..#..####     ",
    b"      #..#..#..#..#     ",
    b"      #..#..#..#..#     ",
    b"  #####..#..#..#..#     ",
    b"  #..##..#..#..#..#     ",
    b"  #...#..#..#..#..#     ",
    b"  ##.............##     ",
    b"   #.............#      ",
    b"   ##............#      ",
    b"    #............#      ",
    b"    ##...........#      ",
    b"     #..........##      ",
    b"     ##.........#       ",
    b"      #.........#       ",
    b"      ###########       ",
    b"                        ",
    b"                        ",
    b"                        ",
];

/// Something is being dragged: [`GRAB`] closed into a fist, knuckles on
/// top. Centred hotspot, for [`GRAB`]'s reason.
const GRABBING: Mask = [
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"       ##########       ",
    b"    ####..#..#..###     ",
    b"    #..#..#..#..#.#     ",
    b"   ##.............#     ",
    b"   #..............#     ",
    b"   #.............##     ",
    b"   ##............#      ",
    b"    #............#      ",
    b"    ##...........#      ",
    b"     #..........##      ",
    b"     ##.........#       ",
    b"      #.........#       ",
    b"      ###########       ",
    b"                        ",
    b"                        ",
    b"                        ",
    b"                        ",
];

/// Busy: an hourglass, with the frame's two bars drawn as closed boxes so
/// it reads at 1×. Centred hotspot.
const WAIT: Mask = [
    b"                        ",
    b"                        ",
    b"     ##############     ",
    b"     #............#     ",
    b"     #............#     ",
    b"     ##############     ",
    b"      #..........#      ",
    b"      ##........##      ",
    b"       ##......##       ",
    b"        ##....##        ",
    b"         ##..##         ",
    b"          #..#          ",
    b"          #..#          ",
    b"         ##..##         ",
    b"        ##....##        ",
    b"       ##......##       ",
    b"      ##........##      ",
    b"      #..........#      ",
    b"     ##############     ",
    b"     #............#     ",
    b"     #............#     ",
    b"     ##############     ",
    b"                        ",
    b"                        ",
];

/// Help is available: the arrow with a question mark beside it. The arrow
/// half is [`ARROW`]'s art verbatim, so its hotspot is the arrow's tip.
const HELP: Mask = [
    b"#                       ",
    b"##             ######   ",
    b"#.#           ##....##  ",
    b"#..#          #......#  ",
    b"#...#         #..##..#  ",
    b"#....#        #####..#  ",
    b"#.....#         ##..##  ",
    b"#......#        #..##   ",
    b"#.......#       #..#    ",
    b"#........#      ####    ",
    b"#.........#     #..#    ",
    b"#..........#    #..#    ",
    b"#......#####    ####    ",
    b"#...#..#                ",
    b"#..# #..#               ",
    b"#.#  #..#               ",
    b"##    #..#              ",
    b"#     #..#              ",
    b"       #..#             ",
    b"       #..#             ",
    b"        #..#            ",
    b"        #..#            ",
    b"         ##             ",
    b"                        ",
];

/// Precise selection: a thin cross whose centre is a 2×2 black spot, so
/// the aimed-at pixel is visible rather than hidden under white. Centred.
const CROSSHAIR: Mask = [
    b"                        ",
    b"                        ",
    b"          ####          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"  #########..#########  ",
    b"  #........##........#  ",
    b"  #........##........#  ",
    b"  #########..#########  ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          #..#          ",
    b"          ####          ",
    b"                        ",
    b"                        ",
];

/// The action is not allowed: a ring with a `╲` bar across it. Centred.
const NOT_ALLOWED: Mask = [
    b"                        ",
    b"                        ",
    b"       ##########       ",
    b"      ##........##      ",
    b"    ###..........###    ",
    b"    #....######....#    ",
    b"   ##...##    ##...##   ",
    b"  ##.....##    ##...##  ",
    b"  #...#...##    ##...#  ",
    b"  #..###...##    ##..#  ",
    b"  #..# ##...##    #..#  ",
    b"  #..#  ##...##   #..#  ",
    b"  #..#   ##...##  #..#  ",
    b"  #..#    ##...## #..#  ",
    b"  #..##    ##...###..#  ",
    b"  #...##    ##...#...#  ",
    b"  ##...##    ##.....##  ",
    b"   ##...##    ##...##   ",
    b"    #....######....#    ",
    b"    ###..........###    ",
    b"      ##........##      ",
    b"       ##########       ",
    b"                        ",
    b"                        ",
];

/// Zoom in: a magnifier with `+` in the lens. Its hotspot is the **lens
/// centre**, `(10, 10)`, which is the point the zoom is about — not the
/// image's middle, which falls on the rim.
const ZOOM_IN: Mask = [
    b"                        ",
    b"                        ",
    b"        #####           ",
    b"      ###...###         ",
    b"    ###.......###       ",
    b"    #...........#       ",
    b"   ##...........##      ",
    b"   #......#......#      ",
    b"  ##......#......##     ",
    b"  #.......#.......#     ",
    b"  #....#######....#     ",
    b"  #.......#.......#     ",
    b"  ##......#......##     ",
    b"   #......#......#      ",
    b"   ##...........##      ",
    b"    #...........###     ",
    b"    ###.......##..##    ",
    b"      ###...#####..##   ",
    b"        #####   ##..##  ",
    b"                 ##..## ",
    b"                  ##..##",
    b"                   ##..#",
    b"                    ####",
    b"                        ",
];

/// Zoom out: [`ZOOM_IN`] with `−` in the lens, and the same hotspot.
const ZOOM_OUT: Mask = [
    b"                        ",
    b"                        ",
    b"        #####           ",
    b"      ###...###         ",
    b"    ###.......###       ",
    b"    #...........#       ",
    b"   ##...........##      ",
    b"   #.............#      ",
    b"  ##.............##     ",
    b"  #...............#     ",
    b"  #....#######....#     ",
    b"  #...............#     ",
    b"  ##.............##     ",
    b"   #.............#      ",
    b"   ##...........##      ",
    b"    #...........###     ",
    b"    ###.......##..##    ",
    b"      ###...#####..##   ",
    b"        #####   ##..##  ",
    b"                 ##..## ",
    b"                  ##..##",
    b"                   ##..#",
    b"                    ####",
    b"                        ",
];

/// Every cursor's ARGB8888 pixels, built once.
///
/// One allocation per shape, at startup: `17 × 24 × 24 × 4` = **39 168
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
    use super::{CURSOR_SIZE, Cursor, CursorShape, HOTSPOT, Shape};
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
        assert_eq!(bytes, 39_168);
        // Storage order is variant order: `index()` is `self as usize`, so
        // a reordered `ALL` would mispaint every cursor silently.
        for (i, s) in Shape::ALL.iter().enumerate() {
            assert_eq!(s.index(), i, "{s:?}");
        }
    }

    /// Every wire value answers, and `None` is the only one that hides.
    #[test]
    fn every_wire_shape_maps_to_a_drawable_shape() {
        let mut hidden = 0;
        for raw in 0..=34u16 {
            let wire = CursorShape::from_raw(raw).expect("0..=34 are all listed");
            match Shape::from_wire(wire) {
                None => {
                    assert_eq!(wire, CursorShape::None);
                    hidden += 1;
                }
                Some(s) => assert!(Shape::ALL.contains(&s), "{wire:?}"),
            }
        }
        assert_eq!(hidden, 1);
        // And every mask is reachable from the wire, or it is dead art.
        for s in Shape::ALL {
            assert!(
                (0..=34u16)
                    .filter_map(|r| CursorShape::from_raw(r).ok())
                    .any(|w| Shape::from_wire(w) == Some(s)),
                "{s:?} is unreachable from SetCursor"
            );
        }
    }

    /// The aliases are the glyph they claim to be.
    #[test]
    fn the_aliases_are_the_glyph_they_claim() {
        let same = |a: CursorShape, b: CursorShape| {
            assert_eq!(Shape::from_wire(a), Shape::from_wire(b), "{a:?} vs {b:?}");
        };
        same(CursorShape::ColResize, CursorShape::EResize);
        same(CursorShape::EwResize, CursorShape::WResize);
        same(CursorShape::RowResize, CursorShape::NResize);
        same(CursorShape::NwseResize, CursorShape::SeResize);
        same(CursorShape::NeswResize, CursorShape::SwResize);
        same(CursorShape::AllScroll, CursorShape::Move);
        same(CursorShape::Progress, CursorShape::Wait);
        same(CursorShape::Cell, CursorShape::Crosshair);
        same(CursorShape::NoDrop, CursorShape::NotAllowed);
        same(CursorShape::Copy, CursorShape::Default);
        assert_eq!(Shape::from_wire(CursorShape::Default), Some(Shape::Arrow));
        assert_eq!(Shape::from_wire(CursorShape::Text), Some(Shape::Text));
        assert_eq!(Shape::from_wire(CursorShape::Pointer), Some(Shape::Hand));
        assert_eq!(
            Shape::from_wire(CursorShape::ColResize),
            Some(Shape::SizeHor)
        );
    }

    /// The hotspot is ink: the pixel the glyph points at is drawn, so a
    /// hotspot that drifted off the art (a mask edited without its
    /// `hotspot()` arm) fails here.
    #[test]
    fn each_shape_hotspot_is_inside_its_art() {
        let c = Cursor::new();
        for s in Shape::ALL {
            let (hx, hy) = s.hotspot();
            assert!((0..CURSOR_SIZE).contains(&hx) && (0..CURSOR_SIZE).contains(&hy));
            let (x, y) = (hx as usize, hy as usize);
            let art = rows(&c, s);
            assert_ne!(
                art[y].as_bytes()[x],
                b' ',
                "{s:?}'s hotspot ({x}, {y}) is not ink:\n{art:#?}"
            );
        }
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

    /// #3724's report: "the mouse pointer tail is weird (off angle)".
    /// A 45° stem, parallel to the body's diagonal edge, still read as a
    /// check mark (#3820). `left_ptr` and the Windows arrow step their
    /// stem one column right every **two** rows — steeper than the body
    /// edge — and that is the property this test holds.
    #[test]
    fn the_tail_is_steeper_than_the_body_edge() {
        let c = Cursor::new();
        let art = rows(&c, Shape::Arrow);
        // The stem is the last ink run of each row below the shoulder
        // (row 12): outline, two interior, outline.
        let starts: Vec<usize> = (13..=21)
            .map(|y| {
                let row = art[y].trim_end();
                let x = row.len() - 4;
                assert_eq!(
                    &row[x..],
                    "#..#",
                    "stem row {y} is not 4 px across: {row:?}"
                );
                assert_ne!(
                    row.as_bytes()[x - 1],
                    b'#',
                    "stem row {y} is wider than 4 px"
                );
                x
            })
            .collect();
        assert_eq!(starts, [4, 5, 5, 6, 6, 7, 7, 8, 8], "{art:#?}");
        for w in starts.windows(3) {
            // At most one column per row, and never two advances running:
            // a slope of at least 2:1, steeper than the 45° body edge.
            assert!(w[1] - w[0] <= 1 && w[2] - w[1] <= 1, "{w:?}");
            assert!(w[2] - w[0] <= 1, "{w:?} is 45°");
        }
        // The stem leaves the notch under the shoulder's start, no jog.
        assert_eq!(art[12].find("#####"), Some(starts[0] + 3));
        // A rounded cap closes it, and nothing is below.
        assert_eq!(art[22].trim_end(), "         ##");
        assert!(art[23].trim().is_empty());
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
            Shape::Text,
            Shape::VerticalText,
            Shape::Wait,
            Shape::Crosshair,
            Shape::NotAllowed,
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
        // stem's last full row (cols 8–11), which at 2× lands at
        // device y = 4 + 42.
        assert_ne!(
            at(4 + 2 * 9, 4 + 2 * 21),
            SENTINEL,
            "the 2x arrow is 48 px tall"
        );
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(Cursor::default().shapes, Cursor::new().shapes);
    }
}

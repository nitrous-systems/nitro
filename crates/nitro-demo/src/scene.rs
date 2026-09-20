//! The demo's scene: ids, layout, the procedural image, and the mutation
//! batches that build and update it.
//!
//! Deliberately free of sockets. Every function here either computes
//! geometry or returns a `Vec<ClientMsg>`, so a test can assert on exactly
//! what would go on the wire — and the integration test can feed the very
//! same batches to a real server instead of a reimplementation that could
//! drift from the binary.
//!
//! # Why `Vec<ClientMsg>` and not the `Transaction` builder
//!
//! `nitro_wire::client::Transaction` is sugar over `Connection::send`, and
//! it writes straight into the connection's buffer — which is exactly what
//! makes it impossible to weigh. The demo reports the bytes its first
//! frame costs (`--stats`, and `docs/budget.md`), so it wants the messages
//! as values it can measure before sending. The cost is one `Vec` of
//! enum values per commit, which is a few hundred bytes and no syscalls.
//!
//! # Node ids
//!
//! Ids are the client's to allocate, and the demo allocates them
//! arithmetically from the window's index ([`Ids::for_window`]) rather than
//! from a counter: window 3's follower is always id `3 * STRIDE + 8`, so a
//! log line names a node without a table, and `--windows N` needs no
//! bookkeeping at all.

use std::os::fd::OwnedFd;

use nitro_core::{Color, IRect, Point, Rect, Size};
use nitro_wire::msg::{
    ClientMsg, CreateBuffer, CreateNode, CreateWindow, Fill, SetBorder, SetBounds, SetCorners,
    SetFill, SetImage, SetVisible,
};
use nitro_wire::types::{BufferId, Layer, NodeId, NodeKind, format};

use crate::geom::{TRAIL_WIDTH, follower_rect, moved_damage, outline};

/// Window size the demo asks for. 800×500 is big enough to hold a row of
/// cards and an image at 1× and still fit twice over on the test box's
/// 1920×1080 with the cascade.
pub const WINDOW_SIZE: Size = Size::new(800.0, 500.0);

/// Id space reserved per window. A power of two so the window index is a
/// shift in a log line, and roomy enough that adding a node later does not
/// renumber anything.
pub const STRIDE: u32 = 32;

/// Edge of the square demo image, in buffer pixels.
pub const IMG_EDGE: u32 = 64;
/// Bytes per image row: `AR24` is four bytes per pixel.
pub const IMG_STRIDE: u32 = IMG_EDGE * 4;
/// Size of the image buffer in bytes.
pub const IMG_BYTES: u32 = IMG_STRIDE * IMG_EDGE;

/// How far the animated rect travels per frame callback, in logical
/// pixels. Small on purpose: the point of `--animate` is frame pacing, not
/// throughput, and 4 px at 60 Hz is a visible, countable 240 px/s.
///
/// Per *callback*, so the speed on screen follows the refresh rate: the
/// same 4 px is 480 px/s at 120 Hz. That is the right behaviour for a
/// pacing demo — the thing being demonstrated is that there is one commit
/// per frame — but it does mean the 240 px/s above is a 60 Hz figure.
pub const ANIMATE_STEP: f32 = 4.0;

/// Colours of the four cards, left to right.
pub const CARD_COLORS: [Color; 4] = [
    Color::rgb(0x33, 0x88, 0xff),
    Color::rgb(0xff, 0x9f, 0x43),
    Color::rgba(0x2e, 0xd5, 0x73, 0xc0),
    Color::rgb(0xe0, 0x3b, 0x8b),
];

/// Every node id one window uses.
///
/// The fields are the documentation: what the demo draws is this list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    /// The window's root group.
    pub window: NodeId,
    /// Rect filling the window, carrying the gradient.
    pub background: NodeId,
    /// The row of rounded, bordered cards.
    pub cards: [NodeId; 4],
    /// Image node sampling the memfd buffer.
    pub image: NodeId,
    /// The 24×24 rect that tracks the pointer.
    pub follower: NodeId,
    /// The hollow rect left behind at the follower's last position.
    pub trail: NodeId,
    /// The rect `--animate` walks across the window.
    pub mover: NodeId,
    /// Eight hairline rects: two damage rectangles, four edges each.
    pub outlines: [NodeId; 8],
    /// The client buffer holding the image's pixels.
    pub buffer: BufferId,
}

impl Ids {
    /// Ids for window `index` (0-based).
    ///
    /// Id 0 is [`NodeId::NONE`], so the space starts at 1.
    #[must_use]
    pub fn for_window(index: u32) -> Self {
        let b = index * STRIDE + 1;
        Self {
            window: NodeId(b),
            background: NodeId(b + 1),
            cards: [NodeId(b + 2), NodeId(b + 3), NodeId(b + 4), NodeId(b + 5)],
            image: NodeId(b + 6),
            follower: NodeId(b + 7),
            trail: NodeId(b + 8),
            mover: NodeId(b + 9),
            outlines: [
                NodeId(b + 10),
                NodeId(b + 11),
                NodeId(b + 12),
                NodeId(b + 13),
                NodeId(b + 14),
                NodeId(b + 15),
                NodeId(b + 16),
                NodeId(b + 17),
            ],
            buffer: BufferId(index + 1),
        }
    }

    /// Which window index a root node id belongs to, or `None` if the id
    /// is not a window root.
    ///
    /// The inverse of [`Ids::for_window`], used to route a `PointerMotion`
    /// back to the window it names.
    #[must_use]
    pub fn window_index(id: NodeId) -> Option<u32> {
        let raw = id.raw().checked_sub(1)?;
        (raw % STRIDE == 0).then_some(raw / STRIDE)
    }
}

/// The four card rects for a window of `size`.
///
/// Laid out proportionally so a `Configure` to a different size still
/// produces a row rather than a pile.
#[must_use]
pub fn cards(size: Size) -> [Rect; 4] {
    let margin = size.w * 0.05;
    let gap = size.w * 0.03;
    let w = ((size.w - 2.0 * margin - 3.0 * gap) / 4.0).max(1.0);
    let h = (size.h * 0.24).max(1.0);
    let y = size.h * 0.12;
    std::array::from_fn(|i| Rect::new(margin + i as f32 * (w + gap), y, w, h))
}

/// Where the image node sits in a window of `size`.
#[must_use]
pub fn image_rect(size: Size) -> Rect {
    let edge = (size.h * 0.32).max(1.0);
    Rect::new(size.w * 0.05, size.h * 0.5, edge, edge)
}

/// The animated rect's bounds for a phase of `x` logical pixels.
#[must_use]
pub fn mover_rect(size: Size, x: f32) -> Rect {
    let edge = (size.h * 0.08).max(1.0);
    Rect::new(x, size.h - edge - size.h * 0.05, edge, edge)
}

/// Advance the animation phase, bouncing off both edges.
///
/// Returns the new phase. A bounce rather than a wrap because a wrap
/// teleports, and a teleport damages two rects at opposite ends of the
/// window — which would make the `--animate` damage figures a different
/// measurement every 200 frames.
#[must_use]
pub fn advance(size: Size, x: f32, step: f32) -> (f32, f32) {
    let edge = mover_rect(size, 0.0).w;
    let limit = (size.w - edge).max(0.0);
    let mut next = x + step;
    let mut dir = step;
    if next > limit {
        next = limit;
        dir = -step;
    } else if next < 0.0 {
        next = 0.0;
        dir = -step;
    }
    (next, dir)
}

/// The vertical gradient behind everything in a window of `size`.
#[must_use]
pub fn backdrop(size: Size) -> Fill {
    Fill::Linear {
        start: Point::new(0.0, 0.0),
        end: Point::new(0.0, size.h),
        c0: Color::rgb(0x10, 0x18, 0x30),
        c1: Color::rgb(0x50, 0x28, 0x70),
    }
}

/// Build one window's whole scene, ready to commit.
///
/// `fd` is the image buffer's descriptor; the server `pread`s the pixels
/// out of it when the message arrives. `None` — which is what a
/// **remote** connection gets — builds the same scene **without** the
/// image node and its buffer: a buffer is a file descriptor, and a
/// descriptor cannot cross TCP (`caps::REMOTE`, `docs/remote.md`).
///
/// Skipping the node rather than sending an empty one is deliberate: an
/// `Image` with no buffer is a hole in the scene, and the demo's job
/// over a remote link is to measure the latency of everything that
/// *does* work.
#[must_use]
pub fn build(ids: Ids, size: Size, title: &str, fd: Option<OwnedFd>) -> Vec<ClientMsg> {
    let mut out = Vec::with_capacity(64);
    out.push(
        CreateWindow {
            id: ids.window,
            size,
            layer: Layer::Normal,
            flags: 0,
            title: title.to_owned(),
        }
        .into(),
    );
    build_backdrop(&mut out, ids, size);
    if let Some(fd) = fd {
        build_image(&mut out, ids, size, fd);
    }
    build_mover(&mut out, ids, size);
    build_follower(&mut out, ids);
    build_outlines(&mut out, ids);
    out
}

/// The gradient backdrop and the row of cards.
fn build_backdrop(out: &mut Vec<ClientMsg>, ids: Ids, size: Size) {
    rect(
        out,
        ids.background,
        ids.window,
        Rect::new(0.0, 0.0, size.w, size.h),
    );
    out.push(
        SetFill {
            id: ids.background,
            fill: backdrop(size),
        }
        .into(),
    );
    for (i, r) in cards(size).into_iter().enumerate() {
        let id = ids.cards[i];
        rect(out, id, ids.window, r);
        out.push(
            SetFill {
                id,
                fill: Fill::Solid(CARD_COLORS[i]),
            }
            .into(),
        );
        out.push(SetCorners { id, radius: 18.0 }.into());
        out.push(
            SetBorder {
                id,
                width: 2.0,
                color: Color::rgba(255, 255, 255, 96),
            }
            .into(),
        );
    }
}

/// The image node and the buffer it samples — in that order, because the
/// server applies a transaction in arrival order and `SetImage` naming an
/// unregistered buffer is fatal.
fn build_image(out: &mut Vec<ClientMsg>, ids: Ids, size: Size, fd: OwnedFd) {
    out.push(
        CreateNode {
            id: ids.image,
            kind: NodeKind::Image,
            parent: ids.window,
            before: NodeId::NONE,
        }
        .into(),
    );
    out.push(
        SetBounds {
            id: ids.image,
            rect: image_rect(size),
        }
        .into(),
    );
    out.push(
        CreateBuffer {
            id: ids.buffer,
            width: IMG_EDGE,
            height: IMG_EDGE,
            stride: IMG_STRIDE,
            format: format::AR24,
            size: IMG_BYTES,
            fd,
        }
        .into(),
    );
    out.push(
        SetImage {
            id: ids.image,
            buffer: ids.buffer,
            src: IRect::new(0, 0, IMG_EDGE.cast_signed(), IMG_EDGE.cast_signed()),
        }
        .into(),
    );
}

/// The animated rect.
///
/// It exists in every mode; `--follow` simply never moves it. Creating it
/// unconditionally keeps the two modes' node counts and first-frame byte
/// counts comparable, which is the whole point of the budget table.
fn build_mover(out: &mut Vec<ClientMsg>, ids: Ids, size: Size) {
    rect(out, ids.mover, ids.window, mover_rect(size, 0.0));
    out.push(
        SetFill {
            id: ids.mover,
            fill: Fill::Solid(Color::rgb(0xf5, 0xf5, 0xf5)),
        }
        .into(),
    );
    out.push(
        SetCorners {
            id: ids.mover,
            radius: 6.0,
        }
        .into(),
    );
}

/// The trail and the follower, in that order: the follower must be on top
/// of the ghost it left behind, and z-order among siblings is creation
/// order.
fn build_follower(out: &mut Vec<ClientMsg>, ids: Ids) {
    rect(out, ids.trail, ids.window, Rect::EMPTY);
    out.push(
        SetFill {
            id: ids.trail,
            fill: Fill::None,
        }
        .into(),
    );
    out.push(
        SetBorder {
            id: ids.trail,
            width: TRAIL_WIDTH,
            color: Color::rgba(0xff, 0xff, 0xff, 0xB0),
        }
        .into(),
    );
    rect(out, ids.follower, ids.window, Rect::EMPTY);
    out.push(
        SetFill {
            id: ids.follower,
            fill: Fill::Solid(Color::rgb(0xff, 0xe0, 0x40)),
        }
        .into(),
    );
    out.push(
        SetCorners {
            id: ids.follower,
            radius: 4.0,
        }
        .into(),
    );
}

/// The eight hairline rects that outline damage, created hidden.
///
/// Opaque, not translucent: a damage outline is a measuring instrument,
/// and a translucent one blends with whatever it is drawn over, so the
/// screenshot that is supposed to say "this is the rectangle that
/// repainted" would instead say "this is that rectangle tinted by
/// whatever was underneath". An exact colour is a checkable one.
fn build_outlines(out: &mut Vec<ClientMsg>, ids: Ids) {
    for id in ids.outlines {
        rect(out, id, ids.window, Rect::EMPTY);
        out.push(
            SetFill {
                id,
                fill: Fill::Solid(Color::rgb(0xff, 0x30, 0x30)),
            }
            .into(),
        );
        out.push(SetVisible { id, visible: false }.into());
    }
}

/// Create a rect node with bounds, the two messages that always go
/// together.
fn rect(out: &mut Vec<ClientMsg>, id: NodeId, parent: NodeId, bounds: Rect) {
    out.push(
        CreateNode {
            id,
            kind: NodeKind::Rect,
            parent,
            before: NodeId::NONE,
        }
        .into(),
    );
    out.push(SetBounds { id, rect: bounds }.into());
}

/// Re-lay-out a window for a new size, in answer to a `Configure`.
///
/// `has_image` says whether this window's scene actually contains the
/// image node. A **remote** window does not ([`build`] with `None`), and
/// naming a node that was never created is a fatal `UnknownNode` — which
/// is exactly how this was found: the remote demo came up, painted, and
/// died on the first `Configure`.
#[must_use]
pub fn reconfigure(ids: Ids, size: Size, has_image: bool) -> Vec<ClientMsg> {
    let mut out = Vec::with_capacity(8);
    out.push(
        SetBounds {
            id: ids.background,
            rect: Rect::new(0.0, 0.0, size.w, size.h),
        }
        .into(),
    );
    // The gradient's endpoints are in the node's own space, so they have
    // to follow the height or the fade stops mid-window.
    out.push(
        SetFill {
            id: ids.background,
            fill: backdrop(size),
        }
        .into(),
    );
    for (i, r) in cards(size).into_iter().enumerate() {
        out.push(
            SetBounds {
                id: ids.cards[i],
                rect: r,
            }
            .into(),
        );
    }
    if has_image {
        out.push(
            SetBounds {
                id: ids.image,
                rect: image_rect(size),
            }
            .into(),
        );
    }
    out
}

/// What one pointer move changes: the follower, the trail, and — when
/// `show_damage` — the outlines around the rects this move damages.
///
/// Returns the messages and the damage rects they describe, so the caller
/// can report the area without recomputing it.
#[must_use]
pub fn follow(ids: Ids, size: Size, from: Rect, pos: Point) -> (Vec<ClientMsg>, Rect, Vec<Rect>) {
    let to = follower_rect(pos, (size.w, size.h));
    let mut out = Vec::with_capacity(4);
    out.push(
        SetBounds {
            id: ids.follower,
            rect: to,
        }
        .into(),
    );
    out.push(
        SetBounds {
            id: ids.trail,
            rect: from,
        }
        .into(),
    );
    let damage = moved_damage(from, to);
    (out, to, damage)
}

/// Show or hide the damage outlines around `damage`.
///
/// Always emits all eight nodes: a node left visible from a previous,
/// larger damage list would draw a stale rectangle, and hiding it is one
/// message either way.
#[must_use]
pub fn damage_outlines(ids: Ids, damage: &[Rect], show: bool) -> Vec<ClientMsg> {
    let mut out = Vec::with_capacity(16);
    for (slot, id) in ids.outlines.into_iter().enumerate() {
        let rect = damage
            .get(slot / 4)
            .map(|r| outline(*r, TRAIL_WIDTH)[slot % 4]);
        let visible = show && rect.is_some_and(|r| !r.is_empty());
        if let Some(r) = rect.filter(|_| visible) {
            out.push(SetBounds { id, rect: r }.into());
        }
        out.push(SetVisible { id, visible }.into());
    }
    out
}

/// Mark a window as the selected one by tinting its follower.
///
/// `--windows N` needs a way to say which window `n`/`p` picked out, and
/// v1 has no stacking message for a client to raise itself with (the
/// server raises on a left click, which is M1's whole window-management
/// story — see `docs/latency.md`). Colouring the follower is the honest
/// substitute: it shows the selection without pretending to restack.
#[must_use]
pub fn select(ids: Ids, selected: bool) -> Vec<ClientMsg> {
    vec![
        SetFill {
            id: ids.follower,
            fill: Fill::Solid(if selected {
                Color::rgb(0xff, 0xe0, 0x40)
            } else {
                Color::rgba(0xff, 0xe0, 0x40, 0x60)
            }),
        }
        .into(),
    ]
}

/// The demo image: an 8-pixel checkerboard faded out radially.
///
/// `AR24` is little-endian `[b, g, r, a]` with straight alpha, so the
/// server compositing this over the gradient exercises the blend path as
/// well as the buffer path.
#[must_use]
pub fn checker(edge: u32) -> Vec<u8> {
    let centre = (edge as f32 - 1.0) / 2.0;
    let radius = edge as f32 / 2.0;
    let stride = edge * 4;
    let mut px = vec![0u8; (stride * edge) as usize];
    for y in 0..edge {
        for x in 0..edge {
            let dx = x as f32 - centre;
            let dy = y as f32 - centre;
            let fade = 1.0 - dx.hypot(dy) / radius;
            let alpha = (fade.clamp(0.0, 1.0) * 255.0) as u8;
            let [r, g, b] = if (x / 8 + y / 8) % 2 == 0 {
                [0xf5, 0xf5, 0xf5]
            } else {
                [0xe0, 0x3b, 0x8b]
            };
            let off = (y * stride + x * 4) as usize;
            px[off..off + 4].copy_from_slice(&[b, g, r, alpha]);
        }
    }
    px
}

/// Put `pixels` in a fresh **sealed** memfd and hand back its descriptor.
///
/// The seals (`F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`) are what let the
/// server *map* the buffer rather than copy it out (#569); an unsealed
/// descriptor is a `BadBuffer`. The demo writes its pixels once, so the
/// one-`pwrite` shape of `nitro_shm::memfd_with` is right here.
///
/// # Errors
/// Any `memfd_create`/`ftruncate`/`F_ADD_SEALS`/`pwrite` failure.
pub fn memfd(pixels: &[u8]) -> Result<OwnedFd, rustix::io::Errno> {
    nitro_shm::memfd_with("nitro-demo", pixels)
}

/// Nearest-neighbour downscale of an `XRGB8888` image by an integer
/// `factor`, returning tightly packed `XRGB8888` rows.
///
/// Nearest neighbour, not a box filter, and that is the point: a box
/// filter would blur the one-pixel damage outlines into invisibility,
/// and the whole reason to downscale a screenshot for `docs/` is to show
/// where the damage was. Sharp aliasing is the desired artefact here.
///
/// # Panics
/// If `factor` is zero.
#[must_use]
pub fn downscale(
    width: u32,
    height: u32,
    stride: u32,
    data: &[u8],
    factor: u32,
) -> (u32, u32, Vec<u8>) {
    assert!(factor > 0, "downscale factor must be positive");
    let w = (width / factor).max(1);
    let h = (height / factor).max(1);
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let src_y = (y * factor).min(height.saturating_sub(1));
        for x in 0..w {
            let src_x = (x * factor).min(width.saturating_sub(1));
            let si = (src_y * stride + src_x * 4) as usize;
            let di = ((y * w + x) * 4) as usize;
            if si + 4 <= data.len() {
                out[di..di + 4].copy_from_slice(&data[si..si + 4]);
            }
        }
    }
    (w, h, out)
}

/// The smallest integer factor that brings `width` down to `max_width`.
#[must_use]
pub fn fit_factor(width: u32, max_width: u32) -> u32 {
    if max_width == 0 || width <= max_width {
        return 1;
    }
    width.div_ceil(max_width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::FOLLOWER_SIZE;

    #[test]
    fn ids_are_arithmetic_and_invertible() {
        for index in [0, 1, 4, 99] {
            let ids = Ids::for_window(index);
            assert_eq!(Ids::window_index(ids.window), Some(index));
            assert_eq!(ids.window.raw(), index * STRIDE + 1);
        }
        // A non-root id is not a window.
        assert_eq!(Ids::window_index(Ids::for_window(2).follower), None);
        assert_eq!(Ids::window_index(NodeId::NONE), None);
    }

    /// Every id a window uses must be distinct, and none may be zero —
    /// the server rejects id 0 and a collision would be a silent overwrite.
    #[test]
    fn a_windows_ids_are_distinct_and_non_zero() {
        for index in [0, 1, 7] {
            let ids = Ids::for_window(index);
            let mut all = vec![ids.window, ids.background, ids.image, ids.follower];
            all.extend(ids.cards);
            all.push(ids.trail);
            all.push(ids.mover);
            all.extend(ids.outlines);
            assert!(all.iter().all(|i| !i.is_none()));
            let mut raw: Vec<u32> = all.iter().map(|i| i.raw()).collect();
            raw.sort_unstable();
            let before = raw.len();
            raw.dedup();
            assert_eq!(raw.len(), before, "duplicate id in window {index}");
        }
    }

    /// Two adjacent windows must not share an id either, or `--windows 2`
    /// would have the second window's `CreateNode` rejected.
    #[test]
    fn windows_do_not_share_ids() {
        let a = Ids::for_window(0);
        let b = Ids::for_window(1);
        assert!(a.outlines[7].raw() < b.window.raw(), "STRIDE too small");
    }

    #[test]
    fn the_cards_are_a_row_inside_the_window() {
        let size = WINDOW_SIZE;
        let cs = cards(size);
        for c in &cs {
            assert!(c.x >= 0.0 && c.right() <= size.w, "{c:?}");
            assert!(c.y >= 0.0 && c.bottom() <= size.h, "{c:?}");
        }
        for pair in cs.windows(2) {
            assert!(pair[0].right() <= pair[1].x, "cards overlap");
            assert!((pair[0].y - pair[1].y).abs() < f32::EPSILON, "not a row");
        }
    }

    #[test]
    fn the_image_and_mover_stay_inside_the_window() {
        let size = WINDOW_SIZE;
        for r in [image_rect(size), mover_rect(size, 0.0)] {
            assert!(r.right() <= size.w && r.bottom() <= size.h, "{r:?}");
        }
    }

    #[test]
    fn the_animation_bounces_off_both_edges() {
        let size = WINDOW_SIZE;
        let limit = size.w - mover_rect(size, 0.0).w;
        let (x, dir) = advance(size, limit - 1.0, ANIMATE_STEP);
        assert!((x - limit).abs() < f32::EPSILON);
        assert!(dir < 0.0, "it must turn around at the right edge");
        let (x, dir) = advance(size, 1.0, -ANIMATE_STEP);
        assert!(x.abs() < f32::EPSILON);
        assert!(dir > 0.0, "and at the left one");
    }

    #[test]
    fn a_build_creates_the_window_first_and_every_node_once() {
        let ids = Ids::for_window(0);
        let fd = memfd(&checker(IMG_EDGE)).unwrap();
        let msgs = build(ids, WINDOW_SIZE, "demo", Some(fd));
        assert!(matches!(msgs.first(), Some(ClientMsg::CreateWindow(_))));
        let mut created: Vec<u32> = msgs
            .iter()
            .filter_map(|m| match m {
                ClientMsg::CreateNode(c) => Some(c.id.raw()),
                _ => None,
            })
            .collect();
        let before = created.len();
        created.sort_unstable();
        created.dedup();
        assert_eq!(created.len(), before, "a node is created twice");
        // 1 background + 4 cards + image + mover + trail + follower + 8 outlines.
        assert_eq!(before, 17);
        // Every created node hangs off the window root.
        assert!(msgs.iter().all(|m| match m {
            ClientMsg::CreateNode(c) => c.parent == ids.window,
            _ => true,
        }));
    }

    /// The buffer must be registered before the node that samples it: the
    /// server applies a transaction in arrival order and `SetImage`
    /// naming an unknown buffer is a fatal `BadBuffer`.
    #[test]
    fn the_buffer_is_created_before_it_is_used() {
        let ids = Ids::for_window(0);
        let fd = memfd(&checker(IMG_EDGE)).unwrap();
        let msgs = build(ids, WINDOW_SIZE, "demo", Some(fd));
        let create = msgs
            .iter()
            .position(|m| matches!(m, ClientMsg::CreateBuffer(_)))
            .unwrap();
        let set = msgs
            .iter()
            .position(|m| matches!(m, ClientMsg::SetImage(_)))
            .unwrap();
        assert!(create < set);
    }

    /// A **remote** connection gets the same scene without the image:
    /// its pixels ride on a descriptor, and TCP has no way to carry one.
    /// Everything else — backdrop, cards, mover, follower, outlines — is
    /// byte-for-byte what a local client sends, which is what makes the
    /// remote latency figures comparable to the local ones.
    #[test]
    fn a_remote_build_drops_the_image_and_keeps_everything_else() {
        let ids = Ids::for_window(0);
        let local = build(
            ids,
            WINDOW_SIZE,
            "demo",
            Some(memfd(&checker(IMG_EDGE)).unwrap()),
        );
        let remote = build(ids, WINDOW_SIZE, "demo", None);
        assert!(
            !remote
                .iter()
                .any(|m| matches!(m, ClientMsg::CreateBuffer(_) | ClientMsg::SetImage(_))),
            "a remote scene carries no buffer and no image"
        );
        let nodes = |msgs: &[ClientMsg]| -> Vec<u32> {
            msgs.iter()
                .filter_map(|m| match m {
                    ClientMsg::CreateNode(c) => Some(c.id.raw()),
                    _ => None,
                })
                .collect()
        };
        let (l, r) = (nodes(&local), nodes(&remote));
        assert_eq!(r.len(), l.len() - 1, "exactly one node fewer");
        assert!(!r.contains(&ids.image.raw()));
        assert!(
            l.iter().filter(|id| **id != ids.image.raw()).eq(r.iter()),
            "and the rest, in the same order"
        );
        // The window still comes first, so the scene is still buildable.
        assert!(matches!(remote.first(), Some(ClientMsg::CreateWindow(_))));
    }

    /// A remote window's `reconfigure` must not name the image node.
    ///
    /// The bug this pins was found on the box and not by any of the 61
    /// tests above: the remote demo connected, built a scene without the
    /// image, painted — and died on the **first `Configure`** with
    /// `UnknownNode: no node with id 7`, because the re-layout still laid
    /// out a node the build had never created. Dropping a node from a
    /// scene is not one edit, it is two, and the second one is in a code
    /// path that only runs when the window is resized.
    #[test]
    fn a_remote_reconfigure_lays_out_only_nodes_that_exist() {
        let ids = Ids::for_window(0);
        let built: Vec<u32> = build(ids, WINDOW_SIZE, "demo", None)
            .iter()
            .filter_map(|m| match m {
                ClientMsg::CreateNode(c) => Some(c.id.raw()),
                _ => None,
            })
            .collect();
        let laid_out: Vec<u32> = reconfigure(ids, Size::new(800.0, 600.0), false)
            .iter()
            .filter_map(|m| match m {
                ClientMsg::SetBounds(b) => Some(b.id.raw()),
                ClientMsg::SetFill(f) => Some(f.id.raw()),
                _ => None,
            })
            .collect();
        for id in &laid_out {
            assert!(
                built.contains(id) || *id == ids.window.raw(),
                "reconfigure names node {id}, which a remote build never created"
            );
        }
        assert!(!laid_out.contains(&ids.image.raw()));
        // And the local form still does lay the image out, or the local
        // window would stop resizing its picture.
        let local: Vec<u32> = reconfigure(ids, Size::new(800.0, 600.0), true)
            .iter()
            .filter_map(|m| match m {
                ClientMsg::SetBounds(b) => Some(b.id.raw()),
                _ => None,
            })
            .collect();
        assert!(local.contains(&ids.image.raw()));
    }

    /// The trail must be created before the follower, or the ghost would
    /// paint over the thing it is a ghost of.
    #[test]
    fn the_follower_is_above_its_trail() {
        let ids = Ids::for_window(0);
        let fd = memfd(&checker(IMG_EDGE)).unwrap();
        let msgs = build(ids, WINDOW_SIZE, "demo", Some(fd));
        let at = |id: NodeId| {
            msgs.iter().position(|m| match m {
                ClientMsg::CreateNode(c) => c.id == id,
                _ => false,
            })
        };
        assert!(at(ids.trail) < at(ids.follower));
    }

    #[test]
    fn following_moves_the_follower_and_leaves_the_trail() {
        let ids = Ids::for_window(0);
        let from = Rect::new(10.0, 10.0, FOLLOWER_SIZE, FOLLOWER_SIZE);
        let (msgs, to, damage) = follow(ids, WINDOW_SIZE, from, Point::new(400.0, 250.0));
        let bounds: Vec<(NodeId, Rect)> = msgs
            .iter()
            .filter_map(|m| match m {
                ClientMsg::SetBounds(b) => Some((b.id, b.rect)),
                _ => None,
            })
            .collect();
        assert_eq!(bounds, vec![(ids.follower, to), (ids.trail, from)]);
        assert_eq!(damage, moved_damage(from, to));
    }

    #[test]
    fn outlines_are_hidden_when_damage_is_off() {
        let ids = Ids::for_window(0);
        let damage = vec![Rect::new(0.0, 0.0, 40.0, 40.0)];
        let msgs = damage_outlines(ids, &damage, false);
        assert!(
            msgs.iter()
                .all(|m| matches!(m, ClientMsg::SetVisible(SetVisible { visible: false, .. })))
        );
        assert_eq!(msgs.len(), 8, "every outline node is addressed");
    }

    /// One damage rect must leave the other four outline nodes hidden, or
    /// the previous move's outline would linger.
    #[test]
    fn a_single_damage_rect_hides_the_second_outline() {
        let ids = Ids::for_window(0);
        let damage = vec![Rect::new(0.0, 0.0, 40.0, 40.0)];
        let msgs = damage_outlines(ids, &damage, true);
        let visible: Vec<NodeId> = msgs
            .iter()
            .filter_map(|m| match m {
                ClientMsg::SetVisible(v) if v.visible => Some(v.id),
                _ => None,
            })
            .collect();
        assert_eq!(visible, ids.outlines[..4].to_vec());
    }

    #[test]
    fn the_checker_is_argb_with_a_transparent_rim() {
        let px = checker(IMG_EDGE);
        assert_eq!(px.len(), IMG_BYTES as usize);
        // The corner is outside the radius: fully transparent.
        assert_eq!(px[3], 0);
        // The centre is very nearly opaque: the exact centre of a 64-px
        // edge falls between pixels, so the nearest one is half a pixel
        // out and fades by that much.
        let c = ((IMG_EDGE / 2) * IMG_STRIDE + (IMG_EDGE / 2) * 4) as usize;
        assert!(px[c + 3] > 240, "centre alpha {}", px[c + 3]);
        // Halfway to the rim it is roughly half faded.
        let m = ((IMG_EDGE / 2) * IMG_STRIDE + (IMG_EDGE / 4) * 4) as usize;
        assert!((100..=160).contains(&px[m + 3]), "mid alpha {}", px[m + 3]);
    }

    #[test]
    fn a_memfd_holds_what_was_written() {
        let px = checker(8);
        let fd = memfd(&px).unwrap();
        let mut back = vec![0u8; px.len()];
        let n = rustix::io::pread(&fd, &mut back, 0).unwrap();
        assert_eq!(n, px.len());
        assert_eq!(back, px);
    }

    #[test]
    fn downscaling_picks_nearest_neighbours() {
        // 4x2 image, stride with padding, factor 2 → 2x1.
        let (w, h, stride) = (4u32, 2u32, 4 * 4 + 8);
        let mut data = vec![0u8; (stride * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = (y * stride + x * 4) as usize;
                data[i] = (x + y * 10) as u8;
            }
        }
        let (ow, oh, out) = downscale(w, h, stride, &data, 2);
        assert_eq!((ow, oh), (2, 1));
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 0, "source (0,0)");
        assert_eq!(out[4], 2, "source (2,0)");
    }

    #[test]
    fn a_factor_of_one_is_a_copy_without_padding() {
        let (w, h, stride) = (2u32, 2u32, 2 * 4 + 4);
        let data: Vec<u8> = (0..(stride * h) as u8).collect();
        let (ow, oh, out) = downscale(w, h, stride, &data, 1);
        assert_eq!((ow, oh, out.len()), (2, 2, 16));
        assert_eq!(&out[..4], &data[..4]);
        assert_eq!(&out[8..12], &data[stride as usize..stride as usize + 4]);
    }

    #[test]
    fn the_fit_factor_reaches_the_target_width() {
        assert_eq!(fit_factor(1920, 960), 2);
        assert_eq!(fit_factor(1920, 900), 3);
        assert_eq!(fit_factor(800, 960), 1);
        assert_eq!(fit_factor(1920, 0), 1);
        for (w, max) in [(1920u32, 640u32), (1366, 500), (3840, 960)] {
            assert!(w / fit_factor(w, max) <= max);
        }
    }
}

//! The bridge to the scene: node-id allocation, the per-widget paint-slot
//! cache, synchronous text measurement, and the mutation tap.
//!
//! Everything a widget draws goes through here, and everything here goes
//! through one [`Connection`]. Two properties are worth stating because
//! the rest of the toolkit is built on them:
//!
//! * **A mutation is only sent when the value actually changed.** Each
//!   paint slot caches the last value sent for every property, so a
//!   repaint that produces the same rect and the same fill costs nothing
//!   on the wire — which is what makes "repaint the dirty widget" cheap
//!   enough to be the only strategy.
//! * **Nothing is sent outside a commit.** [`Wire::commit`] is called once
//!   per [`Ui::flush`](crate::Ui::flush), and only if that flush produced
//!   any mutation at all, so an idle app puts zero bytes on the socket.

use std::collections::HashMap;

use nitro_core::{Color, Rect, Size, Transform};
use nitro_wire::client::Connection;
use nitro_wire::msg::{
    self, ClientMsg, CreateNode, DestroyNode, Fill, Reparent, ServerMsg, SetBorder, SetBounds,
    SetClip, SetCorners, SetFill, SetImage, SetText, SetTransform,
};
use nitro_wire::types::{BufferId, Edge, Layer, NodeId, NodeKind, caps};

use crate::error::Error;
use crate::theme::TextStyle;
use crate::widget::TextRun;

/// One mutation, as the tap records it.
///
/// Only recorded while [`Ui::tap`](crate::Ui::tap) is on, which is a test
/// facility: a test asserts *which* nodes were touched and how many
/// messages a change cost, and that is the only honest way to check the
/// "work is proportional to what changed" claim from the outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    /// The message name, e.g. `SetText`.
    pub op: &'static str,
    /// The node it applies to, or [`NodeId::NONE`] for `Commit`.
    pub node: NodeId,
}

/// Measured extent of a shaped string.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TextMetrics {
    /// Width of the longest line, in logical pixels.
    pub width: f32,
    /// Total height of all lines.
    pub height: f32,
    /// Ascent of the first line above its baseline.
    pub ascent: f32,
    /// Descent of the last line below its baseline.
    pub descent: f32,
    /// Number of laid-out lines.
    pub line_count: u32,
}
impl TextMetrics {
    /// The measured block as a size.
    #[must_use]
    pub fn size(self) -> Size {
        Size::new(self.width, self.height)
    }
}

/// Cache key for a measurement: the string, its style and the wrap width.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MeasureKey {
    text: String,
    family: String,
    size_bits: u32,
    weight: u16,
    italic: bool,
    max_width_bits: u32,
}

impl MeasureKey {
    fn new(text: &str, style: &TextStyle, max_width: f32) -> Self {
        Self {
            text: text.to_owned(),
            family: style.family.clone(),
            size_bits: style.size_px.to_bits(),
            weight: style.weight,
            italic: style.italic,
            max_width_bits: max_width.to_bits(),
        }
    }
}

/// Memo of [`Wire::measure_text`]; see the module docs for why the
/// measurement is synchronous.
#[derive(Debug, Default)]
pub(crate) struct TextMeasureCache {
    map: HashMap<MeasureKey, TextMetrics>,
    /// Cursor positions from the same round trips, kept separately
    /// because only a text field ever asks for them and they are a
    /// `Vec` per string rather than six floats.
    cursors: HashMap<MeasureKey, Vec<(u32, f32)>>,
}

/// Where a paint slot's node belongs in the scene: under `parent`,
/// before `before`, at slot number `index`.
///
/// The three travel together through every `paint_*` call, so they are
/// one argument rather than three.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlotAt {
    pub(crate) parent: NodeId,
    pub(crate) before: NodeId,
    pub(crate) index: usize,
}

/// What a paint slot last drew, so the next paint can diff against it.
#[derive(Debug)]
pub(crate) struct PaintSlot {
    pub(crate) node: NodeId,
    kind: NodeKind,
    bounds: Rect,
    fill: Fill,
    radius: f32,
    border: (f32, Color),
    text: Option<SetText>,
    image: Option<(BufferId, nitro_core::IRect)>,
    /// Group slots only: the transform applied to the slot's children,
    /// and whether they are clipped to its bounds.
    pub(crate) transform: Transform,
    clip: bool,
    /// Whether this slot was emitted by the paint that is running.
    used: bool,
}

impl PaintSlot {
    /// An empty slot: no node, no cached values.
    fn empty(kind: NodeKind) -> Self {
        Self {
            node: NodeId::NONE,
            kind,
            bounds: Rect::EMPTY,
            fill: Fill::None,
            radius: 0.0,
            border: (0.0, Color::TRANSPARENT),
            text: None,
            image: None,
            transform: Transform::IDENTITY,
            clip: false,
            used: false,
        }
    }
}

/// The client half of the scene: one connection, the node-id allocator
/// and the mutation counters.
pub(crate) struct Wire {
    conn: Connection,
    /// Next never-used node id. `NodeId(1)` is the window root.
    next_node: u32,
    free_nodes: Vec<NodeId>,
    serial: u32,
    /// Mutations queued since the last commit. Zero means no commit is
    /// sent, which is what makes idle free.
    pending: u32,
    /// Total commits sent, for tests and stats.
    pub(crate) commits: u32,
    tap: Option<Vec<Mutation>>,
    /// Server messages picked up while waiting for a `TextMeasured`.
    pub(crate) stray: Vec<ServerMsg>,
    next_request: u32,
    /// Next never-used buffer id. `BufferId(0)` is "no buffer".
    next_buffer: u32,
    pub(crate) text_cache: TextMeasureCache,
    /// Whether the window's subtree is shown, so an unchanged
    /// [`Wire::set_visible`] costs no mutation and therefore no commit.
    window_visible: bool,
}

impl Wire {
    pub(crate) fn new(conn: Connection) -> Self {
        Self {
            conn,
            next_node: 2,
            free_nodes: Vec::new(),
            serial: 1,
            pending: 0,
            commits: 0,
            tap: None,
            stray: Vec::new(),
            next_request: 1,
            next_buffer: 1,
            text_cache: TextMeasureCache::default(),
            window_visible: true,
        }
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Turn the mutation tap on or off, clearing whatever it holds.
    pub(crate) fn set_tap(&mut self, on: bool) {
        self.tap = if on { Some(Vec::new()) } else { None };
    }

    /// The mutations recorded since the tap was turned on.
    pub(crate) fn taped(&self) -> &[Mutation] {
        self.tap.as_deref().unwrap_or(&[])
    }

    pub(crate) fn clear_tap(&mut self) {
        if let Some(t) = &mut self.tap {
            t.clear();
        }
    }

    /// Allocate a scene node id.
    pub(crate) fn alloc_node(&mut self) -> NodeId {
        if let Some(id) = self.free_nodes.pop() {
            return id;
        }
        let id = NodeId(self.next_node);
        self.next_node += 1;
        id
    }

    /// Give a node id back. Safe only after a `DestroyNode` for it has
    /// been queued: the server frees the id at the next commit.
    fn free_node(&mut self, id: NodeId) {
        self.free_nodes.push(id);
    }

    fn send(&mut self, msg: &ClientMsg, node: NodeId) -> Result<(), Error> {
        if let Some(t) = &mut self.tap {
            t.push(Mutation {
                op: msg.name(),
                node,
            });
        }
        self.pending += 1;
        self.conn.send(msg)?;
        Ok(())
    }

    /// End the transaction and push it at the socket. Does nothing when
    /// no mutation was queued — an idle app sends no bytes at all.
    pub(crate) fn commit(&mut self) -> Result<bool, Error> {
        if self.pending == 0 {
            return Ok(false);
        }
        self.pending = 0;
        let serial = self.serial;
        self.serial = self.serial.wrapping_add(1).max(1);
        if let Some(t) = &mut self.tap {
            t.push(Mutation {
                op: "Commit",
                node: NodeId::NONE,
            });
        }
        self.conn.commit(serial)?;
        self.commits += 1;
        self.flush_all()?;
        Ok(true)
    }

    /// Write everything queued, waiting for writability as needed.
    ///
    /// A toolkit flush happens outside the frame path (it *is* the frame
    /// path) and the alternative — carrying a half-written transaction
    /// across event-loop turns — would mean the scene could show a torn
    /// batch. The socket only fills up if the server has stopped reading,
    /// which is fatal anyway.
    pub(crate) fn flush_all(&mut self) -> Result<(), Error> {
        while !self.conn.flush()? {
            wait(self.conn.as_fd(), rustix::event::PollFlags::OUT)?;
        }
        Ok(())
    }

    /// Create the one top-level window, on `layer` and with `flags`.
    ///
    /// Both are `Normal`/`0` for an ordinary app. A shell surface passes
    /// its own — and passes them *here*, in the same transaction as the
    /// window itself, because a bar's layer is not a property it acquires
    /// a frame later: a `Top` window created as `Normal` would be visibly
    /// in the window-management z-order until the next commit.
    pub(crate) fn create_window(
        &mut self,
        id: NodeId,
        title: &str,
        size: Size,
        layer: Layer,
        flags: u32,
    ) -> Result<(), Error> {
        self.send(
            &ClientMsg::CreateWindow(msg::CreateWindow {
                id,
                size,
                layer,
                flags,
                title: title.to_owned(),
            }),
            id,
        )
    }

    /// Give the window an application id.
    ///
    /// Sent for every app, from the name it was constructed with, because
    /// the app id is what a *window list* shows and groups by: a bar
    /// cannot tell two untitled windows apart without it, and it is the
    /// only field that names the program rather than the document. It
    /// rides the window's own first commit, so a window never exists
    /// without one.
    pub(crate) fn set_app_id(&mut self, id: NodeId, app_id: &str) -> Result<(), Error> {
        self.send(
            &ClientMsg::SetAppId(msg::SetAppId {
                window: id,
                app_id: app_id.to_owned(),
            }),
            id,
        )
    }

    /// Change the window's title, as a queued mutation.
    ///
    /// Queued rather than sent at once: it is a property of the window
    /// and belongs in the same transaction as whatever else changed with
    /// it — a terminal that clears the screen and sets a new title in one
    /// escape sequence should not show the new title over the old screen
    /// for a frame.
    pub(crate) fn set_window_title(&mut self, id: NodeId, title: &str) -> Result<(), Error> {
        self.send(
            &ClientMsg::SetWindowTitle(msg::SetWindowTitle {
                window: id,
                title: title.to_owned(),
            }),
            id,
        )
    }

    /// Set the window's min/max content size, as a queued mutation.
    pub(crate) fn set_window_limits(
        &mut self,
        id: NodeId,
        min: Size,
        max: Size,
    ) -> Result<(), Error> {
        self.send(
            &ClientMsg::SetWindowLimits(msg::SetWindowLimits {
                window: id,
                min,
                max,
            }),
            id,
        )
    }

    /// Anchor the window to its output's edges (needs `caps::SHELL`).
    ///
    /// Queued as a mutation rather than sent at once, so it rides the
    /// same commit as the `CreateWindow` above. That ordering is not a
    /// nicety: the server buffers `SetAnchor` to the sender's commit
    /// precisely so a bar can create and anchor a window in one
    /// transaction (`docs/shell.md`), and splitting them would make the
    /// bar paint at its placeholder size for a frame.
    pub(crate) fn set_anchor(&mut self, id: NodeId, edges: u8, margin: u32) -> Result<(), Error> {
        self.send(
            &ClientMsg::SetAnchor(msg::SetAnchor {
                window: id,
                edges,
                margin,
            }),
            id,
        )
    }

    /// Reserve `px` logical pixels along `edge` of the window's output
    /// (needs `caps::SHELL`). Buffered to the commit, as `set_anchor` is.
    pub(crate) fn set_exclusive_zone(
        &mut self,
        id: NodeId,
        edge: Edge,
        px: u32,
    ) -> Result<(), Error> {
        self.send(
            &ClientMsg::SetExclusiveZone(msg::SetExclusiveZone {
                window: id,
                edge,
                px,
            }),
            id,
        )
    }

    /// Send a shell op that is answered **on receipt** rather than at a
    /// commit — the questions and the ops on other clients' windows.
    ///
    /// It deliberately does not go through [`Wire::send`]: those count as
    /// pending mutations and would make the next flush commit, so a bar
    /// that merely asked a question would break the idle contract.
    pub(crate) fn send_now(&mut self, msg: &ClientMsg) -> Result<(), Error> {
        self.conn.send(msg)?;
        self.flush_all()
    }

    /// Create a `Group` under `parent`, before `before` (or appended).
    pub(crate) fn create_group(
        &mut self,
        id: NodeId,
        parent: NodeId,
        before: NodeId,
    ) -> Result<(), Error> {
        self.send(
            &ClientMsg::CreateNode(CreateNode {
                id,
                kind: NodeKind::Group,
                parent,
                before,
            }),
            id,
        )
    }

    pub(crate) fn reparent(
        &mut self,
        id: NodeId,
        parent: NodeId,
        before: NodeId,
    ) -> Result<(), Error> {
        self.send(&ClientMsg::Reparent(Reparent { id, parent, before }), id)
    }

    /// Create a `Rect` under `parent`, before `before` (or appended).
    /// Used for the window backdrop, which is not a widget's paint slot.
    pub(crate) fn create_rect(
        &mut self,
        id: NodeId,
        parent: NodeId,
        before: NodeId,
    ) -> Result<(), Error> {
        self.send(
            &ClientMsg::CreateNode(CreateNode {
                id,
                kind: NodeKind::Rect,
                parent,
                before,
            }),
            id,
        )
    }

    /// Size and fill the window backdrop.
    pub(crate) fn set_backdrop(
        &mut self,
        id: NodeId,
        rect: Rect,
        color: Color,
    ) -> Result<(), Error> {
        self.set_bounds(id, rect)?;
        self.send(
            &ClientMsg::SetFill(SetFill {
                id,
                fill: Fill::Solid(color),
            }),
            id,
        )
    }

    pub(crate) fn set_bounds(&mut self, id: NodeId, rect: Rect) -> Result<(), Error> {
        self.send(&ClientMsg::SetBounds(SetBounds { id, rect }), id)
    }

    /// Show or hide a node and its subtree.
    ///
    /// Queued as a mutation, so it rides the next commit. That is what
    /// makes a launcher's hide cost **one** message: the tree underneath
    /// is untouched, so nothing else is dirty and the commit carries this
    /// and nothing more.
    ///
    /// The window's own last-sent value is cached here rather than in
    /// [`Ui`](crate::Ui), because this is the layer that already answers
    /// "has this property changed?" for every paint slot — a second copy
    /// of the same question upstairs is how the two drift.
    pub(crate) fn set_visible(&mut self, id: NodeId, visible: bool) -> Result<(), Error> {
        if id == crate::ui::WINDOW {
            if self.window_visible == visible {
                return Ok(());
            }
            self.window_visible = visible;
        }
        self.send(&ClientMsg::SetVisible(msg::SetVisible { id, visible }), id)
    }

    /// Whether the window is currently shown.
    pub(crate) fn window_visible(&self) -> bool {
        self.window_visible
    }

    /// Take or release the keyboard grab on one of this client's windows
    /// (needs `caps::SHELL`).
    ///
    /// Buffered to the commit like the other three window-naming shell
    /// ops, which is also why it is queued rather than sent now: a
    /// launcher un-hides itself and re-takes the grab in **one**
    /// transaction, and a grab answered on receipt would be looking at a
    /// window the commit has not un-hidden yet — the server drops a grab
    /// on a window that is not showing.
    pub(crate) fn grab_keyboard(&mut self, id: NodeId, on: bool) -> Result<(), Error> {
        self.send(
            &ClientMsg::GrabKeyboard(msg::GrabKeyboard { window: id, on }),
            id,
        )
    }

    pub(crate) fn destroy_node(&mut self, id: NodeId) -> Result<(), Error> {
        self.send(&ClientMsg::DestroyNode(DestroyNode { id }), id)?;
        self.free_node(id);
        Ok(())
    }

    /// Whether the server can actually draw text.
    pub(crate) fn has_text(&self) -> bool {
        self.conn.has_caps(caps::TEXT)
    }

    /// Whether the link to the server is remote: no buffers, no images.
    pub(crate) fn is_remote(&self) -> bool {
        self.conn.has_caps(caps::REMOTE)
    }

    /// Measure a string, synchronously.
    ///
    /// **This blocks on a round trip**, and that is an M2 decision rather
    /// than an oversight. A label cannot say how big it is until the
    /// server — which owns the fonts — has shaped its string, and every
    /// alternative (measure asynchronously and re-lay-out when the answer
    /// arrives, or ship a font library in every client) is a larger
    /// change than M2 can carry. The cost is bounded by the cache: a
    /// string is measured once per `(text, style, max_width)`, so a
    /// settled UI does no round trips at all, and a `MeasureText` is
    /// answered on receipt rather than at a commit, so the wait is one
    /// socket turnaround and not one frame. The async path — measure
    /// optimistically, lay out on `TextMeasured` — is the M3 answer once
    /// there is a widget whose text changes every keystroke.
    ///
    /// With no `TEXT` capability there is nothing to ask, and the answer
    /// is an estimate from the font size so a fontless server still lays
    /// out a plausible tree.
    pub(crate) fn measure_text(
        &mut self,
        text: &str,
        style: &TextStyle,
        max_width: f32,
    ) -> Result<TextMetrics, Error> {
        let key = MeasureKey::new(text, style, max_width);
        if let Some(m) = self.text_cache.map.get(&key) {
            return Ok(*m);
        }
        let metrics = if self.has_text() {
            self.round_trip(text, style, max_width)?
        } else {
            estimate(text, style)
        };
        self.text_cache.map.insert(key, metrics);
        Ok(metrics)
    }

    /// The x of every cursor position inside `text`, as the server
    /// shaped it: `(byte offset, x)` in increasing offset order.
    ///
    /// A text field needs this to place a caret, and it cannot compute
    /// it: the client has no fonts. It rides the same `MeasureText`
    /// round trip and the same cache, so a caret move in a string the
    /// field has already measured costs nothing.
    ///
    /// With no `TEXT` capability the positions are estimated from the
    /// font size, which is what keeps a fontless server usable.
    pub(crate) fn cursor_positions(
        &mut self,
        text: &str,
        style: &TextStyle,
    ) -> Result<Vec<(u32, f32)>, Error> {
        let key = MeasureKey::new(text, style, 0.0);
        if let Some(c) = self.text_cache.cursors.get(&key) {
            return Ok(c.clone());
        }
        if self.has_text() {
            let metrics = self.round_trip(text, style, 0.0)?;
            self.text_cache.map.insert(key.clone(), metrics);
        } else {
            self.text_cache
                .cursors
                .insert(key.clone(), estimate_cursors(text, style));
        }
        Ok(self
            .text_cache
            .cursors
            .get(&key)
            .cloned()
            .unwrap_or_default())
    }

    fn round_trip(
        &mut self,
        text: &str,
        style: &TextStyle,
        max_width: f32,
    ) -> Result<TextMetrics, Error> {
        let request = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        self.conn.measure_text(msg::MeasureText {
            request,
            size_px: style.size_px,
            weight: style.weight,
            italic: style.italic,
            max_width,
            wrap: max_width > 0.0,
            family: style.family.clone(),
            text: text.to_owned(),
        })?;
        self.flush_all()?;
        let mut batch = Vec::new();
        loop {
            batch.clear();
            self.conn.poll(&mut batch)?;
            let mut found = None;
            for m in batch.drain(..) {
                match m {
                    ServerMsg::TextMeasured(t) if t.request == request => {
                        // The cursor vector rides along: it is measured
                        // at the same time, and a text field asking for
                        // it later must not cost a second round trip.
                        self.text_cache.cursors.insert(
                            MeasureKey::new(text, style, max_width),
                            t.cursor_x.iter().map(|c| (c.offset, c.x)).collect(),
                        );
                        found = Some(TextMetrics {
                            width: t.width,
                            height: t.height,
                            ascent: t.ascent,
                            descent: t.descent,
                            line_count: t.line_count,
                        });
                    }
                    // Anything else that arrived while we waited is a real
                    // event: keep it for the caller's next drain rather
                    // than dropping a keystroke on the floor.
                    other => self.stray.push(other),
                }
            }
            if let Some(m) = found {
                return Ok(m);
            }
            wait(self.conn.as_fd(), rustix::event::PollFlags::IN)?;
        }
    }

    /// Emit a rect slot, sending only the properties that changed.
    pub(crate) fn paint_rect(
        &mut self,
        slots: &mut Vec<PaintSlot>,
        at: SlotAt,
        rect: Rect,
        fill: Fill,
        radius: f32,
        border: (f32, Color),
    ) -> Result<(), Error> {
        let index = at.index;
        let fresh = self.ensure_slot(slots, at, NodeKind::Rect)?;
        let slot = &mut slots[index];
        slot.used = true;
        let (node, changed_bounds) = (slot.node, fresh || slot.bounds != rect);
        let changed_fill = fresh || slot.fill != fill;
        // Exact comparison is the right one: the question is whether the
        // value we would send differs from the one we sent, not whether
        // two computed floats are near each other.
        let changed_radius = fresh || slot.radius.to_bits() != radius.to_bits();
        let changed_border =
            fresh || slot.border.0.to_bits() != border.0.to_bits() || slot.border.1 != border.1;
        slot.bounds = rect;
        slot.fill = fill;
        slot.radius = radius;
        slot.border = border;
        if changed_bounds {
            self.set_bounds(node, rect)?;
        }
        if changed_fill {
            self.send(&ClientMsg::SetFill(SetFill { id: node, fill }), node)?;
        }
        if changed_radius {
            self.send(
                &ClientMsg::SetCorners(SetCorners { id: node, radius }),
                node,
            )?;
        }
        if changed_border {
            self.send(
                &ClientMsg::SetBorder(SetBorder {
                    id: node,
                    width: border.0,
                    color: border.1,
                }),
                node,
            )?;
        }
        Ok(())
    }

    /// Emit a text slot, sending only what changed.
    pub(crate) fn paint_text(
        &mut self,
        slots: &mut Vec<PaintSlot>,
        at: SlotAt,
        rect: Rect,
        text: &str,
        paint: TextRun<'_>,
    ) -> Result<(), Error> {
        let TextRun {
            style,
            color,
            align,
            max_width,
        } = paint;
        let index = at.index;
        let fresh = self.ensure_slot(slots, at, NodeKind::Text)?;
        let slot = &mut slots[index];
        slot.used = true;
        let node = slot.node;
        let want = SetText {
            node,
            size_px: style.size_px,
            weight: style.weight,
            italic: style.italic,
            max_width,
            // `wrap` only bites with a non-zero `max_width`, and the two
            // are decided together so the painted run matches the
            // measured one exactly.
            wrap: max_width > 0.0,
            align,
            color,
            family: style.family.clone(),
            text: text.to_owned(),
        };
        let changed_bounds = fresh || slot.bounds != rect;
        let changed_text = fresh || slot.text.as_ref() != Some(&want);
        slot.bounds = rect;
        if changed_text {
            slot.text = Some(want.clone());
        }
        if changed_bounds {
            self.set_bounds(node, rect)?;
        }
        if changed_text {
            self.send(&ClientMsg::SetText(want), node)?;
        }
        Ok(())
    }

    /// Emit a group slot — a `Group` node of this widget's own, with an
    /// optional clip and transform — and return its node, so the widget
    /// can paint further slots *inside* it.
    ///
    /// This is what a scrolling widget is built out of: the content hangs
    /// under a clipping group whose transform is the scroll offset, so
    /// scrolling is one `SetTransform` and nothing underneath repaints.
    pub(crate) fn paint_group(
        &mut self,
        slots: &mut Vec<PaintSlot>,
        at: SlotAt,
        rect: Rect,
        clip: bool,
        transform: Transform,
    ) -> Result<NodeId, Error> {
        let index = at.index;
        let fresh = self.ensure_slot(slots, at, NodeKind::Group)?;
        let slot = &mut slots[index];
        slot.used = true;
        let node = slot.node;
        let changed_bounds = fresh || slot.bounds != rect;
        let changed_clip = fresh || slot.clip != clip;
        let changed_transform = fresh || slot.transform != transform;
        slot.bounds = rect;
        slot.clip = clip;
        slot.transform = transform;
        if changed_bounds {
            self.set_bounds(node, rect)?;
        }
        if changed_clip {
            self.send(&ClientMsg::SetClip(SetClip { id: node, clip }), node)?;
        }
        if changed_transform {
            self.set_transform(node, transform)?;
        }
        Ok(node)
    }

    pub(crate) fn set_clip(&mut self, id: NodeId, clip: bool) -> Result<(), Error> {
        self.send(&ClientMsg::SetClip(SetClip { id, clip }), id)
    }

    pub(crate) fn set_transform(&mut self, id: NodeId, transform: Transform) -> Result<(), Error> {
        self.send(&ClientMsg::SetTransform(SetTransform { id, transform }), id)
    }

    /// Put `pixels` in a memfd, hand the descriptor to the server and
    /// return the buffer id.
    ///
    /// `pwrite` rather than `mmap`: mapping would need `unsafe`, which
    /// this tree denies, and an image's pixels are written once.
    ///
    /// **Refused on a remote link.** A buffer *is* a descriptor, and a
    /// descriptor cannot cross TCP; the server says so in `Welcome` with
    /// [`caps::REMOTE`], and this is where the toolkit acts on it. The
    /// refusal happens before the memfd is created, so a remote app that
    /// paints an image every frame does not allocate one every frame to
    /// throw away — and the error is a value, so the app reports it and
    /// keeps drawing everything else. `docs/remote.md` says which apps
    /// this costs (the wallpaper) and why.
    pub(crate) fn create_buffer(
        &mut self,
        width: u32,
        height: u32,
        alpha: bool,
        pixels: &[u8],
    ) -> Result<BufferId, Error> {
        if self.conn.has_caps(caps::REMOTE) {
            return Err(Error::Wire(nitro_wire::Error::RemoteNoFds));
        }
        let fd = memfd(pixels)?;
        let id = BufferId(self.next_buffer);
        self.next_buffer += 1;
        let stride = width * 4;
        self.send(
            &ClientMsg::CreateBuffer(msg::CreateBuffer {
                id,
                width,
                height,
                stride,
                format: if alpha {
                    nitro_wire::types::format::AR24
                } else {
                    nitro_wire::types::format::XR24
                },
                size: pixels.len() as u32,
                fd,
            }),
            NodeId::NONE,
        )?;
        Ok(id)
    }

    /// Release a buffer id. The server drops its mapping at the next
    /// commit, so the id must not be reused before then — and it is not:
    /// `next_buffer` is monotonic.
    pub(crate) fn destroy_buffer(&mut self, id: BufferId) -> Result<(), Error> {
        self.send(
            &ClientMsg::DestroyBuffer(msg::DestroyBuffer { id }),
            NodeId::NONE,
        )
    }

    /// Emit an image slot, sending only what changed.
    pub(crate) fn paint_image(
        &mut self,
        slots: &mut Vec<PaintSlot>,
        at: SlotAt,
        rect: Rect,
        buffer: BufferId,
        src: nitro_core::IRect,
    ) -> Result<(), Error> {
        let index = at.index;
        let fresh = self.ensure_slot(slots, at, NodeKind::Image)?;
        let slot = &mut slots[index];
        slot.used = true;
        let node = slot.node;
        let changed_bounds = fresh || slot.bounds != rect;
        let changed_image = fresh || slot.image != Some((buffer, src));
        slot.bounds = rect;
        slot.image = Some((buffer, src));
        if changed_bounds {
            self.set_bounds(node, rect)?;
        }
        if changed_image {
            self.send(
                &ClientMsg::SetImage(SetImage {
                    id: node,
                    buffer,
                    src,
                }),
                node,
            )?;
        }
        Ok(())
    }

    /// Make sure slot `index` exists with the right node kind. Returns
    /// whether the node was just created, which forces every property to
    /// be sent.
    fn ensure_slot(
        &mut self,
        slots: &mut Vec<PaintSlot>,
        at: SlotAt,
        kind: NodeKind,
    ) -> Result<bool, Error> {
        let SlotAt {
            parent,
            before,
            index,
        } = at;
        while slots.len() <= index {
            slots.push(PaintSlot::empty(kind));
        }
        // A slot that changed kind is a different node; drop the old one.
        if !slots[index].node.is_none() && slots[index].kind != kind {
            let old = slots[index].node;
            self.destroy_node(old)?;
            slots[index].node = NodeId::NONE;
        }
        if slots[index].node.is_none() {
            let node = self.alloc_node();
            self.send(
                &ClientMsg::CreateNode(CreateNode {
                    id: node,
                    kind,
                    parent,
                    before,
                }),
                node,
            )?;
            slots[index] = PaintSlot {
                node,
                used: true,
                ..PaintSlot::empty(kind)
            };
            return Ok(true);
        }
        Ok(false)
    }

    /// Clear the `used` marks before a widget paints.
    pub(crate) fn begin_paint(slots: &mut [PaintSlot]) {
        for s in slots {
            s.used = false;
        }
    }

    /// Destroy the nodes of slots the paint did not emit this time.
    pub(crate) fn end_paint(&mut self, slots: &mut Vec<PaintSlot>) -> Result<(), Error> {
        for i in (0..slots.len()).rev() {
            if slots[i].used {
                continue;
            }
            if !slots[i].node.is_none() {
                let node = slots[i].node;
                self.destroy_node(node)?;
            }
            // Only trailing slots can be removed; a hole would renumber
            // the ones after it.
            if i + 1 == slots.len() {
                slots.pop();
            } else {
                slots[i].node = NodeId::NONE;
                slots[i].text = None;
                slots[i].image = None;
            }
        }
        Ok(())
    }
}

/// Mark a slot as unchanged, so [`Wire::end_paint`] neither diffs nor
/// destroys it. See [`PaintCx::keep`](crate::PaintCx::keep) for why a
/// widget would want that.
pub(crate) fn keep_slot(slots: &mut [PaintSlot], index: usize) -> bool {
    match slots.get_mut(index) {
        Some(s) if !s.node.is_none() => {
            s.used = true;
            true
        }
        _ => false,
    }
}

/// Put `pixels` in a fresh memfd and hand back its descriptor.
fn memfd(pixels: &[u8]) -> Result<std::os::fd::OwnedFd, Error> {
    use rustix::io::Errno;
    let fd = rustix::fs::memfd_create("nitro-ui-image", rustix::fs::MemfdFlags::CLOEXEC)?;
    rustix::fs::ftruncate(&fd, pixels.len() as u64)?;
    let mut done = 0usize;
    while done < pixels.len() {
        match rustix::io::pwrite(&fd, &pixels[done..], done as u64) {
            Ok(0) => return Err(Errno::IO.into()),
            Ok(n) => done += n,
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(fd)
}

/// Rough metrics for a server with no fonts: enough for a layout that
/// does not collapse to nothing.
fn estimate(text: &str, style: &TextStyle) -> TextMetrics {
    let chars = text.chars().count() as f32;
    TextMetrics {
        width: chars * style.size_px * 0.55,
        height: style.size_px * 1.25,
        ascent: style.size_px * 0.8,
        descent: style.size_px * 0.2,
        line_count: 1,
    }
}

/// Rough cursor positions for a server with no fonts, from the same
/// per-character estimate [`estimate`] uses.
fn estimate_cursors(text: &str, style: &TextStyle) -> Vec<(u32, f32)> {
    let advance = style.size_px * 0.55;
    let mut out = Vec::with_capacity(text.chars().count() + 1);
    let mut x = 0.0;
    out.push((0u32, 0.0));
    for (i, c) in text.char_indices() {
        x += advance;
        out.push(((i + c.len_utf8()) as u32, x));
    }
    out
}

/// Block until `fd` is ready for `events`.
fn wait(fd: std::os::fd::BorrowedFd<'_>, events: rustix::event::PollFlags) -> Result<(), Error> {
    let mut fds = [rustix::event::PollFd::new(&fd, events)];
    loop {
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_estimate_grows_with_the_string() {
        let style = TextStyle::new("sans", 20.0);
        let a = estimate("ab", &style);
        let b = estimate("abcd", &style);
        assert!(b.width > a.width);
        assert_eq!(a.height.to_bits(), b.height.to_bits());
        assert_eq!(a.line_count, 1);
        assert_eq!(a.size().w.to_bits(), a.width.to_bits());
    }

    #[test]
    fn measure_keys_distinguish_style_and_width() {
        let s = TextStyle::new("sans", 14.0);
        let bold = TextStyle {
            weight: 700,
            ..s.clone()
        };
        assert_ne!(MeasureKey::new("x", &s, 0.0), MeasureKey::new("y", &s, 0.0));
        assert_ne!(
            MeasureKey::new("x", &s, 0.0),
            MeasureKey::new("x", &bold, 0.0)
        );
        assert_ne!(
            MeasureKey::new("x", &s, 0.0),
            MeasureKey::new("x", &s, 100.0)
        );
        assert_eq!(MeasureKey::new("x", &s, 0.0), MeasureKey::new("x", &s, 0.0));
    }
}

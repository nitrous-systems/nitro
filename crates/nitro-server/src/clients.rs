//! Wire clients: the listener, one [`WireClient`] per connection, and the
//! translation between a client's id space and the scene's keys.
//!
//! # Ids
//!
//! A client allocates its own `NodeId`s and `BufferId`s and never learns
//! the scene's keys. The server keeps a `HashMap` per client in each
//! direction: ids to keys for looking a mutation up, and keys to ids for
//! naming a node in an input event. A generational scene key makes the
//! reverse map safe — a stale key can never be confused with a recycled
//! one — and the forward map is what makes a client unable to name another
//! client's node at all, which is a cheaper isolation guarantee than any
//! ownership check.
//!
//! # Transactions
//!
//! Nothing a client sends touches the scene until its `Commit`. Mutations
//! are buffered in [`WireClient::pending`] in arrival order and applied in
//! one pass, so a frame never shows half a batch. A mutation that fails
//! aborts the whole batch: the client gets one `Error` and the connection
//! closes, because a client that sent a bad id has lost track of its own
//! tree and anything it sends afterwards is guesswork.
//!
//! # Disconnect
//!
//! Everything a client owned goes with it: its windows (and, with them,
//! their node trees) and its buffers. That is why the maps are the
//! authority — destroying "everything of client X" is a walk of one hash
//! map, not a search of the scene.

use std::collections::HashMap;
use std::os::fd::BorrowedFd;

use nitro_core::{Rect, Role, Size};
use nitro_scene::{
    Border, BufferDesc, BufferKey, ClientId, Error as SceneError, Fill as SceneFill, IconRef,
    ImageRef, NodeKey, NodeKind as SceneNodeKind, PixelStore, Scene, TextAlign, TextRef,
    WindowFlags, WindowKey, WindowState,
};
use nitro_shm::{MapError, Mapping};
use nitro_text::TextKey;
use nitro_wire::error::Error as WireError;
use nitro_wire::msg::{self, ClientMsg, ServerMsg};
use nitro_wire::server::{ClientStream, code_for};
use nitro_wire::types::{
    Align, BufferId, ErrorCode, Layer, NodeId, NodeKind, WindowState as WireWindowState, anchor,
    format, window_flags,
};

use crate::icons::IconEngine;
use crate::shell;
use crate::text::{StyleRequest, TextEngine};
use crate::{debug, warn};

/// Biggest client buffer the server will copy, in bytes. A 4K ARGB frame
/// is 33 MB; the cap is a little over that, so a legitimate fullscreen
/// image fits and a typo in a stride does not ask for a gigabyte.
pub const MAX_BUFFER_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum nodes one client may hold. The scene is a shared resource and a
/// client in a loop must not be able to exhaust it.
pub const MAX_NODES_PER_CLIENT: usize = 20_000;

/// Bytes per pixel of every format the server accepts.
const BYTES_PER_PIXEL: u32 = 4;

/// A buffered mutation, kept until the client's `Commit`.
#[derive(Debug)]
pub enum Pending {
    /// A message with no side effects until it is applied.
    Msg(Box<ClientMsg>),
    /// `CreateBuffer` with its memfd already mapped: the descriptor is
    /// checked and mapped when the message arrives, not when the
    /// transaction commits, so a buffer that fails the seal check is
    /// refused before anything else in the batch is looked at.
    Buffer(BufferId, BufferDesc, MappedPixels),
}

/// One connected wire client.
#[derive(Debug)]
pub struct WireClient {
    /// The socket, framer and outgoing buffer.
    pub stream: ClientStream,
    /// The scene's identity for this client.
    pub id: ClientId,
    /// Mutations received since the last `Commit`.
    pub pending: Vec<Pending>,
    /// Client node id to scene key.
    pub nodes: HashMap<NodeId, NodeKey>,
    /// Scene key to client node id, for naming a node in an input event.
    pub node_ids: HashMap<NodeKey, NodeId>,
    /// Client buffer id to scene key.
    pub buffers: HashMap<BufferId, BufferKey>,
    /// Windows this client owns, by their root node's client id.
    pub windows: HashMap<NodeId, WindowKey>,
    /// Scene window key back to the client's node id.
    pub window_ids: HashMap<WindowKey, NodeId>,
    /// Windows that asked for a frame callback and have not had one.
    pub frame_requests: Vec<NodeId>,
    /// Commit serials applied but not yet presented.
    pub unpresented: Vec<u32>,
    /// The shaped run each of this client's text nodes currently holds, so
    /// a re-`SetText` can release the old one and a destroyed node can
    /// release its own. The scene stores only the opaque key; this map is
    /// what turns that key back into something the store can drop.
    pub texts: HashMap<NodeKey, TextKey>,
}

impl WireClient {
    /// Wrap an accepted stream.
    #[must_use]
    pub fn new(stream: ClientStream, id: ClientId) -> Self {
        Self {
            stream,
            id,
            pending: Vec::new(),
            nodes: HashMap::new(),
            node_ids: HashMap::new(),
            buffers: HashMap::new(),
            windows: HashMap::new(),
            window_ids: HashMap::new(),
            frame_requests: Vec::new(),
            unpresented: Vec::new(),
            texts: HashMap::new(),
        }
    }

    /// The socket, for epoll.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }

    /// The client's node id for a scene key, if it owns it.
    #[must_use]
    pub fn node_id(&self, key: NodeKey) -> NodeId {
        self.node_ids.get(&key).copied().unwrap_or(NodeId::NONE)
    }

    /// The client's node id for one of its windows.
    #[must_use]
    pub fn window_id(&self, win: WindowKey) -> Option<NodeId> {
        self.window_ids.get(&win).copied()
    }

    /// Whether this client owns `win`.
    #[must_use]
    pub fn owns_window(&self, win: WindowKey) -> bool {
        self.window_ids.contains_key(&win)
    }

    /// Queue a message, ignoring an encode failure (the only way one can
    /// happen is a string longer than the protocol allows, which the server
    /// never produces).
    pub fn send(&mut self, msg: &ServerMsg) {
        if let Err(e) = self.stream.send(msg) {
            warn!("client {}: encoding {}: {e}", self.id.0, msg.name());
        }
    }

    /// Remember a node in both directions.
    fn bind_node(&mut self, id: NodeId, key: NodeKey) {
        self.nodes.insert(id, key);
        self.node_ids.insert(key, id);
    }

    /// Forget a node and, recursively, its scene descendants — the scene
    /// destroyed them, so the maps must not keep naming them. Any shaped
    /// runs they held go back to the text store in the same walk: the keys
    /// are unreachable the moment the nodes are, and nothing else would
    /// ever free them.
    fn unbind_subtree(&mut self, scene: &Scene, text: &mut TextEngine, key: NodeKey) {
        let mut stack = vec![key];
        while let Some(k) = stack.pop() {
            if let Ok(node) = scene.node(k) {
                stack.extend(node.children().iter().copied());
            }
            if let Some(id) = self.node_ids.remove(&k) {
                self.nodes.remove(&id);
            }
            text.release(self.texts.remove(&k));
        }
    }
}

/// Why a transaction was refused. Carries the protocol code the client is
/// told and a human-readable detail for its log and ours.
#[derive(Debug)]
pub struct ApplyError {
    /// Protocol error code.
    pub code: ErrorCode,
    /// What went wrong, for logs.
    pub detail: String,
}

impl ApplyError {
    fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// The scene's errors, mapped onto the protocol's.
fn scene_code(e: SceneError) -> ErrorCode {
    match e {
        SceneError::StaleKey => ErrorCode::UnknownNode,
        SceneError::WrongKind => ErrorCode::WrongKind,
        SceneError::BadParent | SceneError::RootNode | SceneError::BadSibling => {
            ErrorCode::BadParent
        }
        SceneError::BadBuffer => ErrorCode::BadBuffer,
        SceneError::TooDeep => ErrorCode::Limit,
        // `NotOwner` and `UnknownOutput` cannot reach a client: the id maps
        // make a foreign key unnameable, and outputs are the server's. The
        // scene's error enum is `non_exhaustive`, so a future variant lands
        // in the same arm, as a plain protocol error.
        _ => ErrorCode::Protocol,
    }
}

fn scene_err(what: &str, e: SceneError) -> ApplyError {
    ApplyError::new(scene_code(e), format!("{what}: {e}"))
}

/// What applying a transaction asked the server to do beyond mutating the
/// scene.
#[derive(Debug, Default)]
pub struct ApplyOutcome {
    /// Windows created, in creation order: the server places them and
    /// sends the `Configure`.
    pub new_windows: Vec<(NodeId, WindowKey)>,
    /// Windows whose client asked for a frame callback.
    pub frame_requests: Vec<NodeId>,
    /// Windows destroyed, so the server can drop focus and z-order state.
    pub closed_windows: Vec<WindowKey>,
    /// Text nodes (re)shaped by this transaction, with the metrics the
    /// client is told in a `TextMetrics`. A client that asked for text gets
    /// the measured size back at the commit, which is how a toolkit lays a
    /// label out without a separate `MeasureText` round trip.
    pub text_metrics: Vec<(NodeId, msg::TextMetrics)>,
    /// Icon names this transaction asked for and the set does not have.
    ///
    /// Collected rather than returned as an error, because
    /// [`ErrorCode::BadIcon`] is **not fatal**: the node was cleared, the
    /// rest of the batch applies, and the client is told. A desktop must
    /// not lose an application because one of its widgets named an icon a
    /// newer set has — see `docs/icons.md`.
    pub bad_icons: Vec<(NodeId, String)>,
    /// Windows whose client asked for a state change, in arrival order. The
    /// scene does not act on these: which rectangle `Maximized` means is
    /// the window manager's business, and it is the one thing in a
    /// transaction that needs the work area.
    pub state_requests: Vec<(WindowKey, WindowState)>,
    /// Windows whose title changed, so the server can redraw the title bar.
    pub retitled: Vec<WindowKey>,
    /// Windows whose `app_id` changed, so the server can re-resolve the
    /// icon in their title bar.
    ///
    /// Separate from `relisted` for the reason `retitled` is: an app id
    /// change is three different jobs — tell the bar its list entry
    /// moved, re-resolve the frame's icon, and *not* reshape the title —
    /// and a list that conflated them would do the expensive one on every
    /// message that touched a window.
    pub reiconed: Vec<WindowKey>,
    /// Windows something in the shell's `WindowInfo` changed on — a title,
    /// an app id. Separate from `retitled` because the two answer different
    /// questions: `retitled` is "reshape the title bar", and this is "tell
    /// the bar its list entry moved". An app id change is the second
    /// without the first.
    pub relisted: Vec<WindowKey>,
    /// Shell ops naming one of this client's own windows, in arrival order.
    ///
    /// Buffered rather than answered on receipt for one reason the hardware
    /// probe found: a bar sends `CreateWindow` and `SetAnchor` in the same
    /// transaction, and an anchor applied on receipt would be looking for a
    /// window the commit has not created yet. The privilege check is still
    /// on receipt, so an unprivileged client never gets this far.
    pub shell_ops: Vec<(WindowKey, shell::WindowOp)>,
    /// Whether this transaction showed or hid one of the client's windows.
    ///
    /// A window that reserves an exclusive zone stops reserving it when it
    /// stops showing, so the server has to recompute the work area — and it
    /// is the server, not this module, that knows which windows hold zones.
    pub visibility_changed: bool,
}

/// Apply one client's buffered mutations to the scene, atomically as far as
/// the client can tell: on error nothing more is applied and the caller
/// disconnects the client, so a half-applied batch is never observable.
///
/// # Errors
/// The first mutation the scene refuses, or a protocol rule the decoder
/// cannot enforce on its own (an id reused, a node kind reserved for a
/// later milestone, a buffer bigger than the cap).
pub fn apply(
    client: &mut WireClient,
    scene: &mut Scene,
    text: &mut TextEngine,
    icons: &mut IconEngine,
    serial: u32,
) -> Result<ApplyOutcome, ApplyError> {
    let mut outcome = ApplyOutcome::default();
    let pending = std::mem::take(&mut client.pending);
    debug!(
        "client {}: commit {serial} with {} mutation(s)",
        client.id.0,
        pending.len()
    );
    for item in pending {
        match item {
            Pending::Buffer(id, desc, data) => {
                if client.buffers.contains_key(&id) || id.is_none() {
                    return Err(ApplyError::new(
                        ErrorCode::BadBuffer,
                        format!("buffer id {} is zero or already in use", id.raw()),
                    ));
                }
                let key = scene
                    .create_buffer(client.id, desc, data)
                    .map_err(|e| scene_err("CreateBuffer", e))?;
                client.buffers.insert(id, key);
            }
            Pending::Msg(msg) => apply_msg(client, scene, text, icons, *msg, &mut outcome)?,
        }
    }
    Ok(outcome)
}

/// Look up a node the client named, or fail with `UnknownNode`.
fn node_key(client: &WireClient, id: NodeId) -> Result<NodeKey, ApplyError> {
    client.nodes.get(&id).copied().ok_or_else(|| {
        ApplyError::new(
            ErrorCode::UnknownNode,
            format!("no node with id {}", id.raw()),
        )
    })
}

/// Resolve an optional `before` sibling: `NodeId::NONE` means "append".
fn before_key(client: &WireClient, id: NodeId) -> Result<Option<NodeKey>, ApplyError> {
    if id.is_none() {
        return Ok(None);
    }
    node_key(client, id).map(Some)
}

fn buffer_key(client: &WireClient, id: BufferId) -> Result<BufferKey, ApplyError> {
    client.buffers.get(&id).copied().ok_or_else(|| {
        ApplyError::new(
            ErrorCode::BadBuffer,
            format!("no buffer with id {}", id.raw()),
        )
    })
}

#[allow(clippy::too_many_lines)] // One arm per op; splitting it would only hide the table.
fn apply_msg(
    client: &mut WireClient,
    scene: &mut Scene,
    text: &mut TextEngine,
    icons: &mut IconEngine,
    msg: ClientMsg,
    outcome: &mut ApplyOutcome,
) -> Result<(), ApplyError> {
    match msg {
        ClientMsg::Hello(_) | ClientMsg::Commit(_) => {
            // Never buffered: the stream handles `Hello`, and `Commit` is
            // what drives this function.
            Ok(())
        }
        ClientMsg::CreateWindow(m) => {
            if m.id.is_none() || client.nodes.contains_key(&m.id) {
                return Err(ApplyError::new(
                    ErrorCode::Protocol,
                    format!("window id {} is zero or already in use", m.id.raw()),
                ));
            }
            if client.nodes.len() >= MAX_NODES_PER_CLIENT {
                return Err(ApplyError::new(ErrorCode::Limit, "too many nodes"));
            }
            let size = sane_size(m.size)?;
            let win = scene.create_window_with(
                client.id,
                m.title,
                size,
                scene_layer(m.layer),
                scene_flags(m.flags),
            );
            let content = scene
                .window_info(win)
                .map_err(|e| scene_err("CreateWindow", e))?
                .content();
            // The client's id names its *content* group, which survives the
            // server wrapping a frame around it: `frame_window` mints a new
            // root above this node and leaves this key alone, so every
            // `CreateNode { parent: id }` still lands inside the client's
            // own group rather than on top of the decorations.
            client.bind_node(m.id, content);
            client.windows.insert(m.id, win);
            client.window_ids.insert(win, m.id);
            outcome.new_windows.push((m.id, win));
            Ok(())
        }
        ClientMsg::SetWindowTitle(m) => {
            let win = window_of(client, m.window)?;
            scene
                .set_window_title(client.id, win, m.title)
                .map_err(|e| scene_err("SetWindowTitle", e))?;
            outcome.retitled.push(win);
            outcome.relisted.push(win);
            Ok(())
        }
        ClientMsg::SetAppId(m) => {
            let win = window_of(client, m.window)?;
            outcome.relisted.push(win);
            // The app id *is* the icon name (`docs/shell.md`), so a
            // window that renames itself after mapping gets a new frame
            // icon. `nitro-term` does exactly this when it learns what it
            // is running, and a frame that resolved once at map time
            // would keep the generic glyph for the rest of the session.
            outcome.reiconed.push(win);
            scene
                .set_app_id(client.id, win, m.app_id)
                .map_err(|e| scene_err("SetAppId", e))
        }
        ClientMsg::SetWindowLimits(m) => {
            let win = window_of(client, m.window)?;
            let (min, max) = (sane_size(m.min)?, sane_size(m.max)?);
            scene
                .set_window_limits(client.id, win, min, max)
                .map_err(|e| scene_err("SetWindowLimits", e))
        }
        ClientMsg::SetWindowState(m) => {
            let win = window_of(client, m.window)?;
            outcome.state_requests.push((win, scene_state(m.state)));
            Ok(())
        }
        ClientMsg::RequestFrame(m) => {
            window_of(client, m.window)?;
            outcome.frame_requests.push(m.window);
            Ok(())
        }
        ClientMsg::CreateNode(m) => {
            if m.id.is_none() || client.nodes.contains_key(&m.id) {
                return Err(ApplyError::new(
                    ErrorCode::Protocol,
                    format!("node id {} is zero or already in use", m.id.raw()),
                ));
            }
            if client.nodes.len() >= MAX_NODES_PER_CLIENT {
                return Err(ApplyError::new(ErrorCode::Limit, "too many nodes"));
            }
            let kind = scene_kind(m.kind)?;
            let parent = node_key(client, m.parent)?;
            let before = before_key(client, m.before)?;
            let key = scene
                .create_node(client.id, kind, parent, before)
                .map_err(|e| scene_err("CreateNode", e))?;
            client.bind_node(m.id, key);
            Ok(())
        }
        ClientMsg::DestroyNode(m) => {
            let key = node_key(client, m.id)?;
            if let Some(win) = client.windows.get(&m.id).copied() {
                // Destroying a window's root closes the window.
                scene
                    .destroy_window(client.id, win)
                    .map_err(|e| scene_err("DestroyNode", e))?;
                client.unbind_subtree(scene, text, key);
                client.windows.remove(&m.id);
                client.window_ids.remove(&win);
                client.frame_requests.retain(|w| *w != m.id);
                outcome.closed_windows.push(win);
                return Ok(());
            }
            client.unbind_subtree(scene, text, key);
            scene
                .destroy_node(client.id, key)
                .map_err(|e| scene_err("DestroyNode", e))
        }
        ClientMsg::Reparent(m) => {
            let key = node_key(client, m.id)?;
            let parent = node_key(client, m.parent)?;
            let before = before_key(client, m.before)?;
            scene
                .reparent(client.id, key, parent, before)
                .map_err(|e| scene_err("Reparent", e))
        }
        ClientMsg::SetBounds(m) => {
            let key = node_key(client, m.id)?;
            scene
                .set_bounds(client.id, key, sane_rect(m.rect)?)
                .map_err(|e| scene_err("SetBounds", e))
        }
        ClientMsg::SetTransform(m) => {
            let key = node_key(client, m.id)?;
            scene
                .set_transform(client.id, key, m.transform)
                .map_err(|e| scene_err("SetTransform", e))
        }
        ClientMsg::SetVisible(m) => {
            let key = node_key(client, m.id)?;
            // Hiding a window that reserves screen space changes the work
            // area, so the server has to reflow. Flagged rather than acted
            // on, because only the server knows whether this window holds a
            // zone at all; `window_of` would reject a non-window node, so the
            // lookup is the tolerant one.
            if client.windows.contains_key(&m.id) {
                outcome.visibility_changed = true;
            }
            scene
                .set_visible(client.id, key, m.visible)
                .map_err(|e| scene_err("SetVisible", e))
        }
        ClientMsg::SetOpacity(m) => {
            let key = node_key(client, m.id)?;
            scene
                .set_opacity(client.id, key, m.opacity)
                .map_err(|e| scene_err("SetOpacity", e))
        }
        ClientMsg::SetClip(m) => {
            let key = node_key(client, m.id)?;
            scene
                .set_clip(client.id, key, m.clip)
                .map_err(|e| scene_err("SetClip", e))
        }
        ClientMsg::SetFill(m) => {
            let key = node_key(client, m.id)?;
            scene
                .set_fill(client.id, key, scene_fill(m.fill))
                .map_err(|e| scene_err("SetFill", e))
        }
        ClientMsg::SetCorners(m) => {
            let key = node_key(client, m.id)?;
            scene
                .set_corner_radius(client.id, key, m.radius)
                .map_err(|e| scene_err("SetCorners", e))
        }
        ClientMsg::SetBorder(m) => {
            let key = node_key(client, m.id)?;
            let border = (m.width > 0.0).then(|| Border::new(m.width, m.color));
            scene
                .set_border(client.id, key, border)
                .map_err(|e| scene_err("SetBorder", e))
        }
        ClientMsg::SetText(m) => {
            let key = node_key(client, m.node)?;
            let request = StyleRequest::new(
                &m.family,
                m.size_px,
                m.weight,
                m.italic,
                m.max_width,
                m.wrap,
            );
            let (text_key, shaped) = text.shape(client.id.0, &request, &m.text);
            let reference = TextRef {
                key: text_key.0,
                size: Size::new(shaped.width, shaped.height),
                ascent: shaped.ascent,
                color: m.color,
                align: scene_align(m.align),
            };
            let metrics = msg::TextMetrics {
                node: m.node,
                width: shaped.width,
                height: shaped.height,
                ascent: shaped.ascent,
                descent: shaped.descent,
                line_count: shaped.lines.len() as u32,
            };
            // Attach first: a `WrongKind` here must not leave the freshly
            // shaped run orphaned in the store.
            match scene.set_text(client.id, key, Some(reference)) {
                Ok(()) => {}
                Err(e) => {
                    text.release(Some(text_key));
                    return Err(scene_err("SetText", e));
                }
            }
            // The node's previous run is unreachable now.
            text.release(client.texts.insert(key, text_key));
            outcome.text_metrics.push((m.node, metrics));
            Ok(())
        }
        ClientMsg::MeasureText(_) => {
            // Answered on receipt, never buffered: a measurement a client
            // has to commit for is a measurement it cannot lay out with.
            Ok(())
        }
        ClientMsg::SetIcon(m) => {
            let key = node_key(client, m.node)?;
            // An empty name clears the node, and clearing is never an
            // error: it is how a widget that stopped showing an icon says
            // so without destroying and recreating a node.
            let reference = if m.name.is_empty() {
                None
            } else if let Some((index, role)) = icon_handle(icons, m.role, &m.name) {
                Some(IconRef::new(index, sane_icon_size(m.size), role))
            } else {
                // Unknown name: the node is cleared, the client is told,
                // and the connection lives. Recorded here and reported
                // after the batch, so the client sees the whole
                // transaction applied before the complaint about one
                // node of it.
                outcome.bad_icons.push((m.node, m.name));
                None
            };
            scene
                .set_icon(client.id, key, reference)
                .map_err(|e| scene_err("SetIcon", e))
        }
        ClientMsg::CreateBuffer(_) => {
            // Turned into `Pending::Buffer` when it arrived: the fd is
            // checked and mapped on receipt, not at the commit.
            Ok(())
        }
        ClientMsg::DestroyBuffer(m) => {
            let key = buffer_key(client, m.id)?;
            client.buffers.remove(&m.id);
            // The scene drops the `Buffer`, and with it the mapping: the
            // `munmap` is the store's `Drop`, so there is no descriptor for
            // the server to remember to release.
            scene
                .destroy_buffer(client.id, key)
                .map_err(|e| scene_err("DestroyBuffer", e))
        }
        ClientMsg::BufferDamage(m) => {
            let key = buffer_key(client, m.id)?;
            // The pixels are already there: the scene's buffer is a live
            // mapping of the client's memfd, so the only work is marking
            // the image nodes that sample the damaged rows for repaint.
            scene
                .buffer_damaged(client.id, key, &m.rects)
                .map_err(|e| scene_err("BufferDamage", e))
        }
        ClientMsg::SetImage(m) => {
            let key = node_key(client, m.id)?;
            let image = if m.buffer.is_none() {
                None
            } else {
                Some(ImageRef::new(buffer_key(client, m.buffer)?, m.src))
            };
            scene
                .set_image(client.id, key, image)
                .map_err(|e| scene_err("SetImage", e))
        }
        // The shell ops that name one of the *sender's own* windows are
        // buffered like every other mutation, so a bar can create a window
        // and anchor it in one transaction. The privilege check happened on
        // receipt (`Server::handle_wire_msg`), so reaching here means the
        // client is allowed to send these.
        ClientMsg::SetLayer(m) => {
            let win = window_of(client, m.window)?;
            if m.layer == Layer::Normal {
                // Not a no-op: a shell surface asking to be an ordinary
                // window has misunderstood the op, and obliging silently
                // would put a bar into the window-management z-order where a
                // click could raise a document over it.
                return Err(ApplyError::new(
                    ErrorCode::Protocol,
                    "SetLayer: Normal is not a shell layer",
                ));
            }
            outcome
                .shell_ops
                .push((win, shell::WindowOp::Layer(scene_layer(m.layer))));
            // The layer is part of `WindowInfo`, and a task list filters
            // on it: a window that becomes a shell surface has to leave
            // the bar's list, so the watchers are told.
            outcome.relisted.push(win);
            Ok(())
        }
        ClientMsg::SetExclusiveZone(m) => {
            let win = window_of(client, m.window)?;
            outcome.shell_ops.push((
                win,
                shell::WindowOp::Zone {
                    edge: m.edge,
                    px: m.px,
                },
            ));
            Ok(())
        }
        ClientMsg::SetAnchor(m) => {
            let win = window_of(client, m.window)?;
            if m.edges & anchor::ALL != m.edges {
                return Err(ApplyError::new(
                    ErrorCode::Protocol,
                    format!("SetAnchor: reserved edge bits in {:#x}", m.edges),
                ));
            }
            outcome.shell_ops.push((
                win,
                shell::WindowOp::Anchor {
                    edges: m.edges,
                    margin: m.margin,
                },
            ));
            Ok(())
        }
        ClientMsg::GrabKeyboard(m) => {
            let win = window_of(client, m.window)?;
            outcome.shell_ops.push((win, shell::WindowOp::Grab(m.on)));
            Ok(())
        }
        // The rest of the shell ops are answered on receipt: they are not
        // scene mutations a frame has to show atomically. `WindowList` is a
        // question, `BindKey` a registration, and the three `WindowRef` ops
        // act on *another* client's window, which this client's commit has
        // nothing to do with. See `Server::handle_shell_msg`.
        ClientMsg::BindKey(_)
        | ClientMsg::UnbindKey(_)
        | ClientMsg::WindowList(_)
        | ClientMsg::FocusWindow(_)
        | ClientMsg::CloseWindow(_)
        | ClientMsg::SetWindowStateFor(_)
        | ClientMsg::Outputs(_) => Ok(()),
    }
}

fn window_of(client: &WireClient, id: NodeId) -> Result<WindowKey, ApplyError> {
    client.windows.get(&id).copied().ok_or_else(|| {
        ApplyError::new(
            ErrorCode::UnknownNode,
            format!("no window with id {}", id.raw()),
        )
    })
}

/// Reject sizes the scene would happily take but nothing could ever draw.
/// A NaN would poison every transform it touched, so it is a protocol error
/// rather than a clamp.
fn sane_size(size: Size) -> Result<Size, ApplyError> {
    if !size.w.is_finite() || !size.h.is_finite() || size.w < 0.0 || size.h < 0.0 {
        return Err(ApplyError::new(
            ErrorCode::Protocol,
            format!("size {}x{} is not a drawable size", size.w, size.h),
        ));
    }
    Ok(size)
}

fn sane_rect(rect: Rect) -> Result<Rect, ApplyError> {
    if !rect.x.is_finite() || !rect.y.is_finite() {
        return Err(ApplyError::new(
            ErrorCode::Protocol,
            "rect origin is not finite",
        ));
    }
    sane_size(rect.size())?;
    Ok(rect)
}

/// An icon's requested box size, clamped rather than refused.
///
/// Unlike a rect's geometry this is not a protocol error: a size is a
/// *hint* about how big to rasterise, the server clamps it to what it will
/// actually draw ([`crate::icons::MIN_PX`]–[`crate::icons::MAX_PX`] at
/// scale 1), and a client that asked for a NaN gets the default rather
/// than a dead connection — an icon must never be able to kill a client.
fn sane_icon_size(size: f32) -> f32 {
    if !size.is_finite() || size <= 0.0 {
        return DEFAULT_ICON_PX;
    }
    size.clamp(crate::icons::MIN_PX as f32, crate::icons::MAX_PX as f32)
}

/// The handle a `SetIcon` names, from whichever of the server's two icon
/// sets its `role` selects, **and the role byte the node must carry**.
///
/// The role byte is the **selector**, not a search order: a palette role
/// means the symbolic set compiled into the server, and
/// [`IconRef::AS_COLOURED`] means the machine's XDG icon theme. Nothing
/// falls back from one to the other, and that is the point — a single
/// namespace searched "ours first" would make `icon("list")` mean the
/// desktop's own list glyph on one box and a theme's list icon on
/// another, invisibly. Here the call site says which it wants, so there
/// is no collision to resolve and no shadowing to document. `docs/icons.md`
/// has the argument in full.
///
/// A tinted *theme* icon — a role byte with a name only the theme has — is
/// therefore refused with `BadIcon` rather than quietly finding the file
/// and tinting its alpha. A theme icon is a picture, not a coverage mask:
/// tinting one throws away the artwork and keeps its silhouette, which
/// looks like a bug on every icon that is not already monochrome.
///
/// # Why the answer includes a role
///
/// Since #3715 an `AS_COLOURED` name may resolve, through one
/// `<name>.desktop` hop, to one of the *symbolic* shapes — which is how
/// `nitro-calc` becomes `calculator`. A symbolic shape is an A8 coverage
/// mask with no colours of its own, so the node cannot keep
/// `AS_COLOURED`: it would resolve the handle against the application
/// cache, find nothing, and draw an empty box. The engine knows which set
/// answered, so it says, and the node stores [`Role::Text`] instead.
///
/// That is the same mixed-tint rule the toolkit's `icon_fallback_tinted`
/// already applies client-side — a coloured icon's fallback is symbolic
/// and tinted — moved server-side for the case the client cannot see: the
/// bar asks for an app id, and the indirection is the server's index.
fn icon_handle(icons: &mut IconEngine, role: u8, name: &str) -> Option<(u32, u8)> {
    if role == IconRef::AS_COLOURED {
        let icon = icons.lookup_app(name)?;
        Some((icon.handle(), icon.role(Role::Text)))
    } else {
        Some((icons.lookup(name)?, role))
    }
}

/// The box an icon gets when the client asked for a nonsense size.
const DEFAULT_ICON_PX: f32 = 16.0;

/// The scene's node kind for a wire kind. `Surface` is reserved: the server
/// advertises no `DMABUF` capability, so a client asking for one is using a
/// feature it was told does not exist. `Text` is live from M2 and is **always
/// accepted** — the `TEXT` capability bit reports whether the text will be
/// *visible* (i.e. whether the server found a font to draw with), not whether
/// the node may be created. A server without it shapes to an empty run rather
/// than refusing, which is why the match below takes `Text` unconditionally.
fn scene_kind(kind: NodeKind) -> Result<SceneNodeKind, ApplyError> {
    match kind {
        NodeKind::Group => Ok(SceneNodeKind::Group),
        NodeKind::Rect => Ok(SceneNodeKind::Rect),
        NodeKind::Image => Ok(SceneNodeKind::Image),
        NodeKind::Text => Ok(SceneNodeKind::Text),
        NodeKind::Icon => Ok(SceneNodeKind::Icon),
        NodeKind::Surface => Err(ApplyError::new(
            ErrorCode::WrongKind,
            "Surface nodes are M5; the server does not advertise the DMABUF capability",
        )),
    }
}

/// The scene's alignment for a wire one.
fn scene_align(align: Align) -> TextAlign {
    match align {
        Align::Left => TextAlign::Left,
        Align::Center => TextAlign::Center,
        Align::Right => TextAlign::Right,
    }
}

fn scene_fill(fill: msg::Fill) -> SceneFill {
    match fill {
        msg::Fill::None => SceneFill::None,
        msg::Fill::Solid(c) => SceneFill::Solid(c),
        msg::Fill::Linear { start, end, c0, c1 } => SceneFill::Linear { start, end, c0, c1 },
    }
}

/// A client's pixels as the scene sees them: a read-only mapping of its
/// sealed memfd.
///
/// A newtype because both [`Mapping`] and [`PixelStore`] are foreign to
/// this crate. The scene never learns what a mapping is; `nitro-shm` never
/// learns what a scene is.
#[derive(Debug)]
pub struct MappedPixels(pub Mapping);

impl PixelStore for MappedPixels {
    fn bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// `None`, always: the client writes these pages, the server only
    /// reads them, and the mapping is `PROT_READ` besides.
    fn bytes_mut(&mut self) -> Option<&mut [u8]> {
        None
    }
}

/// Validate a `CreateBuffer` and map its memfd.
///
/// The bytes are **mapped**, not copied, and the precondition for that is
/// the seal check: a client can shrink an unsealed memfd under a live
/// mapping and turn the server's next read into a `SIGBUS`, so
/// [`Mapping::map`] asks the kernel (`F_GET_SEALS`) whether
/// `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL` are in force on *this* fd
/// and refuses otherwise. There is deliberately no `pread` fallback for an
/// unsealed buffer: a fallback would make the mapped path's safety
/// argument untestable from the outside (which path did the server take?)
/// and leave the copying path rotting unexercised. The proofs are in
/// `nitro-shm/src/map.rs`; `docs/wire.md` states the requirement to
/// clients.
///
/// Exactly `byte_len` (`stride * height`) is mapped, not the declared
/// `size`: a client cannot make the server reserve address space beyond
/// the [`MAX_BUFFER_BYTES`] cap by declaring a large `size`. The `size` is
/// still checked against the file, because `docs/wire.md` says an
/// inconsistent one is a `BadBuffer`.
///
/// # Errors
/// A description that does not add up, a size past [`MAX_BUFFER_BYTES`], a
/// descriptor without the required seals (the detail names which are
/// missing), a file shorter than the declared size, or a descriptor that
/// will not map.
pub fn map_buffer(m: msg::CreateBuffer) -> Result<(BufferDesc, MappedPixels), ApplyError> {
    let desc = validate_buffer(&m)?;
    let bad = |detail: String| ApplyError::new(ErrorCode::BadBuffer, detail);
    let file_len = nitro_shm::sealed_len(&m.fd).map_err(|e| match e {
        MapError::Seals(s) => bad(format!("buffer {s}")),
        other => bad(other.to_string()),
    })?;
    if file_len < u64::from(m.size) {
        return Err(bad(format!(
            "buffer fd is {file_len} bytes, shorter than the declared size {}",
            m.size
        )));
    }
    let mapping = Mapping::map(m.fd, desc.byte_len()).map_err(|e| match e {
        MapError::Seals(s) => bad(format!("buffer {s}")),
        MapError::TooShort { file, need } => bad(format!(
            "buffer fd is {file} bytes, shorter than the {need} the geometry needs"
        )),
        MapError::Os(errno) => bad(format!("mapping the buffer fd: {errno}")),
    })?;
    Ok((desc, MappedPixels(mapping)))
}

/// Check a `CreateBuffer`'s geometry against the format and the caps.
///
/// # Errors
/// [`ErrorCode::BadBuffer`] for a format the server does not know, a stride
/// too small for the width, or a declared size that does not cover the
/// rows; [`ErrorCode::Limit`] past [`MAX_BUFFER_BYTES`].
pub fn validate_buffer(m: &msg::CreateBuffer) -> Result<BufferDesc, ApplyError> {
    if m.format != format::XR24 && m.format != format::AR24 {
        return Err(ApplyError::new(
            ErrorCode::BadBuffer,
            format!("unsupported pixel format {:#010x}", m.format),
        ));
    }
    if m.width == 0 || m.height == 0 {
        return Err(ApplyError::new(
            ErrorCode::BadBuffer,
            "buffer has no pixels",
        ));
    }
    let min_stride = u64::from(m.width) * u64::from(BYTES_PER_PIXEL);
    if u64::from(m.stride) < min_stride {
        return Err(ApplyError::new(
            ErrorCode::BadBuffer,
            format!("stride {} is too small for width {}", m.stride, m.width),
        ));
    }
    let need = u64::from(m.stride) * u64::from(m.height);
    if need > MAX_BUFFER_BYTES {
        return Err(ApplyError::new(
            ErrorCode::Limit,
            format!("buffer of {need} bytes exceeds the {MAX_BUFFER_BYTES} byte cap"),
        ));
    }
    if u64::from(m.size) < need {
        return Err(ApplyError::new(
            ErrorCode::BadBuffer,
            format!("declared size {} does not cover {need} bytes", m.size),
        ));
    }
    // `XR24` has no alpha channel, so every pixel of such a buffer is fully
    // opaque and an image on it can occlude what is behind it. The flag is
    // the scene's only knowledge of what a fourcc means, and it must agree
    // with `frame::pixel_format`'s opaque formats exactly: if the scene says
    // "covered" and the painter then declines to draw, the frame has a hole.
    Ok(
        BufferDesc::new(m.width, m.height, m.stride, m.format)
            .with_opaque(m.format == format::XR24),
    )
}

/// The protocol code for a wire-level failure, re-exported so the event
/// loop does not have to reach into `nitro_wire::server`.
#[must_use]
pub fn wire_code(err: &WireError) -> ErrorCode {
    code_for(err)
}

/// The scene's window state for a wire one, and back. Two enums with the
/// same shape, deliberately: the scene must not depend on the protocol.
#[must_use]
pub fn scene_state(state: WireWindowState) -> WindowState {
    match state {
        WireWindowState::Normal => WindowState::Normal,
        WireWindowState::Maximized => WindowState::Maximized,
        WireWindowState::Fullscreen => WindowState::Fullscreen,
        WireWindowState::Minimized => WindowState::Minimized,
    }
}

/// The wire's window state for a scene one.
#[must_use]
pub fn wire_state(state: WindowState) -> WireWindowState {
    match state {
        WindowState::Normal => WireWindowState::Normal,
        WindowState::Maximized => WireWindowState::Maximized,
        WindowState::Fullscreen => WireWindowState::Fullscreen,
        WindowState::Minimized => WireWindowState::Minimized,
    }
}

/// The scene's window flags for a `CreateWindow`'s bits.
#[must_use]
pub fn scene_flags(flags: u32) -> WindowFlags {
    WindowFlags {
        decorated: flags & window_flags::UNDECORATED == 0,
        fixed_size: flags & window_flags::FIXED_SIZE != 0,
        focusable: flags & window_flags::NO_FOCUS == 0,
    }
}

/// The layer a window asked for, as the scene sees it. The wire and scene
/// enums are deliberately separate types with the same shape — the scene
/// must not depend on the protocol — so this is the one place they meet.
#[must_use]
pub fn scene_layer(layer: Layer) -> nitro_scene::Layer {
    match layer {
        Layer::Background => nitro_scene::Layer::Background,
        Layer::Normal => nitro_scene::Layer::Normal,
        Layer::Top => nitro_scene::Layer::Top,
        Layer::Overlay => nitro_scene::Layer::Overlay,
    }
}

/// The wire's layer for a scene one: [`scene_layer`] the other way round,
/// for the `WindowInfo` a shell reads.
#[must_use]
pub fn wire_layer(layer: nitro_scene::Layer) -> Layer {
    match layer {
        nitro_scene::Layer::Background => Layer::Background,
        nitro_scene::Layer::Normal => Layer::Normal,
        nitro_scene::Layer::Top => Layer::Top,
        nitro_scene::Layer::Overlay => Layer::Overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_core::Color;
    use std::os::fd::OwnedFd;

    fn create_buffer(width: u32, height: u32, stride: u32, size: u32, format: u32) -> OwnedFd {
        let fd =
            nitro_shm::create_sealed("nitro-test", u64::from(stride) * u64::from(height)).unwrap();
        let _ = (width, size, format);
        fd
    }

    /// A memfd of `len` bytes with exactly `seals` on it — for the
    /// per-seal negatives, which `create_sealed` cannot produce.
    fn memfd_with_seals(len: u64, seals: rustix::fs::SealFlags) -> OwnedFd {
        use rustix::fs::{MemfdFlags, fcntl_add_seals, ftruncate, memfd_create};
        let fd = memfd_create(
            "nitro-test",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .unwrap();
        ftruncate(&fd, len).unwrap();
        if !seals.is_empty() {
            fcntl_add_seals(&fd, seals).unwrap();
        }
        fd
    }

    fn msg_buffer(
        width: u32,
        height: u32,
        stride: u32,
        size: u32,
        format: u32,
    ) -> msg::CreateBuffer {
        msg::CreateBuffer {
            id: BufferId(1),
            width,
            height,
            stride,
            format,
            size,
            fd: create_buffer(width, height, stride, size, format),
        }
    }

    #[test]
    fn buffer_validation_checks_format_stride_and_caps() {
        let ok = msg_buffer(16, 8, 64, 512, format::XR24);
        assert_eq!(
            validate_buffer(&ok).unwrap(),
            BufferDesc::new(16, 8, 64, format::XR24).with_opaque(true)
        );
        // `AR24` carries alpha, so it must not be declared opaque: the flag
        // is what lets the compositor skip everything behind an image.
        let alpha = msg_buffer(16, 8, 64, 512, format::AR24);
        assert!(!validate_buffer(&alpha).unwrap().is_opaque());

        let bad_format = msg_buffer(16, 8, 64, 512, 0x1234_5678);
        assert_eq!(
            validate_buffer(&bad_format).unwrap_err().code,
            ErrorCode::BadBuffer
        );

        let thin = msg_buffer(16, 8, 32, 512, format::AR24);
        assert_eq!(
            validate_buffer(&thin).unwrap_err().code,
            ErrorCode::BadBuffer
        );

        let empty = msg_buffer(0, 8, 64, 512, format::XR24);
        assert_eq!(
            validate_buffer(&empty).unwrap_err().code,
            ErrorCode::BadBuffer
        );

        let mut huge = msg_buffer(16, 8, 64, 512, format::XR24);
        huge.width = 65_535;
        huge.height = 65_535;
        huge.stride = 65_535 * 4;
        huge.size = u32::MAX;
        assert_eq!(validate_buffer(&huge).unwrap_err().code, ErrorCode::Limit);

        let mut short = msg_buffer(16, 8, 64, 512, format::XR24);
        short.size = 100;
        assert_eq!(
            validate_buffer(&short).unwrap_err().code,
            ErrorCode::BadBuffer
        );
    }

    /// The mapping is the memfd: a write through the client's descriptor
    /// after the map is visible with no re-read at all, which is what
    /// makes `BufferDamage` a pure "repaint these nodes" message.
    #[test]
    fn mapping_a_buffer_sees_the_memfd_live() {
        use std::io::{Seek as _, Write as _};
        let m = msg_buffer(4, 4, 16, 64, format::XR24);
        let client_fd = m.fd.try_clone().unwrap();
        {
            let mut file = std::fs::File::from(client_fd.try_clone().unwrap());
            file.write_all(&[0xAB; 64]).unwrap();
        }
        let (desc, pixels) = map_buffer(m).unwrap();
        assert_eq!(
            desc,
            BufferDesc::new(4, 4, 16, format::XR24).with_opaque(true)
        );
        assert_eq!(pixels.bytes(), &[0xABu8; 64]);

        // Rewrite one row through the client's fd; the mapping follows.
        {
            let mut file = std::fs::File::from(client_fd);
            file.seek(std::io::SeekFrom::Start(16)).unwrap();
            file.write_all(&[0x11; 16]).unwrap();
        }
        assert_eq!(&pixels.bytes()[0..16], &[0xABu8; 16]);
        assert_eq!(&pixels.bytes()[16..32], &[0x11u8; 16]);
        assert_eq!(&pixels.bytes()[32..64], &[0xABu8; 32]);
    }

    /// The store is read-only from the scene's side: the client writes
    /// the pages, the server does not.
    #[test]
    fn a_mapped_buffer_is_read_only_to_the_scene() {
        let m = msg_buffer(4, 4, 16, 64, format::XR24);
        let (desc, pixels) = map_buffer(m).unwrap();
        let mut scene = Scene::new();
        let key = scene.create_buffer(ClientId(7), desc, pixels).unwrap();
        assert_eq!(
            scene.buffer_mut(ClientId(7), key).unwrap_err(),
            SceneError::ReadOnly
        );
        assert_eq!(scene.buffer(key).unwrap().data().len(), 64);
    }

    #[test]
    fn a_short_fd_is_a_bad_buffer_not_zero_padding() {
        let mut m = msg_buffer(4, 4, 16, 64, format::XR24);
        // Declare twice the rows the fd actually has.
        m.height = 8;
        m.size = 128;
        let err = map_buffer(m).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadBuffer);
        assert!(err.detail.contains("shorter"), "{}", err.detail);
    }

    /// A declared `size` the file does not cover is a `BadBuffer` even when
    /// the geometry fits — `docs/wire.md`'s "inconsistent size" rule.
    #[test]
    fn a_declared_size_past_the_file_is_a_bad_buffer() {
        let mut m = msg_buffer(4, 4, 16, 64, format::XR24);
        m.size = 65;
        let err = map_buffer(m).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadBuffer);
        assert!(err.detail.contains("declared size"), "{}", err.detail);
    }

    /// An unsealed buffer is refused — no `pread` fallback — and the detail
    /// names what is missing. A plain `memfd_create` without
    /// `MFD_ALLOW_SEALING` is what every pre-#569 client sent.
    #[test]
    fn an_unsealed_buffer_is_refused() {
        use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
        let mut m = msg_buffer(4, 4, 16, 64, format::XR24);
        let fd = memfd_create("nitro-test-unsealed", MemfdFlags::CLOEXEC).unwrap();
        ftruncate(&fd, 64).unwrap();
        m.fd = fd;
        let err = map_buffer(m).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadBuffer);
        assert_eq!(err.detail, "buffer fd lacks F_SEAL_SHRINK, F_SEAL_GROW");
    }

    /// Each required seal on its own: missing `SHRINK`, missing `GROW`,
    /// missing `SEAL`. All three are `BadBuffer`, each naming its bit.
    #[test]
    fn each_missing_seal_is_refused_by_name() {
        use rustix::fs::SealFlags;
        for (seals, missing) in [
            (SealFlags::GROW | SealFlags::SEAL, "F_SEAL_SHRINK"),
            (SealFlags::SHRINK | SealFlags::SEAL, "F_SEAL_GROW"),
            (SealFlags::SHRINK | SealFlags::GROW, "F_SEAL_SEAL"),
        ] {
            let mut m = msg_buffer(4, 4, 16, 64, format::XR24);
            m.fd = memfd_with_seals(64, seals);
            let err = map_buffer(m).unwrap_err();
            assert_eq!(err.code, ErrorCode::BadBuffer, "{missing}");
            assert_eq!(err.detail, format!("buffer fd lacks {missing}"));
        }
    }

    /// Something that is not a memfd at all cannot be sealed, and is
    /// refused for that reason rather than mapped on trust.
    #[test]
    fn a_regular_file_is_refused() {
        let mut m = msg_buffer(4, 4, 16, 64, format::XR24);
        let path = std::env::temp_dir().join(format!("nitro-clients-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(64).unwrap();
        m.fd = file.into();
        let err = map_buffer(m).unwrap_err();
        let _ = std::fs::remove_file(path);
        assert_eq!(err.code, ErrorCode::BadBuffer);
        assert!(
            err.detail.contains("does not support sealing"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn text_is_live_and_surface_is_still_reserved() {
        assert_eq!(scene_kind(NodeKind::Group).unwrap(), SceneNodeKind::Group);
        assert_eq!(scene_kind(NodeKind::Rect).unwrap(), SceneNodeKind::Rect);
        assert_eq!(scene_kind(NodeKind::Image).unwrap(), SceneNodeKind::Image);
        assert_eq!(scene_kind(NodeKind::Text).unwrap(), SceneNodeKind::Text);
        assert_eq!(
            scene_kind(NodeKind::Surface).unwrap_err().code,
            ErrorCode::WrongKind
        );
    }

    #[test]
    fn alignments_cross_the_boundary_unchanged() {
        assert_eq!(scene_align(Align::Left), TextAlign::Left);
        assert_eq!(scene_align(Align::Center), TextAlign::Center);
        assert_eq!(scene_align(Align::Right), TextAlign::Right);
    }

    #[test]
    fn sizes_must_be_finite_and_non_negative() {
        assert!(sane_size(Size::new(10.0, 10.0)).is_ok());
        assert!(sane_size(Size::new(f32::NAN, 1.0)).is_err());
        assert!(sane_size(Size::new(1.0, f32::INFINITY)).is_err());
        assert!(sane_size(Size::new(-1.0, 1.0)).is_err());
        assert!(sane_rect(Rect::new(f32::NAN, 0.0, 1.0, 1.0)).is_err());
        assert!(sane_rect(Rect::new(0.0, 0.0, 1.0, 1.0)).is_ok());
    }

    #[test]
    fn fills_and_layers_cross_the_boundary_unchanged() {
        assert_eq!(scene_fill(msg::Fill::None), SceneFill::None);
        assert_eq!(
            scene_fill(msg::Fill::Solid(Color::WHITE)),
            SceneFill::Solid(Color::WHITE)
        );
        assert_eq!(scene_layer(Layer::Top), nitro_scene::Layer::Top);
        assert_eq!(
            scene_layer(Layer::Background),
            nitro_scene::Layer::Background
        );
    }

    #[test]
    fn scene_errors_map_onto_protocol_codes() {
        assert_eq!(scene_code(SceneError::StaleKey), ErrorCode::UnknownNode);
        assert_eq!(scene_code(SceneError::WrongKind), ErrorCode::WrongKind);
        assert_eq!(scene_code(SceneError::BadParent), ErrorCode::BadParent);
        assert_eq!(scene_code(SceneError::TooDeep), ErrorCode::Limit);
        assert_eq!(scene_code(SceneError::BadBuffer), ErrorCode::BadBuffer);
    }
}

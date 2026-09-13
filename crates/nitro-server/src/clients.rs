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
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use nitro_core::{IRect, Point, Rect, Size};
use nitro_scene::{
    Border, BufferDesc, BufferKey, ClientId, Error as SceneError, Fill as SceneFill, ImageRef,
    NodeKey, NodeKind as SceneNodeKind, Scene, WindowKey,
};
use nitro_wire::error::Error as WireError;
use nitro_wire::msg::{self, ClientMsg, ServerMsg};
use nitro_wire::server::{ClientStream, code_for};
use nitro_wire::types::{BufferId, ErrorCode, Layer, NodeId, NodeKind, format};

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

/// How a window is placed when it is created: each window lands
/// [`CASCADE_STEP`] pixels right and down from the previous one, wrapping
/// once it would leave the output.
pub const CASCADE_STEP: f32 = 32.0;

/// How far into the output the cascade may walk before wrapping.
pub const CASCADE_LIMIT: f32 = 320.0;

/// The next cascade position for a window of `size` on an output of
/// `output` logical units, given how many windows were placed before it.
///
/// Deliberately arithmetic rather than stateful: the placement of window
/// `n` depends only on `n`, so it is predictable in a test and identical
/// after a restart.
#[must_use]
pub fn cascade_position(index: u32, size: Size, output: Size) -> Point {
    let steps = f32::from(u16::try_from(index % 16).unwrap_or(0));
    let offset = (steps * CASCADE_STEP) % CASCADE_LIMIT.max(CASCADE_STEP);
    // Never push a window so far that its top-left corner leaves the
    // output: a window the user cannot reach is worse than an overlap.
    let max_x = (output.w - size.w).max(0.0);
    let max_y = (output.h - size.h).max(0.0);
    Point::new(offset.min(max_x), offset.min(max_y))
}

/// A buffered mutation, kept until the client's `Commit`.
#[derive(Debug)]
pub enum Pending {
    /// A message with no side effects until it is applied.
    Msg(Box<ClientMsg>),
    /// `CreateBuffer` with its pixels already read out of the memfd: the
    /// descriptor must be consumed when the message arrives, not when the
    /// transaction commits, or a client could rewrite the buffer in
    /// between and tear its own frame.
    Buffer(BufferId, BufferDesc, Vec<u8>),
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
    /// destroyed them, so the maps must not keep naming them.
    fn unbind_subtree(&mut self, scene: &Scene, key: NodeKey) {
        let mut stack = vec![key];
        while let Some(k) = stack.pop() {
            if let Ok(node) = scene.node(k) {
                stack.extend(node.children().iter().copied());
            }
            if let Some(id) = self.node_ids.remove(&k) {
                self.nodes.remove(&id);
            }
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
    /// Buffers whose rows the server must re-read from the client's memfd,
    /// with the rectangles the client declared damaged. The scene only
    /// learns *which nodes* to repaint; the pixels are the server's job,
    /// because only it holds the descriptor.
    pub buffer_damage: Vec<(BufferKey, Vec<IRect>)>,
    /// Buffers released, so the server can drop the descriptor it kept for
    /// re-reading them. The scene owns the pixels and forgets them on its
    /// own; the *fd* is the server's, and nothing else would ever close it
    /// before the client disconnects.
    pub destroyed_buffers: Vec<BufferKey>,
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
            Pending::Msg(msg) => apply_msg(client, scene, *msg, &mut outcome)?,
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
            let win = scene.create_window(client.id, m.title, size, scene_layer(m.layer));
            let root = scene
                .window_info(win)
                .map_err(|e| scene_err("CreateWindow", e))?
                .root();
            client.bind_node(m.id, root);
            client.windows.insert(m.id, win);
            client.window_ids.insert(win, m.id);
            outcome.new_windows.push((m.id, win));
            Ok(())
        }
        ClientMsg::SetWindowTitle(m) => {
            let win = window_of(client, m.window)?;
            scene
                .set_window_title(client.id, win, m.title)
                .map_err(|e| scene_err("SetWindowTitle", e))
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
                client.unbind_subtree(scene, key);
                client.windows.remove(&m.id);
                client.window_ids.remove(&win);
                client.frame_requests.retain(|w| *w != m.id);
                outcome.closed_windows.push(win);
                return Ok(());
            }
            client.unbind_subtree(scene, key);
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
        ClientMsg::CreateBuffer(_) => {
            // Turned into `Pending::Buffer` when it arrived; the fd cannot
            // wait for the commit.
            Ok(())
        }
        ClientMsg::DestroyBuffer(m) => {
            let key = buffer_key(client, m.id)?;
            client.buffers.remove(&m.id);
            outcome.destroyed_buffers.push(key);
            scene
                .destroy_buffer(client.id, key)
                .map_err(|e| scene_err("DestroyBuffer", e))
        }
        ClientMsg::BufferDamage(m) => {
            let key = buffer_key(client, m.id)?;
            scene
                .buffer_damaged(client.id, key, &m.rects)
                .map_err(|e| scene_err("BufferDamage", e))?;
            outcome.buffer_damage.push((key, m.rects));
            Ok(())
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

/// The scene's node kind for a wire kind. `Text` and `Surface` are
/// reserved: the server advertises neither capability, so a client asking
/// for one is using a feature it was told does not exist.
fn scene_kind(kind: NodeKind) -> Result<SceneNodeKind, ApplyError> {
    match kind {
        NodeKind::Group => Ok(SceneNodeKind::Group),
        NodeKind::Rect => Ok(SceneNodeKind::Rect),
        NodeKind::Image => Ok(SceneNodeKind::Image),
        NodeKind::Text => Err(ApplyError::new(
            ErrorCode::WrongKind,
            "Text nodes are M2; the server does not advertise the TEXT capability",
        )),
        NodeKind::Surface => Err(ApplyError::new(
            ErrorCode::WrongKind,
            "Surface nodes are M5; the server does not advertise the DMABUF capability",
        )),
    }
}

fn scene_fill(fill: msg::Fill) -> SceneFill {
    match fill {
        msg::Fill::None => SceneFill::None,
        msg::Fill::Solid(c) => SceneFill::Solid(c),
        msg::Fill::Linear { start, end, c0, c1 } => SceneFill::Linear { start, end, c0, c1 },
    }
}

/// Validate a `CreateBuffer` and read its pixels out of the memfd.
///
/// The bytes are **copied**, not mapped. Mapping would be one `mmap` and
/// zero copies, but a client can shrink a memfd under a live mapping and
/// turn the server's reads into SIGBUS, so a safe mapping needs either
/// `F_SEAL_SHRINK` enforcement or a signal handler — and `mmap` is `unsafe`
/// in our tree besides. `pread` costs one pass over the pixels per update
/// and is the M1 answer; `BufferDamage` keeps that pass proportional to
/// what actually changed. Revisit with sealing when a client pushes video.
///
/// # Errors
/// A description that does not add up, a size past [`MAX_BUFFER_BYTES`], or
/// a descriptor that will not read.
pub fn read_buffer(m: &msg::CreateBuffer) -> Result<(BufferDesc, Vec<u8>), ApplyError> {
    let desc = validate_buffer(m)?;
    let len = desc.byte_len();
    let mut data = vec![0u8; len];
    read_exact_at(m.fd.as_fd(), &mut data, 0)?;
    Ok((desc, data))
}

/// Re-read the damaged rows of a buffer. Only whole rows are re-read: the
/// rectangles are usually wide and the row is contiguous, so one `pread`
/// per row band beats one per rectangle-row.
///
/// # Errors
/// As [`read_buffer`].
pub fn reread_damage(
    fd: BorrowedFd<'_>,
    desc: BufferDesc,
    rects: &[IRect],
    data: &mut [u8],
) -> Result<(), ApplyError> {
    for rect in rects {
        let rect = rect.intersect(&desc.full_rect());
        if rect.is_empty() {
            continue;
        }
        let y0 = rect.y.cast_unsigned();
        let rows = rect.h.cast_unsigned();
        let start = (y0 as usize) * (desc.stride as usize);
        let end = start + (rows as usize) * (desc.stride as usize);
        if end > data.len() {
            continue;
        }
        read_exact_at(fd, &mut data[start..end], start as u64)?;
    }
    Ok(())
}

/// `pread` in a loop until the slice is full. A short read means the
/// client's memfd is smaller than it declared, which is a bad buffer, not
/// something to pad with zeroes.
fn read_exact_at(fd: BorrowedFd<'_>, buf: &mut [u8], mut offset: u64) -> Result<(), ApplyError> {
    let mut done = 0usize;
    while done < buf.len() {
        match rustix::io::pread(fd, &mut buf[done..], offset) {
            Ok(0) => {
                return Err(ApplyError::new(
                    ErrorCode::BadBuffer,
                    "buffer fd is shorter than the declared size",
                ));
            }
            Ok(n) => {
                done += n;
                offset += n as u64;
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                return Err(ApplyError::new(
                    ErrorCode::BadBuffer,
                    format!("reading the buffer fd: {e}"),
                ));
            }
        }
    }
    Ok(())
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
    Ok(BufferDesc::new(m.width, m.height, m.stride, m.format))
}

/// A buffer's descriptor kept alongside its fd, so `BufferDamage` can
/// re-read rows without the client sending them again.
#[derive(Debug)]
pub struct BufferSource {
    /// The client's memfd.
    pub fd: OwnedFd,
    /// Its declared geometry.
    pub desc: BufferDesc,
}

/// The protocol code for a wire-level failure, re-exported so the event
/// loop does not have to reach into `nitro_wire::server`.
#[must_use]
pub fn wire_code(err: &WireError) -> ErrorCode {
    code_for(err)
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

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_core::Color;

    fn create_buffer(width: u32, height: u32, stride: u32, size: u32, format: u32) -> OwnedFd {
        use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
        let fd = memfd_create("nitro-test", MemfdFlags::CLOEXEC).unwrap();
        ftruncate(&fd, u64::from(stride) * u64::from(height)).unwrap();
        let _ = (width, size, format);
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
    fn cascade_walks_and_wraps_inside_the_output() {
        let size = Size::new(200.0, 100.0);
        let output = Size::new(1920.0, 1080.0);
        assert_eq!(cascade_position(0, size, output), Point::new(0.0, 0.0));
        assert_eq!(cascade_position(1, size, output), Point::new(32.0, 32.0));
        assert_eq!(cascade_position(2, size, output), Point::new(64.0, 64.0));
        // Wraps once past the limit rather than walking off the screen.
        let far = cascade_position(10, size, output);
        assert!(far.x < CASCADE_LIMIT, "{far:?}");
        assert_eq!(cascade_position(10, size, output), Point::new(0.0, 0.0));
        // A window as big as the output is pinned to the origin.
        let full = cascade_position(3, output, output);
        assert_eq!(full, Point::new(0.0, 0.0));
    }

    #[test]
    fn buffer_validation_checks_format_stride_and_caps() {
        let ok = msg_buffer(16, 8, 64, 512, format::XR24);
        assert_eq!(
            validate_buffer(&ok).unwrap(),
            BufferDesc::new(16, 8, 64, format::XR24)
        );

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

    #[test]
    fn reading_a_buffer_copies_the_memfd_and_rereads_damage() {
        use std::io::Write as _;
        let m = msg_buffer(4, 4, 16, 64, format::XR24);
        {
            let mut file = std::fs::File::from(m.fd.try_clone().unwrap());
            file.write_all(&[0xAB; 64]).unwrap();
        }
        let (desc, data) = read_buffer(&m).unwrap();
        assert_eq!(desc, BufferDesc::new(4, 4, 16, format::XR24));
        assert_eq!(data, vec![0xABu8; 64]);

        // Rewrite one row through the fd and re-read only that row.
        {
            use std::io::Seek as _;
            let mut file = std::fs::File::from(m.fd.try_clone().unwrap());
            file.seek(std::io::SeekFrom::Start(16)).unwrap();
            file.write_all(&[0x11; 16]).unwrap();
        }
        let mut copy = data.clone();
        reread_damage(m.fd.as_fd(), desc, &[IRect::new(0, 1, 4, 1)], &mut copy).unwrap();
        assert_eq!(&copy[0..16], &[0xABu8; 16]);
        assert_eq!(&copy[16..32], &[0x11u8; 16]);
        assert_eq!(&copy[32..64], &[0xABu8; 32]);
    }

    #[test]
    fn a_short_fd_is_a_bad_buffer_not_zero_padding() {
        let mut m = msg_buffer(4, 4, 16, 64, format::XR24);
        // Declare twice the rows the fd actually has.
        m.height = 8;
        m.size = 128;
        let err = read_buffer(&m).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadBuffer);
    }

    #[test]
    fn reserved_node_kinds_are_refused() {
        assert_eq!(scene_kind(NodeKind::Group).unwrap(), SceneNodeKind::Group);
        assert_eq!(scene_kind(NodeKind::Rect).unwrap(), SceneNodeKind::Rect);
        assert_eq!(scene_kind(NodeKind::Image).unwrap(), SceneNodeKind::Image);
        assert_eq!(
            scene_kind(NodeKind::Text).unwrap_err().code,
            ErrorCode::WrongKind
        );
        assert_eq!(
            scene_kind(NodeKind::Surface).unwrap_err().code,
            ErrorCode::WrongKind
        );
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

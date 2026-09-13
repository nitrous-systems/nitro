//! Every protocol message: one plain struct per op, and the [`ClientMsg`] /
//! [`ServerMsg`] enums that dispatch on the op code.
//!
//! # Op code blocks
//!
//! Client → server ops have the top bit clear, server → client ops set it.
//! Within each direction, ops are handed out in blocks of 0x100 so a block
//! can grow without renumbering:
//!
//! | block | client | server |
//! |---|---|---|
//! | `0x_0xx` | session and windows | session |
//! | `0x_1xx` | tree | windows |
//! | `0x_2xx` | style (including text style) | input |
//! | `0x_3xx` | buffers | text |
//!
//! # Layout
//!
//! A message with only fixed-size fields *is* a `#[repr(C)]` struct of
//! little-endian, unaligned `zerocopy` fields: encoding is
//! `as_bytes()`, decoding is `ref_from_bytes` plus a range check per
//! tagged field. Messages with a variable tail (`str`, `vec<T>`, a tagged
//! `Fill`) put every fixed field first — in one such struct — and the
//! variable part last, so the same trick applies to the head. Where that
//! reorders the fields relative to the task's sketch, `docs/wire.md`
//! records it.
//!
//! [`SetText`] and [`MeasureText`] are the only messages with *two*
//! variable tails (`family` then `text`); the rule that matters — the
//! fixed head is one packed `repr(C)` struct — still holds, and the
//! strings are read back to back after it.

use std::os::fd::{AsFd as _, OwnedFd};

use nitro_core::{Color, IRect, Point, Rect, Size, Transform};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::codec::{FdQueue, Reader, Writer};
use crate::error::{DecodeError, EncodeError};
use crate::types::{
    Align, AxisSource, BufferId, ButtonState, CursorPos, ErrorCode, Layer, NodeId, NodeKind,
    TouchPhase,
};
use crate::wire::Plain;

/// How one message's payload is written and read.
///
/// Implemented by every message struct; the enums dispatch to it.
pub trait Body: Sized {
    /// Append this message's payload (no frame header).
    ///
    /// # Errors
    /// Only messages carrying a file descriptor can fail, and only when
    /// duplicating it does.
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError>;

    /// Read this message's payload from `r`, claiming any fds it declares.
    ///
    /// # Errors
    /// [`DecodeError`] for any malformed payload.
    fn decode_body(r: &mut Reader<'_>, fds: &mut FdQueue) -> Result<Self, DecodeError>;
}

/// Define messages whose fields are all fixed-size: the struct, and a
/// `Body` impl backed by a `repr(C)` zerocopy twin.
macro_rules! fixed_msg {
    ($(
        $(#[$m:meta])*
        $name:ident { $( $(#[$fm:meta])* $f:ident : $t:ty ),* $(,)? }
    )*) => { $(
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub struct $name {
            $( $(#[$fm])* pub $f: $t, )*
        }

        impl Body for $name {
            fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
                #[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
                #[repr(C)]
                struct Fixed { $( $f: <$t as Plain>::Wire, )* }
                w.put_struct(&Fixed { $( $f: Plain::to_wire(self.$f), )* });
                Ok(())
            }

            fn decode_body(
                r: &mut Reader<'_>,
                _fds: &mut FdQueue,
            ) -> Result<Self, DecodeError> {
                #[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
                #[repr(C)]
                struct Fixed { $( $f: <$t as Plain>::Wire, )* }
                let fixed = r.get_struct::<Fixed>()?;
                Ok(Self { $( $f: Plain::from_wire(fixed.$f)?, )* })
            }
        }
    )* };
}

/// Define an `enum` over message structs with its op table.
macro_rules! msg_enum {
    (
        $(#[$m:meta])*
        $enum_name:ident {
            $( $(#[$vm:meta])* $variant:ident = $op:expr ),* $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, PartialEq)]
        pub enum $enum_name {
            $( $(#[$vm])* $variant($variant), )*
        }

        $(
            impl $variant {
                #[doc = concat!("Op code of [`", stringify!($variant), "`].")]
                pub const OP: u16 = $op;
            }

            impl From<$variant> for $enum_name {
                fn from(m: $variant) -> Self {
                    Self::$variant(m)
                }
            }
        )*

        impl $enum_name {
            /// The op code of this message.
            #[must_use]
            pub fn op(&self) -> u16 {
                match self {
                    $( Self::$variant(_) => $op, )*
                }
            }

            /// The message name, for logs and errors.
            #[must_use]
            pub fn name(&self) -> &'static str {
                match self {
                    $( Self::$variant(_) => stringify!($variant), )*
                }
            }

            /// Whether `op` belongs to this direction of the protocol.
            #[must_use]
            pub fn is_op(op: u16) -> bool {
                matches!(op, $( $op )|*)
            }

            /// Append this message as a complete frame (header included).
            ///
            /// # Errors
            /// [`EncodeError`] if the payload is too large, too many
            /// descriptors are attached, or an attached descriptor cannot
            /// be duplicated. On error nothing is left in `w`.
            pub fn encode(&self, w: &mut Writer) -> Result<(), EncodeError> {
                match self {
                    $( Self::$variant(m) => w.frame($op, |w| m.encode_body(w)), )*
                }
            }

            /// Decode one message from a frame's `op`, `payload` and fds.
            ///
            /// # Errors
            /// [`DecodeError::UnknownOp`] for an op of the wrong direction
            /// or version, and any [`DecodeError`] the payload earns.
            /// Trailing payload bytes and unclaimed descriptors are both
            /// errors.
            pub fn decode(
                op: u16,
                payload: &[u8],
                fds: &mut FdQueue,
            ) -> Result<Self, DecodeError> {
                let mut r = Reader::new(payload);
                let out = match op {
                    $( $op => Self::$variant(<$variant as Body>::decode_body(&mut r, fds)?), )*
                    _ => return Err(DecodeError::UnknownOp(op)),
                };
                r.finish()?;
                fds.finish()?;
                Ok(out)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Client → server
// ---------------------------------------------------------------------------

/// First message on a connection: the version the client speaks and a name
/// for logs (`"nitro-bar"`, `"calc"`). The server answers [`Welcome`] or
/// [`Error`] and closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// Protocol version; [`VERSION`](crate::VERSION) for v1.
    pub version: u32,
    /// Client name, for logs and the window list. Not unique.
    pub name: String,
}

impl Body for Hello {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put_u32(self.version);
        w.put_str(&self.name);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        Ok(Self {
            version: r.get_u32()?,
            name: r.get_str()?,
        })
    }
}

/// Apply every mutation sent since the last commit, atomically.
///
/// Nothing a client sends is visible before its `Commit`; the server never
/// renders half a batch. The `serial` comes back in [`Presented`] when the
/// frame containing this transaction reached the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    /// Client-chosen, monotonically increasing transaction serial.
    pub serial: u32,
}

impl Body for Commit {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put_u32(self.serial);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        Ok(Self {
            serial: r.get_u32()?,
        })
    }
}

/// Create a top-level window: a group node the server places on an output.
///
/// The window's node id is a normal node id in the client's space; its
/// children are created with [`CreateNode`] using `parent: id`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateWindow {
    /// Node id for the window's root group.
    pub id: NodeId,
    /// Requested size in logical pixels; the server answers with the size
    /// it actually gave in [`Configure`].
    pub size: Size,
    /// Stacking layer.
    pub layer: Layer,
    /// Bits from [`window_flags`](crate::types::window_flags).
    pub flags: u32,
    /// Window title (last on the wire, being variable-length).
    pub title: String,
}

impl Body for CreateWindow {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put(self.id);
        w.put(self.size);
        w.put(self.layer);
        w.put_u32(self.flags);
        w.put_str(&self.title);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        Ok(Self {
            id: r.get()?,
            size: r.get()?,
            layer: r.get()?,
            flags: r.get_u32()?,
            title: r.get_str()?,
        })
    }
}

/// Change a window's title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetWindowTitle {
    /// The window's node id.
    pub window: NodeId,
    /// New title.
    pub title: String,
}

impl Body for SetWindowTitle {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put(self.window);
        w.put_str(&self.title);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        Ok(Self {
            window: r.get()?,
            title: r.get_str()?,
        })
    }
}

/// Paint a fill: the tagged union carried by [`SetFill`].
///
/// `Point` is `f32`-based and thus only `PartialEq`; so is `Fill`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fill {
    /// Draw nothing (the node's border, if any, is still drawn).
    None,
    /// A single colour.
    Solid(Color),
    /// A linear gradient between two points in the node's local space.
    Linear {
        /// Where colour `c0` sits.
        start: Point,
        /// Where colour `c1` sits.
        end: Point,
        /// Colour at `start`.
        c0: Color,
        /// Colour at `end`.
        c1: Color,
    },
}

impl Fill {
    /// Wire tag: 0 `None`, 1 `Solid`, 2 `Linear`.
    #[must_use]
    pub const fn tag(&self) -> u8 {
        match self {
            Self::None => 0,
            Self::Solid(_) => 1,
            Self::Linear { .. } => 2,
        }
    }

    fn encode(&self, w: &mut Writer) {
        w.put_u8(self.tag());
        match self {
            Self::None => {}
            Self::Solid(c) => w.put(*c),
            Self::Linear { start, end, c0, c1 } => {
                w.put(*start);
                w.put(*end);
                w.put(*c0);
                w.put(*c1);
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match r.get_u8()? {
            0 => Ok(Self::None),
            1 => Ok(Self::Solid(r.get()?)),
            2 => Ok(Self::Linear {
                start: r.get()?,
                end: r.get()?,
                c0: r.get()?,
                c1: r.get()?,
            }),
            _ => Err(DecodeError::BadValue),
        }
    }
}

/// Set a node's fill. Applies to `Rect` nodes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SetFill {
    /// Node to paint.
    pub id: NodeId,
    /// The fill.
    pub fill: Fill,
}

impl Body for SetFill {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put(self.id);
        self.fill.encode(w);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        Ok(Self {
            id: r.get()?,
            fill: Fill::decode(r)?,
        })
    }
}

/// Set a text node's string and the style it is shaped with.
///
/// Applies at the next [`Commit`], like every other mutation; the server
/// shapes the text and answers with [`TextMetrics`] for each node it
/// (re)shaped in that commit. Requires the
/// [`caps::TEXT`](crate::types::caps::TEXT) capability bit.
///
/// `max_width` 0 means "no limit"; `wrap` only has an effect with a
/// non-zero `max_width`. `family` is a font family name or one of the
/// generic aliases `sans`, `serif`, `mono`. Text is left-to-right only in
/// M2: no bidi, no rich text.
#[derive(Debug, Clone, PartialEq)]
pub struct SetText {
    /// The `Text` node.
    pub node: NodeId,
    /// Font size in logical pixels.
    pub size_px: f32,
    /// CSS-style weight (400 regular, 700 bold).
    pub weight: u16,
    /// Whether to select an italic face.
    pub italic: bool,
    /// Wrapping/alignment width in logical pixels; 0 = no limit.
    pub max_width: f32,
    /// Whether lines wrap at `max_width`. No effect when `max_width` is 0.
    pub wrap: bool,
    /// Horizontal alignment of the lines.
    pub align: Align,
    /// Text colour.
    pub color: Color,
    /// Font family name, or one of `sans`, `serif`, `mono`.
    pub family: String,
    /// The text itself, UTF-8.
    pub text: String,
}

/// The fixed part of [`SetText`] — everything but the two strings.
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct SetTextFixed {
    node: <NodeId as Plain>::Wire,
    size_px: <f32 as Plain>::Wire,
    weight: <u16 as Plain>::Wire,
    italic: <bool as Plain>::Wire,
    max_width: <f32 as Plain>::Wire,
    wrap: <bool as Plain>::Wire,
    align: <Align as Plain>::Wire,
    color: <Color as Plain>::Wire,
}

impl Body for SetText {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put_struct(&SetTextFixed {
            node: Plain::to_wire(self.node),
            size_px: Plain::to_wire(self.size_px),
            weight: Plain::to_wire(self.weight),
            italic: Plain::to_wire(self.italic),
            max_width: Plain::to_wire(self.max_width),
            wrap: Plain::to_wire(self.wrap),
            align: Plain::to_wire(self.align),
            color: Plain::to_wire(self.color),
        });
        w.put_str(&self.family);
        w.put_str(&self.text);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        let f = r.get_struct::<SetTextFixed>()?;
        Ok(Self {
            node: Plain::from_wire(f.node)?,
            size_px: Plain::from_wire(f.size_px)?,
            weight: Plain::from_wire(f.weight)?,
            italic: Plain::from_wire(f.italic)?,
            max_width: Plain::from_wire(f.max_width)?,
            wrap: Plain::from_wire(f.wrap)?,
            align: Plain::from_wire(f.align)?,
            color: Plain::from_wire(f.color)?,
            family: r.get_str()?,
            text: r.get_str()?,
        })
    }
}

/// Ask the server to measure a string without creating a node.
///
/// Unlike every other client message this is answered **immediately on
/// receipt**, not at the next [`Commit`]: it is the protocol's one
/// request/response pair, because a text field needs a measurement before
/// it can lay itself out. The answer is a [`TextMeasured`] carrying the
/// same `request`. Requires the [`caps::TEXT`](crate::types::caps::TEXT)
/// capability bit.
#[derive(Debug, Clone, PartialEq)]
pub struct MeasureText {
    /// Client-chosen request id, echoed in [`TextMeasured`].
    pub request: u32,
    /// Font size in logical pixels.
    pub size_px: f32,
    /// CSS-style weight (400 regular, 700 bold).
    pub weight: u16,
    /// Whether to select an italic face.
    pub italic: bool,
    /// Wrapping width in logical pixels; 0 = no limit.
    pub max_width: f32,
    /// Whether lines wrap at `max_width`. No effect when `max_width` is 0.
    pub wrap: bool,
    /// Font family name, or one of `sans`, `serif`, `mono`.
    pub family: String,
    /// The text to measure, UTF-8.
    pub text: String,
}

/// The fixed part of [`MeasureText`] — everything but the two strings.
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct MeasureTextFixed {
    request: <u32 as Plain>::Wire,
    size_px: <f32 as Plain>::Wire,
    weight: <u16 as Plain>::Wire,
    italic: <bool as Plain>::Wire,
    max_width: <f32 as Plain>::Wire,
    wrap: <bool as Plain>::Wire,
}

impl Body for MeasureText {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put_struct(&MeasureTextFixed {
            request: Plain::to_wire(self.request),
            size_px: Plain::to_wire(self.size_px),
            weight: Plain::to_wire(self.weight),
            italic: Plain::to_wire(self.italic),
            max_width: Plain::to_wire(self.max_width),
            wrap: Plain::to_wire(self.wrap),
        });
        w.put_str(&self.family);
        w.put_str(&self.text);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        let f = r.get_struct::<MeasureTextFixed>()?;
        Ok(Self {
            request: Plain::from_wire(f.request)?,
            size_px: Plain::from_wire(f.size_px)?,
            weight: Plain::from_wire(f.weight)?,
            italic: Plain::from_wire(f.italic)?,
            max_width: Plain::from_wire(f.max_width)?,
            wrap: Plain::from_wire(f.wrap)?,
            family: r.get_str()?,
            text: r.get_str()?,
        })
    }
}

/// Register a shared-memory buffer, passing its descriptor with this
/// frame.
///
/// The server maps the fd read-only; the client keeps writing into it and
/// announces changes with [`BufferDamage`]. `size` must be at least
/// `stride * height`, and `stride` at least `width * bytes-per-pixel`.
///
/// `PartialEq` compares the declared fields only, not which file the
/// descriptor points at — that is what makes round-trip tests possible.
#[derive(Debug)]
pub struct CreateBuffer {
    /// Buffer id, allocated by the client.
    pub id: BufferId,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    /// DRM fourcc pixel format, e.g. [`format::XR24`](crate::types::format::XR24).
    pub format: u32,
    /// Size of the mapping in bytes.
    pub size: u32,
    /// The memfd / shm descriptor.
    pub fd: OwnedFd,
}

impl PartialEq for CreateBuffer {
    fn eq(&self, o: &Self) -> bool {
        self.id == o.id
            && self.width == o.width
            && self.height == o.height
            && self.stride == o.stride
            && self.format == o.format
            && self.size == o.size
    }
}

/// The fixed part of [`CreateBuffer`] — all of it but the descriptor.
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct CreateBufferFixed {
    id: <BufferId as Plain>::Wire,
    width: <u32 as Plain>::Wire,
    height: <u32 as Plain>::Wire,
    stride: <u32 as Plain>::Wire,
    format: <u32 as Plain>::Wire,
    size: <u32 as Plain>::Wire,
}

impl Body for CreateBuffer {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put_struct(&CreateBufferFixed {
            id: Plain::to_wire(self.id),
            width: Plain::to_wire(self.width),
            height: Plain::to_wire(self.height),
            stride: Plain::to_wire(self.stride),
            format: Plain::to_wire(self.format),
            size: Plain::to_wire(self.size),
        });
        let dup = rustix::io::dup(self.fd.as_fd()).map_err(EncodeError::Fd)?;
        w.put_fd(dup);
        Ok(())
    }

    fn decode_body(r: &mut Reader<'_>, fds: &mut FdQueue) -> Result<Self, DecodeError> {
        let f = r.get_struct::<CreateBufferFixed>()?;
        Ok(Self {
            id: Plain::from_wire(f.id)?,
            width: Plain::from_wire(f.width)?,
            height: Plain::from_wire(f.height)?,
            stride: Plain::from_wire(f.stride)?,
            format: Plain::from_wire(f.format)?,
            size: Plain::from_wire(f.size)?,
            fd: fds.take()?,
        })
    }
}

/// Announce that a buffer's pixels changed inside these rectangles; the
/// server re-reads only those rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferDamage {
    /// The buffer.
    pub id: BufferId,
    /// Damaged rectangles, in buffer pixel coordinates.
    pub rects: Vec<IRect>,
}

impl Body for BufferDamage {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put(self.id);
        w.put_vec(&self.rects);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        Ok(Self {
            id: r.get()?,
            rects: r.get_vec()?,
        })
    }
}

fixed_msg! {
    /// Ask for a frame callback on this window: the server answers with
    /// [`Frame`] carrying the deadline for the next flip. One request,
    /// one answer; no free-running render loops.
    RequestFrame {
        /// The window.
        window: NodeId,
    }

    /// Create a scene node under `parent`.
    CreateNode {
        /// Node id, allocated by the client. Must be unused and non-zero.
        id: NodeId,
        /// What the node draws.
        kind: NodeKind,
        /// Parent node; [`NodeId::NONE`] is an error (use
        /// [`CreateWindow`] for a root).
        parent: NodeId,
        /// Insert before this sibling; [`NodeId::NONE`] appends.
        before: NodeId,
    }

    /// Destroy a node and, recursively, its children. Destroying a
    /// window's root group closes the window.
    DestroyNode {
        /// Node to destroy.
        id: NodeId,
    }

    /// Move a node to a new parent and/or position among its siblings.
    Reparent {
        /// Node to move.
        id: NodeId,
        /// New parent.
        parent: NodeId,
        /// Insert before this sibling; [`NodeId::NONE`] appends.
        before: NodeId,
    }

    /// Set a node's rectangle in its parent's coordinate space.
    SetBounds {
        /// The node.
        id: NodeId,
        /// New bounds.
        rect: Rect,
    }

    /// Set a group's transform. `Group` nodes only.
    SetTransform {
        /// The group.
        id: NodeId,
        /// Affine transform applied to the group's children.
        transform: Transform,
    }

    /// Show or hide a node and its subtree.
    SetVisible {
        /// The node.
        id: NodeId,
        /// Whether the subtree is drawn.
        visible: bool,
    }

    /// Set a node's opacity in `0.0..=1.0`; multiplied down the subtree.
    SetOpacity {
        /// The node.
        id: NodeId,
        /// Opacity, clamped by the server.
        opacity: f32,
    }

    /// Clip a group's children to its bounds. `Group` nodes only.
    SetClip {
        /// The group.
        id: NodeId,
        /// Whether children are clipped.
        clip: bool,
    }

    /// Set a rect node's corner radius (0 = square corners).
    SetCorners {
        /// The node.
        id: NodeId,
        /// Corner radius in logical pixels.
        radius: f32,
    }

    /// Set a rect node's border, drawn inside its bounds (width 0 = none).
    SetBorder {
        /// The node.
        id: NodeId,
        /// Border width in logical pixels.
        width: f32,
        /// Border colour.
        color: Color,
    }

    /// Release a buffer id. The server drops its mapping; the client may
    /// reuse the id after the next [`Commit`].
    DestroyBuffer {
        /// The buffer.
        id: BufferId,
    }

    /// Point an `Image` node at a region of a buffer.
    SetImage {
        /// The image node.
        id: NodeId,
        /// Buffer to sample; [`BufferId::NONE`] detaches.
        buffer: BufferId,
        /// Source rectangle in buffer pixels.
        src: IRect,
    }
}

msg_enum! {
    /// Everything a client may send.
    ClientMsg {
        /// Handshake. Must be first.
        Hello = 0x0001,
        /// Apply the pending mutations atomically.
        Commit = 0x0002,
        /// Create a top-level window.
        CreateWindow = 0x0010,
        /// Retitle a window.
        SetWindowTitle = 0x0011,
        /// Ask for the next frame deadline.
        RequestFrame = 0x0012,
        /// Create a node.
        CreateNode = 0x0101,
        /// Destroy a node and its subtree.
        DestroyNode = 0x0102,
        /// Move a node in the tree.
        Reparent = 0x0103,
        /// Set a node's bounds.
        SetBounds = 0x0104,
        /// Set a group's transform.
        SetTransform = 0x0105,
        /// Show or hide a subtree.
        SetVisible = 0x0106,
        /// Set a node's opacity.
        SetOpacity = 0x0201,
        /// Clip a group's children.
        SetClip = 0x0202,
        /// Set a node's fill.
        SetFill = 0x0203,
        /// Set a node's corner radius.
        SetCorners = 0x0204,
        /// Set a node's border.
        SetBorder = 0x0205,
        /// Set a text node's content and style (needs `caps::TEXT`).
        SetText = 0x0206,
        /// Measure a string; answered at once with `TextMeasured`.
        MeasureText = 0x0207,
        /// Register a buffer (carries one fd).
        CreateBuffer = 0x0301,
        /// Release a buffer.
        DestroyBuffer = 0x0302,
        /// Announce changed buffer contents.
        BufferDamage = 0x0303,
        /// Attach a buffer region to an image node.
        SetImage = 0x0304,
    }
}

// ---------------------------------------------------------------------------
// Server → client
// ---------------------------------------------------------------------------

/// Answer to [`Hello`] on a version the server speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    /// The version in force (equal to the client's `Hello`).
    pub version: u32,
    /// Capability bits from [`caps`](crate::types::caps).
    pub caps: u32,
    /// Server name, for logs.
    pub name: String,
}

impl Body for Welcome {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put_u32(self.version);
        w.put_u32(self.caps);
        w.put_str(&self.name);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        Ok(Self {
            version: r.get_u32()?,
            caps: r.get_u32()?,
            name: r.get_str()?,
        })
    }
}

/// Fatal error: the server sends this and closes the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Serial of the transaction being applied, or 0 outside one.
    pub serial: u32,
    /// Machine-readable code.
    pub code: ErrorCode,
    /// Human-readable detail, for logs. Never parsed.
    pub msg: String,
}

impl Body for Error {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put_u32(self.serial);
        w.put(self.code);
        w.put_str(&self.msg);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        Ok(Self {
            serial: r.get_u32()?,
            code: r.get()?,
            msg: r.get_str()?,
        })
    }
}

/// A key press or release, with the text it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// The focused window.
    pub window: NodeId,
    /// Linux evdev keycode (`KEY_*`).
    pub keycode: u32,
    /// Pressed or released.
    pub state: ButtonState,
    /// Active modifiers, as an xkb modifier mask.
    pub mods: u32,
    /// Resolved keysym, or 0.
    pub keysym: u32,
    /// Event time, `CLOCK_MONOTONIC` nanoseconds.
    pub time_ns: u64,
    /// Text this key produced; empty for non-printing keys.
    pub utf8: String,
}

impl Body for Key {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put(self.window);
        w.put_u32(self.keycode);
        w.put(self.state);
        w.put_u32(self.mods);
        w.put_u32(self.keysym);
        w.put_u64(self.time_ns);
        w.put_str(&self.utf8);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        Ok(Self {
            window: r.get()?,
            keycode: r.get_u32()?,
            state: r.get()?,
            mods: r.get_u32()?,
            keysym: r.get_u32()?,
            time_ns: r.get_u64()?,
            utf8: r.get_str()?,
        })
    }
}

/// Answer to a [`MeasureText`], carrying its `request` back.
///
/// Sent as soon as the request is received, independently of any commit.
#[derive(Debug, Clone, PartialEq)]
pub struct TextMeasured {
    /// The `request` of the [`MeasureText`] this answers.
    pub request: u32,
    /// Width of the longest line, in logical pixels.
    pub width: f32,
    /// Total height of all lines, in logical pixels.
    pub height: f32,
    /// Ascent of the first line above its baseline.
    pub ascent: f32,
    /// Descent of the last line below its baseline.
    pub descent: f32,
    /// Number of laid-out lines.
    pub line_count: u32,
    /// Cursor positions inside the measured text, in increasing `offset`
    /// order. Empty when the server reports none.
    pub cursor_x: Vec<CursorPos>,
}

/// The fixed part of [`TextMeasured`] — everything but the cursor vector.
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct TextMeasuredFixed {
    request: <u32 as Plain>::Wire,
    width: <f32 as Plain>::Wire,
    height: <f32 as Plain>::Wire,
    ascent: <f32 as Plain>::Wire,
    descent: <f32 as Plain>::Wire,
    line_count: <u32 as Plain>::Wire,
}

impl Body for TextMeasured {
    fn encode_body(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.put_struct(&TextMeasuredFixed {
            request: Plain::to_wire(self.request),
            width: Plain::to_wire(self.width),
            height: Plain::to_wire(self.height),
            ascent: Plain::to_wire(self.ascent),
            descent: Plain::to_wire(self.descent),
            line_count: Plain::to_wire(self.line_count),
        });
        w.put_vec(&self.cursor_x);
        Ok(())
    }
    fn decode_body(r: &mut Reader<'_>, _fds: &mut FdQueue) -> Result<Self, DecodeError> {
        let f = r.get_struct::<TextMeasuredFixed>()?;
        Ok(Self {
            request: Plain::from_wire(f.request)?,
            width: Plain::from_wire(f.width)?,
            height: Plain::from_wire(f.height)?,
            ascent: Plain::from_wire(f.ascent)?,
            descent: Plain::from_wire(f.descent)?,
            line_count: Plain::from_wire(f.line_count)?,
            cursor_x: r.get_vec()?,
        })
    }
}

fixed_msg! {
    /// A commit reached the screen.
    Presented {
        /// Serial of the [`Commit`] that was presented.
        serial: u32,
        /// Output it was presented on.
        output: u32,
        /// Presentation time, `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
        /// Output frame sequence number (vblank count).
        seq: u64,
    }

    /// The server placed, resized or rescaled a window. The client should
    /// lay out for `size` and commit; the new size takes effect for the
    /// client at that commit.
    Configure {
        /// The window.
        window: NodeId,
        /// Size in logical pixels.
        size: Size,
        /// Top-left corner of the window in the output's logical coordinate
        /// space. This is the position of the window's content origin, so a
        /// client can crop a screenshot of the whole output down to just
        /// itself using `position` and `size`.
        position: Point,

        /// Output scale factor (1.0, 2.0, 1.5, …).
        scale: f32,
        /// Output the window is on.
        output: u32,
    }

    /// Answer to [`RequestFrame`]: aim to commit before `deadline_ns`.
    Frame {
        /// The window.
        window: NodeId,
        /// Target presentation time, `CLOCK_MONOTONIC` nanoseconds.
        deadline_ns: u64,
        /// Output refresh interval in nanoseconds.
        refresh_ns: u32,
    }

    /// Keyboard focus entered or left a window.
    Focus {
        /// The window.
        window: NodeId,
        /// Whether it now has focus.
        focused: bool,
    }

    /// The server closed a window (user action, or the client's own
    /// [`DestroyNode`]). The node id is gone.
    Closed {
        /// The window.
        window: NodeId,
    }

    /// The pointer entered a window.
    PointerEnter {
        /// The window.
        window: NodeId,
        /// Node under the pointer, or [`NodeId::NONE`].
        node: NodeId,
        /// Position in the window's coordinate space.
        pos: Point,
        /// Event time, `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    }

    /// The pointer left a window.
    PointerLeave {
        /// The window.
        window: NodeId,
        /// Event time, `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    }

    /// The pointer moved inside a window.
    PointerMotion {
        /// The window.
        window: NodeId,
        /// Node under the pointer, or [`NodeId::NONE`].
        node: NodeId,
        /// Position in the window's coordinate space.
        pos: Point,
        /// Event time, `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    }

    /// A pointer button changed state.
    PointerButton {
        /// The window.
        window: NodeId,
        /// Linux evdev button code (`BTN_LEFT` = 0x110, …).
        button: u32,
        /// Pressed or released.
        state: ButtonState,
        /// Event time, `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    }

    /// Scrolling.
    PointerAxis {
        /// The window.
        window: NodeId,
        /// Horizontal scroll in logical pixels.
        dx: f32,
        /// Vertical scroll in logical pixels.
        dy: f32,
        /// Where the scroll came from.
        source: AxisSource,
        /// Event time, `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    }

    /// The result of shaping a text node, sent for every node the server
    /// (re)shaped in a commit. The answer to [`SetText`].
    TextMetrics {
        /// The `Text` node that was shaped.
        node: NodeId,
        /// Width of the longest line, in logical pixels.
        width: f32,
        /// Total height of all lines, in logical pixels.
        height: f32,
        /// Ascent of the first line above its baseline.
        ascent: f32,
        /// Descent of the last line below its baseline.
        descent: f32,
        /// Number of laid-out lines.
        line_count: u32,
    }

    /// A touch point changed.
    Touch {
        /// The window.
        window: NodeId,
        /// Touch point id, stable from `Down` to `Up`/`Cancel`.
        id: i32,
        /// What happened.
        phase: TouchPhase,
        /// Position in the window's coordinate space.
        pos: Point,
        /// Event time, `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    }
}

msg_enum! {
    /// Everything the server may send.
    ServerMsg {
        /// Handshake accepted.
        Welcome = 0x8001,
        /// Fatal error; the connection closes.
        Error = 0x8002,
        /// A commit reached the screen.
        Presented = 0x8003,
        /// Window placed, resized or rescaled.
        Configure = 0x8101,
        /// Frame deadline.
        Frame = 0x8102,
        /// Keyboard focus change.
        Focus = 0x8103,
        /// Window closed.
        Closed = 0x8104,
        /// Pointer entered.
        PointerEnter = 0x8201,
        /// Pointer left.
        PointerLeave = 0x8202,
        /// Pointer moved.
        PointerMotion = 0x8203,
        /// Pointer button.
        PointerButton = 0x8204,
        /// Scroll.
        PointerAxis = 0x8205,
        /// Key press or release.
        Key = 0x8206,
        /// Touch point.
        Touch = 0x8207,
        /// Metrics of a text node the server just shaped.
        TextMetrics = 0x8301,
        /// Answer to a `MeasureText`.
        TextMeasured = 0x8302,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_codes_are_in_the_right_direction() {
        assert!(ClientMsg::is_op(Hello::OP));
        assert!(!ClientMsg::is_op(Welcome::OP));
        assert!(ServerMsg::is_op(Welcome::OP));
        assert!(!ServerMsg::is_op(Hello::OP));
        assert_eq!(Hello::OP & 0x8000, 0);
        assert_eq!(Welcome::OP & 0x8000, 0x8000);
    }

    #[test]
    fn wrong_direction_is_unknown_op() {
        let mut fds = FdQueue::new();
        assert_eq!(
            ClientMsg::decode(Welcome::OP, &[], &mut fds),
            Err(DecodeError::UnknownOp(Welcome::OP))
        );
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut w = Writer::new();
        ClientMsg::from(Commit { serial: 1 })
            .encode(&mut w)
            .unwrap();
        let mut payload = w.bytes()[crate::header::SIZE..].to_vec();
        payload.push(0);
        let mut fds = FdQueue::new();
        assert_eq!(
            ClientMsg::decode(Commit::OP, &payload, &mut fds),
            Err(DecodeError::Trailing)
        );
    }

    #[test]
    fn fill_tags_round_trip() {
        for fill in [
            Fill::None,
            Fill::Solid(Color::rgba(1, 2, 3, 4)),
            Fill::Linear {
                start: Point::new(0.0, 0.0),
                end: Point::new(1.0, 2.0),
                c0: Color::BLACK,
                c1: Color::WHITE,
            },
        ] {
            let mut w = Writer::new();
            let msg = SetFill {
                id: NodeId(3),
                fill,
            };
            msg.encode_body(&mut w).unwrap();
            let bytes = w.bytes().to_vec();
            let mut r = Reader::new(&bytes);
            let back = SetFill::decode_body(&mut r, &mut FdQueue::new()).unwrap();
            assert_eq!(back, msg);
            assert!(r.finish().is_ok());
        }
    }

    #[test]
    fn bad_fill_tag_is_an_error() {
        let mut w = Writer::new();
        w.put_u32(1);
        w.put_u8(9);
        let bytes = w.bytes().to_vec();
        let mut fds = FdQueue::new();
        assert_eq!(
            ClientMsg::decode(SetFill::OP, &bytes, &mut fds),
            Err(DecodeError::BadValue)
        );
    }
}

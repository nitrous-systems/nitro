//! Protocol value types: ids, tagged enumerations and the capability and
//! pixel-format constants. Geometry and colour come from `nitro-core`.

use crate::error::DecodeError;

/// A scene-graph node id, allocated by the client.
///
/// `NodeId(0)` is [`NodeId::NONE`] — "no node": an absent parent, "append
/// at the end" for a sibling reference, or "no node under the pointer".
/// Ids are namespaced per client; the server keys nodes by `(client, id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NodeId(pub u32);

impl NodeId {
    /// The "no node" id.
    pub const NONE: Self = Self(0);

    /// Whether this is [`NodeId::NONE`].
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// The raw wire value.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A shared-memory buffer id, allocated by the client.
///
/// `BufferId(0)` is [`BufferId::NONE`]: "no buffer" (detaches an image).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BufferId(pub u32);

impl BufferId {
    /// The "no buffer" id.
    pub const NONE: Self = Self(0);

    /// Whether this is [`BufferId::NONE`].
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// The raw wire value.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Macro for the small `u8`-tagged enumerations: one `repr(u8)` enum plus a
/// checked `from_u8`.
macro_rules! tag_enum {
    (
        $(#[$attr:meta])*
        $name:ident : $repr:ty {
            $( $(#[$vattr:meta])* $variant:ident = $value:expr ),* $(,)?
        }
    ) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr($repr)]
        pub enum $name {
            $( $(#[$vattr])* $variant = $value ),*
        }

        impl $name {
            /// The wire value.
            #[must_use]
            pub const fn raw(self) -> $repr {
                self as $repr
            }

            /// Decode a wire value.
            ///
            /// # Errors
            /// [`DecodeError::BadValue`] for anything not listed.
            pub const fn from_raw(v: $repr) -> Result<Self, DecodeError> {
                match v {
                    $( $value => Ok(Self::$variant), )*
                    _ => Err(DecodeError::BadValue),
                }
            }
        }
    };
}

tag_enum! {
    /// Stacking layer of a top-level window.
    Layer: u8 {
        /// Wallpaper and desktop widgets.
        Background = 0,
        /// Ordinary application windows.
        Normal = 1,
        /// Panels, bars and docks.
        Top = 2,
        /// Menus, tooltips, the lock screen.
        Overlay = 3,
    }
}

tag_enum! {
    /// What a scene node draws.
    NodeKind: u8 {
        /// Container: transform, clip and opacity for its children.
        Group = 1,
        /// A (rounded) rectangle with a fill and an optional border.
        Rect = 2,
        /// A region of a client buffer.
        Image = 3,
        /// Shaped text run. The client sends the string and its style with
        /// [`SetText`](crate::msg::SetText) and the server does the
        /// shaping. Always accepted; [`caps::TEXT`] reports whether the
        /// server has a font, and so whether the text will be *visible*.
        Text = 4,
        /// External surface (dma-buf, Wayland adapter). Reserved for M5.
        Surface = 5,
    }
}

tag_enum! {
    /// What a top-level window is doing: its geometry mode.
    WindowState: u8 {
        /// Floating at its own size and position.
        Normal = 0,
        /// Filling the output's work area.
        Maximized = 1,
        /// Covering the whole output, decorations hidden.
        Fullscreen = 2,
        /// Hidden, but still in the window list and the focus-cycling
        /// order.
        Minimized = 3,
    }
}

tag_enum! {
    /// Horizontal alignment of a text node's lines inside its bounds.
    Align: u8 {
        /// Lines start at the left edge.
        Left = 0,
        /// Lines are centred.
        Center = 1,
        /// Lines end at the right edge.
        Right = 2,
    }
}

/// One cursor position: a byte offset into the text and the x it sits at.
///
/// Reported by [`TextMeasured`](crate::msg::TextMeasured) so a text field
/// can place a caret or a selection edge without shaping the string
/// itself. `offset` is a byte offset into the measured string (always on
/// a UTF-8 character boundary) and `x` is in logical pixels from the
/// text's left edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorPos {
    /// Byte offset into the measured string.
    pub offset: u32,
    /// Horizontal position in logical pixels.
    pub x: f32,
}

impl CursorPos {
    /// A cursor at `offset`, sitting at `x`.
    #[must_use]
    pub const fn new(offset: u32, x: f32) -> Self {
        Self { offset, x }
    }
}

tag_enum! {
    /// Press state of a key or pointer button.
    ButtonState: u8 {
        /// Released.
        Released = 0,
        /// Pressed.
        Pressed = 1,
    }
}

tag_enum! {
    /// Where a scroll event came from; clients use it to pick a scroll
    /// behaviour (stepped wheel versus kinetic finger scrolling).
    AxisSource: u8 {
        /// A notched mouse wheel.
        Wheel = 0,
        /// A finger on a touchpad.
        Finger = 1,
        /// A continuous device (trackpoint, tilting wheel emulation).
        Continuous = 2,
        /// Wheel tilt (horizontal scroll on a mouse).
        WheelTilt = 3,
    }
}

tag_enum! {
    /// Phase of a touch point.
    TouchPhase: u8 {
        /// The finger went down.
        Down = 0,
        /// The finger moved.
        Move = 1,
        /// The finger was lifted.
        Up = 2,
        /// The sequence was cancelled (palm rejection, gesture takeover).
        Cancel = 3,
    }
}

tag_enum! {
    /// Why the server is closing the connection.
    ///
    /// Every error is fatal: the server sends
    /// [`Error`](crate::msg::Error) and closes the socket.
    ErrorCode: u16 {
        /// Malformed frame, unknown op, or a message in the wrong state.
        Protocol = 1,
        /// A node id that does not exist (or belongs to another client).
        UnknownNode = 2,
        /// The operation does not apply to this node kind.
        WrongKind = 3,
        /// The requested parent cannot hold this node (cycle, wrong kind,
        /// or a `before` sibling that is not a child of the parent).
        BadParent = 4,
        /// Unknown buffer, or one whose fd, size or stride do not match its
        /// declared geometry.
        BadBuffer = 5,
        /// A protocol limit was exceeded (node count, buffer size, …).
        Limit = 6,
        /// The client asked for a protocol version the server does not
        /// speak.
        Version = 7,
    }
}

/// Server capability bits, reported in [`Welcome`](crate::msg::Welcome).
///
/// A zero bit means the client must not use the feature. New features are
/// added as new ops guarded by a new bit; v1 itself is frozen.
pub mod caps {
    /// The server can scan out client buffers directly (no copy) when a
    /// node covers a whole output.
    pub const DIRECT_SCANOUT: u32 = 1 << 0;
    /// The server found at least one font, so `Text` nodes will actually
    /// draw (M2).
    ///
    /// `Text` nodes and `SetText` are accepted either way — a fontless
    /// server shapes to an empty run rather than killing the connection.
    /// The bit answers the question a client can act on: is it worth
    /// laying out for text at all?
    pub const TEXT: u32 = 1 << 1;
    /// The server accepts `Surface` nodes backed by dma-bufs (M5).
    pub const DMABUF: u32 = 1 << 2;
    /// The connection is remote: buffers are expensive, text is cheap.
    pub const REMOTE: u32 = 1 << 3;
    /// Server-side window management: decorations, states, limits, app ids
    /// (M3).
    pub const WM: u32 = 1 << 4;
}

/// Pixel formats for [`CreateBuffer`](crate::msg::CreateBuffer), as DRM
/// fourcc codes. Unknown formats are rejected by the server with
/// [`ErrorCode::BadBuffer`], not by the decoder.
pub mod format {
    /// `XR24`: 32 bpp little-endian `[b, g, r, x]`, alpha ignored.
    pub const XR24: u32 = fourcc(b"XR24");
    /// `AR24`: 32 bpp little-endian `[b, g, r, a]`, straight alpha.
    pub const AR24: u32 = fourcc(b"AR24");

    /// Build a fourcc code from four ASCII bytes.
    #[must_use]
    pub const fn fourcc(c: &[u8; 4]) -> u32 {
        (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
    }
}

/// Window flags for [`CreateWindow`](crate::msg::CreateWindow). Unknown
/// bits are reserved and must be zero.
pub mod window_flags {
    /// The server draws no frame around this window (splash, overlay).
    pub const UNDECORATED: u32 = 1 << 0;
    /// The window is not user-resizable: no resize bands, no maximize.
    pub const FIXED_SIZE: u32 = 1 << 1;
    /// The window never takes keyboard focus (launcher, bar).
    pub const NO_FOCUS: u32 = 1 << 2;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip() {
        assert_eq!(
            NodeKind::from_raw(NodeKind::Group.raw()),
            Ok(NodeKind::Group)
        );
        assert_eq!(Layer::from_raw(3), Ok(Layer::Overlay));
        assert_eq!(NodeKind::from_raw(0), Err(DecodeError::BadValue));
        assert_eq!(NodeKind::from_raw(6), Err(DecodeError::BadValue));
        assert_eq!(TouchPhase::from_raw(3), Ok(TouchPhase::Cancel));
        assert_eq!(ErrorCode::from_raw(7), Ok(ErrorCode::Version));
        assert_eq!(ErrorCode::from_raw(0), Err(DecodeError::BadValue));
        assert_eq!(
            WindowState::from_raw(WindowState::Normal.raw()),
            Ok(WindowState::Normal)
        );
        assert_eq!(WindowState::from_raw(1), Ok(WindowState::Maximized));
        assert_eq!(WindowState::from_raw(2), Ok(WindowState::Fullscreen));
        assert_eq!(WindowState::from_raw(3), Ok(WindowState::Minimized));
        assert_eq!(WindowState::from_raw(4), Err(DecodeError::BadValue));
    }

    #[test]
    fn ids_and_formats() {
        assert!(NodeId::NONE.is_none());
        assert!(!NodeId(1).is_none());
        assert!(BufferId::default().is_none());
        assert_eq!(format::XR24, 0x3432_5258);
        assert_eq!(format::AR24, 0x3432_5241);
        assert_eq!(caps::WM, 0x10);
        assert_eq!(
            window_flags::UNDECORATED | window_flags::FIXED_SIZE | window_flags::NO_FOCUS,
            0b111
        );
    }
}

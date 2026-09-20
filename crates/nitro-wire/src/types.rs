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

/// A **server-global** window id, allocated by the server.
///
/// Distinct from [`NodeId`], which is a client's own id for its own node:
/// ids are namespaced per client, so two clients may both use `NodeId(1)`.
/// A shell talks *about* other clients' windows, so it needs a name that is
/// unique across the whole server, and this is it. It appears only in the
/// shell messages (caps [`SHELL`](caps::SHELL)) and a shell only ever
/// learns one from a [`WindowInfo`](crate::msg::WindowInfo).
///
/// `WindowRef(0)` is [`WindowRef::NONE`]: "no window".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct WindowRef(pub u32);

impl WindowRef {
    /// The "no window" id.
    pub const NONE: Self = Self(0);

    /// Whether this is [`WindowRef::NONE`].
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
        /// A symbolic icon, named by the client and drawn by the server
        /// with [`SetIcon`](crate::msg::SetIcon).
        ///
        /// Its own kind rather than an `Image`, because an `Image` *is* a
        /// region of a client buffer — a file descriptor the server maps,
        /// which a remote client cannot have at all. An icon carries no
        /// buffer, works unchanged over TCP, and is recoloured by the
        /// server when the scheme flips. See `docs/icons.md`.
        Icon = 6,
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
    /// An edge of an output, for a shell window's exclusive zone.
    ///
    /// See [`SetExclusiveZone`](crate::msg::SetExclusiveZone): the zone is
    /// taken off *this* edge of the output's work area.
    Edge: u8 {
        /// The top edge (a bar).
        Top = 0,
        /// The bottom edge (a dock).
        Bottom = 1,
        /// The left edge.
        Left = 2,
        /// The right edge.
        Right = 3,
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
        /// [`SetIcon`](crate::msg::SetIcon) named an icon the server does
        /// not have.
        ///
        /// **The one other non-fatal error**, beside a remote client's
        /// buffer op: the node draws nothing and the connection stays up.
        /// A desktop must not die because one app asked for an icon a
        /// newer icon set has — see `docs/icons.md`.
        BadIcon = 8,
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
    /// The connection is **privileged**: it arrived on the shell socket, so
    /// it may send the shell ops (layers, exclusive zones, anchors, global
    /// hotkeys, the window list, output enumeration) — see `docs/shell.md`.
    ///
    /// Bit 5, not bit 3: bit 3 (value 8) is [`REMOTE`] and was taken in M1.
    /// A privilege is granted by *which socket* a client connected to, and
    /// this bit is only how the server reports the grant.
    pub const SHELL: u32 = 1 << 5;
    /// The server owns the colour palette and pushes it: the client will
    /// receive a [`Theme`](crate::msg::Theme) right after its
    /// [`Welcome`](crate::msg::Welcome), and another one every time the
    /// palette changes (M4).
    ///
    /// A client that sees this bit must **not** hard-code colours: the
    /// user's light/dark switch and every per-role override in
    /// `server.conf` arrive through that message and nowhere else. A
    /// client that does not understand the bit keeps its built-in
    /// defaults, which is why the message is behind a bit at all.
    pub const THEME: u32 = 1 << 6;
    /// The server has the symbolic icon set, so `Icon` nodes and
    /// [`SetIcon`](crate::msg::SetIcon) will actually draw (M4-G).
    ///
    /// The same shape as [`TEXT`]: a client names an icon, the server
    /// owns the artwork and rasterises it at the output's scale. Without
    /// the bit a client lays out the same square box and paints nothing,
    /// so an old or icon-less server costs a gap, not a dead connection.
    pub const ICONS: u32 = 1 << 7;
    /// The server accepts [`CreatePopup`](crate::msg::CreatePopup) and
    /// [`RepositionPopup`](crate::msg::RepositionPopup), and reports a
    /// dismissal with [`PopupDone`](crate::msg::PopupDone) (M5-A).
    pub const POPUP: u32 = 1 << 8;
    /// The client may ask for a named cursor shape with
    /// [`SetCursor`](crate::msg::SetCursor) (M5-A).
    pub const CURSOR: u32 = 1 << 9;
    /// The client may start an interactive **window** move or resize with
    /// [`StartMove`](crate::msg::StartMove) /
    /// [`StartResize`](crate::msg::StartResize) (M5-A).
    ///
    /// Not drag-and-drop: that is [`DATA`].
    pub const DRAG: u32 = 1 << 10;
    /// An unprivileged client may enumerate outputs with
    /// [`ListOutputs`](crate::msg::ListOutputs) (M5-A).
    ///
    /// The answer is the *shell* block's
    /// [`OutputInfo`](crate::msg::OutputInfo),
    /// [`OutputWorkArea`](crate::msg::OutputWorkArea),
    /// [`OutputsEnd`](crate::msg::OutputsEnd) and
    /// [`OutputGone`](crate::msg::OutputGone): those four are sent to a
    /// client holding **either** [`SHELL`] **or** `OUTPUTS`. See
    /// `docs/wire.md`.
    pub const OUTPUTS: u32 = 1 << 11;
    /// The server sends the xkb keymap ([`Keymap`](crate::msg::Keymap))
    /// and the modifier masks ([`Modifiers`](crate::msg::Modifiers)), for
    /// a client that owns its own `xkb_state` (M5-A).
    pub const KEYMAP: u32 = 1 << 12;
    /// The server reports when it has finished reading a buffer with
    /// [`BufferReleased`](crate::msg::BufferReleased) (M5-A).
    pub const RELEASE: u32 = 1 << 13;
    /// Data transfer: clipboard **and** drag-and-drop (M5-A).
    ///
    /// One bit for both because they share the whole offer/mime/fd
    /// machinery — [`RequestSelection`](crate::msg::RequestSelection)
    /// names which of the two it means with a
    /// [`DataSource`](crate::types::DataSource).
    pub const DATA: u32 = 1 << 14;
    /// Every bit from M5-A onwards: the range
    /// [`ClientCaps`](crate::msg::ClientCaps) governs.
    ///
    /// A client sends `ClientCaps` only when `Welcome.caps` carried at
    /// least one bit in this mask — a server advertising one necessarily
    /// knows the op, because the bits and the op arrived in the same
    /// change. A future milestone extends this constant rather than
    /// teaching every caller a new number.
    pub const CAPS_M5_MASK: u32 = POPUP | CURSOR | DRAG | OUTPUTS | KEYMAP | RELEASE | DATA;
}

/// Modifier mask for [`BindKey`](crate::msg::BindKey), by *name*.
///
/// Deliberately not xkb's serialized mask, which [`Key::mods`](crate::msg::Key)
/// carries: that bitmap's positions depend on the keymap, so it cannot be
/// compared against a constant and a shell could not express "Super+Return"
/// in it at all. These four bits are the modifiers a desktop shortcut is
/// made of; unknown bits are reserved and must be zero.
pub mod mod_mask {
    /// Shift.
    pub const SHIFT: u32 = 1 << 0;
    /// Control.
    pub const CTRL: u32 = 1 << 1;
    /// Alt (Mod1).
    pub const ALT: u32 = 1 << 2;
    /// Super / Logo / Windows (Mod4).
    pub const SUPER: u32 = 1 << 3;
    /// Every bit defined; anything outside this mask is reserved.
    pub const ALL: u32 = SHIFT | CTRL | ALT | SUPER;
}

/// Edge bitmask for [`SetAnchor`](crate::msg::SetAnchor).
///
/// Opposite edges together mean "span that axis"; neither means "centre on
/// it". A bar is `TOP | LEFT | RIGHT`, a centred launcher is 0. Unknown
/// bits are reserved and must be zero.
pub mod anchor {
    /// Stick to the top edge.
    pub const TOP: u8 = 1 << 0;
    /// Stick to the bottom edge.
    pub const BOTTOM: u8 = 1 << 1;
    /// Stick to the left edge.
    pub const LEFT: u8 = 1 << 2;
    /// Stick to the right edge.
    pub const RIGHT: u8 = 1 << 3;
    /// Every bit defined; anything outside this mask is reserved.
    pub const ALL: u8 = TOP | BOTTOM | LEFT | RIGHT;
}

tag_enum! {
    /// A named cursor shape for [`SetCursor`](crate::msg::SetCursor).
    ///
    /// The numbering is `wp_cursor_shape_device_v1`'s **verbatim** (1–34),
    /// with `None = 0` prepended to mean "hide the cursor". Borrowing an
    /// externally validated list makes a future Wayland adapter a cast and
    /// stops the enum being re-litigated; it also covers every
    /// `ui::mojom::CursorType` a Chromium backend needs, with the panning
    /// family mapping to [`AllScroll`](CursorShape::AllScroll), the
    /// `*NoResize` family to [`NotAllowed`](CursorShape::NotAllowed) and
    /// the `kDnd*` family to `NoDrop`/`Move`/`Copy`/`Alias`.
    ///
    /// A **bitmap** cursor (`CursorType::kCustom`, CSS `cursor: url(…)`)
    /// is deliberately unsupported — see `docs/wire.md` under Versioning
    /// policy.
    CursorShape: u16 {
        /// Hide the cursor entirely.
        None = 0,
        /// The ordinary arrow.
        Default = 1,
        /// A context menu is available.
        ContextMenu = 2,
        /// Help is available.
        Help = 3,
        /// A link or button: the "hand".
        Pointer = 4,
        /// Busy, but still interactive.
        Progress = 5,
        /// Busy and not interactive.
        Wait = 6,
        /// A table cell.
        Cell = 7,
        /// Precise selection.
        Crosshair = 8,
        /// Selectable text: the I-beam.
        Text = 9,
        /// Selectable vertical text.
        VerticalText = 10,
        /// An alias or shortcut will be created.
        Alias = 11,
        /// A copy will be made.
        Copy = 12,
        /// The item will be moved.
        Move = 13,
        /// The item cannot be dropped here.
        NoDrop = 14,
        /// The action is not allowed.
        NotAllowed = 15,
        /// Something can be grabbed.
        Grab = 16,
        /// Something is being dragged.
        Grabbing = 17,
        /// Resize east.
        EResize = 18,
        /// Resize north.
        NResize = 19,
        /// Resize north-east.
        NeResize = 20,
        /// Resize north-west.
        NwResize = 21,
        /// Resize south.
        SResize = 22,
        /// Resize south-east.
        SeResize = 23,
        /// Resize south-west.
        SwResize = 24,
        /// Resize west.
        WResize = 25,
        /// Resize along the east-west axis.
        EwResize = 26,
        /// Resize along the north-south axis.
        NsResize = 27,
        /// Resize along the north-east/south-west diagonal.
        NeswResize = 28,
        /// Resize along the north-west/south-east diagonal.
        NwseResize = 29,
        /// Resize a column.
        ColResize = 30,
        /// Resize a row.
        RowResize = 31,
        /// Scroll in any direction.
        AllScroll = 32,
        /// Zoom in.
        ZoomIn = 33,
        /// Zoom out.
        ZoomOut = 34,
    }
}

tag_enum! {
    /// Which edge or corner of a popup's anchor rectangle the popup hangs
    /// off; see [`CreatePopup`](crate::msg::CreatePopup).
    ///
    /// 1:1 with `ui::OwnedWindowAnchorPosition` **and** with
    /// `xdg_positioner`'s anchor enum, which agree.
    PopupAnchor: u8 {
        /// The centre of the anchor rectangle.
        None = 0,
        /// The middle of its top edge.
        Top = 1,
        /// The middle of its bottom edge.
        Bottom = 2,
        /// The middle of its left edge.
        Left = 3,
        /// The middle of its right edge.
        Right = 4,
        /// Its top-left corner.
        TopLeft = 5,
        /// Its bottom-left corner.
        BottomLeft = 6,
        /// Its top-right corner.
        TopRight = 7,
        /// Its bottom-right corner.
        BottomRight = 8,
    }
}

tag_enum! {
    /// Which direction a popup grows from its anchor point; see
    /// [`CreatePopup`](crate::msg::CreatePopup).
    ///
    /// The same nine values as [`PopupAnchor`], and 1:1 with
    /// `ui::OwnedWindowAnchorGravity`.
    PopupGravity: u8 {
        /// Centred on the anchor point.
        None = 0,
        /// Above it.
        Top = 1,
        /// Below it.
        Bottom = 2,
        /// To its left.
        Left = 3,
        /// To its right.
        Right = 4,
        /// Up and to the left.
        TopLeft = 5,
        /// Down and to the left.
        BottomLeft = 6,
        /// Up and to the right.
        TopRight = 7,
        /// Down and to the right.
        BottomRight = 8,
    }
}

tag_enum! {
    /// The single action a drag-and-drop transfer settled on.
    ///
    /// The *set* a source offers is a
    /// [`drag_actions`](crate::types::drag_actions) bitmask; this is the
    /// one the destination chose.
    DragAction: u8 {
        /// No action: the drop was rejected.
        None = 0,
        /// The data is copied.
        Copy = 1,
        /// The data is moved.
        Move = 2,
        /// A link to the data is made.
        Link = 3,
    }
}

tag_enum! {
    /// Format of the keymap in [`Keymap`](crate::msg::Keymap).
    ///
    /// One value today; the byte is what stops a second format needing a
    /// second op.
    KeymapFormat: u8 {
        /// `XKB_KEYMAP_FORMAT_TEXT_V1`, NUL-terminated, in a sealed memfd.
        XkbV1 = 1,
    }
}

tag_enum! {
    /// Which transfer a selection request is about.
    ///
    /// Clipboard and drag-and-drop share the whole offer/mime/fd
    /// machinery — and the [`DATA`](caps::DATA) bit — so one byte on
    /// [`RequestSelection`](crate::msg::RequestSelection) and
    /// [`SelectionRequest`](crate::msg::SelectionRequest) says which of
    /// the two is meant rather than duplicating four messages.
    DataSource: u8 {
        /// The clipboard selection.
        Clipboard = 0,
        /// The drag offer currently over this client. Only valid between a
        /// [`DragEnter`](crate::msg::DragEnter) and the matching
        /// [`DragLeave`](crate::msg::DragLeave) or the end of the drop.
        Drag = 1,
    }
}

/// Constraint adjustments for [`CreatePopup`](crate::msg::CreatePopup):
/// what the server may do when the popup does not fit on its output.
///
/// Values match `ui::OwnedWindowConstraintAdjustment` (and
/// `xdg_positioner`'s `constraint_adjustment`). Unknown bits are reserved
/// and must be zero.
///
/// **`RESIZE_Y` is spelled correctly here.** Chromium's constant is
/// `kAdjustmentRezizeY` — a typo upstream, deliberately not copied; do not
/// "fix" ours to match.
pub mod constraint_adjust {
    /// Slide along x until the popup fits.
    pub const SLIDE_X: u32 = 1 << 0;
    /// Slide along y until the popup fits.
    pub const SLIDE_Y: u32 = 1 << 1;
    /// Flip to the other side of the anchor on x.
    pub const FLIP_X: u32 = 1 << 2;
    /// Flip to the other side of the anchor on y.
    pub const FLIP_Y: u32 = 1 << 3;
    /// Shrink the popup on x.
    pub const RESIZE_X: u32 = 1 << 4;
    /// Shrink the popup on y.
    pub const RESIZE_Y: u32 = 1 << 5;
    /// Every bit defined; anything outside this mask is reserved.
    pub const ALL: u32 = SLIDE_X | SLIDE_Y | FLIP_X | FLIP_Y | RESIZE_X | RESIZE_Y;
}

/// Flags for [`CreatePopup`](crate::msg::CreatePopup). Unknown bits are
/// reserved and must be zero.
pub mod popup_flags {
    /// Take the pointer grab: a click outside the popup chain dismisses
    /// the whole chain and is **consumed** rather than delivered.
    pub const GRAB: u32 = 1 << 0;
    /// Every bit defined; anything outside this mask is reserved.
    pub const ALL: u32 = GRAB;
}

/// Edge bitmask for [`StartResize`](crate::msg::StartResize): which edges
/// or corner the user grabbed.
///
/// The same bit positions as [`anchor`] by design, so a toolkit that keeps
/// one edge set can pass it to either — but a separate module, because a
/// resize edge set and a window anchor are not the same idea. Unknown bits
/// are reserved and must be zero; 0 means "the server picks", and a
/// corner is two bits.
pub mod resize_edges {
    /// The top edge.
    pub const TOP: u8 = 1 << 0;
    /// The bottom edge.
    pub const BOTTOM: u8 = 1 << 1;
    /// The left edge.
    pub const LEFT: u8 = 1 << 2;
    /// The right edge.
    pub const RIGHT: u8 = 1 << 3;
    /// Every bit defined; anything outside this mask is reserved.
    pub const ALL: u8 = TOP | BOTTOM | LEFT | RIGHT;
}

/// The set of actions a drag **source** offers, for
/// [`StartDrag`](crate::msg::StartDrag) and
/// [`DragEnter`](crate::msg::DragEnter).
///
/// The destination picks one of them and names it as a
/// [`DragAction`]. Unknown bits are reserved and must be zero.
pub mod drag_actions {
    /// The data may be copied.
    pub const COPY: u32 = 1 << 0;
    /// The data may be moved.
    pub const MOVE: u32 = 1 << 1;
    /// A link to the data may be made.
    pub const LINK: u32 = 1 << 2;
    /// Every bit defined; anything outside this mask is reserved.
    pub const ALL: u32 = COPY | MOVE | LINK;
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
        assert_eq!(NodeKind::from_raw(6), Ok(NodeKind::Icon));
        assert_eq!(NodeKind::from_raw(7), Err(DecodeError::BadValue));
        assert_eq!(ErrorCode::from_raw(8), Ok(ErrorCode::BadIcon));
        assert_eq!(ErrorCode::from_raw(9), Err(DecodeError::BadValue));
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
        assert_eq!(Edge::from_raw(Edge::Top.raw()), Ok(Edge::Top));
        assert_eq!(Edge::from_raw(3), Ok(Edge::Right));
        assert_eq!(Edge::from_raw(4), Err(DecodeError::BadValue));
    }

    #[test]
    fn the_m5_tags_decode_strictly() {
        assert_eq!(CursorShape::from_raw(0), Ok(CursorShape::None));
        assert_eq!(CursorShape::from_raw(1), Ok(CursorShape::Default));
        assert_eq!(CursorShape::from_raw(9), Ok(CursorShape::Text));
        assert_eq!(CursorShape::from_raw(34), Ok(CursorShape::ZoomOut));
        assert_eq!(CursorShape::from_raw(35), Err(DecodeError::BadValue));
        assert_eq!(CursorShape::from_raw(0xffff), Err(DecodeError::BadValue));
        assert_eq!(PopupAnchor::from_raw(0), Ok(PopupAnchor::None));
        assert_eq!(PopupAnchor::from_raw(8), Ok(PopupAnchor::BottomRight));
        assert_eq!(PopupAnchor::from_raw(9), Err(DecodeError::BadValue));
        assert_eq!(PopupGravity::from_raw(4), Ok(PopupGravity::Right));
        assert_eq!(PopupGravity::from_raw(9), Err(DecodeError::BadValue));
        assert_eq!(DragAction::from_raw(3), Ok(DragAction::Link));
        assert_eq!(DragAction::from_raw(4), Err(DecodeError::BadValue));
        assert_eq!(KeymapFormat::from_raw(1), Ok(KeymapFormat::XkbV1));
        assert_eq!(KeymapFormat::from_raw(0), Err(DecodeError::BadValue));
        assert_eq!(DataSource::from_raw(1), Ok(DataSource::Drag));
        assert_eq!(DataSource::from_raw(2), Err(DecodeError::BadValue));
    }

    #[test]
    fn the_m5_caps_are_new_bits() {
        let old = caps::DIRECT_SCANOUT
            | caps::TEXT
            | caps::DMABUF
            | caps::REMOTE
            | caps::WM
            | caps::SHELL
            | caps::THEME
            | caps::ICONS;
        assert_eq!(old, 0xff);
        assert_eq!(caps::CAPS_M5_MASK & old, 0);
        assert_eq!(caps::POPUP, 1 << 8);
        assert_eq!(caps::CURSOR, 1 << 9);
        assert_eq!(caps::DRAG, 1 << 10);
        assert_eq!(caps::OUTPUTS, 1 << 11);
        assert_eq!(caps::KEYMAP, 1 << 12);
        assert_eq!(caps::RELEASE, 1 << 13);
        assert_eq!(caps::DATA, 1 << 14);
        assert_eq!(caps::CAPS_M5_MASK, 0x7f00);
        // Every M5 bit is in the mask, and nothing else is.
        for bit in [
            caps::POPUP,
            caps::CURSOR,
            caps::DRAG,
            caps::OUTPUTS,
            caps::KEYMAP,
            caps::RELEASE,
            caps::DATA,
        ] {
            assert_eq!(caps::CAPS_M5_MASK & bit, bit);
        }
    }

    #[test]
    fn the_m5_bitmasks_are_disjoint_and_complete() {
        assert_eq!(constraint_adjust::ALL, 0b11_1111);
        assert_eq!(constraint_adjust::RESIZE_Y, 32);
        assert_eq!(popup_flags::ALL, 0b1);
        // Same bit positions as `anchor`, deliberately.
        assert_eq!(resize_edges::TOP, anchor::TOP);
        assert_eq!(resize_edges::BOTTOM, anchor::BOTTOM);
        assert_eq!(resize_edges::LEFT, anchor::LEFT);
        assert_eq!(resize_edges::RIGHT, anchor::RIGHT);
        assert_eq!(resize_edges::ALL, 0b1111);
        assert_eq!(drag_actions::ALL, 0b111);
    }

    #[test]
    fn ids_and_formats() {
        assert!(WindowRef::NONE.is_none());
        assert!(!WindowRef(7).is_none());
        assert_eq!(WindowRef(7).raw(), 7);
        assert!(NodeId::NONE.is_none());
        assert!(!NodeId(1).is_none());
        assert!(BufferId::default().is_none());
        assert_eq!(format::XR24, 0x3432_5258);
        assert_eq!(format::AR24, 0x3432_5241);
        assert_eq!(caps::WM, 0x10);
        assert_eq!(caps::SHELL, 0x20);
        assert_eq!(caps::THEME, 0x40);
        assert_eq!(caps::ICONS, 0x80);
        // A new bit, not a reuse of any taken one.
        assert_eq!(
            caps::THEME
                & (caps::DIRECT_SCANOUT
                    | caps::TEXT
                    | caps::DMABUF
                    | caps::REMOTE
                    | caps::WM
                    | caps::SHELL),
            0
        );
        assert_eq!(
            caps::ICONS
                & (caps::DIRECT_SCANOUT
                    | caps::TEXT
                    | caps::DMABUF
                    | caps::REMOTE
                    | caps::WM
                    | caps::SHELL
                    | caps::THEME),
            0
        );
        // The shell bit is a *new* bit, not a reuse of `REMOTE`.
        assert_eq!(caps::SHELL & caps::REMOTE, 0);
        assert_eq!(mod_mask::ALL, 0b1111);
        assert_eq!(anchor::ALL, 0b1111);
        assert_eq!(
            window_flags::UNDECORATED | window_flags::FIXED_SIZE | window_flags::NO_FOCUS,
            0b111
        );
    }
}

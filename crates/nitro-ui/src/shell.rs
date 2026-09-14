//! Shell surfaces: what a bar, a dock, a launcher or a wallpaper needs
//! from the toolkit that an ordinary app does not.
//!
//! An app on nitro is a widget tree in one window, and the toolkit is
//! deliberately ignorant of where that window goes — the server decides.
//! A shell surface is the exception the server itself defines: it names
//! its own layer, sticks to its output's edges, and reserves screen space
//! the rest of the desktop may not use. `docs/shell.md` is the model;
//! this module is the part of it a client expresses.
//!
//! Three decisions are worth stating, because each is the reason the
//! bar's code is as short as it is.
//!
//! **The surface is described up front, not configured afterwards.**
//! [`Surface`] is handed to [`App::shell`](crate::App::shell) before the
//! window exists, so the layer, the flags, the anchor and the zone all
//! ride the **same commit** as the `CreateWindow`. The server buffers
//! `SetAnchor` and `SetExclusiveZone` to the sender's commit for exactly
//! this reason (it is the M3-B hardware probe's regression: an anchor
//! applied on receipt names a window the commit has not created yet), and
//! a bar that anchored a frame later would paint once at its placeholder
//! size and then jump.
//!
//! **The events are a separate hook from the widget events.** A
//! `WindowInfo` is not an input event and has no widget to be routed to:
//! it is news about somebody else's window. [`Ui::on_shell`] registers a
//! handler that is offered them with the same `(&mut S, &mut Ui<S>)` a
//! button's callback gets, so a bar's window list is rebuilt by ordinary
//! tree code.
//!
//! **Nothing here polls.** `WindowList` and `Outputs` subscribe, so the
//! shell is told when something changes and sits in `epoll_wait`
//! otherwise. That is what lets the bar keep the toolkit's idle contract:
//! with nothing changing, zero bytes move.

use nitro_wire::types::{anchor, window_flags};

pub use nitro_wire::msg::{OutputInfo, WindowInfo};
/// Modifier bits for [`Ui::bind_key`](crate::Ui::bind_key), by **name**.
///
/// Re-exported here so a shell does not have to name the wire crate for
/// the one constant it needs. Deliberately *not* the xkb mask a
/// [`KeyEvent`](crate::KeyEvent) carries — see `Ui::bind_key`.
pub use nitro_wire::types::mod_mask;
pub use nitro_wire::types::{Edge, Layer, WindowRef, WindowState};

/// Which edges a surface sticks to, and how far from them.
///
/// Opposite edges together mean "span that axis", so a bar is
/// `TOP | LEFT | RIGHT`; neither edge on an axis means "centre on it",
/// which is what a launcher overlay wants. The bits are
/// [`nitro_wire::types::anchor`]'s, re-exported through the constructors
/// below so an app does not have to name the wire crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// Edge bitmask.
    pub edges: u8,
    /// Gap in logical pixels on each anchored edge.
    pub margin: u32,
}

impl Anchor {
    /// Span the top edge: a bar.
    #[must_use]
    pub const fn top() -> Self {
        Self {
            edges: anchor::TOP | anchor::LEFT | anchor::RIGHT,
            margin: 0,
        }
    }

    /// Span the bottom edge: a dock.
    #[must_use]
    pub const fn bottom() -> Self {
        Self {
            edges: anchor::BOTTOM | anchor::LEFT | anchor::RIGHT,
            margin: 0,
        }
    }

    /// Cover the whole output: a wallpaper.
    #[must_use]
    pub const fn fill() -> Self {
        Self {
            edges: anchor::ALL,
            margin: 0,
        }
    }

    /// Centred on both axes: a launcher.
    #[must_use]
    pub const fn centre() -> Self {
        Self {
            edges: 0,
            margin: 0,
        }
    }

    /// The same anchor, inset by `px` on each anchored edge.
    #[must_use]
    pub const fn margin(mut self, px: u32) -> Self {
        self.margin = px;
        self
    }
}

/// A shell surface: the window a bar, dock, launcher or wallpaper opens.
///
/// Built with one of the constructors and handed to
/// [`App::shell`](crate::App::shell). Everything in it is applied in the
/// window's own first commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    /// Which layer the window lives on.
    pub layer: Layer,
    /// `CreateWindow` flags, from [`nitro_wire::types::window_flags`].
    pub flags: u32,
    /// Where it sticks, if anywhere.
    pub anchor: Option<Anchor>,
    /// Screen space it reserves: the edge and how many logical pixels.
    pub zone: Option<(Edge, u32)>,
}

impl Surface {
    /// A bar: `Top` layer, undecorated, never focused, spanning the top
    /// edge and reserving `height` pixels there.
    ///
    /// `NO_FOCUS` is not optional for a panel. A bar that took focus
    /// would make the window behind it look inactive and would move the
    /// MRU order every time the user glanced at the clock; it reads the
    /// keyboard, when it needs to, through a hotkey or a grab instead.
    #[must_use]
    pub const fn bar(height: u32) -> Self {
        Self {
            layer: Layer::Top,
            flags: window_flags::UNDECORATED | window_flags::NO_FOCUS,
            anchor: Some(Anchor::top()),
            zone: Some((Edge::Top, height)),
        }
    }

    /// A dock: as [`Surface::bar`], on the bottom edge.
    #[must_use]
    pub const fn dock(height: u32) -> Self {
        Self {
            layer: Layer::Top,
            flags: window_flags::UNDECORATED | window_flags::NO_FOCUS,
            anchor: Some(Anchor::bottom()),
            zone: Some((Edge::Bottom, height)),
        }
    }

    /// A launcher: `Overlay`, undecorated, `NO_FOCUS`, centred, and
    /// reserving nothing — it is on top of the desktop, not part of it.
    #[must_use]
    pub const fn overlay() -> Self {
        Self {
            layer: Layer::Overlay,
            flags: window_flags::UNDECORATED | window_flags::NO_FOCUS,
            anchor: Some(Anchor::centre()),
            zone: None,
        }
    }

    /// A wallpaper: `Background`, covering the output, reserving nothing.
    #[must_use]
    pub const fn wallpaper() -> Self {
        Self {
            layer: Layer::Background,
            flags: window_flags::UNDECORATED | window_flags::NO_FOCUS,
            anchor: Some(Anchor::fill()),
            zone: None,
        }
    }

    /// Replace the anchor.
    #[must_use]
    pub const fn anchored(mut self, anchor: Anchor) -> Self {
        self.anchor = Some(anchor);
        self
    }

    /// Replace the exclusive zone.
    #[must_use]
    pub const fn reserving(mut self, edge: Edge, px: u32) -> Self {
        self.zone = Some((edge, px));
        self
    }

    /// Reserve nothing: the surface floats over the work area.
    #[must_use]
    pub const fn reserving_nothing(mut self) -> Self {
        self.zone = None;
        self
    }
}

/// News from the shell socket, as [`Ui::on_shell`] delivers it.
///
/// These are not input events and are not routed to a widget: they are
/// facts about the desktop — somebody else's window, an output, a hotkey
/// nobody else may see. A shell turns them into tree edits itself.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellEvent {
    /// A window appeared, or something about it changed: its title, its
    /// app id, its state, its output, or whether it has focus.
    ///
    /// There is no separate "added" event on purpose. The server sends
    /// the same message for the snapshot and for every later change, so a
    /// shell that keyed on "added" would have two code paths that must
    /// agree; keying on the id and upserting has one.
    Window(WindowInfo),
    /// A window is gone. Its [`WindowRef`] is retired and will never name
    /// another window.
    WindowGone(WindowRef),
    /// The end of the [`WindowList`](nitro_wire::msg::WindowList)
    /// snapshot: every window the server knew about has been sent.
    WindowListEnd,
    /// An output appeared or changed. A hotplug re-sends the whole list.
    Output(OutputInfo),
    /// An output was unplugged, by server output id.
    OutputGone(u32),
    /// The end of the [`Outputs`](nitro_wire::msg::Outputs) snapshot.
    OutputsEnd,
    /// A bound hotkey fired. `pressed` is false on the release — and a
    /// bare-modifier tap arrives **once**, with `pressed: false`, because
    /// until the release the server cannot know it was a tap rather than
    /// the start of a chord.
    HotKey {
        /// The client's own id, from
        /// [`Ui::bind_key`](crate::Ui::bind_key).
        id: u32,
        /// Whether this is the press.
        pressed: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bar_spans_the_top_and_reserves_its_own_height() {
        let s = Surface::bar(32);
        assert_eq!(s.layer, Layer::Top);
        assert_eq!(s.zone, Some((Edge::Top, 32)));
        let a = s.anchor.expect("a bar is anchored");
        assert_eq!(a.edges, anchor::TOP | anchor::LEFT | anchor::RIGHT);
        // Opposite edges on the horizontal axis is what makes it span;
        // one edge on the vertical is what keeps it 32 px tall.
        assert_ne!(a.edges & anchor::BOTTOM, anchor::BOTTOM);
    }

    #[test]
    fn a_panel_never_takes_focus() {
        // The whole point of NO_FOCUS on a panel: glancing at the clock
        // must not deactivate the window behind it.
        for s in [Surface::bar(32), Surface::dock(48), Surface::overlay()] {
            assert_eq!(s.flags & window_flags::NO_FOCUS, window_flags::NO_FOCUS);
            assert_eq!(
                s.flags & window_flags::UNDECORATED,
                window_flags::UNDECORATED
            );
        }
    }

    #[test]
    fn an_overlay_and_a_wallpaper_reserve_nothing() {
        // A launcher that reserved space would shrink the desktop every
        // time it opened; a wallpaper *is* the desktop.
        assert_eq!(Surface::overlay().zone, None);
        assert_eq!(Surface::wallpaper().zone, None);
        assert_eq!(Surface::wallpaper().layer, Layer::Background);
        assert_eq!(Surface::overlay().layer, Layer::Overlay);
    }

    #[test]
    fn a_centred_anchor_names_no_edge_at_all() {
        // "Centre on this axis" is the absence of both edges, not a flag:
        // that is how the server's `anchor_rect` reads it.
        assert_eq!(Anchor::centre().edges, 0);
        assert_eq!(Anchor::fill().edges, anchor::ALL);
    }

    #[test]
    fn a_margin_insets_without_changing_the_edges() {
        let a = Anchor::top().margin(4);
        assert_eq!(a.margin, 4);
        assert_eq!(a.edges, Anchor::top().edges);
    }
}

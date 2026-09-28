//! Popups: a menu hung off a rectangle of one of this app's windows.
//!
//! The server already does everything a menu needs (M5-G, `docs/wm.md`
//! §Popups): it places the popup against an anchor rectangle of its
//! parent, constrained to the work area; with [`PopupPlacement::grab`] a
//! press outside the chain **dismisses it and is consumed**, and so is
//! Escape; and a parent that is hidden, unplugged or closed takes its
//! popups with it. The toolkit's half is small: [`Ui::add_popup`] opens
//! a window with `CreatePopup` instead of `CreateWindow`, and the
//! server's `PopupDone` tears it down exactly as a `Closed` tears down a
//! secondary window, so [`Ui::on_window_closed`] is the app's "menu
//! closed" hook whoever closed it.
//!
//! Popups are **fixed-size**: `RepositionPopup` moves one but cannot
//! resize it. A menu whose content changes height (a drill-down view)
//! opens the new popup and removes the old one in the **same commit** —
//! one transaction, so the swap never shows an empty frame.
//!
//! [`Ui::add_popup`]: crate::Ui::add_popup
//! [`Ui::on_window_closed`]: crate::Ui::on_window_closed

use nitro_core::Rect;

pub use nitro_wire::types::{PopupAnchor, PopupGravity, constraint_adjust};

/// Where a popup goes relative to its parent, and whether it grabs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupPlacement {
    /// The rectangle to hang off, in the **parent window's** coordinates
    /// ([`Ui::bounds`](crate::Ui::bounds) of the widget that opened it).
    /// Rounded out to whole logical pixels on the wire.
    pub anchor_rect: Rect,
    /// Which point of `anchor_rect` the popup hangs off.
    pub anchor: PopupAnchor,
    /// Which way it grows from that point.
    pub gravity: PopupGravity,
    /// What the server may do when it does not fit: a
    /// [`constraint_adjust`] bitmask.
    pub constraint: u32,
    /// Take the pointer grab: an outside press (and Escape) dismisses.
    pub grab: bool,
}

impl PopupPlacement {
    /// A drop-down under `rect`, right edges aligned: the popup's
    /// top-right corner at the rectangle's bottom-right, growing down and
    /// to the left. It slides sideways to stay on the output, flips above
    /// if there is no room below, and shrinks as a last resort. Grabs.
    ///
    /// That is where a status-area menu belongs: under its button, flush
    /// with the right edge of the screen when the button is.
    #[must_use]
    pub const fn below(rect: Rect) -> Self {
        Self {
            anchor_rect: rect,
            anchor: PopupAnchor::BottomRight,
            gravity: PopupGravity::BottomLeft,
            constraint: constraint_adjust::SLIDE_X
                | constraint_adjust::FLIP_Y
                | constraint_adjust::RESIZE_Y,
            grab: true,
        }
    }

    /// A drop-down under `rect`, left edges aligned (a menu bar's menu).
    #[must_use]
    pub const fn below_left(rect: Rect) -> Self {
        Self {
            anchor: PopupAnchor::BottomLeft,
            gravity: PopupGravity::BottomRight,
            ..Self::below(rect)
        }
    }

    /// Set whether the popup takes the pointer grab.
    #[must_use]
    pub const fn grab(mut self, on: bool) -> Self {
        self.grab = on;
        self
    }
}

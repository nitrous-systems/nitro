//! Input events, as a widget sees them.
//!
//! The app loop turns `ServerMsg`s into these: pointer positions are
//! already **widget-local**, keys carry the keysym and the text they
//! produced, and the window-level bookkeeping (which window, which node,
//! timestamps) has been stripped. A widget that handles `Event` handles
//! the same thing whether it was driven by a real mouse or by
//! [`Harness::click`](crate::test::Harness::click).

use nitro_core::Point;

/// Whether a widget consumed an event.
///
/// Pointer events are offered to the deepest widget under the pointer
/// first and bubble up to the root until one answers [`Handled::Yes`];
/// key events start at the focused widget and bubble the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// Stop here.
    Yes,
    /// Offer the event to the parent.
    No,
}

impl Handled {
    /// Whether this is [`Handled::Yes`].
    #[must_use]
    pub fn is_handled(self) -> bool {
        self == Handled::Yes
    }
}

impl From<bool> for Handled {
    fn from(v: bool) -> Self {
        if v { Handled::Yes } else { Handled::No }
    }
}

/// Which pointer button; evdev codes, as the wire reports them.
pub mod button {
    /// Left mouse button.
    pub const LEFT: u32 = 0x110;
    /// Right mouse button.
    pub const RIGHT: u32 = 0x111;
    /// Middle mouse button.
    pub const MIDDLE: u32 = 0x112;
}

/// Evdev keycodes the toolkit itself acts on.
pub mod key {
    /// `Enter`/`Return`.
    pub const ENTER: u32 = 28;
    /// `Space`.
    pub const SPACE: u32 = 57;
    /// `Tab`.
    pub const TAB: u32 = 15;
    /// `Escape`.
    pub const ESC: u32 = 1;
    /// Left shift.
    pub const LEFT_SHIFT: u32 = 42;
    /// Right shift.
    pub const RIGHT_SHIFT: u32 = 54;
    /// Left control.
    pub const LEFT_CTRL: u32 = 29;
    /// `Backspace`.
    pub const BACKSPACE: u32 = 14;
    /// `Delete`.
    pub const DELETE: u32 = 111;
    /// Left arrow.
    pub const LEFT: u32 = 105;
    /// Right arrow.
    pub const RIGHT: u32 = 106;
    /// Up arrow.
    pub const UP: u32 = 103;
    /// Down arrow.
    pub const DOWN: u32 = 108;
    /// `Home`.
    pub const HOME: u32 = 102;
    /// `End`.
    pub const END: u32 = 107;
    /// `Page Up`.
    pub const PAGE_UP: u32 = 104;
    /// `Page Down`.
    pub const PAGE_DOWN: u32 = 109;
    /// The letter `a`, for `Ctrl-A`.
    pub const A: u32 = 30;
    /// The letter `c`, for `Ctrl-C`.
    pub const C: u32 = 46;
    /// The letter `h`, for `Ctrl-H`.
    pub const H: u32 = 35;
    /// The letter `n`, for `Ctrl-N` and for answering "no".
    pub const N: u32 = 49;
    /// The letter `s`, for `Ctrl-S`.
    pub const S: u32 = 31;
    /// The letter `v`, for `Ctrl-V`.
    pub const V: u32 = 47;
    /// The letter `y`, for answering "yes".
    pub const Y: u32 = 21;
    /// The letter `q`, for the conventional quit shortcut.
    pub const Q: u32 = 16;
    /// `F2`, which every file manager since the Norton Commander has
    /// used to rename.
    pub const F2: u32 = 60;
}

/// Modifier bits of an xkb modifier mask, for [`KeyEvent::mods`] and
/// [`Ui::set_shortcut`](crate::Ui::set_shortcut).
///
/// The numbers are the canonical xkb modifier list (`Shift`, `Lock`,
/// `Control`, `Mod1`…), which every layout shares — the same reasoning
/// as [`KeyEvent::shift`].
pub mod mods {
    /// No modifier at all.
    pub const NONE: u32 = 0;
    /// Either shift key.
    pub const SHIFT: u32 = 1;
    /// Either control key.
    pub const CTRL: u32 = 4;
    /// Either alt key (`Mod1`).
    pub const ALT: u32 = 8;
    /// The logo/super key (`Mod4`).
    pub const LOGO: u32 = 64;
    /// The bits a shortcut is compared on.
    ///
    /// Everything else — `Lock`, `NumLock`, the layout's own group
    /// modifiers — is masked out, so a shortcut does not stop working
    /// because Caps Lock is on.
    pub const MASK: u32 = SHIFT | CTRL | ALT | LOGO;
}

/// One input event.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The pointer entered this widget's bounds.
    PointerEnter {
        /// Position in the widget's own coordinate space.
        pos: Point,
    },
    /// The pointer left this widget's bounds.
    PointerLeave,
    /// The pointer moved inside this widget.
    PointerMove {
        /// Position in the widget's own coordinate space.
        pos: Point,
    },
    /// A pointer button went down over this widget.
    PointerDown {
        /// Position in the widget's own coordinate space.
        pos: Point,
        /// Evdev button code; see [`button`].
        button: u32,
    },
    /// A pointer button came up.
    PointerUp {
        /// Position in the widget's own coordinate space.
        pos: Point,
        /// Evdev button code; see [`button`].
        button: u32,
    },
    /// Scrolling over this widget, in logical pixels.
    Scroll {
        /// Horizontal delta.
        dx: f32,
        /// Vertical delta.
        dy: f32,
    },
    /// A key went down while this widget had focus.
    KeyDown(KeyEvent),
    /// A key came up.
    KeyUp(KeyEvent),
    /// Text was typed; one event per key that produced characters.
    Text {
        /// The characters, UTF-8.
        text: String,
    },
    /// This widget gained or lost keyboard focus.
    FocusChanged {
        /// Whether it now has focus.
        focused: bool,
    },
}

/// A key press or release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// Linux evdev keycode (`KEY_*`); see [`key`].
    pub keycode: u32,
    /// Resolved keysym, or 0 when the server has no keymap.
    pub keysym: u32,
    /// Active xkb modifier mask.
    pub mods: u32,
    /// Text the key produced; empty for non-printing keys.
    pub text: String,
}

impl KeyEvent {
    /// Whether a shift modifier was held. Bit 0 of an xkb modifier mask is
    /// `Shift` in every layout, because it is the first entry of the
    /// canonical modifier list.
    #[must_use]
    pub fn shift(&self) -> bool {
        self.mods & 1 != 0
    }

    /// Whether a control modifier was held.
    ///
    /// Bit 2 is `Control` in the canonical xkb modifier list (`Shift`,
    /// `Lock`, `Control`, `Mod1`…), the same reasoning as
    /// [`KeyEvent::shift`].
    #[must_use]
    pub fn ctrl(&self) -> bool {
        self.mods & 4 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handled_converts_from_bool() {
        assert_eq!(Handled::from(true), Handled::Yes);
        assert_eq!(Handled::from(false), Handled::No);
        assert!(Handled::Yes.is_handled());
        assert!(!Handled::No.is_handled());
    }

    #[test]
    fn shift_reads_bit_zero() {
        let k = KeyEvent {
            keycode: key::TAB,
            keysym: 0,
            mods: 1,
            text: String::new(),
        };
        assert!(k.shift());
        assert!(!k.ctrl());
        assert!(
            !KeyEvent {
                mods: 4,
                ..k.clone()
            }
            .shift()
        );
        assert!(
            KeyEvent {
                mods: 4,
                ..k.clone()
            }
            .ctrl()
        );
        assert!(KeyEvent { mods: 5, ..k }.ctrl());
    }
}

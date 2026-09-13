//! Keyboard translation: evdev keycodes in, keysyms and text out.
//!
//! The kernel hands us scancodes, not characters. Turning "key 30 went
//! down" into `a`, `A` or `ä` needs the user's layout, the modifier state
//! and the level rules that go with them, which is exactly what
//! xkbcommon already does for every other Linux compositor. We do not
//! reimplement it: the keymap is compiled once from the `XKB_DEFAULT_*`
//! environment variables (the same ones libinput-based compositors and
//! `setxkbmap` write), and one [`xkb::State`] is fed every press and
//! release.
//!
//! Two conversions live in here so callers never have to think about
//! them. First, keycodes: X11 reserved codes 0..7, so an XKB keymap
//! numbers keys eight higher than the evdev codes libinput reports. Every
//! function on [`Keyboard`] takes the evdev code and adds the `+ 8` before
//! touching xkb. Second, ordering: a key event is resolved against the
//! state *before* that event is applied, which is what xkbcommon's own
//! documentation calls the conventional behaviour, so that pressing Shift
//! does not retroactively shift itself.
//!
//! Compilation can fail — a machine with no `xkeyboard-config` data has no
//! keymap to compile — so [`Keyboard::new`] returns `None` rather than
//! aborting. The server then logs a warning and runs without keyboard
//! translation: pointers, outputs and clients all still work, only text
//! and hotkeys are lost. That is a much better failure mode for a display
//! server than refusing to start.

use xkbcommon::xkb;

/// Keyboard state: the compiled keymap plus the live modifier/level state.
pub struct Keyboard {
    /// Kept alongside the state so [`Keyboard::reset`] can rebuild the
    /// state and [`Keyboard::layout_names`] can name the layouts.
    keymap: xkb::Keymap,
    state: xkb::State,
}

/// What one key event resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyResolution {
    /// X11-style keysym, or 0 when the key has none.
    pub keysym: u32,
    /// The text the key produced; empty for non-printing keys.
    pub utf8: String,
    /// Effective modifier mask, as xkb serializes it.
    pub mods: u32,
    /// Whether Ctrl and Alt are both down (the compositor's escape hatch).
    pub ctrl_alt: bool,
}

impl KeyResolution {
    /// What a key resolves to when no keymap compiled: the evdev code
    /// still reaches the client, with no keysym, no text and no
    /// modifiers, so a client that only wants raw keys keeps working.
    #[must_use]
    pub fn none() -> Self {
        Self {
            keysym: 0,
            utf8: String::new(),
            mods: 0,
            ctrl_alt: false,
        }
    }
}

/// X11 keycodes start at 8, so an XKB keymap's code for a key is the
/// evdev code plus this offset. Applied at every xkb boundary below.
const EVDEV_OFFSET: u32 = 8;

/// Layout used when the environment names no layout, or names one that
/// does not compile. `us` is the layout every `xkeyboard-config` install
/// ships, so it is the one fallback most likely to succeed.
const FALLBACK_LAYOUT: &str = "us";

impl Keyboard {
    /// Compile the default keymap from `XKB_DEFAULT_{RULES,MODEL,LAYOUT,VARIANT,OPTIONS}`,
    /// falling back to the `us` layout. Returns None when compilation fails
    /// (the caller logs and runs without keyboard translation).
    pub fn new() -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        // An empty name means "take the context default", which
        // libxkbcommon reads from the matching `XKB_DEFAULT_*` variable.
        // We read them ourselves only so the fallback can replace the
        // layout alone and keep the user's rules, model and options.
        let rules = env_name("XKB_DEFAULT_RULES");
        let model = env_name("XKB_DEFAULT_MODEL");
        let layout = env_name("XKB_DEFAULT_LAYOUT");
        let variant = env_name("XKB_DEFAULT_VARIANT");
        let options = std::env::var("XKB_DEFAULT_OPTIONS").ok();

        let keymap = compile(&context, &rules, &model, &layout, &variant, options.clone())
            .or_else(|| {
                // The configured layout is broken or missing: retry with a
                // layout that is always present, dropping the variant
                // (a variant of `us` need not exist either).
                compile(
                    &context,
                    &rules,
                    &model,
                    FALLBACK_LAYOUT,
                    "",
                    options.clone(),
                )
            })
            .or_else(|| {
                // Still nothing: the options may be at fault (a typo in
                // `terminate:ctrl_alt_bksp`, say). Last try, bare `us`.
                compile(&context, "", "", FALLBACK_LAYOUT, "", None)
            })?;

        let state = xkb::State::new(&keymap);
        Some(Self { keymap, state })
    }

    /// The layout names the keymap compiled to, for the log line.
    pub fn layout_names(&self) -> Vec<String> {
        self.keymap.layouts().map(ToOwned::to_owned).collect()
    }

    /// Feed one evdev keycode (as libinput reports it, i.e. WITHOUT the +8
    /// X11 offset) and its press/release state; returns what it resolved to.
    /// Updates the modifier state.
    pub fn key(&mut self, evdev_keycode: u32, pressed: bool) -> KeyResolution {
        let code = keycode(evdev_keycode);

        // Resolve first, update second, for both directions. xkbcommon
        // documents this order for `xkb_state_update_key`: the keysyms
        // reported for an event should not be affected by the event
        // itself, so pressing Shift reports `Shift_L` rather than the
        // shifted level of Shift, and releasing a key reports what that
        // key had been producing while it was held.
        let keysym = self.state.key_get_one_sym(code).raw();
        // A release produces no text, whatever level it sat at.
        let utf8 = if pressed {
            self.state.key_get_utf8(code)
        } else {
            String::new()
        };

        let direction = if pressed {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        self.state.update_key(code, direction);

        // The modifier fields, by contrast, describe the state *after* the
        // event: a Ctrl press must report Ctrl as held, otherwise the
        // Ctrl+Alt escape hatch would lag a keystroke behind.
        let mods = self.state.serialize_mods(xkb::STATE_MODS_EFFECTIVE);
        let ctrl_alt = self
            .state
            .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE)
            && self
                .state
                .mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE);

        KeyResolution {
            keysym,
            utf8,
            mods,
            ctrl_alt,
        }
    }

    /// The keysym a keycode currently resolves to, without changing state.
    pub fn peek(&self, evdev_keycode: u32) -> u32 {
        self.state.key_get_one_sym(keycode(evdev_keycode)).raw()
    }

    /// Reset the state (after a VT switch, where key releases were missed).
    pub fn reset(&mut self) {
        // Cheaper and more reliable than replaying the releases we never
        // saw: a fresh state has every modifier up and every level at
        // rest, which is precisely the state of a keyboard we have just
        // regained.
        self.state = xkb::State::new(&self.keymap);
    }
}

/// Read one `XKB_DEFAULT_*` variable, mapping unset (and empty) to the
/// empty string, which tells libxkbcommon to use its own default.
fn env_name(var: &str) -> String {
    std::env::var(var).unwrap_or_default()
}

/// Compile one RMLVO combination, or `None` when xkb rejects it.
fn compile(
    context: &xkb::Context,
    rules: &str,
    model: &str,
    layout: &str,
    variant: &str,
    options: Option<String>,
) -> Option<xkb::Keymap> {
    xkb::Keymap::new_from_names(
        context,
        rules,
        model,
        layout,
        variant,
        options,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
}

/// Translate an evdev keycode into the keymap's keycode space.
///
/// Saturating rather than wrapping: a bogus `u32::MAX` from a broken
/// device should stay an invalid keycode, not wrap round to a valid one.
fn keycode(evdev_keycode: u32) -> xkb::Keycode {
    xkb::Keycode::new(evdev_keycode.saturating_add(EVDEV_OFFSET))
}

/// The compositor hotkeys the server recognises, resolved from a keysym.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// Ctrl+Alt+Backspace: quit the server (development safety valve).
    Quit,
    /// Ctrl+Alt+F1..F12: switch to that VT.
    SwitchVt(i32),
}

/// Map a keysym to a compositor hotkey, given Ctrl+Alt are both held.
/// Returns None for anything else. A pure function — unit-test it.
#[must_use]
pub fn hotkey(keysym: u32, ctrl_alt: bool) -> Option<Hotkey> {
    if !ctrl_alt {
        return None;
    }
    // `terminate:ctrl_alt_bksp` rewrites the chord to `Terminate_Server`
    // in the keymap itself, so the same physical keys arrive as one
    // keysym or the other depending on the user's xkb options.
    if keysym == xkb::keysyms::KEY_BackSpace || keysym == xkb::keysyms::KEY_Terminate_Server {
        return Some(Hotkey::Quit);
    }
    // F1..F12 are consecutive keysyms, so the VT number is an offset.
    if (xkb::keysyms::KEY_F1..=xkb::keysyms::KEY_F12).contains(&keysym) {
        // The subtraction cannot underflow inside the range, and the
        // result is 1..=12, so the conversion cannot fail either.
        let n = i32::try_from(keysym - xkb::keysyms::KEY_F1 + 1).ok()?;
        return Some(Hotkey::SwitchVt(n));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Hotkey, Keyboard, hotkey};
    use xkbcommon::xkb;

    /// evdev `KEY_A`, from `linux/input-event-codes.h`.
    const EVDEV_A: u32 = 30;
    /// evdev `KEY_LEFTSHIFT`.
    const EVDEV_LEFTSHIFT: u32 = 42;

    #[test]
    fn hotkey_quit() {
        assert_eq!(
            hotkey(xkb::keysyms::KEY_BackSpace, true),
            Some(Hotkey::Quit)
        );
        assert_eq!(
            hotkey(xkb::keysyms::KEY_Terminate_Server, true),
            Some(Hotkey::Quit)
        );
    }

    #[test]
    fn hotkey_vt_switch() {
        assert_eq!(
            hotkey(xkb::keysyms::KEY_F1, true),
            Some(Hotkey::SwitchVt(1))
        );
        assert_eq!(
            hotkey(xkb::keysyms::KEY_F12, true),
            Some(Hotkey::SwitchVt(12))
        );
    }

    #[test]
    fn hotkey_ignores_other_keysyms() {
        assert_eq!(hotkey(xkb::keysyms::KEY_a, true), None);
        assert_eq!(hotkey(xkb::keysyms::KEY_F12 + 1, true), None);
        assert_eq!(hotkey(0, true), None);
    }

    #[test]
    fn hotkey_needs_ctrl_alt() {
        assert_eq!(hotkey(xkb::keysyms::KEY_BackSpace, false), None);
        assert_eq!(hotkey(xkb::keysyms::KEY_F1, false), None);
    }

    /// Skipped, not failed, on a machine with no xkb keymap data.
    #[test]
    fn unshifted_a_produces_lowercase() {
        let Some(mut kb) = Keyboard::new() else {
            return;
        };
        let r = kb.key(EVDEV_A, true);
        assert_eq!(r.keysym, xkb::keysyms::KEY_a);
        assert_eq!(r.utf8, "a");
        assert!(!r.ctrl_alt);
        // A release carries the keysym but no text.
        let up = kb.key(EVDEV_A, false);
        assert_eq!(up.keysym, xkb::keysyms::KEY_a);
        assert!(up.utf8.is_empty());
    }

    #[test]
    fn shift_selects_the_upper_level() {
        let Some(mut kb) = Keyboard::new() else {
            return;
        };
        let shift = kb.key(EVDEV_LEFTSHIFT, true);
        // Shift resolves to itself, not to a shifted level of itself.
        assert_eq!(shift.keysym, xkb::keysyms::KEY_Shift_L);
        assert_ne!(shift.mods, 0, "shift must be effective after its press");

        let r = kb.key(EVDEV_A, true);
        assert_eq!(r.keysym, xkb::keysyms::KEY_A);
        assert_eq!(r.utf8, "A");
        assert_eq!(kb.peek(EVDEV_A), xkb::keysyms::KEY_A);

        kb.key(EVDEV_A, false);
        kb.key(EVDEV_LEFTSHIFT, false);
        assert_eq!(kb.peek(EVDEV_A), xkb::keysyms::KEY_a);
    }

    #[test]
    fn reset_drops_held_modifiers() {
        let Some(mut kb) = Keyboard::new() else {
            return;
        };
        kb.key(EVDEV_LEFTSHIFT, true);
        assert_eq!(kb.peek(EVDEV_A), xkb::keysyms::KEY_A);

        // The VT switch ate the Shift release.
        kb.reset();
        assert_eq!(kb.peek(EVDEV_A), xkb::keysyms::KEY_a);
        let r = kb.key(EVDEV_A, true);
        assert_eq!(r.utf8, "a");
        assert_eq!(r.mods, 0);
    }

    #[test]
    fn layouts_are_named() {
        let Some(kb) = Keyboard::new() else {
            return;
        };
        assert!(!kb.layout_names().is_empty());
    }
}

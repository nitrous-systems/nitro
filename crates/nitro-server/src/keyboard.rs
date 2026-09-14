//! Keyboard translation: evdev keycodes in, keysyms and text out.
//!
//! The kernel hands us scancodes, not characters. Turning "key 30 went
//! down" into `a`, `A` or `ä` needs the user's layout, the modifier state
//! and the level rules that go with them, which is exactly what
//! xkbcommon already does for every other Linux compositor. We do not
//! reimplement it: the keymap is compiled once from the `XKB_DEFAULT_*`
//! environment variables (the same ones libinput-based compositors and
//! `setxkbmap` write) with `server.conf`'s `keyboard.*` section filling in
//! what they leave unset ([`Keyboard::with_settings`]), and one
//! [`xkb::State`] is fed every press and release.
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

/// The modifier keys held when an event happened.
///
/// xkb's serialized mask is an opaque bitmap whose bit positions depend on
/// the keymap, so it cannot be compared against a constant; this is the
/// same information in a form the window manager can act on.
// Four independent modifier keys, not a state machine: any subset can be
// held at once, and naming the sixteen combinations would say less.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    /// Shift.
    pub shift: bool,
    /// Control.
    pub ctrl: bool,
    /// Alt (Mod1).
    pub alt: bool,
    /// Super / Logo / Windows (Mod4).
    pub logo: bool,
}

impl Mods {
    /// Whether Ctrl and Alt are both down (the compositor's escape hatch).
    #[must_use]
    pub fn ctrl_alt(self) -> bool {
        self.ctrl && self.alt
    }

    /// The same four modifiers as a protocol [`mod_mask`] bitmask.
    ///
    /// [`Mods`] is the compositor's shape and the mask is the wire's; a
    /// shell's [`BindKey`](nitro_wire::msg::BindKey) arrives as a mask and
    /// has to be compared against what a key event resolved to, so the
    /// conversion lives here rather than at the two call sites.
    ///
    /// [`mod_mask`]: nitro_wire::types::mod_mask
    #[must_use]
    pub fn mask(self) -> u32 {
        use nitro_wire::types::mod_mask;
        let mut m = 0;
        if self.shift {
            m |= mod_mask::SHIFT;
        }
        if self.ctrl {
            m |= mod_mask::CTRL;
        }
        if self.alt {
            m |= mod_mask::ALT;
        }
        if self.logo {
            m |= mod_mask::SUPER;
        }
        m
    }

    /// A [`mod_mask`](nitro_wire::types::mod_mask) bitmask as `Mods`.
    /// Unknown bits are ignored; the caller rejects them.
    #[must_use]
    pub fn from_mask(mask: u32) -> Self {
        use nitro_wire::types::mod_mask;
        Self {
            shift: mask & mod_mask::SHIFT != 0,
            ctrl: mask & mod_mask::CTRL != 0,
            alt: mask & mod_mask::ALT != 0,
            logo: mask & mod_mask::SUPER != 0,
        }
    }
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
    /// The named modifiers held, for the window manager's shortcuts.
    pub named: Mods,
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
            named: Mods::default(),
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
        Self::with_settings(&crate::config::KeyboardSettings::default())
    }

    /// The same, with `server.conf`'s `keyboard.*` section filling in what
    /// the environment does not say.
    ///
    /// Precedence is `XKB_DEFAULT_*` > file > `us`, which is the rule the
    /// whole configuration follows (`crates/nitro-server/src/config.rs`):
    /// the environment is the *development* channel and must not be
    /// silently overridden by the box's own config, and the file is the
    /// user's explicit answer where the environment says nothing.
    ///
    /// `Some("")` in the settings is not `None`: an explicit
    /// `keyboard.variant =` means "no variant", which is a different
    /// instruction from saying nothing about it.
    pub fn with_settings(settings: &crate::config::KeyboardSettings) -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        // An empty name means "take the context default", which
        // libxkbcommon reads from the matching `XKB_DEFAULT_*` variable.
        // We read them ourselves only so the fallback can replace the
        // layout alone and keep the user's rules, model and options — and,
        // since the file exists, so the file can fill in what is unset.
        let rules = env_name("XKB_DEFAULT_RULES");
        let model = env_name("XKB_DEFAULT_MODEL");
        let layout = env_or("XKB_DEFAULT_LAYOUT", settings.layout.as_deref());
        let variant = env_or("XKB_DEFAULT_VARIANT", settings.variant.as_deref());
        let options = std::env::var("XKB_DEFAULT_OPTIONS")
            .ok()
            .or_else(|| settings.options.clone());

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
        let named = self.named_mods();

        KeyResolution {
            keysym,
            utf8,
            mods,
            named,
            ctrl_alt: named.ctrl_alt(),
        }
    }

    /// The named modifiers currently held. Pointer events need them too,
    /// and they never pass through [`Keyboard::key`].
    #[must_use]
    pub fn named_mods(&self) -> Mods {
        let active = |name| {
            self.state
                .mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE)
        };
        Mods {
            shift: active(xkb::MOD_NAME_SHIFT),
            ctrl: active(xkb::MOD_NAME_CTRL),
            alt: active(xkb::MOD_NAME_ALT),
            logo: active(xkb::MOD_NAME_LOGO),
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

/// The same, with the configuration file's value as the middle step.
///
/// An *empty* environment variable counts as unset — that is how
/// libxkbcommon already reads it, and a `XKB_DEFAULT_VARIANT=` left over in
/// a session script should not veto the file. The empty string returned
/// when neither says anything is what tells xkb to use its own default.
fn env_or(var: &str, from_file: Option<&str>) -> String {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => v,
        _ => from_file.unwrap_or_default().to_owned(),
    }
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
    /// Alt+Tab / Alt+Shift+Tab: walk the MRU order. `true` = forward, to
    /// the less recently used.
    CycleFocus(bool),
    /// Super+Q: close the focused window.
    Close,
    /// Super+M: toggle maximize.
    ToggleMaximize,
    /// Super+F: toggle fullscreen.
    ToggleFullscreen,
    /// Super+H: minimize.
    Minimize,
    /// Super+Left / Super+Right: tile to that half of the work area.
    /// `true` = left.
    Tile(bool),
}

/// Map a keysym and the modifiers held to a compositor hotkey.
///
/// A pure function, and deliberately the *only* place the shortcut table
/// lives — unit-test it rather than an event loop.
///
/// The compositor's own chords are Ctrl+Alt (the VT and quit escape
/// hatches, which every Linux console user already knows), Alt+Tab (which
/// no application may have, because it is how you leave one) and Super
/// (which is reserved for the desktop by convention, so no client loses a
/// binding it could reasonably expect).
#[must_use]
pub fn hotkey(keysym: u32, mods: Mods) -> Option<Hotkey> {
    if mods.ctrl_alt() {
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
        return None;
    }
    // Alt+Tab, with or without Shift. With Shift held the keymap reports
    // `ISO_Left_Tab` rather than `Tab` on most layouts, so accept both and
    // let the Shift bit decide the direction.
    if mods.alt
        && !mods.logo
        && (keysym == xkb::keysyms::KEY_Tab || keysym == xkb::keysyms::KEY_ISO_Left_Tab)
    {
        return Some(Hotkey::CycleFocus(!mods.shift));
    }
    if !mods.logo || mods.alt || mods.ctrl {
        return None;
    }
    // Super chords. Matched on the *unshifted* letter too, because a
    // keymap may hand us either level depending on what else is held.
    match keysym {
        xkb::keysyms::KEY_q | xkb::keysyms::KEY_Q => Some(Hotkey::Close),
        xkb::keysyms::KEY_m | xkb::keysyms::KEY_M => Some(Hotkey::ToggleMaximize),
        xkb::keysyms::KEY_f | xkb::keysyms::KEY_F => Some(Hotkey::ToggleFullscreen),
        xkb::keysyms::KEY_h | xkb::keysyms::KEY_H => Some(Hotkey::Minimize),
        xkb::keysyms::KEY_Left => Some(Hotkey::Tile(true)),
        xkb::keysyms::KEY_Right => Some(Hotkey::Tile(false)),
        _ => None,
    }
}

/// Which [`mod_mask`](nitro_wire::types::mod_mask) bit a keysym *is*, if it
/// is a modifier key at all.
///
/// Needed for the bare-modifier tap a shell binds with `keysym: 0`: the
/// server has to recognise "the Super key itself went down" as distinct
/// from "a key went down with Super held", and the modifier *state* cannot
/// tell those apart.
#[must_use]
pub fn mod_of_keysym(keysym: u32) -> Option<u32> {
    use nitro_wire::types::mod_mask;
    match keysym {
        xkb::keysyms::KEY_Shift_L | xkb::keysyms::KEY_Shift_R => Some(mod_mask::SHIFT),
        xkb::keysyms::KEY_Control_L | xkb::keysyms::KEY_Control_R => Some(mod_mask::CTRL),
        xkb::keysyms::KEY_Alt_L | xkb::keysyms::KEY_Alt_R | xkb::keysyms::KEY_Meta_L => {
            Some(mod_mask::ALT)
        }
        xkb::keysyms::KEY_Super_L | xkb::keysyms::KEY_Super_R => Some(mod_mask::SUPER),
        _ => None,
    }
}

/// Whether a keysym is one of the `Alt` keys, which is what ends an
/// `Alt+Tab` cycle when released.
#[must_use]
pub fn is_alt(keysym: u32) -> bool {
    matches!(
        keysym,
        xkb::keysyms::KEY_Alt_L | xkb::keysyms::KEY_Alt_R | xkb::keysyms::KEY_Meta_L
    )
}

#[cfg(test)]
mod tests {
    use super::{Hotkey, Keyboard, Mods, hotkey, is_alt, mod_of_keysym};
    use xkbcommon::xkb;

    /// evdev `KEY_A`, from `linux/input-event-codes.h`.
    const EVDEV_A: u32 = 30;
    /// evdev `KEY_LEFTSHIFT`.
    const EVDEV_LEFTSHIFT: u32 = 42;

    const CTRL_ALT: Mods = Mods {
        shift: false,
        ctrl: true,
        alt: true,
        logo: false,
    };
    const LOGO: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        logo: true,
    };
    const ALT: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: true,
        logo: false,
    };

    #[test]
    fn hotkey_quit() {
        assert_eq!(
            hotkey(xkb::keysyms::KEY_BackSpace, CTRL_ALT),
            Some(Hotkey::Quit)
        );
        assert_eq!(
            hotkey(xkb::keysyms::KEY_Terminate_Server, CTRL_ALT),
            Some(Hotkey::Quit)
        );
    }

    #[test]
    fn hotkey_vt_switch() {
        assert_eq!(
            hotkey(xkb::keysyms::KEY_F1, CTRL_ALT),
            Some(Hotkey::SwitchVt(1))
        );
        assert_eq!(
            hotkey(xkb::keysyms::KEY_F12, CTRL_ALT),
            Some(Hotkey::SwitchVt(12))
        );
    }

    #[test]
    fn hotkey_ignores_other_keysyms() {
        assert_eq!(hotkey(xkb::keysyms::KEY_a, CTRL_ALT), None);
        assert_eq!(hotkey(xkb::keysyms::KEY_F12 + 1, CTRL_ALT), None);
        assert_eq!(hotkey(0, CTRL_ALT), None);
    }

    #[test]
    fn hotkey_needs_its_modifiers() {
        let none = Mods::default();
        assert_eq!(hotkey(xkb::keysyms::KEY_BackSpace, none), None);
        assert_eq!(hotkey(xkb::keysyms::KEY_F1, none), None);
        assert_eq!(hotkey(xkb::keysyms::KEY_q, none), None);
        assert_eq!(hotkey(xkb::keysyms::KEY_Tab, none), None);
        // Ctrl+Super is not a window-management chord: an application may
        // reasonably want it, and the Super table must not swallow it.
        let ctrl_logo = Mods { ctrl: true, ..LOGO };
        assert_eq!(hotkey(xkb::keysyms::KEY_q, ctrl_logo), None);
    }

    #[test]
    fn the_window_management_chords() {
        assert_eq!(hotkey(xkb::keysyms::KEY_q, LOGO), Some(Hotkey::Close));
        assert_eq!(
            hotkey(xkb::keysyms::KEY_M, LOGO),
            Some(Hotkey::ToggleMaximize),
            "the shifted level of the same key"
        );
        assert_eq!(
            hotkey(xkb::keysyms::KEY_f, LOGO),
            Some(Hotkey::ToggleFullscreen)
        );
        assert_eq!(hotkey(xkb::keysyms::KEY_h, LOGO), Some(Hotkey::Minimize));
        assert_eq!(
            hotkey(xkb::keysyms::KEY_Left, LOGO),
            Some(Hotkey::Tile(true))
        );
        assert_eq!(
            hotkey(xkb::keysyms::KEY_Right, LOGO),
            Some(Hotkey::Tile(false))
        );
        // Super+Return is *not* a compositor chord any more: the launcher
        // binds it through `BindKey`, and a chord the table still claimed
        // could never reach the shell.
        assert_eq!(hotkey(xkb::keysyms::KEY_Return, LOGO), None);
    }

    #[test]
    fn modifier_keysyms_name_their_mask_bit() {
        use nitro_wire::types::mod_mask;
        assert_eq!(
            mod_of_keysym(xkb::keysyms::KEY_Super_L),
            Some(mod_mask::SUPER)
        );
        assert_eq!(
            mod_of_keysym(xkb::keysyms::KEY_Super_R),
            Some(mod_mask::SUPER)
        );
        assert_eq!(mod_of_keysym(xkb::keysyms::KEY_Alt_L), Some(mod_mask::ALT));
        assert_eq!(
            mod_of_keysym(xkb::keysyms::KEY_Shift_R),
            Some(mod_mask::SHIFT)
        );
        // A plain letter is not a modifier, however it is held.
        assert_eq!(mod_of_keysym(xkb::keysyms::KEY_a), None);
        assert_eq!(mod_of_keysym(0), None);
    }

    #[test]
    fn mods_and_masks_round_trip() {
        use nitro_wire::types::mod_mask;
        assert_eq!(LOGO.mask(), mod_mask::SUPER);
        assert_eq!(CTRL_ALT.mask(), mod_mask::CTRL | mod_mask::ALT);
        assert_eq!(Mods::default().mask(), 0);
        for m in [LOGO, CTRL_ALT, ALT, Mods::default()] {
            assert_eq!(Mods::from_mask(m.mask()), m);
        }
        // Reserved bits are ignored here; `BindKey` rejects them.
        assert_eq!(Mods::from_mask(mod_mask::SUPER | 0x8000), LOGO);
    }

    #[test]
    fn alt_tab_cycles_both_ways() {
        assert_eq!(
            hotkey(xkb::keysyms::KEY_Tab, ALT),
            Some(Hotkey::CycleFocus(true))
        );
        let shift_alt = Mods { shift: true, ..ALT };
        assert_eq!(
            hotkey(xkb::keysyms::KEY_Tab, shift_alt),
            Some(Hotkey::CycleFocus(false))
        );
        // Shift+Tab usually arrives as ISO_Left_Tab instead.
        assert_eq!(
            hotkey(xkb::keysyms::KEY_ISO_Left_Tab, shift_alt),
            Some(Hotkey::CycleFocus(false))
        );
        assert!(is_alt(xkb::keysyms::KEY_Alt_L));
        assert!(!is_alt(xkb::keysyms::KEY_Tab));
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
        assert!(shift.named.shift);

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
        assert!(!r.named.shift);
    }

    #[test]
    fn layouts_are_named() {
        let Some(kb) = Keyboard::new() else {
            return;
        };
        assert!(!kb.layout_names().is_empty());
    }
}

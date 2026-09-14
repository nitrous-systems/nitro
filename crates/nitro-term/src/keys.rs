//! Turning a key press into the bytes a terminal expects.
//!
//! A terminal emulator's keyboard half is a translation, not a decision:
//! the far end of the pty is a program written against `xterm`, and the
//! only honest thing to send it is what `xterm` would have sent. So this
//! module is a pure function over a [`KeyEvent`] and the modes the host
//! terminal is currently in, with no I/O and no state — which is also
//! what makes it exhaustively testable, and why every rule below has a
//! test rather than a comment promising it works.
//!
//! # Text first, keycodes second
//!
//! The common path is [`KeyEvent::text`]: the compositor has already run
//! the key through the xkb keymap, so `text` is what the user's layout
//! says the key means, dead keys and `AltGr` and all. Encoding that as
//! UTF-8 is the whole of the printable case, and deriving anything from
//! keycodes there would mean re-implementing the keymap — badly, and only
//! for `us`.
//!
//! Keycodes are consulted for the keys that produce *no* text: the
//! arrows, the editing pad, the function row. Those have fixed escape
//! sequences that do not vary by layout, so a keycode is exactly the
//! right key to look them up by.
//!
//! `Ctrl` sits in between, and is taken from the produced text
//! (`Ctrl` + the character `c` is 0x03 whether `c` came from a `qwerty`
//! or a `dvorak` key), falling back to the keycode only for the one case
//! a keymap normally reports as empty text, `Ctrl+Space`.
//!
//! # What is deliberately not here
//!
//! `modifyOtherKeys`, `xterm`'s scheme for reporting `Ctrl+Shift+1` and
//! friends as `CSI 27 ; m ; c ~`, is not implemented: almost nothing
//! asks for it, and the applications that do ask first. The modified
//! *special* keys are implemented, because `Ctrl+Left` in a shell is a
//! word jump and people notice its absence immediately.

use nitro_ui::event::{KeyEvent, key, mods};

/// Evdev keycodes the toolkit does not name.
///
/// [`nitro_ui::event::key`] names the handful of keys a widget acts on;
/// a terminal needs the whole editing pad and function row. They are
/// kept private here rather than pushed into the toolkit because they
/// mean nothing to a button or a text field — the toolkit would be
/// carrying a terminal's vocabulary for one caller.
///
/// Values from `/usr/include/linux/input-event-codes.h` (`KEY_*`).
mod code {
    /// `KEY_F1` … `KEY_F10` are contiguous, 59 through 68.
    pub const F1: u32 = 59;
    /// `KEY_F2`.
    pub const F2: u32 = 60;
    /// `KEY_F3`.
    pub const F3: u32 = 61;
    /// `KEY_F4`.
    pub const F4: u32 = 62;
    /// `KEY_F5`.
    pub const F5: u32 = 63;
    /// `KEY_F6`.
    pub const F6: u32 = 64;
    /// `KEY_F7`.
    pub const F7: u32 = 65;
    /// `KEY_F8`.
    pub const F8: u32 = 66;
    /// `KEY_F9`.
    pub const F9: u32 = 67;
    /// `KEY_F10`.
    pub const F10: u32 = 68;
    /// `KEY_F11`, which is not next to `KEY_F10`.
    pub const F11: u32 = 87;
    /// `KEY_F12`.
    pub const F12: u32 = 88;
    /// `KEY_INSERT`.
    pub const INSERT: u32 = 110;
    /// `KEY_KPENTER`, the keypad's own `Enter`.
    pub const KP_ENTER: u32 = 96;
    /// `KEY_LEFTALT`.
    pub const LEFT_ALT: u32 = 56;
    /// `KEY_RIGHTALT`, which a layout may also use as `AltGr`.
    pub const RIGHT_ALT: u32 = 100;
    /// `KEY_RIGHTCTRL`.
    pub const RIGHT_CTRL: u32 = 97;
    /// `KEY_LEFTMETA`, the logo key.
    pub const LEFT_META: u32 = 125;
    /// `KEY_RIGHTMETA`.
    pub const RIGHT_META: u32 = 126;
    /// `KEY_CAPSLOCK`.
    pub const CAPS_LOCK: u32 = 58;
    /// `KEY_NUMLOCK`.
    pub const NUM_LOCK: u32 = 69;
    /// `KEY_SCROLLLOCK`.
    pub const SCROLL_LOCK: u32 = 70;
}

/// The escape character, which half of this module is made of.
const ESC: u8 = 0x1b;

/// What the terminal's modes affect in the encoding.
///
/// The host terminal's state leaks into the *keyboard* in exactly one
/// place in practice, so this is one flag rather than a copy of the DEC
/// mode table: everything else a mode changes is on the output side,
/// where [`crate::vt`] deals with it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Modes {
    /// DECSET 1: arrows and `Home`/`End` send SS3 (`ESC O A`) rather
    /// than CSI (`ESC [ A`).
    ///
    /// Set by full-screen programs and by `readline` when it wants the
    /// keypad; a shell that has just exited `vim` without resetting it
    /// is the usual reason a terminal's arrows "stop working", which is
    /// why this is a mode we follow rather than a preference we hold.
    pub application_cursor: bool,
}

/// The bytes an `xterm` would send for `key`, or `None` when the key
/// produces nothing.
///
/// `None` is the answer for a bare modifier (pressing `Ctrl` by itself
/// sends nothing, it only colours the next key) and for any key this
/// module has no mapping for and which produced no text. Callers pass
/// the result straight to [`crate::pty::Pty::write`]; `None` means "the
/// child hears nothing", not "an error".
#[must_use]
pub fn encode(key: &KeyEvent, modes: Modes) -> Option<Vec<u8>> {
    let m = key.mods & mods::MASK;
    let shift = m & mods::SHIFT != 0;
    let ctrl = m & mods::CTRL != 0;
    let alt = m & mods::ALT != 0;

    if is_bare_modifier(key.keycode) {
        return None;
    }

    // The keys with fixed sequences go first: their encoding is the same
    // whatever the layout says they produce, and some of them (`Enter`,
    // `Tab`, `Backspace`) do produce text we must not send instead.
    if let Some(bytes) = special(key.keycode, modes, shift, alt, ctrl) {
        return Some(bytes);
    }

    // `Ctrl` folds a character into the C0 range. Taken from the text so
    // it follows the keymap; a keymap that already folded it (some send
    // "\u{3}" for `Ctrl+C`) is passed through unchanged.
    let body = if ctrl {
        control_body(key)?
    } else if key.text.is_empty() {
        return None;
    } else {
        key.text.clone().into_bytes()
    };

    // "Meta sends escape": the convention every shell and editor reads,
    // and the reason `Alt+f` is a word jump in `readline`.
    if alt {
        let mut out = Vec::with_capacity(body.len() + 1);
        out.push(ESC);
        out.extend_from_slice(&body);
        return Some(out);
    }
    Some(body)
}

/// Wrap `text` in the bracketed-paste markers when the terminal asked
/// for them (DECSET 2004), else return it unchanged.
///
/// The markers are what let an editor tell a paste from typing, so that
/// pasted text is not auto-indented and a pasted newline does not run a
/// half-finished command. A terminal that sent them unasked would break
/// every program that has not opted in, so the flag is the host's, not
/// ours.
#[must_use]
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Whether this keycode is a modifier, which encodes to nothing on its
/// own. Includes the locks, which are modifiers the keymap applies and
/// the terminal never sees.
fn is_bare_modifier(keycode: u32) -> bool {
    matches!(
        keycode,
        key::LEFT_SHIFT
            | key::RIGHT_SHIFT
            | key::LEFT_CTRL
            | code::RIGHT_CTRL
            | code::LEFT_ALT
            | code::RIGHT_ALT
            | code::LEFT_META
            | code::RIGHT_META
            | code::CAPS_LOCK
            | code::NUM_LOCK
            | code::SCROLL_LOCK
    )
}

/// `Ctrl` + a character, as the single C0 byte it stands for.
///
/// The table is the ASCII one: the control character is the printable
/// character with bit 6 cleared, which is why `Ctrl+A` is 1 and
/// `Ctrl+[` is `ESC`. `Ctrl+Space` is the one entry that usually
/// arrives as empty text, so it is recovered from the keycode.
fn control_body(key: &KeyEvent) -> Option<Vec<u8>> {
    let mut chars = key.text.chars();
    match (chars.next(), chars.next()) {
        // A keymap that has already folded the key: take its word for it.
        (Some(c), None) if (c as u32) < 0x20 || c == '\x7f' => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        (Some(c), None) => ctrl_byte(c).map(|b| vec![b]),
        // No text: only `Ctrl+Space` has a conventional answer, and it
        // is the useful one (`NUL`, which `readline` binds to `set-mark`).
        _ if key.keycode == key::SPACE => Some(vec![0x00]),
        // `Ctrl` plus something with multi-character text — a dead key,
        // an emoji from a compose sequence — has no C0 equivalent.
        _ => None,
    }
}

/// The C0 control `Ctrl` + `c` folds to, if there is one.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        ' ' | '@' => Some(0x00),
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        // Not in the brief, but the same rule and free: `Ctrl+^` and
        // `Ctrl+_` are `RS` and `US`, which `less` and `emacs` bind.
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// The xterm modifier parameter: 1 plus a bit per modifier, so an
/// unmodified key is 1 and `Ctrl+Shift` is 6.
///
/// Returned as an `Option` because parameter 1 is exactly the case
/// where the *unparameterised* form is sent instead — `ESC [ A`, not
/// `ESC [ 1 ; 1 A` — and making that a `None` keeps the decision in one
/// place.
fn modifier_param(shift: bool, alt: bool, ctrl: bool) -> Option<u32> {
    let p = 1 + u32::from(shift) + 2 * u32::from(alt) + 4 * u32::from(ctrl);
    (p > 1).then_some(p)
}

/// The keys whose bytes do not depend on the layout.
///
/// Split out of [`encode`] so the modifier arithmetic is written once:
/// every key here has the same two shapes, a plain sequence and an
/// `ESC [ … ; m …` one, and the only variation is which final byte or
/// which tilde number it carries.
fn special(keycode: u32, modes: Modes, shift: bool, alt: bool, ctrl: bool) -> Option<Vec<u8>> {
    let param = modifier_param(shift, alt, ctrl);
    // The cursor and editing keys, as (final byte, tilde number).
    let bytes = match keycode {
        key::UP => cursor(b'A', modes, param),
        key::DOWN => cursor(b'B', modes, param),
        key::RIGHT => cursor(b'C', modes, param),
        key::LEFT => cursor(b'D', modes, param),
        key::HOME => cursor(b'H', modes, param),
        key::END => cursor(b'F', modes, param),
        key::ENTER | code::KP_ENTER => b"\r".to_vec(),
        key::BACKSPACE => b"\x7f".to_vec(),
        key::ESC => vec![ESC],
        // `Shift+Tab` is not a modified `Tab` in the parameter sense: it
        // has its own sequence, CBT, and every readline-alike knows it.
        key::TAB if shift => b"\x1b[Z".to_vec(),
        key::TAB => b"\t".to_vec(),
        code::INSERT => tilde(2, param),
        key::DELETE => tilde(3, param),
        key::PAGE_UP => tilde(5, param),
        key::PAGE_DOWN => tilde(6, param),
        // F1–F4 are SS3 keys, a leftover of the VT220 keypad; the rest
        // are tilde keys, and the numbering skips 16 and 22 because the
        // VT220 had no key there.
        code::F1 => ss3(b'P', param),
        code::F2 => ss3(b'Q', param),
        code::F3 => ss3(b'R', param),
        code::F4 => ss3(b'S', param),
        code::F5 => tilde(15, param),
        code::F6 => tilde(17, param),
        code::F7 => tilde(18, param),
        code::F8 => tilde(19, param),
        code::F9 => tilde(20, param),
        code::F10 => tilde(21, param),
        code::F11 => tilde(23, param),
        code::F12 => tilde(24, param),
        _ => return None,
    };
    Some(bytes)
}

/// An arrow or `Home`/`End`: CSI normally, SS3 in application-cursor
/// mode, and always CSI-with-parameter once a modifier is held —
/// `xterm` drops application mode there because SS3 has nowhere to put
/// the parameter.
fn cursor(final_byte: u8, modes: Modes, param: Option<u32>) -> Vec<u8> {
    match param {
        Some(p) => format!("\x1b[1;{p}").into_bytes_with(final_byte),
        None if modes.application_cursor => vec![ESC, b'O', final_byte],
        None => vec![ESC, b'[', final_byte],
    }
}

/// An SS3 function key (F1–F4), which grows a CSI form when modified
/// for the same reason the arrows do.
fn ss3(final_byte: u8, param: Option<u32>) -> Vec<u8> {
    match param {
        Some(p) => format!("\x1b[1;{p}").into_bytes_with(final_byte),
        None => vec![ESC, b'O', final_byte],
    }
}

/// A `CSI n ~` key, with the modifier as a second parameter.
fn tilde(n: u32, param: Option<u32>) -> Vec<u8> {
    match param {
        Some(p) => format!("\x1b[{n};{p}~").into_bytes(),
        None => format!("\x1b[{n}~").into_bytes(),
    }
}

/// `String::into_bytes` with one more byte appended, so the sequence
/// builders above stay one expression each.
trait IntoBytesWith {
    /// The string's bytes followed by `b`.
    fn into_bytes_with(self, b: u8) -> Vec<u8>;
}

impl IntoBytesWith for String {
    fn into_bytes_with(self, b: u8) -> Vec<u8> {
        let mut v = self.into_bytes();
        v.push(b);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key event as the toolkit would deliver it.
    fn ev(keycode: u32, text: &str, mods: u32) -> KeyEvent {
        KeyEvent {
            keycode,
            keysym: 0,
            mods,
            text: text.to_owned(),
        }
    }

    fn enc(key: &KeyEvent) -> Vec<u8> {
        encode(key, Modes::default()).expect("this key encodes to something")
    }

    #[test]
    fn printable_text_is_sent_as_the_keymap_produced_it() {
        assert_eq!(enc(&ev(30, "a", 0)), b"a");
        assert_eq!(enc(&ev(30, "A", mods::SHIFT)), b"A");
        // Whatever the layout says, including things ASCII has no
        // opinion about: the compositor already ran the keymap.
        assert_eq!(enc(&ev(18, "€", 0)), "€".as_bytes());
        assert_eq!(enc(&ev(16, "ä", 0)), "ä".as_bytes());
    }

    #[test]
    fn ctrl_c_is_one_byte() {
        assert_eq!(enc(&ev(46, "c", mods::CTRL)), [0x03]);
    }

    #[test]
    fn ctrl_folds_the_whole_ascii_table() {
        assert_eq!(enc(&ev(30, "a", mods::CTRL)), [0x01]);
        assert_eq!(enc(&ev(44, "z", mods::CTRL)), [0x1a]);
        // Shift does not change the fold: `Ctrl+Shift+A` is still 1.
        assert_eq!(enc(&ev(30, "A", mods::CTRL | mods::SHIFT)), [0x01]);
        assert_eq!(enc(&ev(26, "[", mods::CTRL)), [0x1b]);
        assert_eq!(enc(&ev(43, "\\", mods::CTRL)), [0x1c]);
        assert_eq!(enc(&ev(27, "]", mods::CTRL)), [0x1d]);
        assert_eq!(enc(&ev(3, "@", mods::CTRL)), [0x00]);
        assert_eq!(enc(&ev(53, "?", mods::CTRL)), [0x7f]);
    }

    #[test]
    fn ctrl_space_is_nul_even_without_text() {
        // The case a keymap reports as empty text, recovered from the
        // keycode because `NUL` is worth having.
        assert_eq!(enc(&ev(key::SPACE, "", mods::CTRL)), [0x00]);
        assert_eq!(enc(&ev(key::SPACE, " ", mods::CTRL)), [0x00]);
    }

    #[test]
    fn a_keymap_that_already_folded_ctrl_is_believed() {
        // Some servers apply the control transform themselves; sending
        // `ESC`-less 0x03 twice would be wrong, so pass it through.
        assert_eq!(enc(&ev(46, "\u{3}", mods::CTRL)), [0x03]);
    }

    #[test]
    fn ctrl_with_nothing_to_fold_encodes_nothing() {
        assert_eq!(encode(&ev(41, "`", mods::CTRL), Modes::default()), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(enc(&ev(33, "f", mods::ALT)), [ESC, b'f']);
        // And composes with `Ctrl`, which is how `Alt+Ctrl+H` reaches a
        // shell as `ESC` `BS`.
        assert_eq!(enc(&ev(35, "h", mods::ALT | mods::CTRL)), [ESC, 0x08]);
    }

    #[test]
    fn the_editing_keys_send_their_own_sequences() {
        assert_eq!(enc(&ev(key::ENTER, "\r", 0)), b"\r");
        assert_eq!(enc(&ev(code::KP_ENTER, "", 0)), b"\r");
        assert_eq!(enc(&ev(key::BACKSPACE, "", 0)), b"\x7f");
        assert_eq!(enc(&ev(key::TAB, "\t", 0)), b"\t");
        assert_eq!(enc(&ev(key::TAB, "\t", mods::SHIFT)), b"\x1b[Z");
        assert_eq!(enc(&ev(key::ESC, "", 0)), b"\x1b");
        assert_eq!(enc(&ev(code::INSERT, "", 0)), b"\x1b[2~");
        assert_eq!(enc(&ev(key::DELETE, "", 0)), b"\x1b[3~");
        assert_eq!(enc(&ev(key::PAGE_UP, "", 0)), b"\x1b[5~");
        assert_eq!(enc(&ev(key::PAGE_DOWN, "", 0)), b"\x1b[6~");
    }

    #[test]
    fn an_arrow_changes_with_application_cursor_mode() {
        let app = Modes {
            application_cursor: true,
        };
        for (code, csi, ss3) in [
            (key::UP, &b"\x1b[A"[..], &b"\x1bOA"[..]),
            (key::DOWN, b"\x1b[B", b"\x1bOB"),
            (key::RIGHT, b"\x1b[C", b"\x1bOC"),
            (key::LEFT, b"\x1b[D", b"\x1bOD"),
            (key::HOME, b"\x1b[H", b"\x1bOH"),
            (key::END, b"\x1b[F", b"\x1bOF"),
        ] {
            let k = ev(code, "", 0);
            assert_eq!(encode(&k, Modes::default()).unwrap(), csi);
            assert_eq!(encode(&k, app).unwrap(), ss3);
        }
    }

    #[test]
    fn a_modified_arrow_carries_the_parameter() {
        // m = 1 + shift + 2*alt + 4*ctrl.
        assert_eq!(enc(&ev(key::LEFT, "", mods::CTRL)), b"\x1b[1;5D");
        assert_eq!(enc(&ev(key::RIGHT, "", mods::SHIFT)), b"\x1b[1;2C");
        assert_eq!(enc(&ev(key::UP, "", mods::ALT)), b"\x1b[1;3A");
        assert_eq!(
            enc(&ev(key::DOWN, "", mods::CTRL | mods::SHIFT)),
            b"\x1b[1;6B"
        );
        assert_eq!(enc(&ev(key::HOME, "", mods::CTRL)), b"\x1b[1;5H");
        // A tilde key puts the modifier second, after its own number.
        assert_eq!(enc(&ev(key::DELETE, "", mods::CTRL)), b"\x1b[3;5~");
        assert_eq!(enc(&ev(key::PAGE_UP, "", mods::SHIFT)), b"\x1b[5;2~");
        // And application-cursor mode loses to a modifier, because SS3
        // has nowhere to put one.
        let app = Modes {
            application_cursor: true,
        };
        assert_eq!(
            encode(&ev(key::LEFT, "", mods::CTRL), app).unwrap(),
            b"\x1b[1;5D"
        );
    }

    #[test]
    fn the_function_row_is_ss3_then_tilde() {
        assert_eq!(enc(&ev(code::F1, "", 0)), b"\x1bOP");
        assert_eq!(enc(&ev(code::F2, "", 0)), b"\x1bOQ");
        assert_eq!(enc(&ev(code::F3, "", 0)), b"\x1bOR");
        assert_eq!(enc(&ev(code::F4, "", 0)), b"\x1bOS");
        assert_eq!(enc(&ev(code::F5, "", 0)), b"\x1b[15~");
        assert_eq!(enc(&ev(code::F6, "", 0)), b"\x1b[17~");
        assert_eq!(enc(&ev(code::F7, "", 0)), b"\x1b[18~");
        assert_eq!(enc(&ev(code::F8, "", 0)), b"\x1b[19~");
        assert_eq!(enc(&ev(code::F9, "", 0)), b"\x1b[20~");
        assert_eq!(enc(&ev(code::F10, "", 0)), b"\x1b[21~");
        assert_eq!(enc(&ev(code::F11, "", 0)), b"\x1b[23~");
        assert_eq!(enc(&ev(code::F12, "", 0)), b"\x1b[24~");
        // Modified, F1–F4 move to the CSI form, F5+ keep their number.
        assert_eq!(enc(&ev(code::F1, "", mods::SHIFT)), b"\x1b[1;2P");
        assert_eq!(enc(&ev(code::F5, "", mods::CTRL)), b"\x1b[15;5~");
    }

    #[test]
    fn a_bare_modifier_encodes_nothing() {
        for code in [
            key::LEFT_SHIFT,
            key::RIGHT_SHIFT,
            key::LEFT_CTRL,
            code::RIGHT_CTRL,
            code::LEFT_ALT,
            code::RIGHT_ALT,
            code::LEFT_META,
            code::RIGHT_META,
            code::CAPS_LOCK,
            code::NUM_LOCK,
            code::SCROLL_LOCK,
        ] {
            assert_eq!(encode(&ev(code, "", mods::SHIFT), Modes::default()), None);
        }
    }

    #[test]
    fn an_unmapped_key_without_text_encodes_nothing() {
        // A media key, say: no text, no sequence, so the child hears
        // nothing rather than a stray byte.
        assert_eq!(encode(&ev(163, "", 0), Modes::default()), None);
        assert_eq!(encode(&ev(163, "", mods::LOGO), Modes::default()), None);
    }

    #[test]
    fn the_logo_key_is_not_a_terminal_modifier() {
        // `Super` belongs to the compositor; it must not turn into an
        // `ESC` prefix or a parameter.
        assert_eq!(enc(&ev(30, "a", mods::LOGO)), b"a");
        assert_eq!(enc(&ev(key::LEFT, "", mods::LOGO)), b"\x1b[D");
    }

    #[test]
    fn bracketed_paste_wraps_only_when_asked() {
        assert_eq!(paste("ls -l", false), b"ls -l");
        assert_eq!(paste("ls -l", true), b"\x1b[200~ls -l\x1b[201~");
        assert_eq!(paste("", true), b"\x1b[200~\x1b[201~");
    }
}

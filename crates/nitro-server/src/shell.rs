//! The shell's privileged state: exclusive zones, anchors, global hotkeys,
//! the window-list subscription and the server-global window ids.
//!
//! # The privilege is the socket
//!
//! Nothing in here is reachable from the ordinary wire socket. A client
//! that connected to `shell.sock` is given [`caps::SHELL`] in its
//! `Welcome`, and that bit is the only gate: an unprivileged client sending
//! a shell op gets [`ErrorCode::Protocol`] and is disconnected, like every
//! other protocol error. There is no capability *negotiation*, no
//! per-message check against a policy table, and no way for a client to
//! acquire the bit after the fact — see `docs/shell.md` for why that is the
//! whole design and what it defers.
//!
//! # Why the state lives here and not in the scene
//!
//! An exclusive zone and an anchor are *policy*: they say what a maximized
//! window's rectangle is and where a bar sits, which is exactly the kind of
//! decision `docs/wm.md` keeps out of `nitro-scene`. The scene stores
//! layers, states and geometry; this module stores the shell's intent and
//! the window manager reads it through [`Zones::work_area`].
//!
//! [`caps::SHELL`]: nitro_wire::types::caps::SHELL
//! [`ErrorCode::Protocol`]: nitro_wire::types::ErrorCode::Protocol

use std::collections::HashMap;

use nitro_core::{Rect, Size};
use nitro_scene::{OutputId, WindowKey};
use nitro_wire::types::{Edge, WindowRef, anchor, mod_mask};

use crate::keyboard::Mods;

/// One window-targeting shell op, with its target already resolved.
///
/// These four are the shell ops that name the sender's **own** window and
/// change what is on screen, so unlike the rest they are *buffered* and
/// applied at the client's `Commit`, exactly like `SetBounds` or
/// `SetWindowState`. The reason is ordering, and the hardware probe found it
/// the hard way: a bar naturally sends `CreateWindow`, `SetAnchor` and
/// `SetExclusiveZone` in one transaction, and an op answered on receipt
/// would be looking for a window the commit has not created yet.
///
/// The *privilege* check still happens on receipt — see
/// `Server::handle_wire_msg` — so an unprivileged client is disconnected
/// whether or not it ever commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowOp {
    /// `SetLayer`: move between stacking layers.
    Layer(nitro_scene::Layer),
    /// `SetExclusiveZone`: reserve (or with `px: 0` release) screen space.
    Zone {
        /// Which edge of the output the space comes off.
        edge: Edge,
        /// Logical pixels to reserve; 0 releases.
        px: u32,
    },
    /// `SetAnchor`: stick to the output's edges.
    Anchor {
        /// Edge bitmask from [`anchor`](nitro_wire::types::anchor).
        edges: u8,
        /// Gap held on each anchored edge.
        margin: u32,
    },
    /// `GrabKeyboard`: take or release the keyboard.
    Grab(bool),
}

/// One window's reservation along one output edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    /// Which edge the space comes off.
    pub edge: Edge,
    /// Logical pixels reserved.
    pub px: u32,
}

/// One window's anchoring intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// Edge bitmask from [`anchor`](nitro_wire::types::anchor).
    pub edges: u8,
    /// Gap held on each anchored edge, in logical pixels.
    pub margin: u32,
}

/// Every shell window's zone and anchor, keyed by window.
///
/// Deliberately *not* keyed by output: a window moves between outputs and
/// its zone moves with it, so the output is looked up from the scene at the
/// moment the work area is computed. Keying by output would leave a stale
/// reservation on the screen a bar used to be on.
#[derive(Debug, Default)]
pub struct Zones {
    zones: HashMap<WindowKey, Zone>,
    anchors: HashMap<WindowKey, Anchor>,
}

impl Zones {
    /// No reservations at all.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set or (with `px == 0`) clear a window's reservation.
    pub fn set_zone(&mut self, win: WindowKey, edge: Edge, px: u32) {
        if px == 0 {
            self.zones.remove(&win);
        } else {
            self.zones.insert(win, Zone { edge, px });
        }
    }

    /// Set a window's anchoring intent.
    pub fn set_anchor(&mut self, win: WindowKey, edges: u8, margin: u32) {
        self.anchors.insert(win, Anchor { edges, margin });
    }

    /// A window's anchor, if it set one.
    #[must_use]
    pub fn anchor(&self, win: WindowKey) -> Option<Anchor> {
        self.anchors.get(&win).copied()
    }

    /// Every window that has an anchor, for re-applying after a mode or
    /// scale change.
    pub fn anchored(&self) -> impl Iterator<Item = (WindowKey, Anchor)> + '_ {
        self.anchors.iter().map(|(w, a)| (*w, *a))
    }

    /// Forget everything about a window: it closed, stopped showing, or its
    /// client went.
    ///
    /// Whether a window is *showing* is the server's question (it owns the
    /// scene), so a hidden bar's zone is skipped by the work-area
    /// computation rather than removed here; this is the permanent
    /// forgetting, for a window that is gone.
    pub fn forget(&mut self, win: WindowKey) {
        self.zones.remove(&win);
        self.anchors.remove(&win);
    }

    /// Whether any window reserves space at all. The fast path: a desktop
    /// with no shell running must not pay for a per-output lookup.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.zones.is_empty()
    }

    /// How many windows reserve space, for `stats`.
    #[must_use]
    pub fn zone_count(&self) -> usize {
        self.zones.len()
    }

    /// Shrink `area` by every reservation held by a window on `output`.
    ///
    /// `on_output` answers "is this window on that output, and visible?" —
    /// the caller owns the scene, so the lookup is a closure rather than a
    /// `&Scene` parameter, which keeps this function pure and unit-testable.
    ///
    /// Reservations on the same edge **add**: two bars docked to the top
    /// each get their own strip, which is the only rule that composes. A
    /// zone larger than the area collapses it to zero rather than going
    /// negative.
    #[must_use]
    pub fn work_area(
        &self,
        area: Rect,
        output: OutputId,
        mut on_output: impl FnMut(WindowKey) -> Option<OutputId>,
    ) -> Rect {
        if self.zones.is_empty() {
            return area;
        }
        let (mut top, mut bottom, mut left, mut right) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for (win, zone) in &self.zones {
            if on_output(*win) != Some(output) {
                continue;
            }
            let px = zone.px as f32;
            match zone.edge {
                Edge::Top => top += px,
                Edge::Bottom => bottom += px,
                Edge::Left => left += px,
                Edge::Right => right += px,
            }
        }
        shrink(area, top, bottom, left, right)
    }
}

/// `area` with each edge pushed in by the given amount, never inverted.
fn shrink(area: Rect, top: f32, bottom: f32, left: f32, right: f32) -> Rect {
    let w = (area.w - left - right).max(0.0);
    let h = (area.h - top - bottom).max(0.0);
    Rect::new(area.x + left, area.y + top, w, h)
}

/// Where an anchored window's frame goes, given the output's **full**
/// logical rectangle and the size the window currently is.
///
/// Anchored against the full rectangle, not the work area: a bar must not be
/// pushed off the screen by its own exclusive zone, and a launcher centred
/// in the work area would jump every time a bar appeared.
///
/// Opposite edges together mean "span that axis", so the returned rectangle
/// is the window's *new size* as well as its position. Neither edge means
/// "centre on that axis", which is what a launcher asks for with
/// `edges: 0`.
#[must_use]
pub fn anchor_rect(output: Rect, size: Size, anchor_to: Anchor) -> Rect {
    let margin = anchor_to.margin as f32;
    let left = anchor_to.edges & anchor::LEFT != 0;
    let right = anchor_to.edges & anchor::RIGHT != 0;
    let top = anchor_to.edges & anchor::TOP != 0;
    let bottom = anchor_to.edges & anchor::BOTTOM != 0;
    let (x, width) = axis(output.x, output.w, size.w, margin, left, right);
    let (y, height) = axis(output.y, output.h, size.h, margin, top, bottom);
    Rect::new(x, y, width, height)
}

/// One axis of [`anchor_rect`]: origin and extent, given whether the low
/// and/or high edge is anchored.
fn axis(origin: f32, extent: f32, size: f32, margin: f32, low: bool, high: bool) -> (f32, f32) {
    match (low, high) {
        // Both: span the axis, inset by the margin on each side.
        (true, true) => (origin + margin, (extent - 2.0 * margin).max(0.0)),
        (true, false) => (origin + margin, size),
        (false, true) => (origin + extent - margin - size, size),
        // Neither: centred, rounded to a whole logical pixel so a bar's
        // text does not land between them.
        (false, false) => (origin + ((extent - size) / 2.0).round(), size),
    }
}

/// One hotkey binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    /// Which shell client owns it, by epoll token.
    pub token: u64,
    /// The client's own id for it, echoed in `HotKey`.
    pub id: u32,
    /// Modifiers, as named bits.
    pub mods: Mods,
    /// X11 keysym, or 0 for a bare-modifier tap.
    pub keysym: u32,
}

impl Binding {
    /// Whether this is a bare-modifier tap rather than a chord.
    #[must_use]
    pub fn is_tap(&self) -> bool {
        self.keysym == 0
    }
}

/// Every live binding, plus the state the bare-modifier tap needs.
#[derive(Debug, Default)]
pub struct HotKeys {
    bindings: Vec<Binding>,
    /// Modifier mask of the tap candidate currently held, if any: set when a
    /// lone modifier goes down with nothing else held, cleared the moment
    /// any other key is touched.
    tap_candidate: Option<u32>,
}

/// Why a [`HotKeys::bind`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    /// A bit outside [`mod_mask::ALL`] or [`anchor::ALL`] was set.
    ReservedBits,
    /// A bare-modifier tap named no modifier, or more than one. "Tap
    /// Super+Shift" has no meaning: there is no single press to detect.
    BadTap,
    /// The chord is one of the compositor's own, so no client may have it.
    Reserved,
    /// Another client already holds this chord.
    Taken,
}

impl HotKeys {
    /// No bindings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many bindings are live, for `stats`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether nothing is bound: the fast path a keyboard-heavy desktop with
    /// no shell takes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Bind a chord, replacing this client's own binding of the same `id`.
    ///
    /// # Errors
    /// [`BindError`] for reserved bits, a malformed tap, one of the
    /// compositor's own chords, or one another client holds.
    pub fn bind(
        &mut self,
        token: u64,
        id: u32,
        mods_mask: u32,
        keysym: u32,
    ) -> Result<(), BindError> {
        if mods_mask & !mod_mask::ALL != 0 {
            return Err(BindError::ReservedBits);
        }
        let mods = Mods::from_mask(mods_mask);
        if keysym == 0 && mods_mask.count_ones() != 1 {
            return Err(BindError::BadTap);
        }
        if keysym != 0 && crate::keyboard::hotkey(keysym, mods).is_some() {
            // The compositor's chords are not negotiable: Ctrl+Alt+F2 must
            // switch VT even with a wedged shell, and Alt+Tab is how you
            // leave an application that has taken the keyboard.
            return Err(BindError::Reserved);
        }
        if self
            .bindings
            .iter()
            .any(|b| b.token != token && b.mods == mods && b.keysym == keysym)
        {
            return Err(BindError::Taken);
        }
        self.bindings.retain(|b| !(b.token == token && b.id == id));
        self.bindings.push(Binding {
            token,
            id,
            mods,
            keysym,
        });
        Ok(())
    }

    /// Release one binding. Unbinding what was never bound is a no-op: a
    /// shell shutting down should not have to remember what it managed to
    /// bind.
    pub fn unbind(&mut self, token: u64, id: u32) {
        self.bindings.retain(|b| !(b.token == token && b.id == id));
    }

    /// Drop every binding of a client that has gone.
    pub fn forget_client(&mut self, token: u64) {
        self.bindings.retain(|b| b.token != token);
        self.tap_candidate = None;
    }

    /// The chord binding a key event matches, if any.
    ///
    /// Chords only — a tap is decided on release by [`HotKeys::key`], which
    /// is the caller for both.
    fn chord(&self, keysym: u32, mods: Mods) -> Option<Binding> {
        self.bindings
            .iter()
            .find(|b| !b.is_tap() && b.keysym == keysym && b.mods == mods)
            .copied()
    }

    /// Feed one key event; returns the bindings that fired.
    ///
    /// `mods` is the modifier state *after* the event, the same value
    /// [`crate::keyboard::KeyResolution::named`] carries.
    ///
    /// Two things happen here, and they have to happen in one place because
    /// they share the tap state:
    ///
    /// * A **chord** fires on its press and again on its release. The
    ///   release is matched against the same modifier state, so letting go
    ///   of Super first simply means the shell never hears the release — the
    ///   alternative, remembering which chord is down, would fire a release
    ///   for a chord the user broke apart mid-way.
    /// * A **tap** fires once, on the modifier's release, and only if
    ///   nothing else was pressed while it was held. Any other key
    ///   cancels the candidate, which is what makes Super+Q a window
    ///   close and not also a launcher toggle.
    pub fn key(&mut self, keysym: u32, pressed: bool, mods: Mods) -> Vec<(Binding, bool)> {
        let key_mod = crate::keyboard::mod_of_keysym(keysym);
        if pressed {
            self.tap_candidate = match key_mod {
                // A lone modifier going down with nothing else held arms a
                // tap. A second modifier disarms: "Super+Shift tapped" is
                // not a gesture this protocol has.
                Some(bit) if self.tap_candidate.is_none() && mods.mask() == bit => Some(bit),
                _ => None,
            };
        } else if key_mod.is_none() {
            // A non-modifier release cancels too: the key was pressed while
            // the modifier was held, so this was a chord, not a tap.
            self.tap_candidate = None;
        }
        let mut fired = Vec::new();
        if let Some(b) = self.chord(keysym, mods) {
            fired.push((b, pressed));
            // A chord consumed the key, so it was not a bare modifier tap.
            self.tap_candidate = None;
            return fired;
        }
        if !pressed
            && let Some(bit) = key_mod
            && self.tap_candidate == Some(bit)
        {
            self.tap_candidate = None;
            for b in &self.bindings {
                if b.is_tap() && b.mods.mask() == bit {
                    fired.push((*b, false));
                }
            }
        }
        fired
    }

    /// Forget any armed tap. Called when the keyboard state is reset (a VT
    /// switch, a device unplugged): the release that would have completed
    /// the tap was never seen, so firing later would be a guess.
    pub fn reset(&mut self) {
        self.tap_candidate = None;
    }

    /// Forget any armed tap because something that is *not* a key happened
    /// while the modifier was held.
    ///
    /// A pointer button is the case that matters: `Super`+drag is how you
    /// move an undecorated window (`docs/wm.md`), and it ends with the
    /// modifier being released with no key in between — which is exactly
    /// the shape of a tap. Letting it fire would pop the launcher open
    /// every time the user finished dragging a window.
    pub fn cancel_tap(&mut self) {
        self.tap_candidate = None;
    }
}

/// Server-global window ids, and their two-way map to scene keys.
///
/// A [`WindowRef`] is *not* a scene key: scene keys are generational and
/// internal, and handing one to a client would leak the scene's allocation
/// strategy onto the wire. It is not a client's `NodeId` either — those are
/// namespaced per connection, so two clients may both own `NodeId(1)` and a
/// shell could not tell them apart. So the server mints a third id space,
/// dense and monotonic, exactly for talking *about* windows.
#[derive(Debug, Default)]
pub struct WindowRefs {
    to_key: HashMap<WindowRef, WindowKey>,
    to_ref: HashMap<WindowKey, WindowRef>,
    next: u32,
}

impl WindowRefs {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            to_key: HashMap::new(),
            to_ref: HashMap::new(),
            // 0 is `WindowRef::NONE`, so ids start at 1.
            next: 1,
        }
    }

    /// This window's id, minting one the first time it is asked for.
    pub fn id_for(&mut self, win: WindowKey) -> WindowRef {
        if let Some(r) = self.to_ref.get(&win) {
            return *r;
        }
        let r = WindowRef(self.next);
        // Saturating rather than wrapping: a server that somehow opened four
        // billion windows must not start handing out an id that already
        // names a live window, which is the one failure a shell cannot
        // detect. It stops minting new ones instead.
        self.next = self.next.saturating_add(1);
        self.to_ref.insert(win, r);
        self.to_key.insert(r, win);
        r
    }

    /// The window an id names, if it is still live.
    #[must_use]
    pub fn key_for(&self, id: WindowRef) -> Option<WindowKey> {
        self.to_key.get(&id).copied()
    }

    /// This window's id if it has one, without minting.
    #[must_use]
    pub fn existing(&self, win: WindowKey) -> Option<WindowRef> {
        self.to_ref.get(&win).copied()
    }

    /// Retire a window's id. Ids are never reused: a shell holding a stale
    /// `WindowRef` gets "no such window" rather than someone else's window.
    pub fn forget(&mut self, win: WindowKey) -> Option<WindowRef> {
        let id = self.to_ref.remove(&win)?;
        self.to_key.remove(&id);
        Some(id)
    }
}

#[cfg(test)]
// Every number in these tests is exact arithmetic on whole pixels — sums,
// halves and differences of small integers — so equality is the assertion
// that means what it says; an epsilon would only hide a wrong formula.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use nitro_scene::WindowKey;

    fn key(n: u32) -> WindowKey {
        WindowKey::from_parts(n, 1)
    }

    const OUT: OutputId = OutputId(0);
    const OTHER: OutputId = OutputId(1);

    fn screen() -> Rect {
        Rect::new(0.0, 0.0, 1280.0, 720.0)
    }

    #[test]
    fn a_top_zone_comes_off_the_top_of_the_work_area() {
        let mut z = Zones::new();
        z.set_zone(key(1), Edge::Top, 32);
        let area = z.work_area(screen(), OUT, |_| Some(OUT));
        assert_eq!(area, Rect::new(0.0, 32.0, 1280.0, 688.0));
    }

    #[test]
    fn zones_on_one_edge_add_and_opposite_edges_both_apply() {
        let mut z = Zones::new();
        z.set_zone(key(1), Edge::Top, 32);
        z.set_zone(key(2), Edge::Top, 8);
        z.set_zone(key(3), Edge::Bottom, 40);
        z.set_zone(key(4), Edge::Left, 64);
        let area = z.work_area(screen(), OUT, |_| Some(OUT));
        assert_eq!(area, Rect::new(64.0, 40.0, 1216.0, 640.0));
    }

    #[test]
    fn a_zone_on_another_output_does_not_shrink_this_one() {
        let mut z = Zones::new();
        z.set_zone(key(1), Edge::Top, 32);
        let area = z.work_area(screen(), OUT, |_| Some(OTHER));
        assert_eq!(area, screen());
        // And a window on no output at all (unplaced) reserves nothing.
        assert_eq!(z.work_area(screen(), OUT, |_| None), screen());
    }

    #[test]
    fn px_zero_and_forget_both_release() {
        let mut z = Zones::new();
        z.set_zone(key(1), Edge::Top, 32);
        z.set_zone(key(1), Edge::Top, 0);
        assert!(z.is_empty());
        z.set_zone(key(1), Edge::Top, 32);
        z.forget(key(1));
        assert!(z.is_empty());
        assert_eq!(z.work_area(screen(), OUT, |_| Some(OUT)), screen());
    }

    #[test]
    fn an_absurd_zone_collapses_the_area_rather_than_inverting_it() {
        let mut z = Zones::new();
        z.set_zone(key(1), Edge::Top, 10_000);
        let area = z.work_area(screen(), OUT, |_| Some(OUT));
        assert_eq!(area.h, 0.0);
        assert!(area.w > 0.0, "only the anchored axis collapses");
    }

    #[test]
    fn a_bar_spans_the_top_edge() {
        let r = anchor_rect(
            screen(),
            Size::new(100.0, 32.0),
            Anchor {
                edges: anchor::TOP | anchor::LEFT | anchor::RIGHT,
                margin: 0,
            },
        );
        assert_eq!(r, Rect::new(0.0, 0.0, 1280.0, 32.0));
    }

    #[test]
    fn a_margin_insets_every_anchored_edge() {
        let r = anchor_rect(
            screen(),
            Size::new(100.0, 32.0),
            Anchor {
                edges: anchor::BOTTOM | anchor::LEFT | anchor::RIGHT,
                margin: 8,
            },
        );
        assert_eq!(r, Rect::new(8.0, 720.0 - 8.0 - 32.0, 1264.0, 32.0));
    }

    #[test]
    fn no_edges_centres_on_both_axes() {
        let r = anchor_rect(
            screen(),
            Size::new(400.0, 300.0),
            Anchor {
                edges: 0,
                margin: 0,
            },
        );
        assert_eq!(r, Rect::new(440.0, 210.0, 400.0, 300.0));
    }

    #[test]
    fn anchoring_is_relative_to_the_outputs_own_origin() {
        // Second output in a two-monitor row: the anchor is in desktop
        // space, so a bar on it sits at its origin, not at 0.
        let r = anchor_rect(
            Rect::new(1280.0, 0.0, 1920.0, 1080.0),
            Size::new(10.0, 32.0),
            Anchor {
                edges: anchor::TOP | anchor::LEFT | anchor::RIGHT,
                margin: 0,
            },
        );
        assert_eq!(r, Rect::new(1280.0, 0.0, 1920.0, 32.0));
    }

    const SUPER_L: u32 = xkbcommon::xkb::keysyms::KEY_Super_L;
    const RETURN: u32 = xkbcommon::xkb::keysyms::KEY_Return;
    const KEY_A: u32 = xkbcommon::xkb::keysyms::KEY_a;
    const LOGO: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        logo: true,
    };

    #[test]
    fn a_chord_fires_on_press_and_release() {
        let mut h = HotKeys::new();
        h.bind(1, 7, mod_mask::SUPER, RETURN).unwrap();
        let down = h.key(RETURN, true, LOGO);
        assert_eq!(down.len(), 1);
        assert_eq!((down[0].0.id, down[0].1), (7, true));
        let up = h.key(RETURN, false, LOGO);
        assert_eq!((up[0].0.id, up[0].1), (7, false));
    }

    #[test]
    fn a_chord_needs_its_exact_modifiers() {
        let mut h = HotKeys::new();
        h.bind(1, 7, mod_mask::SUPER, RETURN).unwrap();
        assert!(h.key(RETURN, true, Mods::default()).is_empty());
        let shift_logo = Mods {
            shift: true,
            ..LOGO
        };
        assert!(h.key(RETURN, true, shift_logo).is_empty());
    }

    #[test]
    fn a_bare_super_tap_fires_once_on_release() {
        let mut h = HotKeys::new();
        h.bind(1, 9, mod_mask::SUPER, 0).unwrap();
        assert!(h.key(SUPER_L, true, LOGO).is_empty(), "no press event");
        let up = h.key(SUPER_L, false, Mods::default());
        assert_eq!(up.len(), 1);
        assert_eq!((up[0].0.id, up[0].1), (9, false));
    }

    #[test]
    fn another_key_cancels_the_tap() {
        let mut h = HotKeys::new();
        h.bind(1, 9, mod_mask::SUPER, 0).unwrap();
        h.key(SUPER_L, true, LOGO);
        h.key(KEY_A, true, LOGO);
        h.key(KEY_A, false, LOGO);
        assert!(
            h.key(SUPER_L, false, Mods::default()).is_empty(),
            "Super+A must not also be a Super tap"
        );
    }

    #[test]
    fn a_bound_chord_under_the_tap_modifier_cancels_it_too() {
        let mut h = HotKeys::new();
        h.bind(1, 9, mod_mask::SUPER, 0).unwrap();
        h.bind(1, 7, mod_mask::SUPER, RETURN).unwrap();
        h.key(SUPER_L, true, LOGO);
        assert_eq!(h.key(RETURN, true, LOGO).len(), 1);
        h.key(RETURN, false, LOGO);
        assert!(h.key(SUPER_L, false, Mods::default()).is_empty());
    }

    #[test]
    fn a_second_modifier_disarms_the_tap() {
        let mut h = HotKeys::new();
        h.bind(1, 9, mod_mask::SUPER, 0).unwrap();
        h.key(SUPER_L, true, LOGO);
        let shift_logo = Mods {
            shift: true,
            ..LOGO
        };
        h.key(xkbcommon::xkb::keysyms::KEY_Shift_L, true, shift_logo);
        assert!(h.key(SUPER_L, false, Mods::default()).is_empty());
    }

    #[test]
    fn the_compositors_own_chords_cannot_be_bound() {
        let mut h = HotKeys::new();
        assert_eq!(
            h.bind(1, 1, mod_mask::SUPER, xkbcommon::xkb::keysyms::KEY_q),
            Err(BindError::Reserved)
        );
        assert_eq!(
            h.bind(
                1,
                1,
                mod_mask::CTRL | mod_mask::ALT,
                xkbcommon::xkb::keysyms::KEY_F2
            ),
            Err(BindError::Reserved)
        );
        assert_eq!(
            h.bind(1, 1, mod_mask::ALT, xkbcommon::xkb::keysyms::KEY_Tab),
            Err(BindError::Reserved)
        );
    }

    #[test]
    fn a_chord_belongs_to_one_client_and_rebinding_an_id_replaces_it() {
        let mut h = HotKeys::new();
        h.bind(1, 7, mod_mask::SUPER, RETURN).unwrap();
        assert_eq!(
            h.bind(2, 1, mod_mask::SUPER, RETURN),
            Err(BindError::Taken),
            "a second client cannot steal a chord"
        );
        // The owner may re-bind its own id, and does not collide with itself.
        h.bind(1, 7, mod_mask::SUPER, KEY_A).unwrap();
        assert_eq!(h.len(), 1);
        assert!(h.key(RETURN, true, LOGO).is_empty());
        assert_eq!(h.key(KEY_A, true, LOGO).len(), 1);
    }

    #[test]
    fn bad_bind_arguments_are_refused() {
        let mut h = HotKeys::new();
        assert_eq!(
            h.bind(1, 1, 0x8000, RETURN),
            Err(BindError::ReservedBits),
            "unknown modifier bits"
        );
        assert_eq!(
            h.bind(1, 1, 0, 0),
            Err(BindError::BadTap),
            "a tap of no modifier"
        );
        assert_eq!(
            h.bind(1, 1, mod_mask::SUPER | mod_mask::SHIFT, 0),
            Err(BindError::BadTap),
            "a tap of two modifiers"
        );
        assert!(h.is_empty());
    }

    #[test]
    fn unbinding_and_disconnecting_both_release() {
        let mut h = HotKeys::new();
        h.bind(1, 7, mod_mask::SUPER, RETURN).unwrap();
        h.unbind(1, 99); // never bound: a no-op, not an error
        assert_eq!(h.len(), 1);
        h.unbind(1, 7);
        assert!(h.is_empty());

        h.bind(1, 7, mod_mask::SUPER, RETURN).unwrap();
        h.bind(2, 7, mod_mask::SUPER, KEY_A).unwrap();
        h.forget_client(1);
        assert_eq!(h.len(), 1);
        assert!(h.key(RETURN, true, LOGO).is_empty());
        // And the chord client 1 held is free for someone else now.
        h.bind(3, 1, mod_mask::SUPER, RETURN).unwrap();
    }

    #[test]
    fn a_pointer_button_cancels_the_tap() {
        // Super+drag ends with the modifier released and no key in between,
        // which is the shape of a tap; the button is what tells them apart.
        let mut h = HotKeys::new();
        h.bind(1, 9, mod_mask::SUPER, 0).unwrap();
        h.key(SUPER_L, true, LOGO);
        h.cancel_tap();
        assert!(
            h.key(SUPER_L, false, Mods::default()).is_empty(),
            "a Super drag is not a Super tap"
        );
    }

    #[test]
    fn a_reset_drops_an_armed_tap() {
        let mut h = HotKeys::new();
        h.bind(1, 9, mod_mask::SUPER, 0).unwrap();
        h.key(SUPER_L, true, LOGO);
        h.reset();
        assert!(
            h.key(SUPER_L, false, Mods::default()).is_empty(),
            "the press was on another VT"
        );
    }

    #[test]
    fn window_ids_are_stable_dense_and_never_reused() {
        let mut refs = WindowRefs::new();
        let a = refs.id_for(key(1));
        let b = refs.id_for(key(2));
        assert_eq!((a, b), (WindowRef(1), WindowRef(2)));
        assert!(!a.is_none(), "0 is reserved for NONE");
        assert_eq!(refs.id_for(key(1)), a, "asking twice is stable");
        assert_eq!(refs.key_for(a), Some(key(1)));
        assert_eq!(refs.existing(key(2)), Some(b));

        assert_eq!(refs.forget(key(1)), Some(a));
        assert_eq!(refs.key_for(a), None, "a stale ref names nothing");
        assert_eq!(refs.existing(key(1)), None);
        assert_eq!(
            refs.id_for(key(3)),
            WindowRef(3),
            "ids are not reused after a close"
        );
    }
}

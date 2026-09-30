//! The clipboard's bookkeeping (M5-H, caps `DATA`): who owns the selection,
//! what it offers, and which requests are parked waiting for the owner.
//!
//! # The server never touches the bytes
//!
//! A clipboard can hold a 50 MB image, and the server is single-threaded
//! with a strict memory budget, so payloads never pass through it. The
//! owner answers a `SelectionRequest` with a descriptor (`SendSelection`),
//! and the server relays that descriptor to the requester as
//! `SelectionData`. What this module stores is therefore only *state*: a
//! parked [`Transfer`] is four words and **no descriptor**.
//!
//! # Policy only
//!
//! Like `shell.rs`, nothing here does I/O or knows about `Server`: every
//! method is total and unit-tested below, and the wiring — the focus check,
//! the pipes, the messages — lives in `lib.rs`, tested end to end in
//! `tests/data.rs`. See `docs/wire.md` § Data transfer for the contract.
//!
//! # Drag and drop
//!
//! A drop is a selection transfer with a pointer gesture attached (M5-I):
//! [`Dnd`] is the gesture's state machine, and the bytes move through the
//! same parked [`Transfer`]s, tagged [`DataSource::Drag`]. Same rules: no
//! I/O, every transition returns what the caller must send, unit-tested
//! below and end to end in `tests/dnd.rs`.

use nitro_scene::WindowKey;
use nitro_wire::types::{DataSource, DragAction, drag_actions};

/// Largest MIME list a client may offer in `SetSelection`. Chromium offers
/// about ten; the cap exists so a hostile client cannot make every other
/// client's `SelectionOffer` arbitrarily large.
pub const MAX_MIMES: usize = 64;

/// Longest MIME string accepted, in bytes.
pub const MAX_MIME_LEN: usize = 256;

/// The current clipboard selection: who owns it and what it offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The owner's epoll token.
    pub owner: u64,
    /// MIME types, most preferred first. Never empty: an empty
    /// `SetSelection` clears the offer instead.
    pub mimes: Vec<String>,
}

/// One request parked between the requester and the owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transfer {
    /// Server-allocated id, the one sent in `SelectionRequest`.
    pub id: u32,
    /// The requester's epoll token.
    pub requester: u64,
    /// The requester's own id, echoed back in `SelectionData`.
    pub reply_to: u32,
    /// The token the `SelectionRequest` was sent to.
    pub owner: u64,
    /// The clipboard or the drag offer. A new clipboard selection cancels
    /// only clipboard transfers, and the end of a drag only drag ones.
    pub source: DataSource,
}

/// Why a MIME list was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MimeError {
    /// More than [`MAX_MIMES`] entries.
    TooMany,
    /// One entry longer than [`MAX_MIME_LEN`] bytes.
    TooLong,
    /// An empty entry.
    Empty,
    /// A non-ASCII entry. Chromium `CHECK`s that MIME types are ASCII
    /// (`clipboard_format_type_aura.cc`), so one would crash every browser
    /// that saw the offer.
    NotAscii,
}

impl MimeError {
    /// Whether this is a resource limit (`ErrorCode::Limit`) rather than a
    /// malformed message (`ErrorCode::Protocol`).
    #[must_use]
    pub const fn is_limit(self) -> bool {
        matches!(self, Self::TooMany | Self::TooLong)
    }

    /// A human-readable reason, for the `Error` detail; `op` names the
    /// message that carried the list (`SetSelection`, `StartDrag`).
    #[must_use]
    pub fn detail(self, op: &str) -> String {
        let why = match self {
            Self::TooMany => "offers more than 64 MIME types",
            Self::TooLong => "names a MIME type longer than 256 bytes",
            Self::Empty => "names an empty MIME type",
            Self::NotAscii => "names a MIME type that is not ASCII",
        };
        format!("{op} {why}")
    }
}

/// Check a MIME list against the limits in `docs/wire.md` § `SetSelection`.
///
/// # Errors
/// The first rule the list breaks.
pub fn validate_mimes(mimes: &[String]) -> Result<(), MimeError> {
    if mimes.len() > MAX_MIMES {
        return Err(MimeError::TooMany);
    }
    for m in mimes {
        if m.is_empty() {
            return Err(MimeError::Empty);
        }
        if m.len() > MAX_MIME_LEN {
            return Err(MimeError::TooLong);
        }
        if !m.is_ascii() {
            return Err(MimeError::NotAscii);
        }
    }
    Ok(())
}

/// The clipboard: the current offer and every parked transfer.
#[derive(Debug, Default)]
pub struct Selections {
    offer: Option<Offer>,
    /// At most `MAX_PENDING_SELECTIONS` per client; a `Vec` beats a map at
    /// this size.
    transfers: Vec<Transfer>,
    /// The last server id handed out.
    next_id: u32,
}

impl Selections {
    /// No selection, nothing parked.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current offer, if there is a selection.
    #[must_use]
    pub const fn offer(&self) -> Option<&Offer> {
        self.offer.as_ref()
    }

    /// The offered MIME types; empty when there is no selection.
    #[must_use]
    pub fn mimes(&self) -> &[String] {
        self.offer.as_ref().map_or(&[], |o| o.mimes.as_slice())
    }

    /// Parked transfers, for the stats.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.transfers.len()
    }

    /// Install a new selection (an empty `mimes` clears it) and return the
    /// transfers it cancels, whose requesters must be answered at EOF.
    ///
    /// **Every** parked transfer is cancelled, including those aimed at the
    /// same client when an owner replaces its own selection: a new
    /// `SetSelection` is a new data source, and the old one — the thing
    /// those requests were asking about — is gone. Every parked transfer is
    /// aimed at the current owner anyway, because the previous change
    /// cancelled the rest.
    ///
    /// Drag transfers are not the clipboard's and are left alone.
    pub fn set_offer(&mut self, owner: u64, mimes: Vec<String>) -> Vec<Transfer> {
        self.offer = if mimes.is_empty() {
            None
        } else {
            Some(Offer { owner, mimes })
        };
        self.take_source(DataSource::Clipboard)
    }

    /// The drag offer is gone (released, superseded, its source gone):
    /// return every parked drag transfer, whose requesters must be
    /// answered at EOF.
    pub fn cancel_drag(&mut self) -> Vec<Transfer> {
        self.take_source(DataSource::Drag)
    }

    fn take_source(&mut self, source: DataSource) -> Vec<Transfer> {
        let (gone, keep) = self.transfers.drain(..).partition(|t| t.source == source);
        self.transfers = keep;
        gone
    }

    /// Forget a client that disconnected. Returns whether it owned the
    /// selection (now cleared, so the caller broadcasts the empty offer)
    /// and the transfers it *owed*, whose requesters must be answered at
    /// EOF. Transfers it *made* are dropped silently: nobody is left to
    /// answer.
    pub fn forget_client(&mut self, token: u64) -> (bool, Vec<Transfer>) {
        let was_owner = self.offer.as_ref().is_some_and(|o| o.owner == token);
        if was_owner {
            self.offer = None;
        }
        self.transfers.retain(|t| t.requester != token);
        let (owed, keep) = self.transfers.drain(..).partition(|t| t.owner == token);
        self.transfers = keep;
        (was_owner, owed)
    }

    /// How many requests this connection has outstanding.
    #[must_use]
    pub fn outstanding(&self, token: u64) -> usize {
        self.transfers
            .iter()
            .filter(|t| t.requester == token)
            .count()
    }

    /// Whether this connection already has a request with this id parked.
    #[must_use]
    pub fn has_reply_id(&self, token: u64, reply_to: u32) -> bool {
        self.transfers
            .iter()
            .any(|t| t.requester == token && t.reply_to == reply_to)
    }

    /// Park a transfer and return its server id.
    ///
    /// Ids come from a wrapping counter that skips 0 (for legibility in a
    /// trace) and any id still parked, so a late `SendSelection` can never
    /// be mistaken for an answer to a newer request.
    pub fn start(&mut self, requester: u64, reply_to: u32, owner: u64, source: DataSource) -> u32 {
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            let id = self.next_id;
            if id != 0 && !self.transfers.iter().any(|t| t.id == id) {
                self.transfers.push(Transfer {
                    id,
                    requester,
                    reply_to,
                    owner,
                    source,
                });
                return id;
            }
        }
    }

    /// Take the transfer a `SendSelection` from `from` answers. `None` for
    /// an unknown id **or** one parked against a different owner — both are
    /// the stale case, which is not an error.
    pub fn take(&mut self, id: u32, from: u64) -> Option<Transfer> {
        let i = self
            .transfers
            .iter()
            .position(|t| t.id == id && t.owner == from)?;
        Some(self.transfers.swap_remove(i))
    }
}

/// Whether a `drag_actions` set is acceptable in a `StartDrag`: at least
/// one action, and no reserved bit.
#[must_use]
pub const fn valid_actions(actions: u32) -> bool {
    actions != 0 && actions & !drag_actions::ALL == 0
}

/// The `drag_actions` bit a chosen action corresponds to; 0 for `None`.
#[must_use]
pub const fn action_bit(action: DragAction) -> u32 {
    match action {
        DragAction::None => 0,
        DragAction::Copy => drag_actions::COPY,
        DragAction::Move => drag_actions::MOVE,
        DragAction::Link => drag_actions::LINK,
    }
}

/// Where a drag is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The button is down and the pointer is carrying the offer: the
    /// server holds the pointer grab.
    Dragging,
    /// Dropped on an accepting target, which is still reading. The grab is
    /// over; the target's `FinishDrag` completes the drop.
    Dropped,
    /// The source has been told the outcome (`DragFinished`) and owes a
    /// `FinishDrag`, which releases the offer.
    Finished,
}

/// The window a drag is over, and the client that owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DragTarget {
    /// The owning client's epoll token.
    pub token: u64,
    /// The window.
    pub window: WindowKey,
}

/// What a pointer movement did to the drag's target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retarget {
    /// Still over the same window (or still over none): a motion.
    Same,
    /// A different window, or none: `left` gets `DragLeave`, the new
    /// target (if any) `DragEnter`.
    Changed {
        /// The target the drag left.
        left: Option<DragTarget>,
    },
}

/// What the caller must send after a transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing.
    Nothing,
    /// The target gets `DragDrop`.
    Drop(DragTarget),
    /// `leave` (if any) gets `DragLeave`, then the source gets
    /// `DragFinished { accepted, action }`.
    Finished {
        /// A target to tell the offer has gone.
        leave: Option<DragTarget>,
        /// Whether the offer was taken.
        accepted: bool,
        /// What the target did with it.
        action: DragAction,
    },
    /// The drag is over and forgotten: `leave` (if any) gets `DragLeave`,
    /// and every parked drag transfer is answered at EOF.
    Released {
        /// A target to tell the offer has gone.
        leave: Option<DragTarget>,
    },
}

impl Outcome {
    const REJECTED: Self = Self::Finished {
        leave: None,
        accepted: false,
        action: DragAction::None,
    };
}

/// One drag-and-drop gesture, from `StartDrag` to the source's
/// `FinishDrag`. See `docs/wire.md` § The drag-and-drop sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dnd {
    /// The source client's token. The source is the *client*: the window
    /// it started from may go away and the drag carries on.
    pub source: u64,
    /// The drag icon the server adopted, if any.
    pub icon: Option<WindowKey>,
    /// The `drag_actions` the source offers.
    pub actions: u32,
    /// MIME types offered, most preferred first.
    pub mimes: Vec<String>,
    target: Option<DragTarget>,
    /// What the current target said it would do; cleared on every target
    /// change, so an acceptance never outlives the window that made it.
    accepted: Option<(DragAction, String)>,
    phase: Phase,
}

impl Dnd {
    /// A drag just started: over nothing yet, nothing accepted.
    #[must_use]
    pub const fn new(
        source: u64,
        icon: Option<WindowKey>,
        actions: u32,
        mimes: Vec<String>,
    ) -> Self {
        Self {
            source,
            icon,
            actions,
            mimes,
            target: None,
            accepted: None,
            phase: Phase::Dragging,
        }
    }

    /// Where the drag is.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the drag holds the pointer grab.
    #[must_use]
    pub fn grabbing(&self) -> bool {
        self.phase == Phase::Dragging
    }

    /// The window the drag is over (or was dropped on).
    #[must_use]
    pub const fn target(&self) -> Option<DragTarget> {
        self.target
    }

    /// Whether the current target has accepted the offer.
    #[must_use]
    pub const fn accepted(&self) -> bool {
        self.accepted.is_some()
    }

    /// Whether `token` may read the drag offer now: it is the target while
    /// dragging, or the target it was dropped on.
    #[must_use]
    pub fn is_drop_target(&self, token: u64) -> bool {
        matches!(self.phase, Phase::Dragging | Phase::Dropped)
            && self.target.is_some_and(|t| t.token == token)
    }

    /// The pointer is now over `now`. Only while dragging.
    pub fn retarget(&mut self, now: Option<DragTarget>) -> Retarget {
        if self.phase != Phase::Dragging || self.target.map(|t| t.window) == now.map(|t| t.window) {
            return Retarget::Same;
        }
        let left = std::mem::replace(&mut self.target, now);
        self.accepted = None;
        Retarget::Changed { left }
    }

    /// An `AcceptDrop` from `token`. `None` when it is ignored (not the
    /// target, or not dragging any more — a stale answer is a race);
    /// otherwise whether the offer now counts as accepted. A mismatch —
    /// an empty or unoffered MIME type, `None`, or an action the source
    /// did not offer — is a rejection, not an error.
    pub fn accept(&mut self, token: u64, action: DragAction, mime: String) -> Option<bool> {
        if self.phase != Phase::Dragging || self.target.is_none_or(|t| t.token != token) {
            return None;
        }
        let bit = action_bit(action);
        let ok = bit != 0 && self.actions & bit != 0 && self.mimes.contains(&mime);
        self.accepted = ok.then_some((action, mime));
        Some(ok)
    }

    /// The last button came up: drop on an accepting target, or reject.
    pub fn release(&mut self) -> Outcome {
        if self.phase != Phase::Dragging {
            return Outcome::Nothing;
        }
        match (self.target, &self.accepted) {
            (Some(t), Some(_)) => {
                self.phase = Phase::Dropped;
                Outcome::Drop(t)
            }
            _ => self.cancel(),
        }
    }

    /// Escape, a lock, a VT switch: end the gesture as a rejected drop.
    pub fn cancel(&mut self) -> Outcome {
        if self.phase != Phase::Dragging {
            return Outcome::Nothing;
        }
        self.phase = Phase::Finished;
        self.accepted = None;
        Outcome::Finished {
            leave: self.target.take(),
            accepted: false,
            action: DragAction::None,
        }
    }

    /// A `FinishDrag` from `token`. Disambiguated by phase, which is what
    /// makes it work when source and target are one client: in `Dropped`
    /// only the target's completes the drop, in `Finished` only the
    /// source's releases the drag, and a source giving up in `Dragging` or
    /// `Dropped` releases it with the target told (a cancel: the grab
    /// ends, and no `DragFinished` follows). Anything else is a race and
    /// ignored.
    pub fn finish(&mut self, token: u64) -> Outcome {
        match self.phase {
            Phase::Dropped if self.target.is_some_and(|t| t.token == token) => {
                self.phase = Phase::Finished;
                self.target = None;
                let action = self.accepted.take().map_or(DragAction::None, |(a, _)| a);
                Outcome::Finished {
                    leave: None,
                    accepted: true,
                    action,
                }
            }
            Phase::Dragging | Phase::Dropped | Phase::Finished if token == self.source => {
                Outcome::Released {
                    leave: self.target.take(),
                }
            }
            _ => Outcome::Nothing,
        }
    }

    /// A client disconnected. The source going ends everything (the
    /// target is told); a target going mid-drag leaves the drag carrying
    /// on over nothing, and one going after the drop fails it.
    pub fn forget_client(&mut self, token: u64) -> Outcome {
        if token == self.source {
            let leave = self.target.take().filter(|t| t.token != token);
            return Outcome::Released { leave };
        }
        if self.target.is_some_and(|t| t.token == token) {
            return self.target_gone();
        }
        Outcome::Nothing
    }

    /// A window was destroyed. The icon is simply dropped; the target
    /// window going is its client going, minus the message it can no
    /// longer be sent.
    pub fn forget_window(&mut self, win: WindowKey) -> Outcome {
        if self.icon == Some(win) {
            self.icon = None;
        }
        if self.target.is_some_and(|t| t.window == win) {
            return self.target_gone();
        }
        Outcome::Nothing
    }

    fn target_gone(&mut self) -> Outcome {
        self.target = None;
        self.accepted = None;
        match self.phase {
            Phase::Dropped => {
                self.phase = Phase::Finished;
                Outcome::REJECTED
            }
            Phase::Dragging | Phase::Finished => Outcome::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CB: DataSource = DataSource::Clipboard;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_empty_offer_clears() {
        let mut s = Selections::new();
        assert!(s.set_offer(1, mimes(&["text/plain"])).is_empty());
        assert_eq!(s.mimes(), ["text/plain"]);
        assert_eq!(s.offer().unwrap().owner, 1);
        s.set_offer(1, Vec::new());
        assert!(s.offer().is_none());
        assert!(s.mimes().is_empty());
    }

    #[test]
    fn a_new_offer_cancels_every_parked_transfer() {
        let mut s = Selections::new();
        s.set_offer(1, mimes(&["text/plain"]));
        let a = s.start(2, 10, 1, CB);
        let b = s.start(3, 10, 1, CB);
        let cancelled = s.set_offer(4, mimes(&["image/png"]));
        let ids: Vec<u32> = cancelled.iter().map(|t| t.id).collect();
        assert_eq!(ids, [a, b]);
        assert_eq!(s.pending(), 0);
        // And the late answer from the old owner is stale.
        assert!(s.take(a, 1).is_none());
    }

    #[test]
    fn the_same_owner_replacing_its_selection_also_cancels() {
        let mut s = Selections::new();
        s.set_offer(1, mimes(&["text/plain"]));
        let a = s.start(2, 10, 1, CB);
        let cancelled = s.set_offer(1, mimes(&["text/html"]));
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].id, a);
    }

    #[test]
    fn forgetting_a_client_partitions_owed_and_made() {
        let mut s = Selections::new();
        s.set_offer(1, mimes(&["text/plain"]));
        let owed = s.start(2, 7, 1, CB);
        s.start(1, 8, 1, CB); // a self-paste: made *and* owed by 1
        let (was_owner, list) = s.forget_client(1);
        assert!(was_owner);
        assert!(s.offer().is_none());
        // The self-paste is dropped (nobody to answer), the other is owed.
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, owed);
        assert_eq!(list[0].reply_to, 7);
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn forgetting_a_requester_drops_only_its_requests() {
        let mut s = Selections::new();
        s.set_offer(1, mimes(&["text/plain"]));
        s.start(2, 7, 1, CB);
        let kept = s.start(3, 7, 1, CB);
        let (was_owner, owed) = s.forget_client(2);
        assert!(!was_owner);
        assert!(owed.is_empty());
        assert_eq!(s.pending(), 1);
        assert_eq!(s.offer().unwrap().owner, 1);
        assert!(s.take(kept, 1).is_some());
    }

    #[test]
    fn outstanding_and_duplicates_are_per_connection() {
        let mut s = Selections::new();
        s.start(2, 1, 1, CB);
        s.start(2, 2, 1, CB);
        s.start(3, 1, 1, CB);
        assert_eq!(s.outstanding(2), 2);
        assert_eq!(s.outstanding(3), 1);
        assert_eq!(s.outstanding(4), 0);
        assert!(s.has_reply_id(2, 1));
        assert!(s.has_reply_id(3, 1));
        assert!(!s.has_reply_id(3, 2));
    }

    #[test]
    fn take_needs_the_right_owner() {
        let mut s = Selections::new();
        let id = s.start(2, 1, 1, CB);
        assert!(s.take(id, 9).is_none(), "another client cannot answer");
        assert!(s.take(id + 1, 1).is_none(), "unknown id");
        let t = s.take(id, 1).unwrap();
        assert_eq!((t.requester, t.reply_to), (2, 1));
        assert!(s.take(id, 1).is_none(), "answered once");
    }

    #[test]
    fn ids_skip_zero_and_live_ids_across_wraparound() {
        let mut sel = Selections::new();
        sel.next_id = u32::MAX - 2;
        let first = sel.start(2, 1, 1, CB); // MAX - 1
        let second = sel.start(2, 2, 1, CB); // MAX
        let third = sel.start(2, 3, 1, CB); // wraps past 0 to 1
        assert_eq!((first, second, third), (u32::MAX - 1, u32::MAX, 1));
        // Park at 2, then force the counter to collide with live ids.
        let fourth = sel.start(2, 4, 1, CB);
        assert_eq!(fourth, 2);
        sel.next_id = u32::MAX - 2;
        let fifth = sel.start(2, 5, 1, CB);
        assert_eq!(fifth, 3, "MAX-1, MAX, 0, 1 and 2 are live or reserved");
    }

    #[test]
    fn mime_validation() {
        assert_eq!(validate_mimes(&[]), Ok(()));
        assert_eq!(
            validate_mimes(&mimes(&["text/plain;charset=utf-8"])),
            Ok(())
        );
        let many = vec!["a/b".to_owned(); MAX_MIMES];
        assert_eq!(validate_mimes(&many), Ok(()));
        let too_many = vec!["a/b".to_owned(); MAX_MIMES + 1];
        assert_eq!(validate_mimes(&too_many), Err(MimeError::TooMany));
        let long = "x".repeat(MAX_MIME_LEN);
        assert_eq!(validate_mimes(std::slice::from_ref(&long)), Ok(()));
        let too_long = "x".repeat(MAX_MIME_LEN + 1);
        assert_eq!(validate_mimes(&[too_long]), Err(MimeError::TooLong));
        assert_eq!(
            validate_mimes(&mimes(&["text/plain", ""])),
            Err(MimeError::Empty)
        );
        assert_eq!(
            validate_mimes(&mimes(&["text/ü"])),
            Err(MimeError::NotAscii)
        );
        assert!(MimeError::TooMany.is_limit());
        assert!(MimeError::TooLong.is_limit());
        assert!(!MimeError::Empty.is_limit());
        assert!(!MimeError::NotAscii.is_limit());
    }

    // ------------------------------------------------ drag and drop

    const A: u64 = 1;
    const B: u64 = 2;

    fn win(i: u32) -> WindowKey {
        WindowKey::from_parts(i, 1)
    }

    fn tgt(token: u64, i: u32) -> DragTarget {
        DragTarget {
            token,
            window: win(i),
        }
    }

    fn drag() -> Dnd {
        Dnd::new(
            A,
            None,
            drag_actions::COPY | drag_actions::MOVE,
            mimes(&["text/plain"]),
        )
    }

    #[test]
    fn actions_need_a_bit_and_no_reserved_one() {
        assert!(valid_actions(drag_actions::COPY));
        assert!(valid_actions(drag_actions::ALL));
        assert!(!valid_actions(0));
        assert!(!valid_actions(8));
    }

    #[test]
    fn a_full_accepted_drop() {
        let mut d = drag();
        assert!(d.grabbing());
        assert_eq!(
            d.retarget(Some(tgt(A, 1))),
            Retarget::Changed { left: None }
        );
        assert_eq!(d.retarget(Some(tgt(A, 1))), Retarget::Same);
        assert_eq!(
            d.retarget(Some(tgt(B, 2))),
            Retarget::Changed {
                left: Some(tgt(A, 1))
            }
        );
        assert!(d.is_drop_target(B));
        assert!(!d.is_drop_target(A));
        assert_eq!(
            d.accept(A, DragAction::Copy, "text/plain".into()),
            None,
            "not the target"
        );
        assert_eq!(
            d.accept(B, DragAction::Copy, "text/plain".into()),
            Some(true)
        );
        assert_eq!(d.release(), Outcome::Drop(tgt(B, 2)));
        assert_eq!(d.phase(), Phase::Dropped);
        assert!(!d.grabbing());
        assert!(d.is_drop_target(B), "still reading");
        assert_eq!(
            d.accept(B, DragAction::None, String::new()),
            None,
            "frozen at drop"
        );
        assert_eq!(
            d.finish(A),
            Outcome::Released {
                leave: Some(tgt(B, 2))
            },
            "source gave up"
        );
    }

    #[test]
    fn the_target_finish_then_the_source_finish() {
        let mut d = drag();
        d.retarget(Some(tgt(B, 2)));
        d.accept(B, DragAction::Move, "text/plain".into());
        d.release();
        assert_eq!(
            d.finish(B),
            Outcome::Finished {
                leave: None,
                accepted: true,
                action: DragAction::Move
            }
        );
        assert!(!d.is_drop_target(B));
        assert_eq!(d.finish(B), Outcome::Nothing, "a stray target finish");
        assert_eq!(d.finish(A), Outcome::Released { leave: None });
    }

    #[test]
    fn the_source_finishing_mid_drag_cancels_it() {
        let mut d = drag();
        d.retarget(Some(tgt(B, 2)));
        d.accept(B, DragAction::Copy, "text/plain".into());
        assert_eq!(d.finish(B), Outcome::Nothing, "the target cannot, mid-drag");
        assert!(d.grabbing());
        assert_eq!(
            d.finish(A),
            Outcome::Released {
                leave: Some(tgt(B, 2))
            }
        );
        assert_eq!(d.target(), None);

        // Over its own window, the source's own window is told too.
        let mut d = drag();
        d.retarget(Some(tgt(A, 1)));
        assert_eq!(
            d.finish(A),
            Outcome::Released {
                leave: Some(tgt(A, 1))
            }
        );
    }

    #[test]
    fn source_and_target_one_client_is_disambiguated_by_phase() {
        let mut d = drag();
        d.retarget(Some(tgt(A, 1)));
        d.accept(A, DragAction::Copy, "text/plain".into());
        assert_eq!(d.release(), Outcome::Drop(tgt(A, 1)));
        assert!(matches!(
            d.finish(A),
            Outcome::Finished { accepted: true, .. }
        ));
        assert_eq!(d.finish(A), Outcome::Released { leave: None });
    }

    #[test]
    fn mismatches_reject_and_a_target_change_forgets_acceptance() {
        let mut d = drag();
        d.retarget(Some(tgt(B, 2)));
        assert_eq!(
            d.accept(B, DragAction::Link, "text/plain".into()),
            Some(false),
            "not offered"
        );
        assert_eq!(
            d.accept(B, DragAction::Copy, "image/png".into()),
            Some(false),
            "no such mime"
        );
        assert_eq!(
            d.accept(B, DragAction::None, "text/plain".into()),
            Some(false)
        );
        assert_eq!(d.accept(B, DragAction::Copy, String::new()), Some(false));
        assert_eq!(
            d.accept(B, DragAction::Copy, "text/plain".into()),
            Some(true)
        );
        d.retarget(Some(tgt(B, 3)));
        assert!(!d.accepted(), "a new window has said nothing yet");
        assert_eq!(
            d.release(),
            Outcome::Finished {
                leave: Some(tgt(B, 3)),
                accepted: false,
                action: DragAction::None
            }
        );
        assert_eq!(d.phase(), Phase::Finished);
        assert_eq!(d.finish(B), Outcome::Nothing, "only the source releases");
        assert_eq!(d.finish(A), Outcome::Released { leave: None });
    }

    #[test]
    fn a_release_over_nothing_rejects() {
        let mut d = drag();
        assert_eq!(d.release(), Outcome::REJECTED);
        assert_eq!(d.release(), Outcome::Nothing, "once");
    }

    #[test]
    fn the_source_going_releases_and_tells_the_target() {
        let mut d = drag();
        d.retarget(Some(tgt(B, 2)));
        assert_eq!(
            d.forget_client(A),
            Outcome::Released {
                leave: Some(tgt(B, 2))
            }
        );
        let mut d = drag();
        d.retarget(Some(tgt(A, 1)));
        assert_eq!(
            d.forget_client(A),
            Outcome::Released { leave: None },
            "nobody to tell"
        );
    }

    #[test]
    fn the_target_going_mid_drag_carries_on_and_after_the_drop_fails() {
        let mut d = drag();
        d.retarget(Some(tgt(B, 2)));
        d.accept(B, DragAction::Copy, "text/plain".into());
        assert_eq!(d.forget_client(B), Outcome::Nothing);
        assert!(d.grabbing() && d.target().is_none() && !d.accepted());
        assert_eq!(d.release(), Outcome::REJECTED);

        let mut d = drag();
        d.retarget(Some(tgt(B, 2)));
        d.accept(B, DragAction::Copy, "text/plain".into());
        d.release();
        assert_eq!(d.forget_window(win(2)), Outcome::REJECTED);
        assert_eq!(d.phase(), Phase::Finished);
        assert_eq!(d.forget_client(9), Outcome::Nothing);
    }

    #[test]
    fn the_icon_window_going_drops_the_icon() {
        let mut d = Dnd::new(A, Some(win(7)), drag_actions::COPY, mimes(&["a/b"]));
        assert_eq!(d.forget_window(win(7)), Outcome::Nothing);
        assert_eq!(d.icon, None);
        assert!(d.grabbing());
    }

    #[test]
    fn a_new_clipboard_offer_leaves_drag_transfers_alone() {
        let mut s = Selections::new();
        let c = s.start(2, 1, 1, CB);
        let d = s.start(2, 2, 1, DataSource::Drag);
        let gone = s.set_offer(3, mimes(&["x/y"]));
        assert_eq!(gone.iter().map(|t| t.id).collect::<Vec<_>>(), [c]);
        let gone = s.cancel_drag();
        assert_eq!(gone.iter().map(|t| t.id).collect::<Vec<_>>(), [d]);
        assert_eq!(s.pending(), 0);
    }
}

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

    /// A human-readable reason, for the `Error` detail.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::TooMany => "SetSelection offers more than 64 MIME types",
            Self::TooLong => "SetSelection names a MIME type longer than 256 bytes",
            Self::Empty => "SetSelection names an empty MIME type",
            Self::NotAscii => "SetSelection names a MIME type that is not ASCII",
        }
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
    pub fn set_offer(&mut self, owner: u64, mimes: Vec<String>) -> Vec<Transfer> {
        self.offer = if mimes.is_empty() {
            None
        } else {
            Some(Offer { owner, mimes })
        };
        std::mem::take(&mut self.transfers)
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
    pub fn start(&mut self, requester: u64, reply_to: u32, owner: u64) -> u32 {
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            let id = self.next_id;
            if id != 0 && !self.transfers.iter().any(|t| t.id == id) {
                self.transfers.push(Transfer {
                    id,
                    requester,
                    reply_to,
                    owner,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let a = s.start(2, 10, 1);
        let b = s.start(3, 10, 1);
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
        let a = s.start(2, 10, 1);
        let cancelled = s.set_offer(1, mimes(&["text/html"]));
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].id, a);
    }

    #[test]
    fn forgetting_a_client_partitions_owed_and_made() {
        let mut s = Selections::new();
        s.set_offer(1, mimes(&["text/plain"]));
        let owed = s.start(2, 7, 1);
        s.start(1, 8, 1); // a self-paste: made *and* owed by 1
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
        s.start(2, 7, 1);
        let kept = s.start(3, 7, 1);
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
        s.start(2, 1, 1);
        s.start(2, 2, 1);
        s.start(3, 1, 1);
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
        let id = s.start(2, 1, 1);
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
        let first = sel.start(2, 1, 1); // MAX - 1
        let second = sel.start(2, 2, 1); // MAX
        let third = sel.start(2, 3, 1); // wraps past 0 to 1
        assert_eq!((first, second, third), (u32::MAX - 1, u32::MAX, 1));
        // Park at 2, then force the counter to collide with live ids.
        let fourth = sel.start(2, 4, 1);
        assert_eq!(fourth, 2);
        sel.next_id = u32::MAX - 2;
        let fifth = sel.start(2, 5, 1);
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
}

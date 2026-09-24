//! The session lock: who holds it, and who may take it or give it up.
//!
//! Policy only, like `shell.rs`: no scene, no sockets, testable with
//! plain numbers. The server applies the answer in two places. The scene
//! paints and hit-tests only the owner's windows (`nitro_scene::Admit`),
//! and every input path asks the scene whether a window is admitted. See
//! `docs/shell.md`, "The session lock".
//!
//! The rules are `ext-session-lock`'s:
//!
//! - A shell client **locks** with `Lock`, and is then the **owner**.
//! - Only the owner **unlocks**. From anyone else, `Unlock` is fatal.
//! - An owner that **disconnects** leaves the session locked with no
//!   owner. A crashed lock screen must never be a way in.
//! - A lock with no owner is **taken over** by the next `Lock`. That is
//!   how a restarted lock screen resumes, and how a server started locked
//!   (`NITRO_LOCKED=1`) gets its first owner.
//! - A second `Lock` while someone else owns it is fatal: two lock
//!   screens at once is a bug in whoever started the second.

/// The lock state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lock {
    /// The ordinary state.
    #[default]
    Unlocked,
    /// Locked. `owner` is the epoll token of the connection that may
    /// unlock, `None` while nobody holds it.
    Locked {
        /// The owning connection, if any.
        owner: Option<u64>,
    },
}

/// Why a lock request was refused. Each is fatal for the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// `Lock` while another connection owns the lock.
    Owned,
    /// `Unlock` from a connection that does not own the lock.
    NotOwner,
    /// `Unlock` while the session is not locked.
    NotLocked,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Owned => "the session lock is held by another connection",
            Self::NotOwner => "only the lock owner may unlock the session",
            Self::NotLocked => "the session is not locked",
        })
    }
}

/// What a successful `Lock` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claimed {
    /// The session was unlocked and is now locked.
    Locked,
    /// The session was locked with no owner; the sender owns it now.
    TookOver,
    /// The sender already owned it.
    Already,
}

impl Lock {
    /// A session that starts locked, with no owner yet.
    #[must_use]
    pub fn locked() -> Self {
        Self::Locked { owner: None }
    }

    /// Whether the session is locked.
    #[must_use]
    pub fn is_locked(self) -> bool {
        matches!(self, Self::Locked { .. })
    }

    /// The owning connection, if the session is locked and someone owns it.
    #[must_use]
    pub fn owner(self) -> Option<u64> {
        match self {
            Self::Locked { owner } => owner,
            Self::Unlocked => None,
        }
    }

    /// `Lock` from `token`.
    ///
    /// # Errors
    /// [`Refused::Owned`] when another connection owns the lock.
    pub fn claim(&mut self, token: u64) -> Result<Claimed, Refused> {
        match *self {
            Self::Unlocked => {
                *self = Self::Locked { owner: Some(token) };
                Ok(Claimed::Locked)
            }
            Self::Locked { owner: None } => {
                *self = Self::Locked { owner: Some(token) };
                Ok(Claimed::TookOver)
            }
            Self::Locked { owner: Some(o) } if o == token => Ok(Claimed::Already),
            Self::Locked { owner: Some(_) } => Err(Refused::Owned),
        }
    }

    /// `Unlock` from `token`.
    ///
    /// # Errors
    /// [`Refused::NotLocked`] or [`Refused::NotOwner`].
    pub fn release(&mut self, token: u64) -> Result<(), Refused> {
        match *self {
            Self::Unlocked => Err(Refused::NotLocked),
            Self::Locked { owner: Some(o) } if o == token => {
                *self = Self::Unlocked;
                Ok(())
            }
            Self::Locked { .. } => Err(Refused::NotOwner),
        }
    }

    /// `token` disconnected. Returns whether that changed anything: it was
    /// the owner, and the lock is now ownerless (and still locked).
    pub fn forget(&mut self, token: u64) -> bool {
        if self.owner() == Some(token) {
            *self = Self::Locked { owner: None };
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u64 = 7;
    const B: u64 = 9;

    #[test]
    fn a_lock_is_owned_by_whoever_took_it_and_released_only_by_them() {
        let mut l = Lock::default();
        assert!(!l.is_locked());
        assert_eq!(l.claim(A), Ok(Claimed::Locked));
        assert_eq!(l.owner(), Some(A));
        assert_eq!(l.claim(A), Ok(Claimed::Already), "idempotent for the owner");
        assert_eq!(l.claim(B), Err(Refused::Owned));
        assert_eq!(l.release(B), Err(Refused::NotOwner));
        assert!(l.is_locked(), "a refused unlock changed nothing");
        assert_eq!(l.release(A), Ok(()));
        assert_eq!(l, Lock::Unlocked);
    }

    #[test]
    fn unlocking_an_unlocked_session_is_refused() {
        let mut l = Lock::Unlocked;
        assert_eq!(l.release(A), Err(Refused::NotLocked));
    }

    #[test]
    fn an_owner_that_goes_away_leaves_the_session_locked() {
        let mut l = Lock::Unlocked;
        l.claim(A).unwrap();
        assert!(!l.forget(B), "someone else leaving changes nothing");
        assert_eq!(l.owner(), Some(A));
        assert!(l.forget(A));
        assert!(l.is_locked());
        assert_eq!(l.owner(), None);
        // And nobody can unlock an ownerless lock: not the old owner's
        // token, not anyone else's.
        assert_eq!(l.release(A), Err(Refused::NotOwner));
        assert_eq!(l.release(B), Err(Refused::NotOwner));
    }

    #[test]
    fn an_ownerless_lock_is_taken_over_by_the_next_claim() {
        let mut l = Lock::locked();
        assert!(l.is_locked());
        assert_eq!(l.owner(), None);
        assert_eq!(l.claim(B), Ok(Claimed::TookOver));
        assert_eq!(l.owner(), Some(B));
        assert_eq!(l.claim(A), Err(Refused::Owned));
        assert_eq!(l.release(B), Ok(()));
    }

    #[test]
    fn forgetting_while_unlocked_does_not_lock() {
        let mut l = Lock::Unlocked;
        assert!(!l.forget(A));
        assert_eq!(l, Lock::Unlocked);
    }
}

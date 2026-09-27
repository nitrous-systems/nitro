//! Key repeat: a held key keeps typing.
//!
//! # Where it lives, and why here
//!
//! libinput reports one press and one release however long a key is
//! held, so *something* has to synthesise the presses in between. It is
//! the server, for the reason X does it centrally: every client gets
//! repeat for free — `nitro-term`, every toolkit app, and any future
//! client that never links the toolkit — and it behaves the same
//! desktop-wide, from one `keyboard.repeat` line. The alternative, a
//! repeat loop in `nitro-ui`, would leave every non-toolkit client to
//! write its own and let two apps disagree about the rate.
//!
//! The one exception is a **`KEYMAP` client** (M5-C): it owns its own
//! `xkb_state` and, like a Wayland client, repeats on its own using the
//! `rate_hz`/`delay_ms` the `Keymap` message carries. The server sends
//! such a client exactly one press, or every key would repeat twice.
//!
//! # What repeats
//!
//! Only a key that was **delivered to a client** — the last step of
//! `Server::route_key`. A compositor hotkey, a shell binding, a popup's
//! Escape and a withheld key all return before that step, so holding
//! Super+Q closes one window, not the whole MRU list. Of the delivered
//! keys, the keymap decides: xkb marks modifiers (and a few others) as
//! non-repeating, and [`crate::keyboard::Keyboard::repeats`] asks it,
//! with [`crate::keyboard::mod_of_keysym`] as a belt to its braces.
//!
//! One key repeats at a time, the most recently pressed, as on every
//! other desktop: pressing `b` while `a` is held switches the repeat to
//! `b`, and releasing `a` afterwards leaves `b` repeating.
//!
//! # When it stops
//!
//! On the key's release; on the next press of any other non-modifier key;
//! on a focus change (a key held while focus moves must not keep typing
//! into the new window — the classic stuck-key bug); on a keyboard-grab
//! change; on every keyboard reset (VT switch, a keyboard unplugged, a
//! keymap reload); when the session goes inactive; and, checked at every
//! repeat, whenever the recipient the press went to is no longer the one
//! keys would go to now.
//!
//! # The timer
//!
//! One `CLOCK_MONOTONIC` timerfd in the epoll set, armed with an absolute
//! deadline only while a key is actually repeating and disarmed the
//! moment it stops — the same bargain [`crate::defer`] makes, so an idle
//! desktop still makes zero wakeups. Each expiry re-arms for the next
//! repeat; nothing polls.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use nitro_scene::WindowKey;
use rustix::io::Errno;
use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};

use crate::config::Repeat;

/// Nanoseconds in a millisecond.
const NS_PER_MS: u64 = 1_000_000;
/// Nanoseconds in a second.
const NS_PER_SEC: u64 = 1_000_000_000;

/// The key being held, and when it next repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    /// The evdev keycode, as the press carried it.
    pub keycode: u32,
    /// The window the press was delivered to. A repeat goes only there.
    pub window: WindowKey,
    /// Absolute `CLOCK_MONOTONIC` deadline of the next repeat.
    next_ns: u64,
}

/// The repeat timer and the one key it is repeating.
#[derive(Debug)]
pub struct KeyRepeat {
    /// Non-blocking timerfd, in the server's epoll set for the whole run.
    /// Armed only while [`KeyRepeat::held`] is `Some`.
    timer: OwnedFd,
    held: Option<Held>,
    /// Repeats synthesised so far: `key_repeats` in `stats`.
    pub repeats: u64,
}

impl KeyRepeat {
    /// Create the timer, disarmed.
    ///
    /// # Errors
    /// If `timerfd_create` fails.
    pub fn new() -> rustix::io::Result<Self> {
        let timer = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )?;
        Ok(Self {
            timer,
            held: None,
            repeats: 0,
        })
    }

    /// The fd to add to the epoll set.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.timer.as_fd()
    }

    /// The key repeating now, if any.
    #[must_use]
    pub fn held(&self) -> Option<Held> {
        self.held
    }

    /// Start repeating `keycode` into `window`, `repeat.delay_ms` after
    /// `now_ns`. Replaces whatever was repeating. A disabled repeat
    /// (`rate 0`) just stops.
    ///
    /// # Errors
    /// The `timerfd_settime` failure; the key is then simply not repeated.
    pub fn start(
        &mut self,
        keycode: u32,
        window: WindowKey,
        now_ns: u64,
        repeat: Repeat,
    ) -> rustix::io::Result<()> {
        if !repeat.enabled() {
            return self.stop();
        }
        let next_ns = now_ns.saturating_add(u64::from(repeat.delay_ms) * NS_PER_MS);
        crate::defer::arm(self.timer.as_fd(), next_ns)?;
        self.held = Some(Held {
            keycode,
            window,
            next_ns,
        });
        Ok(())
    }

    /// Stop repeating. A no-op (and no syscall) when nothing is.
    ///
    /// # Errors
    /// The `timerfd_settime` failure. A timer that would not disarm costs
    /// one spurious wakeup, which [`KeyRepeat::fire`] answers with `None`.
    pub fn stop(&mut self) -> rustix::io::Result<()> {
        if self.held.take().is_none() {
            return Ok(());
        }
        let disarmed = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        };
        timerfd_settime(self.timer.as_fd(), TimerfdTimerFlags::empty(), &disarmed)?;
        // An expiration that already happened survives the disarm, and a
        // level-triggered epoll would then wake for it on every turn.
        self.drain();
        Ok(())
    }

    /// The timer fired: return the key to repeat now and re-arm for the
    /// next one, `1 / rate_hz` later.
    ///
    /// The next deadline is measured from the one that just passed, so
    /// the rate does not drift with wakeup latency — but never from before
    /// `now_ns`: a loop that was blocked for half a second sends **one**
    /// repeat when it wakes, not the dozen it "owes". A burst of catch-up
    /// keystrokes is exactly what a user who saw nothing happen does not
    /// want to see next.
    ///
    /// `None` for a stale expiry (nothing held, or the deadline not yet
    /// reached) and when the rate has become 0; the latter also stops.
    ///
    /// # Errors
    /// The `timerfd_settime` failure; the repeat is stopped.
    pub fn fire(&mut self, now_ns: u64, repeat: Repeat) -> rustix::io::Result<Option<Held>> {
        self.drain();
        let Some(held) = self.held else {
            return Ok(None);
        };
        if now_ns < held.next_ns {
            return Ok(None);
        }
        if !repeat.enabled() {
            self.stop()?;
            return Ok(None);
        }
        let period = NS_PER_SEC / u64::from(repeat.rate_hz);
        let next_ns = held.next_ns.saturating_add(period).max(now_ns + 1);
        if let Err(e) = crate::defer::arm(self.timer.as_fd(), next_ns) {
            self.held = None;
            return Err(e);
        }
        self.held = Some(Held { next_ns, ..held });
        self.repeats += 1;
        Ok(Some(held))
    }

    /// Swallow a pending expiration, if any.
    fn drain(&self) {
        let mut buf = [0u8; 8];
        match rustix::io::read(self.timer.as_fd(), &mut buf[..]) {
            Ok(_) | Err(Errno::AGAIN | Errno::INTR) => {}
            Err(e) => crate::warn!("repeat timer: read: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::event::{PollFd, PollFlags, poll};
    use rustix::time::{ClockId, clock_gettime};

    fn now_ns() -> u64 {
        let t = clock_gettime(ClockId::Monotonic);
        t.tv_sec.cast_unsigned() * NS_PER_SEC + t.tv_nsec.cast_unsigned()
    }

    fn readable_within(r: &KeyRepeat, ms: i64) -> bool {
        let fd = r.as_fd();
        let mut pfd = [PollFd::new(&fd, PollFlags::IN)];
        let t = Timespec {
            tv_sec: 0,
            tv_nsec: ms * 1_000_000,
        };
        poll(&mut pfd, Some(&t)).unwrap() == 1
    }

    fn window() -> WindowKey {
        WindowKey::from_parts(1, 1)
    }

    const FAST: Repeat = Repeat {
        delay_ms: 100,
        rate_hz: 50,
    };

    #[test]
    fn an_idle_repeat_never_wakes() {
        let r = KeyRepeat::new().unwrap();
        assert!(r.held().is_none());
        assert!(!readable_within(&r, 20), "idle must be zero wakeups");
    }

    #[test]
    fn the_first_repeat_waits_the_delay_then_the_rate_follows() {
        let mut r = KeyRepeat::new().unwrap();
        let t0 = now_ns();
        r.start(30, window(), t0, FAST).unwrap();
        assert!(!readable_within(&r, 50), "not before the delay");
        assert!(readable_within(&r, 200), "the delay elapsed");
        let held = r.fire(now_ns(), FAST).unwrap().expect("a repeat");
        assert_eq!(held.keycode, 30);
        assert!(now_ns() - t0 >= 100 * NS_PER_MS);
        // Then every 20 ms.
        assert!(readable_within(&r, 100));
        assert!(r.fire(now_ns(), FAST).unwrap().is_some());
        assert_eq!(r.repeats, 2);
    }

    #[test]
    fn stopping_disarms_and_clears_a_pending_expiry() {
        let mut r = KeyRepeat::new().unwrap();
        // A deadline already in the past fires at once.
        r.start(30, window(), 0, FAST).unwrap();
        assert!(readable_within(&r, 50));
        r.stop().unwrap();
        assert!(r.held().is_none());
        assert!(!readable_within(&r, 150), "no wakeup after a stop");
        // A stale expiry is not a repeat.
        assert_eq!(r.fire(now_ns(), FAST).unwrap(), None);
        assert_eq!(r.repeats, 0);
    }

    #[test]
    fn a_rate_of_zero_never_starts() {
        let mut r = KeyRepeat::new().unwrap();
        let off = Repeat {
            delay_ms: 100,
            rate_hz: 0,
        };
        r.start(30, window(), 0, off).unwrap();
        assert!(r.held().is_none());
        assert!(!readable_within(&r, 50));
    }

    #[test]
    fn a_late_wakeup_sends_one_repeat_not_a_burst() {
        let mut r = KeyRepeat::new().unwrap();
        r.start(30, window(), 0, FAST).unwrap();
        // "Blocked" for a second: the deadline is far behind.
        let late = now_ns();
        assert!(r.fire(late, FAST).unwrap().is_some());
        // The next one is scheduled after now, not back at 0 + 20 ms.
        assert!(r.held().unwrap().next_ns > late);
        assert_eq!(r.fire(late, FAST).unwrap(), None, "not due yet");
    }

    #[test]
    fn a_new_key_replaces_the_old_one() {
        let mut r = KeyRepeat::new().unwrap();
        r.start(30, window(), 0, FAST).unwrap();
        r.start(48, window(), 0, FAST).unwrap();
        assert_eq!(r.held().unwrap().keycode, 48);
    }
}

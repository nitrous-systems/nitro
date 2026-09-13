//! Holding a cursor-only flip back until the client's answer arrives.
//!
//! # Why a flip is ever deferred
//!
//! A pointer move damages the **cursor** immediately — before the client
//! under the pointer has heard anything — so the naive schedule paints and
//! flips a cursor-only frame at once. The client's answering commit reaches
//! the server 0.12 ms later (measured, `docs/latency.md`), by which time a
//! flip is in flight and [`Server::paint`](crate::Server::paint) cannot
//! start another: the client's pixels ride the *following* vblank, one
//! whole frame behind the cursor that provoked them. That is the entire
//! 25 ms → 9 ms gap; 0.3 ms of it is work.
//!
//! So when the damage on a wakeup is cursor-only *and* an input event was
//! just routed to a client, the flip waits. It is released by whichever
//! comes first:
//!
//! * the client's commit — the wakeup after, usually the same one — which
//!   adds content damage and lets one flip carry cursor *and* content;
//! * the deadline, [`frame_deadline`](crate::frame::frame_deadline): the
//!   next expected vblank minus the frame margin. A client that never
//!   answers therefore costs nothing at all — the cursor still reaches
//!   that same vblank, because 2 ms is ten paint passes.
//!
//! # Why a timerfd
//!
//! The deadline has to wake a thread that is sitting in `epoll_wait` with
//! nothing else to do, and it must not wake it at any other time: "idle is
//! zero wakeups" is the property the whole server is built around. One
//! `CLOCK_MONOTONIC` timerfd in the epoll set, armed with an absolute
//! deadline only while a flip is actually held and disarmed the moment it
//! is released, is exactly that — and it costs one `timerfd_settime` per
//! deferral, not a poll timeout on every loop iteration.
//!
//! # What this module does and does not decide
//!
//! [`DeferredFlip`] is the mechanism: which clients are being waited on,
//! the timer, and the two counters `stats` reports. The *policy* — what
//! counts as cursor-only damage, and which deadline to aim for — is
//! [`Server::hold_flip`](crate::Server), because only the server can see
//! the outputs' damage.
//!
//! # Extending it
//!
//! The same shape generalises to holding a flip for a client that is known
//! to be responding (an animating client that has committed on every one
//! of the last N frames, say): the release condition is still "that
//! client's commit or the deadline", and only the predicate that decides
//! whether to wait would change. Nothing here assumes the trigger was an
//! input event.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::io::Errno;
use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};

/// A disarmed `itimerspec`: both fields zero, which is how
/// `timerfd_settime` is told to stop the timer.
const DISARMED: Itimerspec = Itimerspec {
    it_interval: Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    },
    it_value: Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    },
};

/// Nanoseconds in a second, for the `u64` → `timespec` split.
const NS_PER_SEC: u64 = 1_000_000_000;

/// The clients an input was just routed to, and the timer that puts a
/// bound on how long their answer is waited for.
#[derive(Debug)]
pub struct DeferredFlip {
    /// `CLOCK_MONOTONIC` timerfd, non-blocking, in the server's epoll set
    /// for the whole run. Armed only while a flip is held.
    timer: OwnedFd,
    /// The absolute deadline the timer is armed for, or `None` when no
    /// flip is being held.
    until_ns: Option<u64>,
    /// Epoll tokens of the clients that were told about an input and have
    /// not committed since. Small by construction — one, or two while the
    /// pointer crosses a window boundary — so a `Vec` beats a hash set.
    awaiting: Vec<u64>,
    /// Flips held back so far, counted once per episode: `flips_deferred`.
    pub deferred: u64,
    /// Episodes that ended at the deadline instead of at a commit:
    /// `defer_timeouts`. A number that climbs is a client that is not
    /// answering — which is exactly what the deadline exists for.
    pub timeouts: u64,
}

impl DeferredFlip {
    /// Create the timer. It starts disarmed, so registering it costs
    /// nothing until a flip is actually held.
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
            until_ns: None,
            awaiting: Vec::new(),
            deferred: 0,
            timeouts: 0,
        })
    }

    /// The fd to add to the epoll set.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.timer.as_fd()
    }

    /// Note that `token`'s client was told about an input and its answer
    /// is worth waiting for. Idempotent.
    pub fn expect(&mut self, token: u64) {
        if !self.awaiting.contains(&token) {
            self.awaiting.push(token);
        }
    }

    /// Stop waiting for one client: it committed, or it went away.
    pub fn forget(&mut self, token: u64) {
        self.awaiting.retain(|t| *t != token);
    }

    /// Stop waiting for everyone: the frame they would have ridden has
    /// been painted, so the episode is over however it ended.
    pub fn forget_all(&mut self) {
        self.awaiting.clear();
    }

    /// Whether any client's answer is still outstanding.
    #[must_use]
    pub fn awaiting(&self) -> bool {
        !self.awaiting.is_empty()
    }

    /// Whether a flip is currently being held.
    #[must_use]
    pub fn holding(&self) -> bool {
        self.until_ns.is_some()
    }

    /// Hold the flip until `deadline_ns` (`CLOCK_MONOTONIC`), arming the
    /// timer if it is not already set for that moment or earlier.
    ///
    /// Re-arming is skipped while an *earlier* deadline is already
    /// pending: a burst of pointer motion inside one frame period must not
    /// keep pushing the cursor's own backstop further out.
    ///
    /// Returns whether the flip is held. `false` means the timer could not
    /// be armed, and the caller must paint now rather than risk a frame
    /// nothing would ever wake it for.
    ///
    /// # Errors
    /// The `Err` carries the `timerfd_settime` failure for logging; the
    /// caller should treat it as "do not defer".
    pub fn hold_until(&mut self, deadline_ns: u64) -> rustix::io::Result<()> {
        if self.until_ns.is_none_or(|until| deadline_ns < until) {
            arm(self.timer.as_fd(), deadline_ns)?;
            if self.until_ns.is_none() {
                self.deferred += 1;
            }
            self.until_ns = Some(deadline_ns);
        }
        Ok(())
    }

    /// The deadline passed: drain the expiration and end the episode.
    ///
    /// Counts a `defer_timeouts` only if a flip really was being held —
    /// a stale expiration read after the flip was already released is not
    /// a client failing to answer.
    pub fn expired(&mut self) {
        self.drain();
        if self.until_ns.take().is_some() {
            self.timeouts += 1;
        }
        self.awaiting.clear();
    }

    /// Stop holding: disarm the timer. The set of clients being waited
    /// for is *not* cleared — that is [`DeferredFlip::forget_all`], and
    /// the two are separate because a wakeup that could not paint (a flip
    /// was still in flight) must keep the wait alive for the wakeup that
    /// can, or the saturating-input case loses the answer it was holding
    /// for.
    ///
    /// # Errors
    /// The `Err` carries the `timerfd_settime` failure for logging. A
    /// timer that will not disarm can only cost one spurious wakeup,
    /// which [`DeferredFlip::expired`] absorbs.
    pub fn disarm(&mut self) -> rustix::io::Result<()> {
        if self.until_ns.take().is_none() {
            return Ok(());
        }
        timerfd_settime(self.timer.as_fd(), TimerfdTimerFlags::empty(), &DISARMED)?;
        // Disarming does not clear an expiration that already happened,
        // and the fd would stay readable and wake every `epoll_wait` from
        // here on. The fd is non-blocking, so this just drops that count.
        self.drain();
        Ok(())
    }

    /// Swallow a pending expiration, if any. The fd is non-blocking, so
    /// `EAGAIN` is the ordinary "nothing there" answer.
    fn drain(&self) {
        let mut buf = [0u8; 8];
        match rustix::io::read(self.timer.as_fd(), &mut buf[..]) {
            Ok(_) | Err(Errno::AGAIN | Errno::INTR) => {}
            Err(e) => crate::warn!("defer timer: read: {e}"),
        }
    }
}

/// Arm `fd` for an absolute `CLOCK_MONOTONIC` deadline.
///
/// A deadline of 0 would *disarm* the timer rather than fire immediately,
/// which is the one value that must not silently mean "never", so it is
/// clamped to the first nanosecond of the epoch — long past, and a
/// timerfd armed for the past fires at once.
fn arm(fd: BorrowedFd<'_>, deadline_ns: u64) -> rustix::io::Result<()> {
    let deadline_ns = deadline_ns.max(1);
    let spec = Itimerspec {
        it_interval: Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: Timespec {
            tv_sec: (deadline_ns / NS_PER_SEC).cast_signed(),
            tv_nsec: (deadline_ns % NS_PER_SEC).cast_signed(),
        },
    };
    timerfd_settime(fd, TimerfdTimerFlags::ABSTIME, &spec)?;
    Ok(())
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

    /// Whether the timer fd is readable right now.
    fn readable(d: &DeferredFlip) -> bool {
        wait(d, &Timespec::default())
    }

    /// Poll the timer fd for at most `timeout`.
    fn wait(d: &DeferredFlip, timeout: &Timespec) -> bool {
        let fd = d.as_fd();
        let mut pfd = [PollFd::new(&fd, PollFlags::IN)];
        poll(&mut pfd, Some(timeout)).unwrap() == 1
    }

    #[test]
    fn a_fresh_timer_is_disarmed_and_never_wakes() {
        let d = DeferredFlip::new().unwrap();
        assert!(!d.holding());
        assert!(!d.awaiting());
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!readable(&d), "an idle deferral must make no wakeups");
    }

    #[test]
    fn awaited_clients_are_a_set_and_are_forgotten_one_at_a_time() {
        let mut d = DeferredFlip::new().unwrap();
        d.expect(7);
        d.expect(7);
        d.expect(9);
        assert!(d.awaiting());
        d.forget(7);
        assert!(d.awaiting(), "client 9 has still not answered");
        d.forget(9);
        assert!(!d.awaiting());
    }

    #[test]
    fn holding_arms_the_timer_and_releasing_disarms_it() {
        let mut d = DeferredFlip::new().unwrap();
        d.expect(1);
        d.hold_until(now_ns() + 5_000_000).unwrap();
        assert!(d.holding());
        assert_eq!(d.deferred, 1);
        assert!(!readable(&d), "not yet due");
        d.disarm().unwrap();
        d.forget_all();
        assert!(!d.holding());
        assert!(!d.awaiting());
        std::thread::sleep(std::time::Duration::from_millis(15));
        assert!(!readable(&d), "a released deferral makes no wakeup");
        assert_eq!(d.timeouts, 0);
    }

    #[test]
    fn one_episode_is_counted_once_and_never_pushed_later() {
        let mut d = DeferredFlip::new().unwrap();
        let deadline = now_ns() + 8_000_000;
        d.hold_until(deadline).unwrap();
        // A second motion inside the same frame period must not move the
        // cursor's backstop further out, and is not a second episode.
        d.hold_until(deadline + 4_000_000).unwrap();
        assert_eq!(d.deferred, 1);
        assert_eq!(d.until_ns, Some(deadline));
        // An earlier deadline does win: whichever output is due first sets
        // the pace.
        d.hold_until(deadline - 4_000_000).unwrap();
        assert_eq!(d.deferred, 1);
        assert_eq!(d.until_ns, Some(deadline - 4_000_000));
    }

    #[test]
    fn the_deadline_fires_and_counts_a_timeout() {
        let mut d = DeferredFlip::new().unwrap();
        d.expect(3);
        d.hold_until(now_ns() + 2_000_000).unwrap();
        assert!(
            wait(
                &d,
                &Timespec {
                    tv_sec: 1,
                    tv_nsec: 0
                }
            ),
            "the deadline never woke us"
        );
        d.expired();
        assert_eq!(d.timeouts, 1);
        assert!(!d.holding());
        assert!(!d.awaiting(), "a timed-out client is no longer waited for");
        assert!(!readable(&d), "the expiration was drained");
        // A stale expiration read afterwards is not a second timeout.
        d.expired();
        assert_eq!(d.timeouts, 1);
    }

    #[test]
    fn a_deadline_already_past_fires_immediately() {
        let mut d = DeferredFlip::new().unwrap();
        d.hold_until(1).unwrap();
        assert!(
            wait(
                &d,
                &Timespec {
                    tv_sec: 1,
                    tv_nsec: 0
                }
            ),
            "a deadline in the past must not mean `never`"
        );
        d.expired();
        assert_eq!(d.timeouts, 1);
    }
}

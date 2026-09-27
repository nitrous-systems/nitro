//! Synthetic input from the control socket's `input` request.
//!
//! # Same path as hardware
//!
//! Nothing here routes anything. An injected event is an ordinary
//! [`InputEvent`] handed to `Server::route_input`, exactly like one
//! libinput produced, so focus, grabs, popups, drag and drop, key repeat
//! and the input-to-photon stamp all behave as they do for a real device,
//! on the fake backend and on DRM alike. This module only holds what is
//! due *later* — `count`/`every`/`after` sequences — and the ASCII table
//! behind `input type`.
//!
//! # The timer
//!
//! One `CLOCK_MONOTONIC` timerfd, armed at the earliest due time only while
//! something is queued: the bargain [`crate::repeat`] and [`crate::defer`]
//! make, so an idle server still takes zero wakeups. Sequencing server-side
//! rather than in the calling script is the point — a Python loop sleeping
//! 16 ms jitters by milliseconds, a timerfd by microseconds, and each event
//! is stamped with its **due** time either way (the analogue of a kernel
//! evdev timestamp), so input-to-photon is measured from when the event was
//! meant to happen.

use std::collections::VecDeque;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::io::Errno;
use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};

use crate::input::InputEvent;
use crate::protocol::{InputAction, InputSpec, Press, ScrollSource};
use nitro_wire::types::{AxisSource, ButtonState};

/// Evdev `KEY_LEFTSHIFT`.
pub const KEY_LEFTSHIFT: u32 = 42;

/// Something queued for later.
#[derive(Debug, Clone, PartialEq)]
pub enum Pending {
    /// Absolute motion to global device pixels. Kept absolute until it
    /// fires, because the delta it becomes depends on where the pointer is
    /// *then* — something else may have moved it in between.
    MotionTo {
        /// Device-pixel x.
        x: f64,
        /// Device-pixel y.
        y: f64,
    },
    /// A ready event; its `time_ns` is re-stamped with the due time.
    Event(InputEvent),
}

/// The queue of not-yet-due injected input and its timer.
#[derive(Debug)]
pub struct Injector {
    /// Non-blocking timerfd, in the epoll set for the whole run; armed
    /// only while `queue` is non-empty.
    timer: OwnedFd,
    /// `(due_ns, what)`, sorted by due; equal dues keep insertion order.
    queue: VecDeque<(u64, Pending)>,
    /// Events routed so far, now or later: `input_injected` in `stats`.
    pub injected: u64,
}

impl Injector {
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
            queue: VecDeque::new(),
            injected: 0,
        })
    }

    /// The fd to add to the epoll set.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.timer.as_fd()
    }

    /// Queue `what` for `due_ns`, after anything already due at the same
    /// instant. Call [`Injector::rearm`] once done scheduling.
    pub fn schedule(&mut self, due_ns: u64, what: Pending) {
        let at = self.queue.partition_point(|(d, _)| *d <= due_ns);
        self.queue.insert(at, (due_ns, what));
    }

    /// Items not yet fired.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Everything due by `now_ns`, in order. Swallows the timer expiry.
    pub fn take_due(&mut self, now_ns: u64) -> Vec<(u64, Pending)> {
        self.drain();
        let n = self.queue.partition_point(|(d, _)| *d <= now_ns);
        self.queue.drain(..n).collect()
    }

    /// Arm for the earliest queued item, or disarm when there is none.
    ///
    /// # Errors
    /// The `timerfd_settime` failure.
    pub fn rearm(&mut self) -> rustix::io::Result<()> {
        if let Some(&(due, _)) = self.queue.front() {
            return crate::defer::arm(self.timer.as_fd(), due);
        }
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        timerfd_settime(
            self.timer.as_fd(),
            TimerfdTimerFlags::empty(),
            &Itimerspec {
                it_interval: zero,
                it_value: zero,
            },
        )?;
        self.drain();
        Ok(())
    }

    /// Drop everything queued (the session went inactive) and disarm.
    ///
    /// # Errors
    /// The `timerfd_settime` failure.
    pub fn clear(&mut self) -> rustix::io::Result<()> {
        self.queue.clear();
        self.rearm()
    }

    /// Swallow a pending expiration, if any.
    fn drain(&self) {
        let mut buf = [0u8; 8];
        match rustix::io::read(self.timer.as_fd(), &mut buf[..]) {
            Ok(_) | Err(Errno::AGAIN | Errno::INTR) => {}
            Err(e) => crate::warn!("inject timer: read: {e}"),
        }
    }
}

/// Expand a parsed `input` request into `(due_ns, what)` items, in order.
///
/// `now_ns` is the default for `t=`; `origin` is added to a `motion`'s
/// coordinates (the named output's device origin, else zero). A `click` or
/// `tap` is a press and a release at the same due time; `type` is one
/// step per character, `every` apart, each `[Shift down] key down, key up
/// [Shift up]`. Every event carries its due time as `time_ns`.
#[must_use]
pub fn expand(spec: &InputSpec, now_ns: u64, origin: (f64, f64)) -> Vec<(u64, Pending)> {
    let start = spec.t_ns.unwrap_or(now_ns).saturating_add(spec.after_ns);
    let presses = |press: Press| -> &'static [bool] {
        match press {
            Press::Down => &[true],
            Press::Up => &[false],
            Press::Both => &[true, false],
        }
    };
    let text: Vec<char> = match &spec.action {
        InputAction::Type(t) => t.chars().collect(),
        _ => Vec::new(),
    };
    let per = text.len().max(1) as u64;
    let steps = u64::from(spec.count) * per;
    let mut items = Vec::new();
    for step in 0..steps {
        let due = start.saturating_add(step.saturating_mul(spec.every_ns));
        let key = |keycode, pressed| {
            Pending::Event(InputEvent::Key {
                keycode,
                pressed,
                time_ns: due,
            })
        };
        match &spec.action {
            InputAction::Motion { x, y, .. } => items.push((
                due,
                Pending::MotionTo {
                    x: x + origin.0,
                    y: y + origin.1,
                },
            )),
            InputAction::Rel { dx, dy } => items.push((
                due,
                Pending::Event(InputEvent::PointerMotion {
                    dx: *dx,
                    dy: *dy,
                    time_ns: due,
                }),
            )),
            InputAction::Button { code, press } => {
                for &down in presses(*press) {
                    let state = if down {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    };
                    items.push((
                        due,
                        Pending::Event(InputEvent::PointerButton {
                            button: *code,
                            state,
                            time_ns: due,
                        }),
                    ));
                }
            }
            InputAction::Wheel { dx, dy, source } => items.push((
                due,
                Pending::Event(InputEvent::PointerAxis {
                    dx: *dx,
                    dy: *dy,
                    source: match source {
                        ScrollSource::Wheel => AxisSource::Wheel,
                        ScrollSource::Finger => AxisSource::Finger,
                        ScrollSource::Continuous => AxisSource::Continuous,
                        ScrollSource::Tilt => AxisSource::WheelTilt,
                    },
                    time_ns: due,
                }),
            )),
            InputAction::Key { code, press } => {
                for &down in presses(*press) {
                    items.push((due, key(*code, down)));
                }
            }
            InputAction::Type(_) => {
                // The parser admits printable ASCII only, all mapped.
                let Some((code, shift)) = ascii_to_key(text[(step % per) as usize]) else {
                    continue;
                };
                if shift {
                    items.push((due, key(KEY_LEFTSHIFT, true)));
                }
                items.push((due, key(code, true)));
                items.push((due, key(code, false)));
                if shift {
                    items.push((due, key(KEY_LEFTSHIFT, false)));
                }
            }
        }
    }
    items
}

/// The evdev keycode of a printable ASCII character on a US layout, and
/// whether Shift is needed. `None` for anything else.
///
/// US because that is the server's fallback layout and the one the test
/// box and CI run; on another layout the *keycodes* are still these, and
/// the text a client sees is whatever that layout maps them to — which is
/// exactly what a real keyboard would produce too.
#[must_use]
pub fn ascii_to_key(c: char) -> Option<(u32, bool)> {
    const ROW_Q: &[u8] = b"qwertyuiop";
    const ROW_A: &[u8] = b"asdfghjkl";
    const ROW_Z: &[u8] = b"zxcvbnm";
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        let b = lower as u8;
        let code = if let Some(i) = ROW_Q.iter().position(|&x| x == b) {
            16 + i as u32
        } else if let Some(i) = ROW_A.iter().position(|&x| x == b) {
            30 + i as u32
        } else {
            44 + ROW_Z.iter().position(|&x| x == b)? as u32
        };
        return Some((code, c.is_ascii_uppercase()));
    }
    Some(match c {
        '1'..='9' => (2 + (c as u32 - '1' as u32), false),
        '0' => (11, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        ' ' => (57, false),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::event::{PollFd, PollFlags, poll};

    fn readable_within(i: &Injector, ms: i64) -> bool {
        let fd = i.as_fd();
        let mut pfd = [PollFd::new(&fd, PollFlags::IN)];
        let t = Timespec {
            tv_sec: 0,
            tv_nsec: ms * 1_000_000,
        };
        poll(&mut pfd, Some(&t)).unwrap() == 1
    }

    fn key(code: u32) -> Pending {
        Pending::Event(InputEvent::Key {
            keycode: code,
            pressed: true,
            time_ns: 0,
        })
    }

    #[test]
    fn the_us_table_covers_letters_digits_and_symbols() {
        assert_eq!(ascii_to_key('a'), Some((30, false)));
        assert_eq!(ascii_to_key('A'), Some((30, true)));
        assert_eq!(ascii_to_key('q'), Some((16, false)));
        assert_eq!(ascii_to_key('p'), Some((25, false)));
        assert_eq!(ascii_to_key('l'), Some((38, false)));
        assert_eq!(ascii_to_key('z'), Some((44, false)));
        assert_eq!(ascii_to_key('M'), Some((50, true)));
        assert_eq!(ascii_to_key('1'), Some((2, false)));
        assert_eq!(ascii_to_key('0'), Some((11, false)));
        assert_eq!(ascii_to_key('!'), Some((2, true)));
        assert_eq!(ascii_to_key(' '), Some((57, false)));
        assert_eq!(ascii_to_key('?'), Some((53, true)));
        assert_eq!(ascii_to_key('\n'), None);
        assert_eq!(ascii_to_key('é'), None);
        for c in (0x20u8..0x7f).map(char::from) {
            assert!(ascii_to_key(c).is_some(), "{c:?} unmapped");
        }
    }

    #[test]
    fn expansion_stamps_due_times_and_splits_clicks_and_text() {
        let spec = |line: &str| match crate::protocol::parse(line) {
            Ok(crate::protocol::Request::Input(s)) => s,
            other => panic!("{other:?}"),
        };
        let items = expand(
            &spec("input wheel 0 15 count=3 every=16 after=4"),
            1_000,
            (0.0, 0.0),
        );
        let dues: Vec<u64> = items.iter().map(|(d, _)| *d).collect();
        assert_eq!(dues, [4_001_000, 20_001_000, 36_001_000]);
        for (d, p) in &items {
            assert!(matches!(p, Pending::Event(e) if e.time_ns() == *d));
        }
        let items = expand(&spec("input button left click t=7"), 1_000, (0.0, 0.0));
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|(d, _)| *d == 7));
        let items = expand(&spec("input motion 5 6 X"), 0, (100.0, 0.0));
        assert_eq!(items, [(0, Pending::MotionTo { x: 105.0, y: 6.0 })]);
        // `Ab`: Shift down, a down, a up, Shift up, b down, b up.
        let codes: Vec<(u32, bool)> = expand(&spec("input type Ab"), 0, (0.0, 0.0))
            .into_iter()
            .map(|(_, p)| match p {
                Pending::Event(InputEvent::Key {
                    keycode, pressed, ..
                }) => (keycode, pressed),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            codes,
            [
                (42, true),
                (30, true),
                (30, false),
                (42, false),
                (48, true),
                (48, false)
            ]
        );
        let items = expand(&spec("input type every=10 ab"), 0, (0.0, 0.0));
        assert_eq!(items[2].0, 10_000_000);
    }

    #[test]
    fn an_empty_injector_never_wakes() {
        let mut i = Injector::new().unwrap();
        i.rearm().unwrap();
        assert!(!readable_within(&i, 20));
    }

    #[test]
    fn items_come_out_in_due_order_and_only_when_due() {
        let mut i = Injector::new().unwrap();
        i.schedule(30, key(3));
        i.schedule(10, key(1));
        i.schedule(20, key(2));
        i.schedule(20, key(22));
        assert_eq!(i.pending(), 4);
        assert!(i.take_due(5).is_empty());
        let due: Vec<u64> = i.take_due(20).into_iter().map(|(d, _)| d).collect();
        assert_eq!(due, [10, 20, 20]);
        let rest = i.take_due(u64::MAX);
        assert_eq!(rest, [(30, key(3))]);
        assert_eq!(i.pending(), 0);
    }

    #[test]
    fn a_past_deadline_fires_and_clear_disarms() {
        let mut i = Injector::new().unwrap();
        i.schedule(1, key(1));
        i.rearm().unwrap();
        assert!(readable_within(&i, 50));
        i.clear().unwrap();
        assert_eq!(i.pending(), 0);
        assert!(!readable_within(&i, 50));
    }
}

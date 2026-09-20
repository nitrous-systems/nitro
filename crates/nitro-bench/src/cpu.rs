//! CPU time of a process, read out of `/proc/<pid>/stat`.
//!
//! # Why this is the headline number
//!
//! Every other figure a graphics benchmark produces is capped by the
//! display. A scenario that asks for 1 000 frames per second and a
//! scenario that asks for 61 both present 60, so frames-per-second stops
//! discriminating the moment the client is fast enough — which, for a
//! retained scene graph, is most of the time. What does *not* saturate is
//! **how much CPU was burned to put those 60 frames up**: 300 µs per
//! frame and 9 000 µs per frame look identical on the glass and are three
//! orders of magnitude apart in battery, heat and headroom.
//!
//! So the ratio this module exists to compute is
//! `(utime + stime) delta / presented frames`, in microseconds of CPU per
//! presented frame, sampled for the **server** and the **client**
//! separately. `DESIGN.md`'s first goal is "work proportional to what
//! changed"; this is the only instrument in the tree that measures the
//! constant of proportionality.
//!
//! # Why `/proc` and not `getrusage`
//!
//! `getrusage(RUSAGE_SELF)` answers for the calling process, and the
//! process this benchmark most wants to charge is the *server* — another
//! pid entirely. One mechanism that works for both ends is worth more
//! than a slightly cheaper one that only works for us, and the cost is a
//! `read(2)` of about 300 bytes twice per run.
//!
//! # The `comm` field trap
//!
//! Field 2 of `/proc/<pid>/stat` is the executable name **in
//! parentheses**, and it may itself contain spaces and parentheses — a
//! process called `nitro (test) 1` is legal. Splitting the line on
//! whitespace and indexing therefore silently reads the wrong fields for
//! such a process. The documented fix, which this module implements, is
//! to find the **last** `)` in the line and parse fields from there: the
//! comm is the only field that can contain one, so the last `)` in the
//! line always ends it.

use std::fs;
use std::io;

/// Ticks of CPU time a process has used, split as the kernel reports it.
///
/// Kept in *ticks* rather than converted at the point of reading because
/// the conversion needs `sysconf(_SC_CLK_TCK)` and the difference of two
/// tick counts is exact where the difference of two rounded microsecond
/// counts is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTicks {
    /// Ticks spent in user mode (field 14 of `/proc/<pid>/stat`).
    pub utime: u64,
    /// Ticks spent in kernel mode on this process's behalf (field 15).
    pub stime: u64,
}

impl CpuTicks {
    /// User plus system: the number a "how much CPU did this cost" question
    /// is actually asking for.
    ///
    /// The split is kept in the struct because it diagnoses: a pixel-push
    /// scenario that is all `stime` is spending its life in the kernel —
    /// socket writes, and before #569 the `pwrite`/`pread` of the buffer
    /// too — and one that is all `utime` is spending it in the effect.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.utime + self.stime
    }

    /// `self - earlier`, saturating.
    ///
    /// Saturating rather than wrapping or panicking: a counter that went
    /// backwards means the pid was reused between the two samples, and the
    /// honest answer to "how much CPU did that process use" is then zero,
    /// not `u64::MAX` microseconds.
    #[must_use]
    pub fn since(&self, earlier: Self) -> Self {
        Self {
            utime: self.utime.saturating_sub(earlier.utime),
            stime: self.stime.saturating_sub(earlier.stime),
        }
    }

    /// Microseconds of CPU, given the platform's clock tick rate.
    #[must_use]
    pub fn micros(&self, ticks_per_second: u64) -> u64 {
        if ticks_per_second == 0 {
            return 0;
        }
        self.total().saturating_mul(1_000_000) / ticks_per_second
    }
}

/// Clock ticks per second (`sysconf(_SC_CLK_TCK)`), 100 on every Linux
/// this tree runs on.
///
/// Read through `rustix` rather than hard-coded: it is 100 on x86-64 Linux
/// and has been for twenty years, but a benchmark whose unit conversion is
/// a magic number is a benchmark that lies quietly on the one machine
/// where it is wrong.
#[must_use]
pub fn ticks_per_second() -> u64 {
    rustix::param::clock_ticks_per_second()
}

/// Read a process's CPU counters.
///
/// # Errors
/// The process does not exist, `/proc` is not mounted, or the line does
/// not parse — all of which are reported rather than papered over with a
/// zero, because a silently-zero CPU column is the most flattering
/// possible wrong answer for a display server.
pub fn read(pid: u32) -> io::Result<CpuTicks> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse(&text).ok_or_else(|| io::Error::other(format!("/proc/{pid}/stat did not parse")))
}

/// Our own pid, for the client half of the measurement.
#[must_use]
pub fn self_pid() -> u32 {
    rustix::process::getpid()
        .as_raw_nonzero()
        .get()
        .cast_unsigned()
}

/// The pid on the other end of a connected socket (`SO_PEERCRED`).
///
/// **This is the right way to find the server**, and the reason it is
/// here rather than a `pgrep`: the benchmark is connected to exactly one
/// server, and the kernel knows which process that is. Scanning `/proc`
/// for the name (see [`find_by_comm`]) finds *a* `nitro-server`, which on
/// a development machine with three of them running is a coin toss — and
/// a coin toss that lands on an idle one reports a server CPU cost of
/// **zero**, the single most flattering wrong answer a compositor
/// benchmark can produce. It was caught by noticing exactly that column
/// of zeroes in a local run.
///
/// # Errors
/// `SO_PEERCRED` is a Unix-socket option: a TCP connection has no peer
/// pid and reports one, which is correct and means the server CPU column
/// is honestly missing on a remote link rather than wrong.
pub fn peer_pid(fd: std::os::fd::BorrowedFd<'_>) -> io::Result<u32> {
    let cred = rustix::net::sockopt::socket_peercred(fd)?;
    Ok(cred.pid.as_raw_nonzero().get().cast_unsigned())
}

/// Find the one process whose executable name is `name`, by scanning
/// `/proc/*/comm`.
///
/// The fallback for when there is no connected socket to ask; prefer
/// [`peer_pid`], which cannot pick the wrong process. Returns the pids in
/// ascending order, and the caller is told how many there were rather
/// than being handed one silently: two `nitro-server`s means a stale one
/// is still around, which is a state worth noticing rather than averaging
/// over.
///
/// `comm` is truncated to 15 bytes by the kernel, so a name longer than
/// that is compared against its own truncation. `nitro-server` is twelve
/// characters and safe; the truncation is applied anyway so a future
/// `nitro-something-long` does not silently never match.
#[must_use]
pub fn find_by_comm(name: &str) -> Vec<u32> {
    let want = name.as_bytes();
    let want = &want[..want.len().min(15)];
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out: Vec<u32> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|c| c.trim_end().as_bytes() == want)
        })
        .collect();
    out.sort_unstable();
    out
}

/// Parse the `utime`/`stime` fields out of a `/proc/<pid>/stat` line.
///
/// Fields are counted from the character after the **last** `)`, which is
/// what makes a process named `weird (name)` parse correctly; see the
/// module docs. After that `)` the fields are, in order, `state`, `ppid`,
/// … and `utime` is the 12th, `stime` the 13th (fields 14 and 15 of the
/// whole line, one-based, as `proc(5)` numbers them).
#[must_use]
pub fn parse(stat: &str) -> Option<CpuTicks> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    // `state` is the first field after the comm; utime is 12 further on.
    let utime = fields.nth(11)?.parse().ok()?;
    let stime = fields.next()?.parse().ok()?;
    Some(CpuTicks { utime, stime })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real line, taken verbatim from the box, with the fields this
    /// module cares about at their real offsets.
    const REAL: &str = "217261 (nitro-server) S 1 217261 217261 0 -1 4194560 12841 0 30 0 \
        913 271 0 0 20 0 1 0 5049318 21159936 4785 18446744073709551615 1 1 0 0 0 0 0 0 0";

    #[test]
    fn the_two_cpu_fields_are_read_from_a_real_line() {
        assert_eq!(
            parse(REAL),
            Some(CpuTicks {
                utime: 913,
                stime: 271
            })
        );
    }

    /// The trap the module docs describe: a comm containing a space *and*
    /// a closing parenthesis. Splitting on whitespace and indexing reads
    /// `20` and `0` here instead of `913` and `271`.
    #[test]
    fn a_comm_with_spaces_and_parens_does_not_shift_the_fields() {
        let line = REAL.replacen("(nitro-server)", "(ni (tro) server)", 1);
        assert_eq!(parse(&line), parse(REAL));
    }

    #[test]
    fn a_truncated_line_is_none_rather_than_zero() {
        assert_eq!(parse("123 (x) S 1 2 3"), None);
        assert_eq!(parse("no parenthesis here"), None);
    }

    #[test]
    fn a_delta_is_the_two_fields_separately() {
        let a = CpuTicks {
            utime: 100,
            stime: 20,
        };
        let b = CpuTicks {
            utime: 130,
            stime: 25,
        };
        assert_eq!(
            b.since(a),
            CpuTicks {
                utime: 30,
                stime: 5
            }
        );
        assert_eq!(b.since(a).total(), 35);
    }

    /// A pid reused between two samples reads as no CPU, not as an
    /// astronomical one.
    #[test]
    fn a_counter_that_went_backwards_saturates_to_zero() {
        let later = CpuTicks { utime: 1, stime: 1 };
        let earlier = CpuTicks {
            utime: 900,
            stime: 900,
        };
        assert_eq!(later.since(earlier), CpuTicks::default());
    }

    #[test]
    fn ticks_convert_to_microseconds_at_the_platforms_rate() {
        let t = CpuTicks {
            utime: 30,
            stime: 5,
        };
        // 35 ticks at 100 Hz is 350 ms.
        assert_eq!(t.micros(100), 350_000);
        // A nonsense rate gives zero rather than a division fault.
        assert_eq!(t.micros(0), 0);
    }

    /// The instrument that matters: asking the *socket* who the server is
    /// cannot pick the wrong process, and `/proc`-scanning can.
    ///
    /// A socketpair stands in for the wire connection; both ends belong
    /// to this process, so the answer is checkable against a pid we
    /// already know.
    #[test]
    fn a_connected_socket_names_its_peer() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let pid = peer_pid(std::os::fd::AsFd::as_fd(&a)).expect("SO_PEERCRED");
        assert_eq!(pid, self_pid());
    }

    /// The instrument has to work on the process running the test, which
    /// is the cheapest possible end-to-end check that the path and the
    /// field offsets are right on this kernel.
    ///
    /// It does **not** assert the counter is non-zero. Ticks are 10 ms
    /// and a test binary can easily finish a whole suite inside one, so
    /// "my own utime is 0" is a true and common reading rather than a
    /// misalignment — asserting otherwise would be a flaky test that
    /// fails on the fastest machines. What is asserted is that the read
    /// succeeds, that it is repeatable, and that the counter never goes
    /// backwards, which is what a misaligned field would show as.
    #[test]
    fn our_own_process_reports_a_monotonic_counter() {
        let a = read(self_pid()).expect("/proc/self/stat");
        // A little arithmetic, so there is something to have counted.
        let mut acc = 0u64;
        for i in 0..2_000_000u64 {
            acc = acc.wrapping_add(i.wrapping_mul(2_654_435_761));
        }
        let b = read(self_pid()).expect("/proc/self/stat");
        assert!(acc > 0);
        assert!(b.utime >= a.utime && b.stime >= a.stime, "{a:?} then {b:?}");
        assert!(ticks_per_second() > 0);
    }
}

//! Asking the server whether it took the file.
//!
//! Validation is the compositor's job, not this app's. A keyboard layout
//! is valid if xkbcommon can compile it, a scale is valid if the server's
//! own bounds accept it, and a settings app that re-implemented either
//! check would eventually disagree with the thing it is configuring — and
//! the user would be told `de-latin1` is wrong by a dialog while the
//! compositor was perfectly happy with it.
//!
//! So Apply writes the file and then *asks*: the server's control socket
//! answers `stats` with a `config_reloads` counter, which the reload path
//! bumps every time it successfully re-reads `server.conf`. If the number
//! goes up, the file was taken. If it does not within a short bounded
//! wait, something refused it and the compositor's log says what — which
//! is the message this app shows, because it is the honest one.
//!
//! # The protocol
//!
//! The v0 control socket is a line protocol and this is all of it:
//!
//! ```text
//! → stats\n
//! ← ok\n
//! ← frames 1234\n
//! ← config_reloads 2\n
//! ← \n            (a blank line ends the body)
//! ```
//!
//! Which is why there is no client crate here: `UnixStream`, `write_all`,
//! and a reader that stops at the blank line. [`parse_stats`] is the only
//! part with any judgement in it, and it has its own tests.
//!
//! # Bounded, because a settings dialog may not hang
//!
//! Every read has a timeout and the whole wait has a deadline
//! ([`RELOAD_WAIT`]). A compositor that is wedged, a socket left behind by
//! a server that died, a `NITRO_CONTROL` pointing at a fifo — none of
//! them may freeze the app, because the app is running *on* that
//! compositor and a frozen window is the user's only way to fix it.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The counter the server bumps on every successful reload of
/// `server.conf`.
pub const RELOADS_KEY: &str = "config_reloads";

/// How long each individual socket operation may take.
///
/// Short: the server answers `stats` from its own event loop without
/// touching the disk, so a reply that has not arrived in half a second is
/// a reply that is not coming.
const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// How long Apply waits for the reload counter to move.
///
/// The server's inotify watch fires on the rename and the reload happens
/// in the same turn of its loop, so this is dominated by scheduling, not
/// by work. A second and a half is long enough for a loaded box and short
/// enough that a user does not wonder whether the button did anything.
pub const RELOAD_WAIT: Duration = Duration::from_millis(1_500);

/// How often the counter is re-read while waiting.
const POLL: Duration = Duration::from_millis(50);

/// Where the server's control socket is: `$NITRO_CONTROL`, else
/// `$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/…`.
///
/// Delegated to [`nitro_ui::shot::control_path`] rather than re-derived,
/// because it already resolves exactly what the server does and a second
/// copy of that rule is a second chance to get it wrong.
#[must_use]
pub fn control_path() -> PathBuf {
    nitro_ui::shot::control_path()
}

/// Parse the body of a `stats` reply into `(key, value)` pairs.
///
/// Defensive in the two ways that matter. A line with no space, or whose
/// value is not a number, is **skipped** rather than failing the parse:
/// the server may grow a counter this app has never heard of, and a
/// settings dialog must not break because the compositor learned to count
/// something new. And only the first token is the key, so a value with
/// trailing junk does not silently become part of it.
#[must_use]
pub fn parse_stats(body: &str) -> Vec<(String, u64)> {
    body.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (key, value) = line.split_once(' ')?;
            let value = value.trim().parse::<u64>().ok()?;
            Some((key.to_owned(), value))
        })
        .collect()
}

/// One counter out of a parsed `stats` body.
#[must_use]
pub fn stat(pairs: &[(String, u64)], key: &str) -> Option<u64> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
}

/// Ask the control socket at `path` for its counters.
///
/// # Errors
/// If the socket cannot be reached, the request cannot be written, or the
/// server answers anything but `ok`. A server that is not running is the
/// ordinary case of the first one, and the caller shows it as "no server"
/// rather than as a fault.
pub fn stats_at(path: &Path) -> std::io::Result<Vec<(String, u64)>> {
    let mut sock = UnixStream::connect(path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    sock.set_read_timeout(Some(IO_TIMEOUT))?;
    sock.set_write_timeout(Some(IO_TIMEOUT))?;
    sock.write_all(b"stats\n")?;
    let mut reader = BufReader::new(sock);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if status.trim() != "ok" {
        return Err(std::io::Error::other(format!("server: {}", status.trim())));
    }
    let mut body = String::new();
    loop {
        let mut line = String::new();
        // Zero bytes is the server closing the connection, which ends the
        // body as surely as the blank line does; treating it as an error
        // would turn a clean hang-up into a spurious failure.
        if reader.read_line(&mut line)? == 0 || line == "\n" {
            break;
        }
        body.push_str(&line);
    }
    Ok(parse_stats(&body))
}

/// The server's counters, from wherever the environment says it is.
///
/// # Errors
/// As [`stats_at`].
pub fn stats() -> std::io::Result<Vec<(String, u64)>> {
    stats_at(&control_path())
}

/// The `config_reloads` counter, or `None` when there is no server to ask
/// or it does not publish one.
///
/// `None` for both cases on purpose: the caller's question is "can I
/// observe a reload?", and an old server that has no such counter is as
/// unobservable as no server at all. What it must *not* do is treat a
/// missing counter as zero, which would make the first Apply look like it
/// succeeded when nothing had been checked.
#[must_use]
pub fn config_reloads_at(path: &Path) -> Option<u64> {
    stat(&stats_at(path).ok()?, RELOADS_KEY)
}

/// What Apply learned by watching the counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The counter moved: the server read the file and took it.
    Reloaded,
    /// The counter did not move within [`RELOAD_WAIT`]. Either the file
    /// was rejected or the reload never happened; the compositor's log is
    /// the place that says which, so that is what the app points at.
    Rejected,
    /// There is no server to ask, or it publishes no counter. The file is
    /// written and correct; nothing has confirmed it.
    Unknown,
}

/// Watch `config_reloads` until it passes `before`, or `wait` runs out.
///
/// `before` is read *before* the file is renamed, which is what makes
/// this a comparison rather than a guess: a server that reloaded for some
/// other reason a second earlier would otherwise be read as having taken
/// our file.
///
/// This blocks the app's loop for up to `wait`, and that is the
/// deliberate choice over a timer and a callback. Apply is a modal
/// moment — the user pressed a button and is waiting for a verdict — and
/// a bounded block keeps the verdict in the same function as the write,
/// where the `before` value lives. A timer would spread one decision over
/// three callbacks to save a second and a half of a UI that has nothing
/// else to do meanwhile.
///
/// `wait` is a parameter rather than a constant for the tests: the
/// `Rejected` verdict is *defined* as "the counter did not move before
/// the deadline", so the deadline is that test's whole runtime.
/// [`RELOAD_WAIT`] is what the app passes.
#[must_use]
pub fn wait_for_reload(path: &Path, before: Option<u64>, wait: Duration) -> Applied {
    let Some(before) = before else {
        return Applied::Unknown;
    };
    let deadline = Instant::now() + wait;
    loop {
        match config_reloads_at(path) {
            Some(now) if now > before => return Applied::Reloaded,
            // The server went away between the two reads. Nothing is
            // going to confirm anything now.
            None => return Applied::Unknown,
            Some(_) => {}
        }
        if Instant::now() >= deadline {
            return Applied::Rejected;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stats_body_parses_into_pairs() {
        let pairs = parse_stats("frames 1234\nconfig_reloads 2\nflips_pending 0\n");
        assert_eq!(stat(&pairs, "frames"), Some(1234));
        assert_eq!(stat(&pairs, RELOADS_KEY), Some(2));
        assert_eq!(stat(&pairs, "flips_pending"), Some(0));
        assert_eq!(stat(&pairs, "nothing"), None);
    }

    #[test]
    fn unusable_lines_are_skipped_rather_than_failing_the_parse() {
        // A server that grows a counter this app has never seen, or
        // reports one that is not a number, must not break the dialog.
        let pairs = parse_stats("frames 7\nnovalue\nrate 1.5\nempty \nconfig_reloads 9\n\n");
        assert_eq!(stat(&pairs, "frames"), Some(7));
        assert_eq!(stat(&pairs, RELOADS_KEY), Some(9));
        assert_eq!(stat(&pairs, "rate"), None, "not an integer");
        assert_eq!(stat(&pairs, "novalue"), None, "no value at all");
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn an_empty_body_is_no_pairs_and_not_a_panic() {
        assert!(parse_stats("").is_empty());
        assert!(parse_stats("\n\n").is_empty());
        assert!(parse_stats("   ").is_empty());
    }

    #[test]
    fn a_key_is_the_first_token_only() {
        // `ok 3 extra` would be a key of `ok` and an unparseable value,
        // which is skipped — not a key of `ok 3`.
        let pairs = parse_stats("counter 3 extra\ncounter2 4\n");
        assert_eq!(stat(&pairs, "counter"), None);
        assert_eq!(stat(&pairs, "counter2"), Some(4));
    }

    #[test]
    fn no_server_is_unknown_rather_than_rejected() {
        // The distinction the whole `Applied` enum exists for: a file
        // nothing confirmed is not a file something refused.
        let missing = std::env::temp_dir().join("nitro-settings-no-such-control.sock");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(config_reloads_at(&missing), None);
        assert_eq!(
            wait_for_reload(&missing, Some(3), RELOAD_WAIT),
            Applied::Unknown
        );
        // And a `before` of `None` never waits at all.
        assert_eq!(
            wait_for_reload(&missing, None, RELOAD_WAIT),
            Applied::Unknown
        );
    }
}

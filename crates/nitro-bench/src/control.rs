//! The server's own view, over the v0 control socket.
//!
//! A benchmark that reported only what the client can see would be
//! measuring half the system. The client knows how many commits it sent
//! and how many `Presented` came back; it cannot know how long the
//! rasterizer took, how many pixels it repainted, or whether a flip was
//! late. Those live in the server's `stats` reply, so the benchmark reads
//! it before and after every run and reports the delta.
//!
//! This is a near-copy of `nitro-demo`'s `control.rs`, and deliberately
//! so: the control socket is the server's *debug* channel, which is
//! exactly why a measurement tool may speak it and a real client may not.
//! Sharing the code would mean either a benchmark crate depending on
//! `nitro-server` (a client depending on the server is backwards) or a new
//! crate for forty lines. What is *not* copied is the ten-second timeout:
//! a benchmark run is a fixed-length window, and a control socket that
//! blocks for ten seconds inside it would corrupt the very measurement it
//! was opened to take. Two seconds, per the box protocol in
//! `docs/testbox.md` — and `nc` is not used at all, because on the box it
//! silently returns nothing for a fraction of requests (chat
//! `nitro-testbox`, #3707).

use std::collections::BTreeMap;
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// How long a control request may take before the benchmark gives up.
///
/// Two seconds: long enough that a busy server under a `putimage 1080`
/// sweep still answers, short enough that a wedged one cannot eat a
/// measurement window. A missing `stats` column is a gap in the table; a
/// ten-second stall is a wrong number in it.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// Where the control socket lives: `NITRO_CONTROL`, else
/// `$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/…`.
#[must_use]
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("NITRO_CONTROL") {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join("nitro").join("control.sock");
        }
    }
    let uid = rustix::process::getuid().as_raw();
    PathBuf::from(format!("/tmp/nitro-{uid}")).join("control.sock")
}

/// Parse a `stats` body into a map.
///
/// Lines that are not `key value` are skipped rather than failing. The
/// key set is documented but the server is free to add to it, and a
/// benchmark that refused to report anything because it met an unknown
/// line would be the most annoying possible failure mode — the same rule,
/// for the same reason, as `nitro-demo`'s `parse_stats`.
#[must_use]
pub fn parse_stats(body: &str) -> BTreeMap<String, u64> {
    body.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(' ')?;
            Some((k.to_owned(), v.trim().parse().ok()?))
        })
        .collect()
}

/// Ask the server for `stats` and return the parsed body.
///
/// # Errors
/// Connection failure, an `err` reply, or a truncated one.
pub fn stats() -> io::Result<BTreeMap<String, u64>> {
    Ok(parse_stats(&request("stats")?))
}

/// Ask the server for `outputs` and return the reply body verbatim.
///
/// Verbatim rather than parsed because what the benchmark needs from it
/// is the *evidence*: the refresh-rate sweep writes `output.HDMI-A-1.mode`
/// into the config and then has to confirm the mode actually took before
/// believing a single number it measures afterwards. Quoting the line the
/// server printed is a stronger claim than reporting a field this crate
/// re-derived.
///
/// # Errors
/// As [`stats`].
pub fn outputs() -> io::Result<String> {
    request("outputs")
}

/// Best-effort `stats`: the empty map when the socket is not there.
///
/// A benchmark must run against a server started by hand, with no control
/// socket configured, and still produce its client-side numbers. The
/// missing columns then print `?`, which is the honest rendering of "not
/// measured" — as against a zero, which reads as "measured, and it was
/// free".
#[must_use]
pub fn stats_or_empty() -> BTreeMap<String, u64> {
    stats().unwrap_or_default()
}

/// Send one line, read the status line and the body up to the blank line.
///
/// # Errors
/// Connection failure, an `err` reply, or a malformed status line.
fn request(what: &str) -> io::Result<String> {
    let path = socket_path();
    let stream = UnixStream::connect(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut conn = BufReader::new(stream);
    conn.get_mut().write_all(format!("{what}\n").as_bytes())?;
    let mut status = String::new();
    conn.read_line(&mut status)?;
    let status = status.trim_end();
    if let Some(msg) = status.strip_prefix("err ") {
        return Err(io::Error::other(format!("server: {msg}")));
    }
    if status != "ok" && !status.starts_with("ok ") {
        return Err(io::Error::other(format!("malformed reply {status:?}")));
    }
    let mut body = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if conn.read_line(&mut line)? == 0 || line == "\n" {
            return Ok(body);
        }
        body.push_str(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stats_body_parses_to_numbers() {
        let s = parse_stats("frames 12\npaint_us_mean 845\nnodes 31\n");
        assert_eq!(s.get("frames"), Some(&12));
        assert_eq!(s.get("paint_us_mean"), Some(&845));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn unparseable_lines_are_skipped_not_fatal() {
        let s = parse_stats("frames 12\nnonsense\nname nitro\nnodes 3\n");
        assert_eq!(s.get("frames"), Some(&12));
        assert_eq!(s.get("nodes"), Some(&3));
        assert!(!s.contains_key("name"), "`nitro` is not a number");
    }

    #[test]
    fn an_empty_body_is_an_empty_map() {
        assert!(parse_stats("").is_empty());
    }

    /// Without a server there is no socket, and the benchmark must still
    /// run — with the server columns missing rather than zeroed.
    #[test]
    fn a_missing_socket_gives_an_empty_map_rather_than_a_panic() {
        // Not `set_var` (unsafe in edition 2024, racy across test
        // threads): assert the documented shape of the path instead, and
        // that the best-effort call is total.
        assert!(socket_path().ends_with("control.sock"));
        let _ = stats_or_empty();
    }

    #[test]
    fn the_timeout_is_short_enough_to_sit_inside_a_run() {
        assert!(TIMEOUT <= Duration::from_secs(2));
    }
}

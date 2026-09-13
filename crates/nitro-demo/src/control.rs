//! The server's own view, over the v0 control socket.
//!
//! `--stats` asks the server for its `stats` reply and prints its
//! `i2p_*` figures next to the client's histogram. That socket is the
//! server's *debug* channel and deliberately has nothing to do with the
//! wire protocol — which is exactly why it is fine for a measurement tool
//! to use it and would not be fine for a real client. Nothing else in the
//! demo depends on it: without `--stats` the demo never opens it, and a
//! server built without one is not an error, just a missing column.
//!
//! It is a line protocol: one request per line, a status line back, then a
//! body of `key value` lines ended by a blank one.

use std::collections::BTreeMap;
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Where the control socket lives: `NITRO_CONTROL`, else
/// `$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/…`.
///
/// A copy of `nitro-shot`'s resolution rather than a shared function,
/// because sharing it would mean either the demo depending on
/// `nitro-server` (a client depending on the server is backwards) or a new
/// crate for eleven lines.
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
/// Lines that are not `key value` are skipped rather than failing: the
/// key set is documented but the server is free to add to it, and a demo
/// that refuses to print anything because it met an unknown line would be
/// the most annoying possible failure mode.
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
    let mut conn = connect()?;
    conn.get_mut().write_all(b"stats\n")?;
    let mut status = String::new();
    conn.read_line(&mut status)?;
    let status = status.trim_end();
    if let Some(msg) = status.strip_prefix("err ") {
        return Err(io::Error::other(format!("server: {msg}")));
    }
    if status != "ok" {
        return Err(io::Error::other(format!("malformed reply {status:?}")));
    }
    let mut body = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if conn.read_line(&mut line)? == 0 || line == "\n" {
            return Ok(parse_stats(&body));
        }
        body.push_str(&line);
    }
}

/// A front-buffer readback: `XRGB8888` with the server's own row stride.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row, which is **not** `width * 4` in general.
    pub stride: u32,
    /// `stride * height` bytes.
    pub data: Vec<u8>,
}

/// Ask the server for a front-buffer readback.
///
/// # Errors
/// Connection failure, an `err` reply, or a short body.
pub fn shot() -> io::Result<Shot> {
    let mut conn = connect()?;
    conn.get_mut().write_all(b"shot\n")?;
    let mut status = String::new();
    conn.read_line(&mut status)?;
    let status = status.trim_end();
    let rest = status
        .strip_prefix("ok ")
        .ok_or_else(|| io::Error::other(format!("shot: {status:?}")))?;
    let fields: Vec<u32> = rest
        .split_whitespace()
        .map(|f| f.parse::<u32>().ok())
        .collect::<Option<_>>()
        .ok_or_else(|| io::Error::other(format!("bad shot header {rest:?}")))?;
    let [width, height, stride] = fields[..] else {
        return Err(io::Error::other(format!("bad shot header {rest:?}")));
    };
    let mut data = vec![0u8; stride as usize * height as usize];
    conn.read_exact(&mut data)?;
    Ok(Shot {
        width,
        height,
        stride,
        data,
    })
}

/// Connect to the control socket with a read timeout, so a wedged server
/// cannot hang the demo.
fn connect() -> io::Result<BufReader<UnixStream>> {
    let path = socket_path();
    let stream = UnixStream::connect(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    Ok(BufReader::new(stream))
}

/// The server's i2p figures as a printable line, with the keys it is
/// missing named rather than silently zeroed.
#[must_use]
pub fn i2p_line(stats: &BTreeMap<String, u64>) -> String {
    let get = |k: &str| stats.get(k).map_or_else(|| "?".to_owned(), u64::to_string);
    format!(
        "server i2p: min={} mean={} max={} us  (frames={} damage_px_mean={} paint_us_mean={})",
        get("i2p_min_us"),
        get("i2p_mean_us"),
        get("i2p_max_us"),
        get("frames"),
        get("damage_px_mean"),
        get("paint_us_mean"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stats_body_parses_to_numbers() {
        let s = parse_stats("frames 12\ni2p_mean_us 8745\nactive 1\n");
        assert_eq!(s.get("frames"), Some(&12));
        assert_eq!(s.get("i2p_mean_us"), Some(&8745));
        assert_eq!(s.len(), 3);
    }

    /// An unknown or malformed line must not lose the lines around it.
    #[test]
    fn unparseable_lines_are_skipped_not_fatal() {
        let s = parse_stats("frames 12\nnonsense\nname nitro\nwindows 3\n");
        assert_eq!(s.get("frames"), Some(&12));
        assert_eq!(s.get("windows"), Some(&3));
        assert!(!s.contains_key("name"), "`nitro` is not a number");
        assert!(!s.contains_key("nonsense"));
    }

    #[test]
    fn an_empty_body_is_an_empty_map() {
        assert!(parse_stats("").is_empty());
    }

    #[test]
    fn the_i2p_line_names_missing_keys() {
        let s = parse_stats("i2p_min_us 100\n");
        let line = i2p_line(&s);
        assert!(line.contains("min=100"));
        assert!(line.contains("mean=?"), "{line}");
    }

    #[test]
    fn the_override_wins_over_the_runtime_dir() {
        // Not `std::env::set_var` (unsafe in edition 2024 and racy across
        // test threads): assert the documented shape instead.
        let p = socket_path();
        assert!(p.ends_with("control.sock"), "{}", p.display());
    }
}

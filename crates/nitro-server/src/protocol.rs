//! Control protocol v0: line-based requests, `ok`/`err` replies.
//!
//! Deliberately trivial; `nitro-wire` replaces it in M1. Requests are one
//! line each (`\n`-terminated, ASCII). Replies start with a status line —
//! `ok ...\n` or `err <message>\n` — followed by a request-specific body:
//!
//! | request              | reply                                                        |
//! |----------------------|--------------------------------------------------------------|
//! | `shot [output-name]` | `ok <w> <h> <stride>\n` + `stride*h` bytes `XRGB8888`         |
//! | `shot-front [name]`  | the same, read off the **scanout** buffer; for tests          |
//! | `outputs`            | `ok\n` + one `name WxH@refresh_mhz\n` per output + `\n`       |
//! | `stats`              | `ok\n` + `key value\n` lines + `\n`                           |
//! | `quit`               | `ok\n`, then the server shuts down                           |
//! | `plug WxH`           | `ok\n`; fake backend only — hotplugs an output in, for tests |
//! | `unplug`             | `ok\n`; fake backend only — removes the last output          |
//! | `focus`              | `ok\n`; focuses the topmost window, for tests                |
//!
//! This module only parses and formats; it never touches a socket.

use std::fmt::Write as _;

use nitro_kms::{Image, OutputInfo};

/// A parsed request line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Readback of one output (the first when unnamed): the shadow buffer
    /// when there is one, else the front buffer. Both hold the same image;
    /// the shadow is simply cheaper to read.
    Shot(Option<String>),
    /// Readback of the **scanout** buffer specifically, bypassing the
    /// shadow.
    ///
    /// Test-only, like [`Request::Plug`], and it exists because the shadow
    /// makes [`Request::Shot`] unable to see the one thing a shadow test
    /// has to check: that the right pixels were streamed *out* of the
    /// shadow into the buffer the display actually scans.
    ShotFront(Option<String>),
    /// List outputs.
    Outputs,
    /// Frame counters.
    Stats,
    /// Orderly shutdown.
    Quit,
    /// Plug a `WxH` output into the fake backend. Test-only: on a real
    /// backend outputs come from connectors, and this is refused.
    Plug(u32, u32),
    /// Unplug the last fake output. Test-only, and the other half of
    /// [`Request::Plug`]: output *removal* is where window migration
    /// lives, and on a real backend it means pulling a cable out.
    Unplug,
    /// Give keyboard focus to the topmost window.
    ///
    /// Test-only, and it exists because focus normally *follows the
    /// click*: a toolkit test that wants to check Tab traversal would
    /// otherwise have to synthesise a click on some widget first, which
    /// changes the very state it is about to assert on.
    Focus,
}

/// Parse one request line (without or with its trailing newline).
///
/// # Errors
/// A human-readable message suitable for an `err` reply.
pub fn parse(line: &str) -> Result<Request, String> {
    let line = line.trim_end_matches(['\n', '\r']);
    let mut words = line.split_ascii_whitespace();
    let Some(cmd) = words.next() else {
        return Err("empty request".to_owned());
    };
    let arg = words.next();
    if words.next().is_some() {
        return Err(format!("too many arguments for `{cmd}`"));
    }
    match (cmd, arg) {
        ("shot", name) => Ok(Request::Shot(name.map(str::to_owned))),
        ("shot-front", name) => Ok(Request::ShotFront(name.map(str::to_owned))),
        ("plug", Some(size)) => {
            let (w, h) = size
                .split_once(['x', 'X'])
                .ok_or_else(|| format!("`plug` wants WxH, got `{size}`"))?;
            let w = w.parse().map_err(|_| format!("bad width `{w}`"))?;
            let h = h.parse().map_err(|_| format!("bad height `{h}`"))?;
            Ok(Request::Plug(w, h))
        }
        ("plug", None) => Err("`plug` needs a WxH size".to_owned()),
        ("unplug", None) => Ok(Request::Unplug),
        ("outputs", None) => Ok(Request::Outputs),
        ("stats", None) => Ok(Request::Stats),
        ("quit", None) => Ok(Request::Quit),
        ("focus", None) => Ok(Request::Focus),
        ("outputs" | "stats" | "quit" | "focus" | "unplug", Some(_)) => {
            Err(format!("`{cmd}` takes no argument"))
        }
        _ => Err(format!("unknown request `{cmd}`")),
    }
}

/// Split the first complete line off `buf`, returning it (without the
/// newline) and draining it from the buffer. `None` when no newline yet.
pub fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let nl = buf.iter().position(|&b| b == b'\n')?;
    let line: Vec<u8> = buf.drain(..=nl).collect();
    Some(String::from_utf8_lossy(&line[..nl]).into_owned())
}

/// Longest request line accepted before the client is dropped.
pub const MAX_LINE: usize = 256;

/// `err <message>\n`. Newlines in the message are flattened.
pub fn err_reply(message: &str) -> Vec<u8> {
    let msg = message.replace(['\n', '\r'], " ");
    format!("err {msg}\n").into_bytes()
}

/// `ok\n` — the whole reply to `quit`.
pub fn ok_reply() -> Vec<u8> {
    b"ok\n".to_vec()
}

/// `ok <w> <h> <stride>\n` followed by the pixel bytes.
pub fn shot_reply(image: &Image) -> Vec<u8> {
    let header = format!("ok {} {} {}\n", image.width, image.height, image.stride);
    let mut out = Vec::with_capacity(header.len() + image.data.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&image.data);
    out
}

/// `ok\n`, one `name WxH@refresh_mhz` line per output, blank line.
pub fn outputs_reply(outputs: &[OutputInfo]) -> Vec<u8> {
    let mut s = String::from("ok\n");
    for o in outputs {
        // Writing to a String cannot fail.
        let _ = writeln!(s, "{} {}x{}@{}", o.name, o.width, o.height, o.refresh_mhz);
    }
    s.push('\n');
    s.into_bytes()
}

/// `ok\n`, one `key value` line per pair, blank line.
pub fn stats_reply(pairs: &[(&str, u64)]) -> Vec<u8> {
    let mut s = String::from("ok\n");
    for (k, v) in pairs {
        let _ = writeln!(s, "{k} {v}");
    }
    s.push('\n');
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_kms::OutputId;

    #[test]
    fn parses_every_request() {
        assert_eq!(parse("shot\n"), Ok(Request::Shot(None)));
        assert_eq!(
            parse("shot HDMI-A-1\r\n"),
            Ok(Request::Shot(Some("HDMI-A-1".to_owned())))
        );
        assert_eq!(parse("shot-front\n"), Ok(Request::ShotFront(None)));
        assert_eq!(
            parse("shot-front HDMI-A-1"),
            Ok(Request::ShotFront(Some("HDMI-A-1".to_owned())))
        );
        assert_eq!(parse("  outputs  "), Ok(Request::Outputs));
        assert_eq!(parse("stats"), Ok(Request::Stats));
        assert_eq!(parse("quit\n"), Ok(Request::Quit));
        assert_eq!(parse("plug 640x480"), Ok(Request::Plug(640, 480)));
        assert_eq!(parse("unplug\n"), Ok(Request::Unplug));
        assert_eq!(
            parse("unplug all"),
            Err("`unplug` takes no argument".to_owned())
        );
        assert_eq!(parse("focus\n"), Ok(Request::Focus));
        assert_eq!(
            parse("focus now"),
            Err("`focus` takes no argument".to_owned())
        );
    }

    #[test]
    fn plug_needs_a_size() {
        assert_eq!(parse("plug"), Err("`plug` needs a WxH size".to_owned()));
        assert_eq!(
            parse("plug wide"),
            Err("`plug` wants WxH, got `wide`".to_owned())
        );
        assert!(parse("plug 12x").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse(""), Err("empty request".to_owned()));
        assert_eq!(parse("\n"), Err("empty request".to_owned()));
        assert_eq!(
            parse("frobnicate"),
            Err("unknown request `frobnicate`".to_owned())
        );
        assert_eq!(
            parse("quit now"),
            Err("`quit` takes no argument".to_owned())
        );
        assert_eq!(
            parse("shot a b"),
            Err("too many arguments for `shot`".to_owned())
        );
    }

    #[test]
    fn take_line_splits_and_keeps_rest() {
        let mut buf = b"outputs\nsta".to_vec();
        assert_eq!(take_line(&mut buf).as_deref(), Some("outputs"));
        assert_eq!(buf, b"sta");
        assert_eq!(take_line(&mut buf), None);
        buf.extend_from_slice(b"ts\n\n");
        assert_eq!(take_line(&mut buf).as_deref(), Some("stats"));
        assert_eq!(take_line(&mut buf).as_deref(), Some(""));
        assert!(buf.is_empty());
    }

    #[test]
    fn formats_replies() {
        assert_eq!(err_reply("no such\noutput"), b"err no such output\n");
        assert_eq!(ok_reply(), b"ok\n");
        let img = Image {
            width: 2,
            height: 1,
            stride: 8,
            data: vec![1, 2, 3, 0, 4, 5, 6, 0],
        };
        let mut want = b"ok 2 1 8\n".to_vec();
        want.extend_from_slice(&img.data);
        assert_eq!(shot_reply(&img), want);
        let outs = [OutputInfo {
            id: OutputId(1),
            name: "Virtual-1".to_owned(),
            width: 640,
            height: 480,
            refresh_mhz: 60_000,
            phys_mm: (0, 0),
        }];
        assert_eq!(outputs_reply(&outs), b"ok\nVirtual-1 640x480@60000\n\n");
        assert_eq!(outputs_reply(&[]), b"ok\n\n");
        assert_eq!(
            stats_reply(&[("frames", 7), ("flips_pending", 0)]),
            b"ok\nframes 7\nflips_pending 0\n\n"
        );
    }
}

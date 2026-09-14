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
//! | `outputs`            | `ok\n` + one [`OutputLine`] per output + `\n`                 |
//! | `stats`              | `ok\n` + `key value\n` lines + `\n`                           |
//! | `quit`               | `ok\n`, then the server shuts down                           |
//! | `reload`             | `ok\n`; re-reads `server.conf` and applies it                |
//! | `plug WxH`           | `ok\n`; fake backend only — hotplugs an output in, for tests |
//! | `unplug`             | `ok\n`; fake backend only — removes the last output          |
//! | `focus`              | `ok\n`; focuses the topmost window, for tests                |
//! | `theme`              | `ok <scheme> <serial>\n` + `role #rrggbb[aa]\n` lines + `\n` |
//!
//! This module only parses and formats; it never touches a socket.

use std::fmt::Write as _;

use nitro_kms::Image;

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
    /// Re-read `server.conf` and apply it.
    ///
    /// The third way into `Server::reload_config`, beside SIGHUP and the
    /// inotify watch, and the one a *test* can use: it is synchronous —
    /// the `ok` comes back after the reload has been applied — where the
    /// other two are races against the loop noticing.
    Reload,
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
    /// Print the current colour scheme and every role's colour.
    ///
    /// Read-only, and the cheap way to answer "what colour is the
    /// desktop actually using" without a client, a screenshot or a
    /// `hey`: the palette is server state, so the server is the only
    /// thing that can say.
    Theme,
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
        ("reload", None) => Ok(Request::Reload),
        ("focus", None) => Ok(Request::Focus),
        ("theme", None) => Ok(Request::Theme),
        ("outputs" | "stats" | "quit" | "reload" | "focus" | "unplug" | "theme", Some(_)) => {
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

/// One output's line in the `outputs` reply.
///
/// The reply used to be formatted straight out of [`nitro_kms::OutputInfo`],
/// which knows the mode and nothing else. Scale, desktop position and
/// which output is primary are the server's view — the scene's, the
/// configuration file's and the window manager's — so the caller assembles
/// this and the formatting stays here.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputLine {
    /// Connector name, e.g. `HDMI-A-1`.
    pub name: String,
    /// Mode width in device pixels.
    pub width: u32,
    /// Mode height in device pixels.
    pub height: u32,
    /// Refresh rate in millihertz.
    pub refresh_mhz: u32,
    /// Logical-to-device scale in force, after `NITRO_SCALE`, the file and
    /// the EDID default have had their say.
    pub scale: f32,
    /// This output's origin in **desktop** (logical) space — the space
    /// window positions are in, not the device-pixel space of the mode.
    pub position: (i32, i32),
    /// Whether this is the primary output: where orphaned windows migrate
    /// and what a window with no output of its own is measured against.
    pub primary: bool,
}

/// Format a scale the way a person writes one: `2`, not `2.0`; `1.25`
/// stays `1.25`.
///
/// A trailing `.0` is noise in a line meant to be read on a terminal, and
/// `scale=2` is also what the configuration file that produced it says.
fn scale_text(scale: f32) -> String {
    if scale.fract() == 0.0 && scale.is_finite() {
        format!("{}", scale.trunc() as i64)
    } else {
        format!("{scale}")
    }
}

/// `ok\n`, one line per output, blank line.
///
/// Each line is `name WxH@refresh_mhz scale=<s> pos=<x>,<y> primary=<0|1>`,
/// where `pos` is the **desktop-space logical** origin: the number a window
/// position on that output is relative to, which is what someone debugging
/// a two-monitor layout is actually asking for. The device-pixel rectangle
/// is `WxH` at that origin times the scale, so both spaces are recoverable
/// from the one line.
pub fn outputs_reply(outputs: &[OutputLine]) -> Vec<u8> {
    let mut s = String::from("ok\n");
    for o in outputs {
        // Writing to a String cannot fail.
        let _ = writeln!(
            s,
            "{} {}x{}@{} scale={} pos={},{} primary={}",
            o.name,
            o.width,
            o.height,
            o.refresh_mhz,
            scale_text(o.scale),
            o.position.0,
            o.position.1,
            u8::from(o.primary),
        );
    }
    s.push('\n');
    s.into_bytes()
}

/// `ok\n`, one `key value` line per pair, blank line.
pub fn stats_reply(pairs: &[(&str, u64)]) -> Vec<u8> {
    stats_reply_with(pairs, &[])
}

/// The same, plus trailing pairs whose value is **text**.
///
/// Every statistic was a `u64` until M4-E1, and almost all of them still
/// are — a counter is the right shape for "how many" and it parses in one
/// line. `remote_listen` is not a count: it is the address the remote
/// listener bound, or `off`, and the question a caller asks of it ("which
/// port did `:0` resolve to?") has no numeric answer. The line format is
/// unchanged, `key value`, so a reader that splits on the first space and
/// parses the rest as a number simply skips it — which is exactly what
/// `nitro-demo`'s parser already does with a line it cannot use.
pub fn stats_reply_with(pairs: &[(&str, u64)], text: &[(&str, String)]) -> Vec<u8> {
    let mut s = String::from("ok\n");
    for (k, v) in pairs {
        let _ = writeln!(s, "{k} {v}");
    }
    for (k, v) in text {
        let _ = writeln!(s, "{k} {v}");
    }
    s.push('\n');
    s.into_bytes()
}

/// The `theme` reply: a status line naming the scheme and the palette's
/// serial, then one `role #rrggbb[aa]` line per role, then a blank line.
///
/// The role names are [`Role::key`]'s, which are also the `server.conf`
/// keys — so a line of this output, with `theme.` in front of it, *is*
/// the configuration that would pin that colour. That is the point:
/// reading a colour out and writing it back must not need a translation
/// table.
#[must_use]
pub fn theme_reply(
    scheme: nitro_core::Scheme,
    serial: u32,
    palette: &nitro_core::Palette,
) -> Vec<u8> {
    let mut s = format!("ok {} {serial}\n", scheme.name());
    for (role, color) in palette.iter() {
        let _ = writeln!(
            s,
            "{} {}",
            role.key(),
            nitro_core::palette::format_color(color)
        );
    }
    s.push('\n');
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parse("reload"), Ok(Request::Reload));
        assert_eq!(parse("  reload \r\n"), Ok(Request::Reload));
        assert_eq!(
            parse("reload now"),
            Err("`reload` takes no argument".to_owned())
        );
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
        let outs = [OutputLine {
            name: "Virtual-1".to_owned(),
            width: 640,
            height: 480,
            refresh_mhz: 60_000,
            scale: 1.0,
            position: (0, 0),
            primary: true,
        }];
        assert_eq!(
            outputs_reply(&outs),
            b"ok\nVirtual-1 640x480@60000 scale=1 pos=0,0 primary=1\n\n"
        );
        assert_eq!(outputs_reply(&[]), b"ok\n\n");
        assert_eq!(
            stats_reply(&[("frames", 7), ("flips_pending", 0)]),
            b"ok\nframes 7\nflips_pending 0\n\n"
        );
    }

    #[test]
    fn an_outputs_line_carries_the_scale_position_and_primary_flag() {
        // Two outputs, the second to the right of the first and 2x, which
        // is the layout the format exists to describe: `pos` is in
        // *desktop* units, so the second one starts at the first one's
        // logical width and its own 1280 device pixels are 640 wide there.
        let outs = [
            OutputLine {
                name: "HDMI-A-1".to_owned(),
                width: 1920,
                height: 1080,
                refresh_mhz: 59_951,
                scale: 1.0,
                position: (0, 0),
                primary: false,
            },
            OutputLine {
                name: "DP-1".to_owned(),
                width: 1280,
                height: 720,
                refresh_mhz: 60_000,
                scale: 2.0,
                position: (1920, 0),
                primary: true,
            },
        ];
        let text = String::from_utf8(outputs_reply(&outs)).unwrap();
        assert_eq!(
            text,
            "ok\n\
             HDMI-A-1 1920x1080@59951 scale=1 pos=0,0 primary=0\n\
             DP-1 1280x720@60000 scale=2 pos=1920,0 primary=1\n\n"
        );
    }

    #[test]
    fn a_whole_scale_prints_without_a_trailing_zero() {
        // `scale=2` is what the configuration file that produced it says,
        // and `scale=2.0` in a line meant to be read on a terminal is
        // noise. A fractional scale still prints in full.
        assert_eq!(scale_text(2.0), "2");
        assert_eq!(scale_text(1.0), "1");
        assert_eq!(scale_text(1.25), "1.25");
        assert_eq!(scale_text(0.5), "0.5");
    }

    #[test]
    fn a_negative_output_position_survives_the_round_trip() {
        // A screen may be to the *left* of the origin, which is what the
        // configuration file allows and what a signed `pos` is for.
        let outs = [OutputLine {
            name: "VGA-1".to_owned(),
            width: 1024,
            height: 768,
            refresh_mhz: 60_000,
            scale: 1.0,
            position: (-1024, -100),
            primary: false,
        }];
        assert_eq!(
            outputs_reply(&outs),
            b"ok\nVGA-1 1024x768@60000 scale=1 pos=-1024,-100 primary=0\n\n"
        );
    }
}

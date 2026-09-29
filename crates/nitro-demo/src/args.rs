//! Command-line parsing: a plain struct, no dependency.
//!
//! Parsing is a free function over an iterator so it is testable without a
//! process, which is the same shape `nitro-shot` uses.

use std::fmt;

/// What the demo does with the window it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Commit only in response to input. An idle desktop is then *exactly*
    /// zero frames, which is the property the whole design exists for, and
    /// it is the mode the latency numbers are taken in.
    #[default]
    Follow,
    /// Drive an animation off `RequestFrame`/`Frame`, one commit per
    /// frame. Verifies frame pacing and that a client aiming at the
    /// deadline never commits twice for one flip.
    Animate,
    /// Feed a `Surface` node NV12 frames with `PresentSurface`: colour
    /// bars, a moving box and a frame counter ([`crate::video`]).
    Video,
}

/// The `--video` options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoOpts {
    /// Buffer size in pixels (`--size WxH`), both even.
    pub size: (u32, u32),
    /// Frame rate (`--fps 30|60`).
    pub fps: u32,
    /// Ask for fullscreen at start (`--fullscreen`).
    pub fullscreen: bool,
    /// Reallocate the ring at the size a `SurfaceHint` asks for
    /// (`--follow-hint`).
    pub follow_hint: bool,
    /// Quit after this many `Presented` frames (`--frames N`, 0 = never).
    pub frames: u64,
}

impl Default for VideoOpts {
    fn default() -> Self {
        Self {
            size: (1280, 720),
            fps: 60,
            fullscreen: false,
            follow_hint: false,
            frames: 0,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Follow => "follow",
            Mode::Animate => "animate",
            Mode::Video => "video",
        })
    }
}

/// Parsed command line.
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    /// Follow the pointer, or animate.
    pub mode: Mode,
    /// How many windows to open (`--windows N`), at least 1.
    pub windows: u32,
    /// Outline damage rectangles. `NITRO_DEMO_SHOW_DAMAGE=1` presets it
    /// and `d` toggles it at runtime.
    pub show_damage: bool,
    /// Also read the server's own `stats` over the control socket and
    /// print both views.
    pub stats: bool,
    /// Exit after this many seconds (0 = run until told to stop). What
    /// lets the measurement script be a one-liner.
    pub seconds: u64,
    /// Write a downscaled PNG of the *client's own* idea of its window to
    /// this path and exit. The fallback for a box without `ImageMagick`;
    /// see [`crate::scene::save_small`].
    pub save_small: Option<String>,
    /// The `--video` options (ignored in the other modes).
    pub video: VideoOpts,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            windows: 1,
            show_damage: false,
            stats: false,
            seconds: 0,
            save_small: None,
            video: VideoOpts::default(),
        }
    }
}

/// Usage text, printed for `--help` and for a bad argument.
pub const USAGE: &str = "\
usage: nitro-demo [--follow | --animate] [--windows N] [--damage] [--stats]
                  [--seconds S] [--save-small FILE]
       nitro-demo --video [--size WxH] [--fps 30|60] [--fullscreen]
                  [--follow-hint] [--frames N] [--seconds S]

  --follow           commit only on input (default); idle costs zero frames
  --animate          move a rect one step per Frame callback
  --windows N        open N windows; `n`/`p` select the next/previous one
  --damage           outline the rects each commit damages
  --stats            also print the server's own i2p figures (control socket)
  --seconds S        quit after S seconds
  --save-small FILE  write a downscaled PNG of the demo image and exit

  --video            NV12 Surface test client: colour bars, moving box, counter
  --size WxH         video buffer size (default 1280x720; even)
  --fps 30|60        video frame rate (default 60)
  --fullscreen       ask for fullscreen at start
  --follow-hint      reallocate buffers at the size a SurfaceHint asks for
  --frames N         quit after N presented video frames

Environment: NITRO_SOCKET, NITRO_CONTROL, NITRO_DEMO_SHOW_DAMAGE=1.
Keys: q quit, d toggle damage outlines, Esc close the window, n/p select.
Video keys: space pause, f toggle fullscreen, q quit.
Selecting tints a window's follower: v1 has no client-initiated stacking
message, so a client cannot raise itself (the server raises on a click).";

/// Parse `args` (without the program name).
///
/// # Errors
/// A message ready to print: unknown flag, missing value, or `--help`.
pub fn parse(
    args: impl IntoIterator<Item = String>,
    show_damage_env: bool,
) -> Result<Args, String> {
    let mut out = Args {
        show_damage: show_damage_env,
        ..Args::default()
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--follow" => out.mode = Mode::Follow,
            "--animate" => out.mode = Mode::Animate,
            "--video" => out.mode = Mode::Video,
            "--size" => out.video.size = size(it.next())?,
            "--fps" => {
                out.video.fps = match number(&mut it, "--fps")? {
                    f @ (30 | 60) => f as u32,
                    f => return Err(format!("--fps: {f} is not 30 or 60")),
                };
            }
            "--fullscreen" => out.video.fullscreen = true,
            "--follow-hint" => out.video.follow_hint = true,
            "--frames" => out.video.frames = number(&mut it, "--frames")?,
            "--damage" => out.show_damage = true,
            "--stats" => out.stats = true,
            "--windows" => out.windows = number(&mut it, "--windows")?.max(1) as u32,
            "--seconds" => out.seconds = number(&mut it, "--seconds")?,
            "--save-small" => {
                out.save_small = Some(it.next().ok_or("--save-small needs a FILE")?);
            }
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(out)
}

/// Parse `--size`'s `WxH`: both positive, even, at most 8192.
fn size(raw: Option<String>) -> Result<(u32, u32), String> {
    let raw = raw.ok_or("--size needs WxH")?;
    let bad = || format!("--size: {raw:?} is not WxH with even sides in 2..=8192");
    let (w, h) = raw.split_once(['x', 'X']).ok_or_else(bad)?;
    let (w, h): (u32, u32) = (w.parse().map_err(|_| bad())?, h.parse().map_err(|_| bad())?);
    let ok = |v: u32| (2..=8192).contains(&v) && v.is_multiple_of(2);
    if ok(w) && ok(h) {
        Ok((w, h))
    } else {
        Err(bad())
    }
}

/// Read the next argument as a `u64`.
fn number(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    let raw = it.next().ok_or_else(|| format!("{flag} needs a number"))?;
    raw.parse()
        .map_err(|_| format!("{flag}: {raw:?} is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Result<Args, String> {
        parse(s.split_whitespace().map(str::to_owned), false)
    }

    #[test]
    fn the_default_is_follow_one_window_no_damage() {
        let a = args("").unwrap();
        assert_eq!(a.mode, Mode::Follow);
        assert_eq!(a.windows, 1);
        assert!(!a.show_damage && !a.stats);
        assert_eq!(a.seconds, 0);
        assert_eq!(a.save_small, None);
    }

    #[test]
    fn flags_are_parsed() {
        let a = args("--animate --windows 5 --damage --stats --seconds 10").unwrap();
        assert_eq!(a.mode, Mode::Animate);
        assert_eq!(a.windows, 5);
        assert!(a.show_damage && a.stats);
        assert_eq!(a.seconds, 10);
    }

    #[test]
    fn the_last_mode_wins() {
        assert_eq!(args("--animate --follow").unwrap().mode, Mode::Follow);
    }

    /// Zero windows would open nothing and then wait forever for input
    /// that has nowhere to land; one is the only sane floor.
    #[test]
    fn zero_windows_is_clamped_to_one() {
        assert_eq!(args("--windows 0").unwrap().windows, 1);
    }

    #[test]
    fn the_environment_preset_can_be_overridden_upward_only() {
        let on = parse(std::iter::empty(), true).unwrap();
        assert!(on.show_damage, "NITRO_DEMO_SHOW_DAMAGE=1 presets it");
    }

    #[test]
    fn bad_arguments_explain_themselves() {
        assert!(args("--windows").unwrap_err().contains("needs a number"));
        assert!(args("--windows x").unwrap_err().contains("not a number"));
        assert!(args("--save-small").unwrap_err().contains("needs a FILE"));
        assert!(args("--wat").unwrap_err().contains("unknown argument"));
        assert!(args("--help").unwrap_err().contains("usage:"));
    }

    #[test]
    fn video_defaults_are_720p60_windowed() {
        let a = args("--video").unwrap();
        assert_eq!(a.mode, Mode::Video);
        assert_eq!(a.video, VideoOpts::default());
        assert_eq!((a.video.size, a.video.fps), ((1280, 720), 60));
        assert!(!a.video.fullscreen && !a.video.follow_hint);
        assert_eq!(a.video.frames, 0);
        assert_eq!(Mode::Video.to_string(), "video");
    }

    #[test]
    fn video_flags_are_parsed() {
        let a =
            args("--video --size 640x360 --fps 30 --fullscreen --follow-hint --frames 10").unwrap();
        assert_eq!(
            a.video,
            VideoOpts {
                size: (640, 360),
                fps: 30,
                fullscreen: true,
                follow_hint: true,
                frames: 10,
            }
        );
        assert_eq!(
            args("--video --size 1920X1080").unwrap().video.size,
            (1920, 1080)
        );
    }

    #[test]
    fn bad_video_arguments_explain_themselves() {
        assert!(args("--size").unwrap_err().contains("needs WxH"));
        for bad in ["1280", "x720", "1281x720", "0x0", "99999x2", "axb"] {
            assert!(
                args(&format!("--size {bad}"))
                    .unwrap_err()
                    .contains("not WxH"),
                "{bad}"
            );
        }
        assert!(args("--fps 50").unwrap_err().contains("not 30 or 60"));
        assert!(args("--fps").unwrap_err().contains("needs a number"));
    }
}

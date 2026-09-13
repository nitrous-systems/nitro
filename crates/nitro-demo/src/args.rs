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
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Follow => "follow",
            Mode::Animate => "animate",
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
        }
    }
}

/// Usage text, printed for `--help` and for a bad argument.
pub const USAGE: &str = "\
usage: nitro-demo [--follow | --animate] [--windows N] [--damage] [--stats]
                  [--seconds S] [--save-small FILE]

  --follow           commit only on input (default); idle costs zero frames
  --animate          move a rect one step per Frame callback
  --windows N        open N windows; `n`/`p` select the next/previous one
  --damage           outline the rects each commit damages
  --stats            also print the server's own i2p figures (control socket)
  --seconds S        quit after S seconds
  --save-small FILE  write a downscaled PNG of the demo image and exit

Environment: NITRO_SOCKET, NITRO_CONTROL, NITRO_DEMO_SHOW_DAMAGE=1.
Keys: q quit, d toggle damage outlines, Esc close the window, n/p select.
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
}

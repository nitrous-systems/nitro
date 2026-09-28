//! Volume and mute, by shelling out to whatever the machine has.
//!
//! nitro has no audio in it and is not going to grow any: `PipeWire` is
//! the sound server on every desktop this runs on, and a display server
//! that also owned the mixer would be two daemons in one process. So the
//! audio section is a *remote control* — it runs `wpctl` (`PipeWire`'s
//! own CLI), falls back to `pactl` (`PulseAudio`'s, which `PipeWire` also
//! answers), and says so plainly when neither is installed.
//!
//! # No daemon, no D-Bus, no polling timer
//!
//! The volume is read **once**, when the tree is built, and written when
//! the user moves the slider. There is no subscription and no timer,
//! because either would cost a wakeup per interval for a number that
//! changes only when somebody changes it — and the person most likely to
//! change it is the one looking at this window. A volume altered by a
//! media key while the dialog is open will be stale until Revert; that is
//! the price of the idle contract, and `docs/`-facing crate docs say so.
//!
//! # Parsing defensively
//!
//! `wpctl get-volume @DEFAULT_AUDIO_SINK@` prints
//!
//! ```text
//! Volume: 0.65 [MUTED]
//! ```
//!
//! and that is not a contract. It has changed format before and will
//! again, so [`parse_wpctl_volume`] looks for *a float somewhere in the
//! line* and for the word `MUTED` anywhere, rather than matching a shape.
//! An output it cannot make sense of is `None` — the section then shows
//! that it could not read the volume instead of showing a confident zero.
//!
//! # The search path is injected, not read from the environment
//!
//! [`Backend::detect_in`] takes the directories to look in. The tests
//! point it at a temporary directory holding a fake `wpctl` script, and
//! doing that by setting `PATH` would not do: `std::env::set_var` is
//! `unsafe` and process-global, so one test's `PATH` would be every
//! concurrently running test's `PATH`. This is the same reasoning that
//! makes `nitro-bar` inject its sensor source.
//!
//! # Output devices
//!
//! [`Backend::sinks`] lists the sinks and which one is the default, and
//! [`Backend::set_default_sink`] moves the default — the quick-settings
//! "Sound Output" list. `wpctl status` is a tree drawn for people, so
//! [`parse_wpctl_sinks`] reads it by shape (a `*`, an `id.`, a
//! description, a `[vol: …]` suffix) inside the `Audio` → `Sinks:` block
//! and nowhere else; `pactl list sinks` is read as `Name:`/`Description:`
//! pairs by [`parse_pactl_sinks`].

use std::path::{Path, PathBuf};
use std::process::Command;

/// The default sink, as each tool spells it.
const WPCTL_SINK: &str = "@DEFAULT_AUDIO_SINK@";
/// The same thing in `pactl`'s vocabulary.
const PACTL_SINK: &str = "@DEFAULT_SINK@";

/// A volume and a mute flag, as one reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume {
    /// Linear volume, `0.0..=1.0`. Values above 1.0 (both tools allow
    /// them) are clamped, because the slider's range is the promise the
    /// UI makes and a knob that can point past its own end is a bug.
    pub level: f32,
    /// Whether the sink is muted.
    pub muted: bool,
}

/// One output device, as the mixer names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    /// What [`Backend::set_default_sink`] passes back to the tool:
    /// `wpctl`'s numeric object id, or `pactl`'s sink *name*
    /// (`alsa_output.pci-0000_00_1f.3.analog-stereo`).
    pub id: String,
    /// The human description, for a list row.
    pub name: String,
    /// Whether this is the current default sink.
    pub default: bool,
}

/// Which command-line mixer this machine has.
///
/// An enum rather than a trait object: there are exactly two, they are
/// known at compile time, and the difference between them is three
/// argument vectors. A `Box<dyn AudioBackend>` would buy dispatch nobody
/// needs and cost the tests a mock they would have to keep honest — the
/// fake in the tests is a *shell script called `wpctl`*, which exercises
/// the argument building and the parsing rather than replacing them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// `PipeWire`'s own tool. Preferred: it is what the sound server on
    /// these machines ships, and it speaks to it natively.
    Wpctl(PathBuf),
    /// `PulseAudio`'s tool, which `PipeWire`'s pulse shim also answers.
    /// The fallback, so a box running plain `PulseAudio` still works.
    Pactl(PathBuf),
}

impl Backend {
    /// Find a backend among `dirs`, preferring `wpctl`.
    ///
    /// Takes the directories rather than reading `PATH` so a test can
    /// point it at a fake; see the module docs for why that is not done
    /// with `std::env::set_var`.
    #[must_use]
    pub fn detect_in(dirs: &[PathBuf]) -> Option<Self> {
        for (name, make) in [
            ("wpctl", Self::Wpctl as fn(PathBuf) -> Self),
            ("pactl", Self::Pactl as fn(PathBuf) -> Self),
        ] {
            for dir in dirs {
                let candidate = dir.join(name);
                if is_executable(&candidate) {
                    return Some(make(candidate));
                }
            }
        }
        None
    }

    /// Find a backend on the process's `PATH`: what the binary does.
    #[must_use]
    pub fn detect() -> Option<Self> {
        Self::detect_in(&path_dirs())
    }

    /// The tool's name, for a message the user reads.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Wpctl(_) => "wpctl",
            Self::Pactl(_) => "pactl",
        }
    }

    /// Read the default sink's volume and mute state.
    ///
    /// `None` when the tool fails or prints something unparseable. Both
    /// are the same answer to the caller — "no reading" — because a
    /// mixer that ran and said nothing useful is no more informative than
    /// one that did not run.
    #[must_use]
    pub fn volume(&self) -> Option<Volume> {
        match self {
            Self::Wpctl(bin) => {
                let out = run(bin, &["get-volume", WPCTL_SINK])?;
                parse_wpctl_volume(&out)
            }
            Self::Pactl(bin) => {
                let level = parse_pactl_volume(&run(bin, &["get-sink-volume", PACTL_SINK])?)?;
                // Two commands, because `pactl` reports the two facts
                // separately. A missing mute answer is taken as unmuted
                // rather than failing the whole reading: the volume is
                // the number the slider needs, and a checkbox that is
                // wrong in one direction beats a section that shows
                // nothing.
                let muted = run(bin, &["get-sink-mute", PACTL_SINK])
                    .and_then(|s| parse_pactl_mute(&s))
                    .unwrap_or(false);
                Some(Volume { level, muted })
            }
        }
    }

    /// Set the default sink's volume, as a linear `0.0..=1.0`.
    ///
    /// Both tools are told a **percentage**, because both accept one and
    /// an integer percent is what the slider's 5-point steps produce
    /// exactly — where `0.65` printed from an `f32` is a rounding
    /// argument nobody needs to have.
    ///
    /// # Errors
    /// If the tool cannot be run or exits non-zero.
    pub fn set_volume(&self, level: f32) -> Result<(), String> {
        let percent = format!("{}%", (level.clamp(0.0, 1.0) * 100.0).round() as u32);
        match self {
            Self::Wpctl(bin) => try_run(bin, &["set-volume", WPCTL_SINK, &percent]),
            Self::Pactl(bin) => try_run(bin, &["set-sink-volume", PACTL_SINK, &percent]),
        }
    }

    /// The output devices, with the default marked.
    ///
    /// `None` when the tool fails or its output cannot be read; an empty
    /// list when it reads fine and there are no sinks.
    #[must_use]
    pub fn sinks(&self) -> Option<Vec<Sink>> {
        match self {
            Self::Wpctl(bin) => parse_wpctl_sinks(&run(bin, &["status"])?),
            Self::Pactl(bin) => {
                let default = run(bin, &["get-default-sink"]);
                let default = default.as_deref().map(str::trim).filter(|d| !d.is_empty());
                run(bin, &["list", "sinks"])
                    .and_then(|out| parse_pactl_sinks(&out, default))
                    // The long listing is localised and verbose; the short
                    // one is tab-separated columns and is not.
                    .or_else(|| {
                        parse_pactl_short_sinks(&run(bin, &["list", "short", "sinks"])?, default)
                    })
            }
        }
    }

    /// Make `sink` the default output.
    ///
    /// # Errors
    /// If the tool cannot be run or exits non-zero (an id that no longer
    /// exists, for one).
    pub fn set_default_sink(&self, sink: &Sink) -> Result<(), String> {
        match self {
            Self::Wpctl(bin) => try_run(bin, &["set-default", &sink.id]),
            Self::Pactl(bin) => try_run(bin, &["set-default-sink", &sink.id]),
        }
    }

    /// Mute or unmute the default sink.
    ///
    /// # Errors
    /// If the tool cannot be run or exits non-zero.
    pub fn set_muted(&self, muted: bool) -> Result<(), String> {
        let flag = if muted { "1" } else { "0" };
        match self {
            Self::Wpctl(bin) => try_run(bin, &["set-mute", WPCTL_SINK, flag]),
            Self::Pactl(bin) => try_run(bin, &["set-sink-mute", PACTL_SINK, flag]),
        }
    }
}

/// The directories on `PATH`, in order.
#[must_use]
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Whether `path` is a file the current user may execute.
///
/// The mode bits rather than a `try-and-see`: running a candidate to find
/// out whether it is runnable would run the first `wpctl`-shaped thing on
/// `PATH`, and the point of the check is to decide whether to run it.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Run a tool and return its stdout, or `None` if it failed.
fn run(bin: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Run a tool for effect, reporting what went wrong.
fn try_run(bin: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = stderr.lines().next().unwrap_or("failed").trim();
    Err(format!("{}: {detail}", bin.display()))
}

/// Parse `wpctl get-volume`'s output.
///
/// Looks for a **decimal fraction** somewhere in the text and for `MUTED`
/// anywhere in it, rather than matching `Volume: <x> [MUTED]` exactly:
/// that string is not a contract, and a version that adds a channel
/// breakdown or renames the label should move the slider, not blank the
/// section.
///
/// "A token containing a dot" rather than "the first number", because
/// the first number in `Sink 42 volume: 0.50` is the sink id — which
/// would be read as a volume of 4200 %, clamp to full, and set the
/// machine's volume to maximum on a format this function was supposed to
/// tolerate. `wpctl` prints the volume with decimals in every version
/// there has been; a text with no dotted number at all falls back to the
/// first integer, so a future `Volume: 1` still works.
#[must_use]
pub fn parse_wpctl_volume(out: &str) -> Option<Volume> {
    let muted = out.to_ascii_uppercase().contains("MUTED");
    let numbers = || out.split_whitespace().filter_map(number_in);
    let level = numbers()
        .find(|(token, _)| token.contains('.'))
        .or_else(|| numbers().next())
        .map(|(_, v)| v)
        .filter(|v: &f32| v.is_finite())?;
    Some(Volume {
        level: level.clamp(0.0, 1.0),
        muted,
    })
}

/// The number inside a whitespace-separated token, with its cleaned text.
///
/// The punctuation around it is stripped (`[MUTED]`, `0.65,`, `(0.65)`)
/// so a decoration nobody anticipated does not hide the value; the
/// cleaned text comes back too, because the caller's rule is about
/// whether the number had a decimal point.
fn number_in(token: &str) -> Option<(&str, f32)> {
    let cleaned = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
    Some((cleaned, cleaned.parse::<f32>().ok()?))
}

/// Parse `pactl get-sink-volume`'s output.
///
/// Its line is
///
/// ```text
/// Volume: front-left: 42926 /  65% / -11.10 dB,   front-right: ...
/// ```
///
/// so the useful number is the **first percentage**, not the first float
/// (which is the raw 0–65536 value) and not the last (which is decibels,
/// and negative). A per-channel volume is reduced to the first channel:
/// this app has one slider, and offering to set a stereo pair to one
/// value is exactly what that slider means.
#[must_use]
pub fn parse_pactl_volume(out: &str) -> Option<f32> {
    let percent = out
        .split_whitespace()
        .find_map(|t| t.strip_suffix('%')?.parse::<f32>().ok())?;
    if !percent.is_finite() {
        return None;
    }
    Some((percent / 100.0).clamp(0.0, 1.0))
}

/// Parse `pactl get-sink-mute`'s output: `Mute: yes` / `Mute: no`.
#[must_use]
pub fn parse_pactl_mute(out: &str) -> Option<bool> {
    let lower = out.to_ascii_lowercase();
    let value = lower.split_once("mute:")?.1.trim();
    match value.split_whitespace().next()? {
        "yes" | "true" | "1" => Some(true),
        "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// Box-drawing and indentation `wpctl status` draws its tree with.
fn is_tree(c: char) -> bool {
    c.is_whitespace() || matches!(c, '│' | '├' | '└' | '─' | '|' | '`' | '-')
}

/// Parse `wpctl status`'s `Audio` → `Sinks:` block.
///
/// ```text
/// Audio
///  ├─ Devices:
///  │      42. Built-in Audio                      [alsa]
///  │
///  ├─ Sinks:
///  │  *   46. Built-in Audio Analog Stereo        [vol: 0.40]
///  │      51. HDMI 1.2 Output                     [vol: 1.00 MUTED]
///  │
///  ├─ Sources:
/// ```
///
/// By shape rather than by column: the tree characters are stripped, an
/// entry is an optional `*` (the default), digits and a `.`, then the
/// description with a trailing `[…]` removed. The block ends at the next
/// header (`Sources:`, `Filters:`, …), a blank tree line, or the next
/// top-level section (`Video` has a `Sinks:` of its own). `None` when
/// there is no `Audio` → `Sinks:` block at all.
#[must_use]
pub fn parse_wpctl_sinks(out: &str) -> Option<Vec<Sink>> {
    let mut in_audio = false;
    let mut in_sinks = false;
    let mut found = false;
    let mut sinks = Vec::new();
    for line in out.lines() {
        let top_level = line.chars().next().is_some_and(|c| !is_tree(c));
        if top_level {
            in_audio = line.trim() == "Audio";
            in_sinks = false;
            continue;
        }
        if !in_audio {
            continue;
        }
        let body = line.trim_start_matches(is_tree).trim_end();
        if body.is_empty() {
            in_sinks = false;
            continue;
        }
        if body.ends_with(':') && !body.starts_with(|c: char| c == '*' || c.is_ascii_digit()) {
            in_sinks = body == "Sinks:";
            found |= in_sinks;
            continue;
        }
        if in_sinks && let Some(sink) = wpctl_entry(body) {
            sinks.push(sink);
        }
    }
    found.then_some(sinks)
}

/// One `*   46. Description [vol: 0.40]` entry, tree already stripped.
fn wpctl_entry(body: &str) -> Option<Sink> {
    let (default, rest) = match body.strip_prefix('*') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, body),
    };
    let (id, rest) = rest.split_once('.')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut name = rest.trim();
    if name.ends_with(']')
        && let Some(open) = name.rfind('[')
    {
        name = name[..open].trim_end();
    }
    if name.is_empty() {
        return None;
    }
    Some(Sink {
        id: id.to_owned(),
        name: name.to_owned(),
        default,
    })
}

/// Parse `pactl list sinks`: one `Sink #N` record per sink, each with a
/// `Name:` and a `Description:` line. `default` is
/// `pactl get-default-sink`'s answer, matched against the names.
///
/// `None` when no record with a name was found — which is also what a
/// localised `pactl` looks like, and why [`Backend::sinks`] then falls
/// back to the short listing. A record with no description is listed
/// under its name.
#[must_use]
pub fn parse_pactl_sinks(out: &str, default: Option<&str>) -> Option<Vec<Sink>> {
    let mut records: Vec<(Option<String>, Option<String>)> = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.starts_with("Sink #") {
            records.push((None, None));
        } else if let Some(record) = records.last_mut() {
            if let Some(v) = line.strip_prefix("Name:") {
                record.0.get_or_insert_with(|| v.trim().to_owned());
            } else if let Some(v) = line.strip_prefix("Description:") {
                record.1.get_or_insert_with(|| v.trim().to_owned());
            }
        }
    }
    let sinks: Vec<Sink> = records
        .into_iter()
        .filter_map(|(id, name)| {
            let id = id.filter(|i| !i.is_empty())?;
            let name = name.filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone());
            Some(Sink {
                default: default == Some(id.as_str()),
                id,
                name,
            })
        })
        .collect();
    (!sinks.is_empty()).then_some(sinks)
}

/// Parse `pactl list short sinks`: `index<TAB>name<TAB>driver…`, the
/// name (column 2) doubling as the description.
///
/// `None` for text that has lines but none of this shape; an empty
/// output is `Some` of no sinks.
#[must_use]
pub fn parse_pactl_short_sinks(out: &str, default: Option<&str>) -> Option<Vec<Sink>> {
    let mut any_line = false;
    let mut sinks = Vec::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        any_line = true;
        let mut cols = line.split_whitespace();
        let (Some(index), Some(name)) = (cols.next(), cols.next()) else {
            continue;
        };
        if !index.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        sinks.push(Sink {
            id: name.to_owned(),
            name: name.to_owned(),
            default: default == Some(name),
        });
    }
    (!any_line || !sinks.is_empty()).then_some(sinks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_wpctl_output_parses() {
        let v = parse_wpctl_volume("Volume: 0.65\n").expect("a volume");
        assert!((v.level - 0.65).abs() < 1e-6, "{v:?}");
        assert!(!v.muted);

        let v = parse_wpctl_volume("Volume: 0.65 [MUTED]\n").expect("a volume");
        assert!((v.level - 0.65).abs() < 1e-6, "{v:?}");
        assert!(v.muted);
    }

    #[test]
    fn wpctl_output_is_parsed_by_shape_not_by_format() {
        // Every one of these is a plausible future `wpctl`, and each one
        // must move the slider rather than blank the section.
        for text in [
            "Volume: 0.50",
            "volume 0.50 for sink 42",
            "Sink 42 volume: 0.50 [some note]",
            "Volume: 0.50,",
            "  0.50  ",
        ] {
            let v = parse_wpctl_volume(text).unwrap_or_else(|| panic!("{text:?}"));
            assert!((v.level - 0.5).abs() < 1e-6, "{text:?} → {v:?}");
        }
    }

    #[test]
    fn a_sink_id_in_front_of_the_volume_is_not_mistaken_for_it() {
        // The bug this rule exists for: the first *number* in
        // `Sink 42 volume: 0.50` is the sink id, and reading it as a
        // volume would clamp to full — setting the machine to maximum on
        // a format this parser was supposed to tolerate.
        let v = parse_wpctl_volume("Sink 42 volume: 0.10").expect("a volume");
        assert!((v.level - 0.1).abs() < 1e-6, "{v:?}");
        // And an integer volume still parses, when there is no fraction
        // anywhere to prefer.
        let v = parse_wpctl_volume("Volume: 1").expect("a volume");
        assert!((v.level - 1.0).abs() < 1e-6, "{v:?}");
    }

    #[test]
    fn an_unparseable_reading_is_none_rather_than_zero() {
        // The distinction that matters: "I could not read it" must not
        // be drawn as a volume of nothing.
        assert_eq!(parse_wpctl_volume(""), None);
        assert_eq!(parse_wpctl_volume("Volume: quiet"), None);
        assert_eq!(parse_wpctl_volume("no such sink\n"), None);
    }

    #[test]
    fn a_volume_above_one_is_clamped_to_the_sliders_range() {
        // Both tools allow over-amplification; the slider's range is
        // 0..1 and a knob past its own end is a bug, not a feature.
        let v = parse_wpctl_volume("Volume: 1.40").expect("a volume");
        assert!((v.level - 1.0).abs() < 1e-6, "{v:?}");
    }

    #[test]
    fn the_pactl_percentage_is_taken_not_the_raw_value_or_the_decibels() {
        let line = "Volume: front-left: 42926 /  65% / -11.10 dB,   \
                    front-right: 42926 /  65% / -11.10 dB\n         balance 0.00\n";
        let level = parse_pactl_volume(line).expect("a level");
        assert!((level - 0.65).abs() < 1e-6, "{level}");
    }

    #[test]
    fn pactl_mute_takes_yes_and_no() {
        assert_eq!(parse_pactl_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_pactl_mute("Mute: no\n"), Some(false));
        assert_eq!(parse_pactl_mute("mute: NO"), Some(false));
        assert_eq!(parse_pactl_mute("Volume: 65%"), None);
        assert_eq!(parse_pactl_mute(""), None);
    }

    #[test]
    fn detection_prefers_wpctl_and_finds_nothing_in_an_empty_path() {
        let dir = std::env::temp_dir().join(format!(
            "nitro-system-audio-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let dirs = vec![dir.clone()];
        assert_eq!(Backend::detect_in(&dirs), None, "an empty directory");

        write_script(&dir.join("pactl"));
        assert_eq!(
            Backend::detect_in(&dirs).map(|b| b.name()),
            Some("pactl"),
            "the fallback alone is enough"
        );
        write_script(&dir.join("wpctl"));
        assert_eq!(
            Backend::detect_in(&dirs).map(|b| b.name()),
            Some("wpctl"),
            "and wpctl wins when both are there"
        );

        // A file that is not executable is not a backend: a stray
        // `wpctl.txt`-shaped thing must not be run.
        let plain =
            std::env::temp_dir().join(format!("nitro-system-audio-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&plain);
        std::fs::create_dir_all(&plain).expect("temp dir");
        std::fs::write(plain.join("wpctl"), "not executable").expect("write");
        assert_eq!(Backend::detect_in(std::slice::from_ref(&plain)), None);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&plain);
    }

    /// `wpctl status` as `PipeWire` 1.0 prints it, trimmed: a `Video`
    /// section with its own `Sinks:` and an `Audio` `Sources:` block
    /// that must not leak into the audio sinks.
    const WPCTL_STATUS: &str = "\
PipeWire 'pipewire-0' [1.0.5, kaspar@box, cookie:1234]
 └─ Clients:
        33. WirePlumber                         [1.0.5, kaspar@box, pid:811]

Audio
 ├─ Devices:
 │      42. Built-in Audio                      [alsa]
 │  
 ├─ Sinks:
 │      46. Built-in Audio Analog Stereo        [vol: 0.40]
 │  *   51. HDMI 1.2 Output (LG 27UL500)        [vol: 1.00 MUTED]
 │      63. USB-C Dock 2.0                      [vol: 0.75]
 │  
 ├─ Sink endpoints:
 │  
 ├─ Sources:
 │  *   47. Built-in Audio Analog Stereo        [vol: 1.00]
 │  
 ├─ Source endpoints:
 │  
 └─ Streams:
        70. Firefox

Video
 ├─ Devices:
 │      50. Integrated Camera                   [v4l2]
 │  
 ├─ Sinks:
 │      99. Not an audio sink
 │  
 └─ Streams:

Settings
 └─ Default Configured Node Names:
         0. Audio/Sink    alsa_output.pci-0000_00_1f.3.analog-stereo
";

    fn sink(id: &str, name: &str, default: bool) -> Sink {
        Sink {
            id: id.to_owned(),
            name: name.to_owned(),
            default,
        }
    }

    #[test]
    fn wpctl_status_yields_the_audio_sinks_and_nothing_else() {
        assert_eq!(
            parse_wpctl_sinks(WPCTL_STATUS),
            Some(vec![
                sink("46", "Built-in Audio Analog Stereo", false),
                sink("51", "HDMI 1.2 Output (LG 27UL500)", true),
                sink("63", "USB-C Dock 2.0", false),
            ])
        );
    }

    #[test]
    fn wpctl_status_is_read_by_shape_not_by_column() {
        // ASCII tree, different indentation, no `[vol: …]` suffix.
        let text = "Audio\n |- Sinks:\n |   * 7. Speakers\n |     8. Headset 3.5mm [vol: 0.1]\n |- Sources:\n |     9. Mic\n";
        assert_eq!(
            parse_wpctl_sinks(text),
            Some(vec![
                sink("7", "Speakers", true),
                sink("8", "Headset 3.5mm", false)
            ])
        );
        // A sinks block that is there and empty is an empty list …
        assert_eq!(
            parse_wpctl_sinks("Audio\n ├─ Sinks:\n │  \n ├─ Sources:\n"),
            Some(vec![])
        );
        // … and text with no such block is no reading at all.
        assert_eq!(parse_wpctl_sinks(""), None);
        assert_eq!(parse_wpctl_sinks("command not found\n"), None);
        assert_eq!(
            parse_wpctl_sinks("Video\n ├─ Sinks:\n │  *  3. Screen\n"),
            None
        );
    }

    /// `pactl list sinks`, trimmed to the lines that matter plus enough
    /// noise to prove the rest is skipped.
    const PACTL_LIST: &str = "\
Sink #0
\tState: SUSPENDED
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo
\tDescription: Built-in Audio Analog Stereo
\tDriver: PipeWire
\tProperties:
\t\tdevice.description = \"Built-in Audio\"

Sink #1
\tState: RUNNING
\tName: alsa_output.usb-Dock_2.0-00.analog-stereo
\tDescription: USB-C Dock 2.0
\tDriver: PipeWire
";

    #[test]
    fn pactl_sinks_are_name_description_pairs_with_the_default_marked() {
        assert_eq!(
            parse_pactl_sinks(
                PACTL_LIST,
                Some("alsa_output.usb-Dock_2.0-00.analog-stereo")
            ),
            Some(vec![
                sink(
                    "alsa_output.pci-0000_00_1f.3.analog-stereo",
                    "Built-in Audio Analog Stereo",
                    false
                ),
                sink(
                    "alsa_output.usb-Dock_2.0-00.analog-stereo",
                    "USB-C Dock 2.0",
                    true
                ),
            ])
        );
        // No default known: nothing is marked.
        let none = parse_pactl_sinks(PACTL_LIST, None).expect("sinks");
        assert!(none.iter().all(|s| !s.default));
        // Unparseable (or localised) text is `None`, so the caller falls back.
        assert_eq!(parse_pactl_sinks("Senke #0\n\tNom: x\n", None), None);
        assert_eq!(parse_pactl_sinks("", None), None);
    }

    #[test]
    fn the_short_pactl_listing_is_the_fallback() {
        let text = "0\talsa_output.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n\
                    1\tbluez_output.00_11_22.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n";
        assert_eq!(
            parse_pactl_short_sinks(text, Some("bluez_output.00_11_22.1")),
            Some(vec![
                sink(
                    "alsa_output.pci-0000_00_1f.3.analog-stereo",
                    "alsa_output.pci-0000_00_1f.3.analog-stereo",
                    false
                ),
                sink("bluez_output.00_11_22.1", "bluez_output.00_11_22.1", true),
            ])
        );
        assert_eq!(parse_pactl_short_sinks("", None), Some(vec![]));
        assert_eq!(parse_pactl_short_sinks("Connection failure\n", None), None);
    }

    #[test]
    fn a_fake_wpctl_round_trips_status_and_set_default() {
        let dir = std::env::temp_dir().join(format!(
            "nitro-system-audio-fake-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        // State in files, so a read reflects the last write; every call
        // appended to `log`.
        std::fs::write(dir.join("default"), "46\n").expect("state");
        let script = format!(
            r#"#!/bin/sh
d='{dir}'
echo "$*" >> "$d/log"
def=$(cat "$d/default")
mark() {{ [ "$1" = "$def" ] && echo '*' || echo ' '; }}
case "$1" in
  status)
    echo 'Audio'
    echo ' ├─ Sinks:'
    echo " │  $(mark 46)   46. Built-in Audio Analog Stereo [vol: 0.40]"
    echo " │  $(mark 51)   51. HDMI 1.2 Output [vol: 1.00]"
    echo ' │  '
    echo ' ├─ Sources:'
    echo ' │  *   47. Microphone [vol: 1.00]'
    ;;
  set-default)
    case "$2" in 46|51) echo "$2" > "$d/default" ;; *) echo "no such node $2" >&2; exit 1 ;; esac
    ;;
  *) exit 1 ;;
esac
"#,
            dir = dir.display()
        );
        let bin = dir.join("wpctl");
        std::fs::write(&bin, script).expect("write");
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let backend = Backend::detect_in(std::slice::from_ref(&dir)).expect("the fake");

        let sinks = backend.sinks().expect("sinks");
        assert_eq!(
            sinks,
            vec![
                sink("46", "Built-in Audio Analog Stereo", true),
                sink("51", "HDMI 1.2 Output", false),
            ]
        );
        backend.set_default_sink(&sinks[1]).expect("set-default");
        let after = backend.sinks().expect("sinks");
        assert_eq!(
            after.iter().find(|s| s.default).map(|s| s.id.as_str()),
            Some("51")
        );

        let err = backend
            .set_default_sink(&sink("9", "Gone", false))
            .expect_err("unknown id");
        assert!(err.contains("no such node 9"), "{err}");

        let log = std::fs::read_to_string(dir.join("log")).expect("log");
        assert_eq!(log, "status\nset-default 51\nstatus\nset-default 9\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write an executable no-op script at `path`.
    fn write_script(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, "#!/bin/sh\nexit 0\n").expect("write");
        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    }
}

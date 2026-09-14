//! `server.conf`, as this app reads and writes it.
//!
//! This is a **second implementation** of the format
//! `nitro_server::config` defines, and that is a deliberate duplication
//! rather than an oversight. The alternative is for a settings app to
//! depend on the compositor crate, which would link libinput, drm,
//! xkbcommon and the rasterizer into a binary whose whole job is to
//! render nine lines of text — for the benefit of forty lines of parser.
//!
//! What keeps the two copies honest is a test rather than a comment:
//! `the_server_parser_reads_back_what_we_write` in `tests/settings.rs`
//! feeds this module's output to `nitro_server::config::parse` (through
//! the dev-dependency, where the compositor may be linked) and asserts
//! that every value survives. A divergence in either direction fails
//! there, which is the only place it can be caught without shipping the
//! compositor to the user.
//!
//! # The file is rewritten wholesale
//!
//! [`render`] produces the *entire* file from a [`Conf`], and [`write`]
//! renames it over whatever was there. Comments a person typed, keys this
//! app does not know about, and the order they were written in are all
//! lost. That is a real limitation and it is documented in the crate docs
//! and the README rather than hidden: a settings app that round-tripped
//! unknown keys would need to keep the whole file's token stream, and
//! nitro's answer for now is "edit the file *or* use the app, not both".
//!
//! # Why the write is a rename
//!
//! The server watches the config file's **directory** with inotify and
//! reloads on the rename, because that is the one operation that makes a
//! new file appear atomically: a reader either sees the old file or the
//! new one, never a half-written one. Writing in place would let the
//! compositor reload a file that is three lines long, apply a scale of
//! `1` to the primary output and re-apply the real one a millisecond
//! later. See [`write`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nitro_ui::Scheme;

/// The file's name inside the configuration directory. Must agree with
/// `nitro_server::config::FILE_NAME`.
pub const FILE_NAME: &str = "server.conf";

/// The subdirectory of `$XDG_CONFIG_HOME` the file lives in. Must agree
/// with `nitro_server::config::SUBDIR`.
pub const SUBDIR: &str = "nitro";

/// The header every written file carries.
///
/// It says who wrote it, because the first question of anyone who finds
/// their comments gone is "what did that?", and it points at the
/// documentation for the format rather than restating it.
const HEADER: &str = "# nitro server configuration — written by nitro-settings.\n\
                      # Plain `key = value` lines; see docs/settings.md.\n";

/// What one output's section says.
///
/// The field order here is the order [`render`] emits, which is the order
/// a reader wants them in: how big, where, and whether it is the primary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutputConf {
    /// `output.<connector>.scale`.
    pub scale: Option<f32>,
    /// `output.<connector>.position`, as `x,y` in desktop logical space.
    pub position: Option<(i32, i32)>,
    /// `output.<connector>.primary`. Emitted only when true — a
    /// `primary = false` line says exactly what its absence says, and
    /// writing one for every non-primary output would triple the file.
    pub primary: bool,
}

/// What the `keyboard.*` keys say.
///
/// Plain `String`s rather than `Option<String>`: the app always writes
/// all three lines, so "absent" is not a state it can produce, and an
/// empty layout is the same instruction to xkbcommon as no layout. The
/// server's own type keeps the distinction because a *file* can make it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardConf {
    /// `keyboard.layout`, e.g. `us`, `de`, `us,de`.
    pub layout: String,
    /// `keyboard.variant`, e.g. `nodeadkeys`.
    pub variant: String,
    /// `keyboard.options`, e.g. `ctrl:nocaps`.
    pub options: String,
}

/// What the `theme.*` keys say, as far as this app models them.
///
/// The scheme and nothing else. Per-role overrides (`theme.accent =
/// #6ca8f0`) are a real `server.conf` feature and this app deliberately
/// does **not** offer them: a colour picker per role is thirty-odd
/// controls for a thing a user does once, and the file is the better
/// interface for it. Which makes the round-trip rule below load-bearing
/// — see [`ThemeConf::overrides`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeConf {
    /// `theme.scheme`. `None` means the file said nothing, and the
    /// server's default (light) applies.
    pub scheme: Option<Scheme>,
    /// Every `theme.<role> = <colour>` line the file carried, verbatim
    /// and in file order.
    ///
    /// Kept and written back **unchanged** although nothing in the UI
    /// can edit them. This is the one exception to "the file is
    /// rewritten wholesale", and it earns it: the alternative is that
    /// pressing Apply silently deletes a user's hand-picked accent
    /// colour, which is a destructive surprise from a button whose whole
    /// promise is "save what I typed". Unknown *keys* are still lost —
    /// that limitation stands — but a key this app understands well
    /// enough to name is not thrown away for want of a widget.
    pub overrides: Vec<(String, String)>,
}

/// A whole `server.conf`, as this app models it.
///
/// The outputs are a [`BTreeMap`] rather than a `HashMap` for one
/// reason and it is the file: the connectors are written in sorted order,
/// so a run of the app that changed nothing produces byte-for-byte the
/// file the last run did, and `diff` on a dotfile repository stays quiet.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Conf {
    /// Per-connector sections, keyed by connector name (`HDMI-A-1`).
    pub outputs: BTreeMap<String, OutputConf>,
    /// The keyboard section.
    pub keyboard: KeyboardConf,
    /// The colour section.
    pub theme: ThemeConf,
}

impl Conf {
    /// An empty configuration: no outputs, no keyboard settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One connector's section, creating it if this is the first mention.
    pub fn output_mut(&mut self, connector: &str) -> &mut OutputConf {
        self.outputs.entry(connector.to_owned()).or_default()
    }

    /// One connector's section, if the file mentions it.
    #[must_use]
    pub fn output(&self, connector: &str) -> Option<&OutputConf> {
        self.outputs.get(connector)
    }

    /// Make `connector` the primary output and no other.
    ///
    /// Exactly one output is primary, so this is a *move* rather than a
    /// set: the server resolves a file naming two by taking the
    /// alphabetically first, and a settings app that could write such a
    /// file would be offering the user a checkbox whose effect depends on
    /// connector names.
    pub fn set_primary(&mut self, connector: &str) {
        for (name, out) in &mut self.outputs {
            out.primary = name == connector;
        }
    }
}

/// Format a scale the way the file spells it.
///
/// A whole number is written without its `.0` (`2`, not `2.0`) because
/// that is what a person writes and what the documented example shows;
/// anything else keeps the digits it needs (`1.25`). Rust's own `f32`
/// formatting already does both — `{}` on `2.0f32` is `2` — so this is
/// one call, and it exists as a function so the rule has a name and a
/// test.
#[must_use]
pub fn format_scale(scale: f32) -> String {
    format!("{scale}")
}

/// Render a whole `server.conf`.
///
/// The layout is pinned by `apply_writes_exactly_the_expected_file`: two
/// header comment lines, a blank line, one block per output in connector
/// order with a blank line between blocks, a blank line, the three
/// keyboard lines, and — when there is anything to say about colours — a
/// blank line and the `theme.*` block. Within an output the order is
/// scale, position, primary — biggest effect first, and `primary` last
/// because it is the one line that may be missing.
#[must_use]
pub fn render(conf: &Conf) -> String {
    use std::fmt::Write as _;

    let mut out = String::from(HEADER);
    // `writeln!` into a `String` cannot fail, so the `Result` is dropped
    // rather than propagated: this function returns the file, and there
    // is no I/O here to go wrong.
    for (connector, o) in &conf.outputs {
        out.push('\n');
        if let Some(scale) = o.scale {
            let _ = writeln!(out, "output.{connector}.scale = {}", format_scale(scale));
        }
        if let Some((x, y)) = o.position {
            let _ = writeln!(out, "output.{connector}.position = {x},{y}");
        }
        if o.primary {
            let _ = writeln!(out, "output.{connector}.primary = true");
        }
    }
    out.push('\n');
    let k = &conf.keyboard;
    out.push_str(&keyboard_line("keyboard.layout", &k.layout));
    out.push_str(&keyboard_line("keyboard.variant", &k.variant));
    out.push_str(&keyboard_line("keyboard.options", &k.options));
    // The theme block is written only when there is something to say,
    // unlike the keyboard's three always-present lines. A `theme.scheme`
    // line the user never asked for would pin today's default into the
    // file, and the default is the one thing that should stay free to
    // change in a later release.
    let t = &conf.theme;
    if t.scheme.is_some() || !t.overrides.is_empty() {
        out.push('\n');
        if let Some(scheme) = t.scheme {
            let _ = writeln!(out, "theme.scheme = {}", scheme.name());
        }
        for (role, value) in &t.overrides {
            let _ = writeln!(out, "theme.{role} = {value}");
        }
    }
    out
}

/// One `keyboard.*` line, with no trailing space when the value is empty.
///
/// `keyboard.variant =` rather than `keyboard.variant = `. The server
/// trims and cannot tell the difference, so this is entirely about the
/// file being the file a person would have written: a trailing space is
/// invisible in an editor, shows up as whitespace noise in a `git diff`
/// of a dotfile repository, and differs from the documented example for
/// no reason a reader could see.
fn keyboard_line(key: &str, value: &str) -> String {
    if value.is_empty() {
        format!("{key} =\n")
    } else {
        format!("{key} = {value}\n")
    }
}

/// Parse a `server.conf`.
///
/// Nothing here fails, for the reason the server's parser does not: this
/// is a file a person may have edited by hand while the app was running,
/// and a settings dialog that refused to open because line 12 says
/// `scale = wide` would be the worst possible response. An unusable line
/// is skipped; the widgets then show what *was* understood, and pressing
/// Apply rewrites the file with the bad line gone.
///
/// Deliberately more permissive than the server in one direction and
/// exactly as strict in another: it accepts any finite scale (the
/// *widget* clamps to its own range) but takes the same `x,y` position
/// and the same boolean spellings, because those are the two places a
/// mismatch would show as a value that vanished on Apply.
#[must_use]
pub fn parse(text: &str) -> Conf {
    let mut conf = Conf::new();
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if let Some(rest) = key.strip_prefix("output.") {
            // The connector name is everything between the first and the
            // last dot: `DP-1.2` is what a DisplayPort MST branch
            // reports, so splitting on every dot would make a real
            // monitor unconfigurable. Same rule as the server's.
            let Some((connector, field)) = rest.rsplit_once('.') else {
                continue;
            };
            if connector.is_empty() {
                continue;
            }
            let entry = conf.output_mut(connector);
            match field {
                "scale" => {
                    if let Ok(s) = value.parse::<f32>()
                        && s.is_finite()
                        && s > 0.0
                    {
                        entry.scale = Some(s);
                    }
                }
                "position" => {
                    if let Some(p) = parse_position(value) {
                        entry.position = Some(p);
                    }
                }
                "primary" => {
                    if let Some(b) = parse_bool(value) {
                        entry.primary = b;
                    }
                }
                _ => {}
            }
            continue;
        }
        match key {
            "keyboard.layout" => value.clone_into(&mut conf.keyboard.layout),
            "keyboard.variant" => value.clone_into(&mut conf.keyboard.variant),
            "keyboard.options" => value.clone_into(&mut conf.keyboard.options),
            "theme.scheme" => conf.theme.scheme = Scheme::from_name(value),
            // Any other `theme.<something>` is a per-role override. It is
            // not validated here: this app cannot render it and does not
            // need to understand it, it only has to hand it back
            // unchanged. The server warns about a role it does not know,
            // which is the right place for that judgement.
            other => {
                if let Some(role) = other.strip_prefix("theme.")
                    && !role.is_empty()
                {
                    conf.theme
                        .overrides
                        .push((role.to_owned(), value.to_owned()));
                }
            }
        }
    }
    conf
}

/// Strip a `#` comment, counting a `#` as one only at the start of the
/// line or after whitespace — and never when it begins a colour literal.
///
/// The server's rule exactly, and for the server's reasons, both of
/// them: `ctrl:nocaps` is not the only xkb option string with
/// punctuation in it, so an inner `#` is not a comment; and
/// `theme.accent = #6ca8f0` puts a `#` exactly where a comment would
/// start, so a `#` followed by six or eight hex digits and then
/// whitespace is a value. See `nitro_server::config::strip_comment`,
/// which this has to agree with byte for byte: a value truncated here
/// but not there is a setting that silently changes meaning between the
/// app and the compositor — and, in this app's case, one that Apply
/// would then delete from the file.
fn strip_comment(line: &str) -> &str {
    let mut prev_space = true;
    for (i, b) in line.as_bytes().iter().enumerate() {
        if *b == b'#' && prev_space && !is_colour_literal(&line[i..]) {
            return &line[..i];
        }
        prev_space = b.is_ascii_whitespace();
    }
    line
}

/// Whether `rest` starts with a `#rrggbb`/`#rrggbbaa` token. The
/// server's rule; see [`strip_comment`].
fn is_colour_literal(rest: &str) -> bool {
    let token = rest.split_ascii_whitespace().next().unwrap_or("");
    let Some(hex) = token.strip_prefix('#') else {
        return false;
    };
    (hex.len() == 6 || hex.len() == 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse an `x,y` position.
fn parse_position(value: &str) -> Option<(i32, i32)> {
    let (x, y) = value.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Parse a boolean, taking the spellings a person types.
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Where `server.conf` lives, given the environment.
///
/// Taken as arguments rather than read here so the rule can be tested
/// without mutating the process's environment — which is `unsafe` and
/// races every other test in the binary. The same three-step rule the
/// server applies, because the two must agree on the file or the app
/// would configure a compositor that reads somewhere else.
#[must_use]
pub fn resolve(
    override_path: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(p) = override_path {
        return Some(p.to_path_buf());
    }
    if let Some(d) = xdg_config_home.filter(|d| d.is_absolute()) {
        return Some(d.join(SUBDIR).join(FILE_NAME));
    }
    let home = home.filter(|h| h.is_absolute())?;
    Some(home.join(".config").join(SUBDIR).join(FILE_NAME))
}

/// Where `server.conf` lives: `$NITRO_CONFIG`, else
/// `$XDG_CONFIG_HOME/nitro/server.conf`, else
/// `$HOME/.config/nitro/server.conf`.
#[must_use]
pub fn path() -> Option<PathBuf> {
    let over = std::env::var_os("NITRO_CONFIG").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve(over.as_deref(), xdg.as_deref(), home.as_deref())
}

/// Read and parse a file.
///
/// A file that is not there is not a problem — it is the state every
/// fresh installation is in — so it reads as an empty [`Conf`]. So does
/// one that cannot be read at all: the dialog opens showing the defaults
/// and Apply will say whether the write worked, which is a better answer
/// than refusing to start.
#[must_use]
pub fn load(path: &Path) -> Conf {
    std::fs::read_to_string(path).map_or_else(|_| Conf::new(), |text| parse(&text))
}

/// Write `conf` to `path`, atomically.
///
/// The sequence is the one the server's inotify watch is designed for:
/// write `server.conf.tmp-<pid>` in the **same directory**, then
/// `rename(2)` it over the target. Same directory because a rename across
/// filesystems is not atomic and would be a copy; `<pid>` in the name so
/// two settings apps cannot scribble on each other's temporary file.
///
/// The parent directory is created if missing, because a fresh
/// installation has no `~/.config/nitro` and "could not save: no such
/// directory" is not an answer a settings dialog may give.
///
/// # Errors
/// Any I/O failure creating the directory, writing the temporary file or
/// renaming it. The caller shows it; a failed write leaves the previous
/// file untouched, which is the other half of what atomic means here.
pub fn write(path: &Path, conf: &Conf) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{FILE_NAME}.tmp-{}", std::process::id()));
    // The temporary file is removed on a failed rename rather than left
    // behind: a config directory that accumulates `server.conf.tmp-*`
    // after every failure is how a user concludes the app is broken.
    if let Err(e) = std::fs::write(&tmp, render(conf)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file the spec pins, as a [`Conf`].
    fn example() -> Conf {
        let mut c = Conf::new();
        let hdmi = c.output_mut("HDMI-A-1");
        hdmi.scale = Some(2.0);
        hdmi.position = Some((0, 0));
        hdmi.primary = true;
        let vga = c.output_mut("VGA-1");
        vga.scale = Some(1.0);
        vga.position = Some((1920, 0));
        c.keyboard = KeyboardConf {
            layout: "de".to_owned(),
            variant: String::new(),
            options: "ctrl:nocaps".to_owned(),
        };
        c
    }

    /// The exact bytes the example renders to.
    const EXPECTED: &str = "\
# nitro server configuration — written by nitro-settings.
# Plain `key = value` lines; see docs/settings.md.

output.HDMI-A-1.scale = 2
output.HDMI-A-1.position = 0,0
output.HDMI-A-1.primary = true

output.VGA-1.scale = 1
output.VGA-1.position = 1920,0

keyboard.layout = de
keyboard.variant =
keyboard.options = ctrl:nocaps
";

    #[test]
    fn render_writes_exactly_the_documented_file() {
        assert_eq!(render(&example()), EXPECTED);
    }

    #[test]
    fn a_whole_scale_has_no_trailing_zero_and_a_fraction_keeps_its_digits() {
        assert_eq!(format_scale(2.0), "2");
        assert_eq!(format_scale(1.0), "1");
        assert_eq!(format_scale(1.25), "1.25");
        assert_eq!(format_scale(1.5), "1.5");
    }

    #[test]
    fn rendering_round_trips_through_our_own_parser() {
        assert_eq!(parse(&render(&example())), example());
    }

    #[test]
    fn outputs_are_written_in_connector_order() {
        // Inserted out of order; written sorted, so a run that changed
        // nothing writes the file the last run wrote.
        let mut c = Conf::new();
        c.output_mut("VGA-1").scale = Some(1.0);
        c.output_mut("HDMI-A-1").scale = Some(2.0);
        c.output_mut("DP-1").scale = Some(1.0);
        let text = render(&c);
        let order: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("output."))
            .map(|l| l.split('.').nth(1).unwrap())
            .collect();
        assert_eq!(order, ["DP-1", "HDMI-A-1", "VGA-1"]);
    }

    #[test]
    fn primary_is_written_only_when_true() {
        let mut c = Conf::new();
        c.output_mut("X").primary = false;
        assert!(!render(&c).contains("primary"));
        c.output_mut("X").primary = true;
        assert!(render(&c).contains("output.X.primary = true"));
    }

    #[test]
    fn setting_primary_clears_the_previous_one() {
        let mut c = example();
        c.set_primary("VGA-1");
        assert!(!c.output("HDMI-A-1").unwrap().primary);
        assert!(c.output("VGA-1").unwrap().primary);
    }

    #[test]
    fn the_three_keyboard_lines_are_always_written() {
        let text = render(&Conf::new());
        assert!(text.contains("keyboard.layout =\n"));
        assert!(text.contains("keyboard.variant =\n"));
        assert!(text.contains("keyboard.options =\n"));
    }

    #[test]
    fn a_connector_name_may_contain_a_dot() {
        let c = parse("output.DP-1.2.scale = 2\n");
        assert_eq!(c.output("DP-1.2").expect("the branch").scale, Some(2.0));
    }

    #[test]
    fn a_trailing_comment_is_stripped_but_a_hash_in_a_value_is_not() {
        let c = parse("output.X.scale = 2  # HiDPI panel\nkeyboard.options = a#b\n");
        assert_eq!(c.output("X").expect("X").scale, Some(2.0));
        assert_eq!(c.keyboard.options, "a#b");
    }

    #[test]
    fn garbage_is_skipped_rather_than_fatal() {
        for text in [
            "",
            "=",
            "output",
            "output.",
            "output..scale = 1",
            "output.X.scale = wide",
            "output.X.scale = NaN",
            "output.X.scale = -1",
            "output.X.position = a,b",
            "output.X.primary = maybe",
            "output.X.rotation = 90",
            "keyboard.repeat = 300,25",
            "nonsense",
            "\0\0\0",
        ] {
            let c = parse(text);
            assert!(
                c.outputs
                    .values()
                    .all(|o| o.scale.is_none() || text.contains("scale = 1")),
                "{text:?} produced {c:?}"
            );
        }
        assert_eq!(
            parse("output.X.scale = 1.5").output("X").unwrap().scale,
            Some(1.5)
        );
    }

    #[test]
    fn bools_take_the_spellings_people_type() {
        for text in ["true", "yes", "on", "1", "TRUE"] {
            let c = parse(&format!("output.X.primary = {text}\n"));
            assert!(c.output("X").expect("X").primary, "{text}");
        }
        for text in ["false", "no", "off", "0"] {
            let c = parse(&format!("output.X.primary = {text}\n"));
            assert!(!c.output("X").expect("X").primary, "{text}");
        }
    }

    #[test]
    fn a_negative_position_is_kept_because_a_screen_may_be_to_the_left() {
        let c = parse("output.X.position = -1920,-100\n");
        assert_eq!(c.output("X").expect("X").position, Some((-1920, -100)));
    }

    #[test]
    fn resolve_prefers_the_override_then_xdg_then_home() {
        let over = Path::new("/tmp/explicit.conf");
        let xdg = Path::new("/xdg");
        let home = Path::new("/home/u");
        assert_eq!(
            resolve(Some(over), Some(xdg), Some(home)).as_deref(),
            Some(over)
        );
        assert_eq!(
            resolve(None, Some(xdg), Some(home)),
            Some(PathBuf::from("/xdg/nitro/server.conf"))
        );
        assert_eq!(
            resolve(None, None, Some(home)),
            Some(PathBuf::from("/home/u/.config/nitro/server.conf"))
        );
        // A relative `XDG_CONFIG_HOME` is ignored rather than resolved
        // against a working directory the app does not control.
        assert_eq!(
            resolve(None, Some(Path::new("relative")), Some(home)),
            Some(PathBuf::from("/home/u/.config/nitro/server.conf"))
        );
        assert_eq!(resolve(None, None, None), None);
    }

    #[test]
    fn writing_creates_the_directory_and_leaves_no_temporary_file() {
        let dir = std::env::temp_dir().join(format!(
            "nitro-settings-conf-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // Two levels deep: a fresh installation has neither.
        let path = dir.join("nitro").join(FILE_NAME);
        write(&path, &example()).expect("write");
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), EXPECTED);
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .expect("read dir")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.contains("tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_file_loads_as_an_empty_configuration() {
        let missing = std::env::temp_dir().join("nitro-settings-no-such-file.conf");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(load(&missing), Conf::new());
    }

    #[test]
    fn the_theme_section_round_trips_including_the_overrides() {
        // The round trip that stops Apply from being destructive: this
        // app has no widget for `theme.accent`, so the only way the key
        // survives a rewrite is by being carried verbatim.
        let text = "theme.scheme = dark\n\
                    theme.accent = #6ca8f0\n\
                    theme.terminal_background = #14141880\n\
                    keyboard.layout = de\n";
        let c = parse(text);
        assert_eq!(c.theme.scheme, Some(Scheme::Dark));
        assert_eq!(
            c.theme.overrides,
            vec![
                ("accent".to_owned(), "#6ca8f0".to_owned()),
                ("terminal_background".to_owned(), "#14141880".to_owned()),
            ]
        );
        let back = parse(&render(&c));
        assert_eq!(back.theme, c.theme);
        assert_eq!(back.keyboard.layout, "de");
    }

    #[test]
    fn a_colour_literal_is_not_a_comment() {
        // The `#` in `#6ca8f0` sits exactly where a comment starts. The
        // server makes the same exception; if these two ever disagree,
        // Apply silently deletes the user's colour.
        let c = parse("theme.accent = #6ca8f0  # the blue one\n");
        assert_eq!(
            c.theme.overrides,
            vec![("accent".to_owned(), "#6ca8f0".to_owned())]
        );
        // And a comment that is not hex is still a comment.
        assert!(
            parse("# theme.accent = #ff0000\n")
                .theme
                .overrides
                .is_empty()
        );
    }

    #[test]
    fn no_theme_section_writes_no_theme_lines() {
        // A `theme.scheme` nobody asked for would pin today's default
        // into the file, and the default is the one thing that should
        // stay free to change.
        let text = render(&example());
        assert!(!text.contains("theme."), "{text}");
        assert_eq!(parse(&text).theme, ThemeConf::default());
    }
}

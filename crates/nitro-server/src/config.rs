//! `server.conf`: the persistent half of the display configuration.
//!
//! Everything the compositor knows about a *particular machine* — which
//! output is the primary one, how large its pixels are, where the screens
//! sit relative to each other, which keyboard layout the user types — is
//! read from one plain text file:
//!
//! ```text
//! $XDG_CONFIG_HOME/nitro/server.conf     (default ~/.config/nitro/server.conf)
//! ```
//!
//! ```text
//! # nitro server configuration
//! output.HDMI-A-1.scale    = 2
//! output.HDMI-A-1.position = 0,0
//! output.HDMI-A-1.primary  = true
//! output.VGA-1.position    = 960,0
//!
//! keyboard.layout  = de
//! keyboard.variant =
//! keyboard.options = ctrl:nocaps
//!
//! theme.scheme = dark
//! theme.accent = #6ca8f0
//!
//! remote.listen    = 127.0.0.1:7700
//! ```
//!
//! # Why `key = value` and not TOML
//!
//! The whole grammar is "one assignment per line, `#` starts a comment",
//! which is forty lines of parser and no dependency. A configuration file
//! that needs a parser crate to read a flat list of scalars has bought
//! nothing: there are no tables, no arrays and no types beyond a float, a
//! pair of integers and a string. The key *is* the path
//! (`output.<connector>.scale`), which is what a table would have spelled
//! anyway, and it survives being edited by `sed`, by a settings app, and
//! by a person with a broken desktop and a text console.
//!
//! # Nothing here fails
//!
//! A configuration file is user input that arrives *while the compositor is
//! running* (see the reload path in `lib.rs`), so there is no useful sense
//! in which parsing it can fail: a bad line cannot take the desktop down.
//! Every error is a [`Settings::warnings`] entry and the line is skipped —
//! unknown key, unparseable value, missing `=`, a scale of `-3`. The caller
//! logs the warnings and applies the rest. `garbage_never_panics` in the
//! tests below feeds it bytes nobody would write.
//!
//! # Precedence
//!
//! Environment beats file beats EDID/default, and the server applies it in
//! exactly that order:
//!
//! | setting | wins | then | then |
//! |---|---|---|---|
//! | output scale | `NITRO_SCALE` | `output.<c>.scale` | EDID dpi step |
//! | output position | — | `output.<c>.position` | left-to-right in connector order |
//! | primary output | — | `output.<c>.primary` | the first connector |
//! | keyboard | `XKB_DEFAULT_*` | `keyboard.*` | the `us` layout |
//! | colour scheme | — | `theme.scheme` | `light` |
//! | one colour | — | `theme.<role>` | the scheme's value |
//! | remote listener | — | `remote.listen` | off |
//!
//! The environment wins because it is the *development* channel — a
//! `NITRO_SCALE=HDMI-A-1=2 just fake` must not be silently overridden by
//! whatever the box's own config says. The file wins over the EDID because
//! it is the user's explicit answer to the EDID's guess.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nitro_core::{Palette, Role, Scheme, palette};

/// The file's name inside the configuration directory.
pub const FILE_NAME: &str = "server.conf";

/// The subdirectory of `$XDG_CONFIG_HOME` the file lives in.
pub const SUBDIR: &str = "nitro";

/// The largest scale a file may ask for. A typo (`scale = 20`) would
/// otherwise make the desktop 20× and leave no way to click anything.
const MAX_SCALE: f32 = 8.0;

/// The smallest scale a file may ask for; below this the decorations are
/// sub-pixel and a window cannot be grabbed.
const MIN_SCALE: f32 = 0.5;

/// What one connector's section says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutputSettings {
    /// `output.<connector>.scale`: logical-to-device factor, overriding the
    /// EDID-derived default. `NITRO_SCALE` still wins over it.
    pub scale: Option<f32>,
    /// `output.<connector>.position`: this output's top-left corner in the
    /// **desktop** (logical) coordinate space. Absent means "place it after
    /// the last positioned output", which is the connector-order row the
    /// server laid out before this file existed.
    pub position: Option<(i32, i32)>,
    /// `output.<connector>.primary`: the output orphaned windows migrate
    /// to, and the one a client gets when it has no say. At most one
    /// output wins; a file naming two gets the first in file order.
    pub primary: bool,
}

impl OutputSettings {
    /// Whether this section says anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scale.is_none() && self.position.is_none() && !self.primary
    }
}

/// What the `keyboard.*` keys say.
///
/// Every field is `Option`, and `Some("")` is **not** `None`: an explicit
/// `keyboard.variant =` means "no variant", which is a different
/// instruction from "say nothing about the variant" (where the environment
/// or xkbcommon's own default decides).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardSettings {
    /// `keyboard.layout`, e.g. `us`, `de`, `us,de`.
    pub layout: Option<String>,
    /// `keyboard.variant`, e.g. `nodeadkeys`.
    pub variant: Option<String>,
    /// `keyboard.options`, e.g. `ctrl:nocaps`.
    pub options: Option<String>,
}

impl KeyboardSettings {
    /// Whether the file says anything about the keyboard.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layout.is_none() && self.variant.is_none() && self.options.is_none()
    }
}

/// What the `theme.*` keys say.
///
/// A scheme plus a sparse list of per-role overrides, which is the whole
/// shape of the "central switch" this file is for: `theme.scheme` moves
/// every colour on the desktop at once, and a `theme.<role>` line moves
/// exactly one and leaves the rest following the scheme.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeSettings {
    /// `theme.scheme`: `light` or `dark`. `None` means the file said
    /// nothing, and the built-in default ([`Scheme::Light`]) applies.
    pub scheme: Option<Scheme>,
    /// `theme.<role>`: per-role `#rrggbb[aa]` overrides, applied on top
    /// of the scheme in role order rather than file order, so two lines
    /// for the same role resolve the same way a repeated `scale` does
    /// (the last one wins, because it overwrote the entry).
    pub overrides: Vec<(Role, nitro_core::Color)>,
}

impl ThemeSettings {
    /// Whether the file says anything about colours.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scheme.is_none() && self.overrides.is_empty()
    }

    /// The palette this section describes: the scheme, then the
    /// overrides on top.
    #[must_use]
    pub fn palette(&self) -> Palette {
        let mut p = self.scheme.unwrap_or_default().palette();
        for (role, color) in &self.overrides {
            p.set(*role, *color);
        }
        p
    }

    /// Record an override, replacing any earlier one for the same role.
    fn set(&mut self, role: Role, color: nitro_core::Color) {
        match self.overrides.iter_mut().find(|(r, _)| *r == role) {
            Some(slot) => slot.1 = color,
            None => self.overrides.push((role, color)),
        }
    }
}

/// What the `remote.*` keys say.
///
/// One key so far. It is a section rather than a bare `remote_listen`
/// because the next thing a remote link will want (an allow list, a
/// connection limit) belongs next to it, and a flat key would have to be
/// renamed to get there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteSettings {
    /// `remote.listen = <addr>:<port>`, e.g. `127.0.0.1:7700`,
    /// `[::1]:7700`, `0.0.0.0:7700`. Absent means **no TCP listener at
    /// all**, which is the default and costs a server that does not want
    /// remote clients exactly nothing.
    ///
    /// A literal address, never a name: a listener resolved through DNS
    /// is a foot-gun, because the name may move and the address the
    /// server bound is then not the one the file names.
    pub listen: Option<std::net::SocketAddr>,
}

impl RemoteSettings {
    /// Whether the file says anything about remote clients.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.listen.is_none()
    }
}

/// A parsed `server.conf`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// Per-connector sections, by connector name (`HDMI-A-1`).
    pub outputs: HashMap<String, OutputSettings>,
    /// The keyboard section.
    pub keyboard: KeyboardSettings,
    /// The colour section.
    pub theme: ThemeSettings,
    /// The remote section.
    pub remote: RemoteSettings,
    /// Every line that was skipped, and why. The caller logs these; they
    /// are not errors, because a configuration file cannot be allowed to
    /// stop a running compositor.
    pub warnings: Vec<String>,
}

impl Settings {
    /// One connector's section, if the file mentions it.
    #[must_use]
    pub fn output(&self, connector: &str) -> Option<&OutputSettings> {
        self.outputs.get(connector)
    }

    /// The connector marked `primary = true`, if any.
    ///
    /// A file that marks two is answered with the alphabetically first, so
    /// that the answer does not depend on `HashMap` iteration order — which
    /// would make the desktop's primary output change between runs of the
    /// same file.
    #[must_use]
    pub fn primary(&self) -> Option<&str> {
        let mut names: Vec<&str> = self
            .outputs
            .iter()
            .filter(|(_, o)| o.primary)
            .map(|(name, _)| name.as_str())
            .collect();
        names.sort_unstable();
        names.first().copied()
    }

    /// Whether the file configured nothing at all (missing, empty, or
    /// entirely comments).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keyboard.is_empty()
            && self.theme.is_empty()
            && self.remote.is_empty()
            && self.outputs.values().all(OutputSettings::is_empty)
    }

    /// The palette the file asks for: the scheme it names (or the
    /// built-in default) with its per-role overrides applied.
    #[must_use]
    pub fn palette(&self) -> Palette {
        self.theme.palette()
    }
}

/// Strip a `#` comment from a line.
///
/// A `#` counts as a comment only at the start of the line or after
/// whitespace, so a value may contain one (`keyboard.options = foo#bar`)
/// without being truncated — the rule every `.conf` in `/etc` uses, and the
/// one that does not surprise someone pasting an xkb option string.
///
/// # The colour exception
///
/// `theme.accent = #6ca8f0` puts a `#` exactly where the rule above says
/// a comment starts, and it is the spelling every person and every other
/// program writes a colour in. So a `#` followed by **six or eight hex
/// digits and then nothing but whitespace** is a value, not a comment.
/// The alternatives were worse: demanding `accent = 6ca8f0` breaks
/// copy-paste from anywhere colours are written, and recognising `#` only
/// at the start of a line would silently swallow every trailing comment
/// this file already documents.
///
/// A trailing comment still works after a colour (`= #6ca8f0  # blue`),
/// because `# blue` is not hex digits. The one thing it costs is a
/// comment whose entire text is six or eight hex characters —
/// `scale = 2 #beefed` keeps the `#beefed`, which then fails to parse as
/// a scale and is warned about. That is the price, and it is written
/// down here because it is the kind of thing that is otherwise found
/// once, by someone, at the worst moment.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut prev_space = true;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' && prev_space && !is_colour_literal(&line[i..]) {
            return &line[..i];
        }
        prev_space = b.is_ascii_whitespace();
    }
    line
}

/// Whether `rest` starts with a `#rrggbb`/`#rrggbbaa` **token** — a `#`,
/// six or eight hex digits, and then whitespace or the end of the line.
/// See [`strip_comment`].
fn is_colour_literal(rest: &str) -> bool {
    let token = rest.split_ascii_whitespace().next().unwrap_or("");
    let Some(hex) = token.strip_prefix('#') else {
        return false;
    };
    (hex.len() == 6 || hex.len() == 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse a `true`/`false` value, tolerating the spellings a person
/// actually types.
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Parse an `x,y` position.
fn parse_position(value: &str) -> Option<(i32, i32)> {
    let (x, y) = value.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Parse the whole file.
///
/// Never fails: every unusable line lands in [`Settings::warnings`] and is
/// skipped. Later assignments to the same key win, so a file that sets
/// `keyboard.layout` twice ends up with the second — the rule that makes
/// appending a line a working way to override one.
#[must_use]
#[allow(clippy::too_many_lines)] // One `match` over the key space: splitting it would hide the table.
pub fn parse(text: &str) -> Settings {
    let mut settings = Settings::default();
    for (n, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let number = n + 1;
        let Some((key, value)) = line.split_once('=') else {
            settings
                .warnings
                .push(format!("line {number}: {line:?} is not `key = value`"));
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            settings.warnings.push(format!("line {number}: empty key"));
            continue;
        }
        // `output.<connector>.<field>`: the connector name is whatever is
        // between the first and last dot, because a connector name may
        // itself contain one — `DP-1.2` is what a DisplayPort MST branch
        // reports, and splitting on every dot would make it unconfigurable.
        if let Some(rest) = key.strip_prefix("output.") {
            let Some((connector, field)) = rest.rsplit_once('.') else {
                settings.warnings.push(format!(
                    "line {number}: `{key}` wants `output.<connector>.<scale|position|primary>`"
                ));
                continue;
            };
            if connector.is_empty() {
                settings
                    .warnings
                    .push(format!("line {number}: `{key}` names no connector"));
                continue;
            }
            let entry = settings.outputs.entry(connector.to_owned()).or_default();
            match field {
                "scale" => match value.parse::<f32>() {
                    Ok(s) if s.is_finite() && (MIN_SCALE..=MAX_SCALE).contains(&s) => {
                        entry.scale = Some(s);
                    }
                    _ => settings.warnings.push(format!(
                        "line {number}: scale {value:?} is not a number in {MIN_SCALE}..={MAX_SCALE}"
                    )),
                },
                "position" => match parse_position(value) {
                    Some(p) => entry.position = Some(p),
                    None => settings
                        .warnings
                        .push(format!("line {number}: position {value:?} is not `x,y`")),
                },
                "primary" => match parse_bool(value) {
                    Some(b) => entry.primary = b,
                    None => settings
                        .warnings
                        .push(format!("line {number}: primary {value:?} is not a boolean")),
                },
                other => settings.warnings.push(format!(
                    "line {number}: unknown output key `{other}` (want scale, position or primary)"
                )),
            }
            continue;
        }
        // `theme.<role>` and `theme.scheme`. The role names are the
        // `Role::key()` table in `nitro-core`, so there is exactly one
        // list of them and a role added there is configurable the same
        // day.
        if let Some(field) = key.strip_prefix("theme.") {
            if field == "scheme" {
                match Scheme::from_name(value) {
                    Some(s) => settings.theme.scheme = Some(s),
                    None => settings.warnings.push(format!(
                        "line {number}: scheme {value:?} is not `light` or `dark`"
                    )),
                }
                continue;
            }
            let Some(role) = Role::from_key(field) else {
                settings.warnings.push(format!(
                    "line {number}: unknown colour role `{field}` (see docs/theme.md)"
                ));
                continue;
            };
            match palette::parse_color(value) {
                Some(c) => settings.theme.set(role, c),
                None => settings.warnings.push(format!(
                    "line {number}: colour {value:?} is not `#rrggbb` or `#rrggbbaa`"
                )),
            }
            continue;
        }
        match key {
            "keyboard.layout" => settings.keyboard.layout = Some(value.to_owned()),
            "keyboard.variant" => settings.keyboard.variant = Some(value.to_owned()),
            "keyboard.options" => settings.keyboard.options = Some(value.to_owned()),
            // `remote.listen =` with nothing after it is "no listener",
            // not a parse failure: it is how a settings app turns the
            // feature off without deleting the line, and the same shape
            // `keyboard.variant =` already has.
            "remote.listen" => {
                if value.is_empty() {
                    settings.remote.listen = None;
                } else {
                    match nitro_wire::endpoint::parse_listen(value) {
                        Ok(addr) => settings.remote.listen = Some(addr),
                        Err(e) => settings
                            .warnings
                            .push(format!("line {number}: remote.listen {value:?}: {e}")),
                    }
                }
            }
            // Named explicitly rather than falling into "unknown key",
            // because it is the one key a reader expects to find and it is
            // deliberately absent: nothing in this stack repeats keys.
            // libinput reports a press and a release, the server forwards
            // them, and no client synthesises repeats — so a
            // `keyboard.repeat` would be a promise with nothing behind it.
            // See `docs/settings.md`.
            "keyboard.repeat" => settings.warnings.push(format!(
                "line {number}: `keyboard.repeat` is not implemented — nothing in nitro repeats keys yet (docs/settings.md)"
            )),
            other => settings
                .warnings
                .push(format!("line {number}: unknown key `{other}`")),
        }
    }
    settings
}

/// Where `server.conf` lives, given the environment.
///
/// Taken as arguments rather than read here so the rule can be tested
/// without mutating the process's environment — which is `unsafe` and races
/// every other test in the binary. `None` when there is neither an
/// `$XDG_CONFIG_HOME` nor a `$HOME` to hang it off, which is the state a
/// system service with an empty environment is in: no config, no watch, and
/// the compositor runs on its defaults.
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
/// `$XDG_CONFIG_HOME/nitro/server.conf`, else `$HOME/.config/nitro/server.conf`.
#[must_use]
pub fn path() -> Option<PathBuf> {
    let over = std::env::var_os("NITRO_CONFIG").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve(over.as_deref(), xdg.as_deref(), home.as_deref())
}

/// Read and parse a file.
///
/// A file that is not there is not a problem — it is the state every fresh
/// installation is in — so it parses as empty settings with no warning. A
/// file that exists and cannot be read *is* worth a warning: it is a
/// permissions or I/O fault the user asked for and did not get.
#[must_use]
pub fn load(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => Settings {
            warnings: vec![format!("{}: {e}", path.display())],
            ..Settings::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_literal_survives_the_comment_rule() {
        // `#rrggbb` is where a `#` is a value, not a comment. See
        // `strip_comment`.
        let s = parse("theme.accent = #6ca8f0  # the dark scheme's blue\n");
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(
            s.palette().get(Role::Accent),
            nitro_core::Color::rgb(0x6c, 0xa8, 0xf0)
        );
        // Eight digits too, and with no trailing comment.
        let s = parse("theme.modal_background = #08090cb0\n");
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(
            s.palette().get(Role::ModalBackground),
            nitro_core::Color::rgba(0x08, 0x09, 0x0c, 0xb0)
        );
        // A comment that is *not* hex is still a comment, and a
        // whole-line comment still is one whatever it contains.
        assert!(parse("# theme.accent = #ff0000\n").is_empty());
        let s = parse("theme.accent = #ff0000 # was #00ff00 last week\n");
        assert_eq!(
            s.palette().get(Role::Accent),
            nitro_core::Color::rgb(0xff, 0, 0)
        );
    }

    #[test]
    fn the_theme_section_parses() {
        let s = parse(
            "theme.scheme = dark\n\
             theme.accent = #6ca8f0\n\
             theme.modal_background = #0809 0c\n\
             theme.ansi1 = cc333c\n",
        );
        assert_eq!(s.theme.scheme, Some(Scheme::Dark));
        // The bad line is a warning, and only that line is lost.
        assert_eq!(s.warnings.len(), 1, "{:?}", s.warnings);
        let p = s.palette();
        assert_eq!(
            p.get(Role::Accent),
            nitro_core::Color::rgb(0x6c, 0xa8, 0xf0)
        );
        assert_eq!(p.get(Role::Ansi1), nitro_core::Color::rgb(0xcc, 0x33, 0x3c));
        // Everything not overridden follows the scheme.
        assert_eq!(p.get(Role::Text), Palette::dark().get(Role::Text));
        assert_eq!(
            p.get(Role::ModalBackground),
            Palette::dark().get(Role::ModalBackground)
        );
        assert!(!s.is_empty());
    }

    #[test]
    fn no_theme_section_is_the_light_scheme() {
        // The documented default, and the one every screenshot in
        // `docs/` was taken on.
        let s = parse("keyboard.layout = de\n");
        assert_eq!(s.theme.scheme, None);
        assert_eq!(s.palette(), Palette::light());
        assert!(s.theme.is_empty());
        assert_eq!(Settings::default().palette(), Palette::light());
    }

    #[test]
    fn an_override_survives_a_scheme_change_and_the_last_one_wins() {
        // Order-independence: the override applies on top of whatever
        // scheme the file names, wherever the two lines sit.
        let s = parse("theme.accent = #ff0000\ntheme.scheme = dark\n");
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(
            s.palette().get(Role::Accent),
            nitro_core::Color::rgb(255, 0, 0)
        );
        assert_eq!(s.palette().get(Role::Text), Palette::dark().get(Role::Text));
        let s = parse("theme.accent = #ff0000\ntheme.accent = #00ff00\n");
        assert_eq!(s.theme.overrides.len(), 1);
        assert_eq!(
            s.palette().get(Role::Accent),
            nitro_core::Color::rgb(0, 255, 0)
        );
    }

    #[test]
    fn a_malformed_colour_or_scheme_is_warned_and_ignored() {
        for (text, role) in [
            ("theme.accent = blue\n", Role::Accent),
            ("theme.accent = #ff88\n", Role::Accent),
            ("theme.accent =\n", Role::Accent),
            ("theme.ansi1 = ##cc333c\n", Role::Ansi1),
        ] {
            let s = parse(text);
            assert_eq!(s.warnings.len(), 1, "{text:?}");
            assert!(s.theme.overrides.is_empty(), "{text:?}");
            assert_eq!(s.palette().get(role), Palette::light().get(role));
        }
        let s = parse("theme.scheme = solarized\n");
        assert_eq!(s.warnings.len(), 1);
        assert_eq!(s.theme.scheme, None);
        // A key that names no role at all says so, rather than reading as
        // a generic "unknown key".
        let s = parse("theme.chartreuse = #ff0000\n");
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("colour role"), "{:?}", s.warnings);
        assert!(s.is_empty());
    }

    #[test]
    fn every_role_is_configurable_by_its_key() {
        // The table in `nitro-core` and the parser cannot drift: a role
        // added there is settable here the same day, or this fails.
        for role in Role::ALL {
            let s = parse(&format!("theme.{} = #010203\n", role.key()));
            assert!(s.warnings.is_empty(), "{}: {:?}", role.key(), s.warnings);
            assert_eq!(
                s.palette().get(*role),
                nitro_core::Color::rgb(1, 2, 3),
                "{}",
                role.key()
            );
        }
    }

    #[test]
    fn the_documented_example_parses() {
        let s = parse(
            "# nitro server configuration\n\
             output.HDMI-A-1.scale    = 2\n\
             output.HDMI-A-1.position = 0,0\n\
             output.HDMI-A-1.primary  = true\n\
             output.VGA-1.position    = 960,0\n\
             \n\
             keyboard.layout  = de\n\
             keyboard.variant =\n\
             keyboard.options = ctrl:nocaps\n",
        );
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        let hdmi = s.output("HDMI-A-1").expect("the HDMI section");
        assert_eq!(hdmi.scale, Some(2.0));
        assert_eq!(hdmi.position, Some((0, 0)));
        assert!(hdmi.primary);
        assert_eq!(s.output("VGA-1").expect("VGA").position, Some((960, 0)));
        assert!(!s.output("VGA-1").expect("VGA").primary);
        assert_eq!(s.primary(), Some("HDMI-A-1"));
        assert_eq!(s.keyboard.layout.as_deref(), Some("de"));
        // The distinction the whole `Option<String>` shape exists for.
        assert_eq!(s.keyboard.variant.as_deref(), Some(""));
        assert_eq!(s.keyboard.options.as_deref(), Some("ctrl:nocaps"));
        assert!(!s.is_empty());
    }

    #[test]
    fn an_absent_file_is_empty_settings_and_no_warning() {
        let missing = std::env::temp_dir().join("nitro-no-such-config-file.conf");
        let _ = std::fs::remove_file(&missing);
        let s = load(&missing);
        assert!(s.is_empty());
        assert!(s.warnings.is_empty());
        assert_eq!(s.primary(), None);
    }

    #[test]
    fn comments_and_blank_lines_say_nothing() {
        let s = parse("\n   \n# everything\n#output.X.scale = 4\n");
        assert!(s.is_empty());
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn a_trailing_comment_is_stripped_but_a_hash_in_a_value_is_not() {
        let s = parse("output.X.scale = 2  # HiDPI panel\nkeyboard.options = a#b\n");
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.output("X").expect("X").scale, Some(2.0));
        assert_eq!(s.keyboard.options.as_deref(), Some("a#b"));
    }

    #[test]
    fn garbage_never_panics() {
        for text in [
            "",
            "=",
            "= 3",
            "output",
            "output.",
            "output.=1",
            "output..scale = 1",
            "output.X = 1",
            "output.X.scale",
            "output.X.scale =",
            "output.X.scale = wide",
            "output.X.scale = NaN",
            "output.X.scale = inf",
            "output.X.scale = -1",
            "output.X.scale = 0",
            "output.X.scale = 99",
            "output.X.position = 1",
            "output.X.position = a,b",
            "output.X.position = 1,2,3",
            "output.X.primary = maybe",
            "output.X.rotation = 90",
            "keyboard = de",
            "keyboard.repeat = 300,25",
            "remote.listen",
            "remote.listen = ",
            "remote.listen = :",
            "remote.listen = 1.2.3.4.5:1",
            "remote = 127.0.0.1:7700",
            "nonsense",
            "\0\0\0",
            "key = \u{1f4a9}",
            "output.X.scale = 1 = 2",
            "theme",
            "theme.",
            "theme.=1",
            "theme. = #ff0000",
            "theme.scheme",
            "theme.scheme =",
            "theme.accent = #",
            "theme.accent = \u{1f4a9}\u{1f4a9}\u{1f4a9}",
        ] {
            let s = parse(text);
            // Whatever it decided, it decided without panicking; and a line
            // it could not use is a warning rather than silence.
            let _ = s.primary();
            let _ = s.is_empty();
        }
        assert_eq!(parse("output.X.scale = 99").warnings.len(), 1);
        assert_eq!(parse("nonsense").warnings.len(), 1);
        // `1 = 2` splits at the first `=`, so the value is `1 = 2`, which
        // is not a number: one warning, no panic, no half-applied scale.
        assert!(
            parse("output.X.scale = 1 = 2")
                .output("X")
                .is_none_or(|o| o.scale.is_none())
        );
    }

    #[test]
    fn remote_listen_takes_a_literal_and_nothing_else() {
        let s = parse("remote.listen = 127.0.0.1:7700\n");
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(
            s.remote.listen,
            Some("127.0.0.1:7700".parse().expect("literal"))
        );
        assert!(!s.is_empty());
        // v6 and the wildcard, which is what a LAN measurement run uses.
        assert_eq!(
            parse("remote.listen = [::1]:7700\n").remote.listen,
            Some("[::1]:7700".parse().expect("literal"))
        );
        assert_eq!(
            parse("remote.listen = 0.0.0.0:7700\n").remote.listen,
            Some("0.0.0.0:7700".parse().expect("literal"))
        );
        // Port 0 is legal: the kernel picks one, which is how the tests
        // run without choosing a number.
        assert_eq!(
            parse("remote.listen = 127.0.0.1:0\n").remote.listen,
            Some("127.0.0.1:0".parse().expect("literal"))
        );
        // An empty value is "off", not a warning: that is how a settings
        // app disables the listener without deleting the line.
        let off = parse("remote.listen =\n");
        assert!(off.warnings.is_empty(), "{:?}", off.warnings);
        assert_eq!(off.remote.listen, None);
        assert!(off.is_empty());
    }

    #[test]
    fn a_bad_remote_listen_is_a_warning_and_no_listener() {
        for bad in [
            "7700",
            "localhost:7700",
            "nitro.example.com:7700",
            "127.0.0.1",
            "127.0.0.1:",
            "127.0.0.1:nope",
            "127.0.0.1:99999",
            "::1:7700",
            "tcp://127.0.0.1:7700",
        ] {
            let s = parse(&format!("remote.listen = {bad}\n"));
            assert_eq!(s.remote.listen, None, "{bad}");
            assert_eq!(s.warnings.len(), 1, "{bad}: {:?}", s.warnings);
            assert!(s.warnings[0].contains("remote.listen"), "{bad}");
        }
    }

    #[test]
    fn an_unknown_key_is_warned_and_ignored() {
        let s = parse("output.X.scale = 2\nmonitor.X.gamma = 1.1\nkeyboard.model = pc105\n");
        assert_eq!(s.output("X").expect("X").scale, Some(2.0));
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);
        assert!(s.warnings.iter().any(|w| w.contains("monitor.X.gamma")));
        assert!(s.warnings.iter().any(|w| w.contains("keyboard.model")));
    }

    #[test]
    fn keyboard_repeat_is_refused_by_name() {
        // Not "unknown": it is the key a reader most expects, and the
        // warning has to say *why* it does nothing rather than imply a typo.
        let s = parse("keyboard.repeat = 300,25\n");
        assert_eq!(s.warnings.len(), 1);
        assert!(
            s.warnings[0].contains("not implemented"),
            "{:?}",
            s.warnings
        );
        assert!(s.is_empty());
    }

    #[test]
    fn a_connector_name_may_contain_a_dot() {
        // `DP-1.2` is what a DisplayPort MST branch reports.
        let s = parse("output.DP-1.2.scale = 2\n");
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.output("DP-1.2").expect("the branch").scale, Some(2.0));
    }

    #[test]
    fn the_last_assignment_wins() {
        let s = parse(
            "keyboard.layout = us\nkeyboard.layout = de\noutput.X.scale = 1\noutput.X.scale = 2\n",
        );
        assert_eq!(s.keyboard.layout.as_deref(), Some("de"));
        assert_eq!(s.output("X").expect("X").scale, Some(2.0));
    }

    #[test]
    fn two_primaries_resolve_to_one_stable_answer() {
        let s = parse("output.B.primary = true\noutput.A.primary = true\n");
        assert_eq!(s.primary(), Some("A"), "a HashMap must not decide this");
        // And `primary = false` is not the same as absent-but-listed.
        let s = parse("output.A.primary = false\noutput.B.primary = true\n");
        assert_eq!(s.primary(), Some("B"));
    }

    #[test]
    fn bools_take_the_spellings_people_type() {
        for text in ["true", "yes", "on", "1", "TRUE", "Yes"] {
            let s = parse(&format!("output.X.primary = {text}\n"));
            assert!(s.output("X").expect("X").primary, "{text}");
            assert!(s.warnings.is_empty(), "{text}: {:?}", s.warnings);
        }
        for text in ["false", "no", "off", "0"] {
            let s = parse(&format!("output.X.primary = {text}\n"));
            assert!(!s.output("X").expect("X").primary, "{text}");
        }
    }

    #[test]
    fn scale_bounds_are_enforced_because_a_typo_locks_the_desktop() {
        assert_eq!(
            parse("output.X.scale = 0.5").output("X").expect("X").scale,
            Some(0.5)
        );
        assert_eq!(
            parse("output.X.scale = 8").output("X").expect("X").scale,
            Some(8.0)
        );
        for bad in ["0.4", "8.1", "-2", "0"] {
            let s = parse(&format!("output.X.scale = {bad}\n"));
            assert_eq!(s.output("X").expect("X").scale, None, "{bad}");
            assert_eq!(s.warnings.len(), 1, "{bad}");
        }
    }

    #[test]
    fn a_negative_position_is_fine_because_a_screen_may_be_to_the_left() {
        let s = parse("output.X.position = -1920,-100\n");
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.output("X").expect("X").position, Some((-1920, -100)));
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
        // against a working directory the server does not control.
        assert_eq!(
            resolve(None, Some(Path::new("relative")), Some(home)),
            Some(PathBuf::from("/home/u/.config/nitro/server.conf"))
        );
        assert_eq!(resolve(None, None, None), None);
    }

    #[test]
    fn load_reads_a_real_file() {
        let dir = std::env::temp_dir().join(format!("nitro-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "output.Virtual-1.scale = 2\n").expect("write");
        let s = load(&path);
        assert_eq!(s.output("Virtual-1").expect("virtual").scale, Some(2.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

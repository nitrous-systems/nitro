//! `app_id` → `.desktop` → `Icon=`: the third and last step of the
//! server's icon-name resolution.
//!
//! # The problem this exists for
//!
//! `nitro-bar`'s window list asks for an icon by the window's **app id**
//! (`docs/shell.md`), because the freedesktop convention is that an
//! application's desktop file is named after its app id and its `Icon=`
//! usually matches. The convention is right often enough to be worth one
//! string, and it is exactly wrong for our own applications: `nitro-calc`
//! is an app id, the shape it wants is `calculator`, and the fact that
//! ties the two together is written in `deploy/nitro-calc.desktop` —
//! `Icon=calculator` — which nothing consulted. On the test box every
//! nitro application showed the fallback `window` glyph, which is
//! `docs/icons.md`'s "still deferred" list turning into a visible defect.
//!
//! So the server reads the `.desktop` files after all. One hop: a name
//! that resolves in neither the symbolic set nor the icon theme is looked
//! up as a desktop-entry basename, and its `Icon=` value goes back
//! through the *normal* resolution — symbolic first, then theme. An
//! `Icon=` that resolves nowhere is a `BadIcon`, **not** another
//! `.desktop` lookup: a chain of indirections is a cycle waiting to
//! happen and answers no question the one hop does not.
//!
//! # Why this parser is not `nitro-launcher`'s
//!
//! `crates/nitro-launcher/src/desktop.rs` already parses these files, and
//! this module deliberately duplicates the thirty lines it needs instead
//! of depending on it. Three reasons, in order of weight:
//!
//! * **The dependency edge is backwards.** `nitro-launcher` is a *client*
//!   of the server: it links `nitro-wire` and `nitro-ui` and spawns
//!   processes. A compositor that linked its launcher would pull an
//!   application's tree into the process that owns the screen, to reuse a
//!   `split('=')`.
//! * **The two want different answers.** The launcher needs `Name`,
//!   `Exec`, `Terminal`, `NoDisplay`, `Hidden`, `Type` and the `%f` field
//!   codes, and it needs to *skip* entries; this needs one key from files
//!   it never filters, because an icon for a `NoDisplay=true` entry is
//!   still the right icon for that application's window. Sharing would
//!   mean one parser with two modes.
//! * **It is an index, not a parse.** The expensive artefact here is the
//!   `basename -> Icon=` map built once at start; the launcher builds a
//!   `Vec<Entry>` it then sorts and filters.
//!
//! What *is* shared is the format's two traps, and they are the reason
//! this is a scan rather than a `split('=')`: **only the
//! `[Desktop Entry]` group counts** (a later `[Desktop Action …]` has its
//! own `Icon=`), and **a localized key is not the key** (`Icon[de]=` must
//! never overwrite `Icon=`).
//!
//! # The search path
//!
//! `$XDG_DATA_HOME/applications`, `~/.local/share/applications`, then
//! each `$XDG_DATA_DIRS` entry plus `/applications` — the same directory
//! precedence [`crate::icon_theme`] uses for icons, minus the flat
//! `/usr/share/pixmaps` (which is an icon directory and holds no desktop
//! entries). Earlier directories win, which is what lets a user override
//! a packaged entry from `~/.local/share/applications`.
//!
//! # Cost
//!
//! One `read_dir` per directory and one `read_to_string` per `.desktop`
//! file, at start and on `reload`. The test box has 60-odd entries; a
//! developer box with a full desktop installed has 300, which is about a
//! megabyte of text read once. The map is `basename -> Icon=` and holds
//! nothing else, so the residency is the icon names themselves.
//!
//! Files are read **eagerly** rather than on the first miss, because the
//! alternative is a `read_dir` on the paint-adjacent commit path and a
//! staleness question with no answer: nothing here watches the
//! filesystem, so a lazily built index would be as stale as this one and
//! would pay for it at a worse moment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The extension a desktop entry has, including the dot.
const EXTENSION: &str = ".desktop";

/// The group whose keys are the entry's own.
const GROUP: &str = "[Desktop Entry]";

/// The most entries indexed, over every directory.
///
/// A bound rather than a policy: `$XDG_DATA_DIRS` is user-controlled and
/// a directory with a million files in it must not be able to make the
/// server's start unbounded. 4 096 is an order of magnitude past a fully
/// loaded desktop (the dev box has 312).
const MAX_ENTRIES: usize = 4096;

/// The largest desktop file that is read at all.
///
/// A `.desktop` is a few hundred bytes; anything past 64 KiB is not one,
/// and reading it would be a way to make the server hold somebody's disk
/// image in memory.
const MAX_BYTES: u64 = 64 * 1024;

/// `basename -> Icon=`, for every desktop entry on the search path.
#[derive(Debug, Default)]
pub struct DesktopIndex {
    /// The file's basename without `.desktop` — which is what an app id
    /// is conventionally equal to — mapped to its `Icon=` value.
    icons: HashMap<String, String>,
    /// The directories scanned, for diagnostics and the tests.
    dirs: Vec<PathBuf>,
}

impl DesktopIndex {
    /// Scan the XDG application directories.
    #[must_use]
    pub fn load() -> Self {
        Self::with_dirs(default_dirs())
    }

    /// Scan exactly `dirs`, in order, earlier winning.
    ///
    /// The hermetic form, for the tests and for [`Config::desktop_dirs`]:
    /// an index built from the developer's own `/usr/share/applications`
    /// would make every test's answer depend on what the box running it
    /// happens to have installed.
    ///
    /// [`Config::desktop_dirs`]: crate::Config::desktop_dirs
    #[must_use]
    pub fn with_dirs(dirs: Vec<PathBuf>) -> Self {
        let mut index = Self {
            icons: HashMap::new(),
            dirs,
        };
        index.scan();
        index
    }

    /// The `Icon=` value the entry named `name` declares, if any.
    ///
    /// `name` is a desktop-entry basename — an app id, in the case this
    /// exists for. A name containing a path separator or a NUL is refused
    /// rather than looked up: the index is keyed by basename, so such a
    /// name cannot be in it, and refusing says so at the boundary.
    #[must_use]
    pub fn icon(&self, name: &str) -> Option<&str> {
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return None;
        }
        self.icons.get(name).map(String::as_str)
    }

    /// How many entries the index holds — the `desktop_entries` stat.
    #[must_use]
    pub fn len(&self) -> usize {
        self.icons.len()
    }

    /// Whether the index found nothing at all, which is the normal state
    /// on a box with no applications installed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.icons.is_empty()
    }

    /// The directories scanned, in precedence order.
    #[must_use]
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    fn scan(&mut self) {
        for dir in self.dirs.clone() {
            if self.icons.len() >= MAX_ENTRIES {
                return;
            }
            self.scan_dir(&dir);
        }
    }

    fn scan_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if self.icons.len() >= MAX_ENTRIES {
                return;
            }
            let path = entry.path();
            let Some(name) = basename(&path) else {
                continue;
            };
            // An earlier directory wins, so a user's own entry in
            // `~/.local/share/applications` overrides the packaged one —
            // which is the whole point of the search path's order.
            if self.icons.contains_key(&name) {
                continue;
            }
            let too_big = entry
                .metadata()
                .is_ok_and(|m| !m.is_file() || m.len() > MAX_BYTES);
            if too_big {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(icon) = icon_key(&text) {
                self.icons.insert(name, icon);
            }
        }
    }
}

/// The basename of a `.desktop` file, without its extension.
fn basename(path: &Path) -> Option<String> {
    let file = path.file_name()?.to_str()?;
    let stem = file.strip_suffix(EXTENSION)?;
    (!stem.is_empty() && !stem.contains('\0')).then(|| stem.to_owned())
}

/// The `[Desktop Entry]` group's `Icon=` value, if the file has one.
///
/// Thirty lines, and every one of them is one of the format's two traps:
///
/// * **Group.** Keys are only read inside `[Desktop Entry]`. A file's
///   later groups are `[Desktop Action new-window]` and friends, which
///   carry their own `Icon=`; a parser that ignored headers would take
///   the last one in the file.
/// * **Locale.** `Icon[de]=` is a *different key*. The suffix is
///   recognised and skipped rather than trimmed off, so a hostile or
///   merely German file cannot overwrite the plain key.
///
/// The first `Icon=` in the group wins, matching the spec's "later
/// duplicate keys are undefined" by picking the deterministic one.
fn icon_key(text: &str) -> Option<String> {
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // A group header ends the previous group, so leaving
            // `[Desktop Entry]` stops the scan: everything after it
            // belongs to an action, not to the application.
            if in_group {
                return None;
            }
            in_group = line == GROUP;
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim_end() != "Icon" {
            continue;
        }
        let value = value.trim();
        if value.is_empty() || value.contains('\0') {
            return None;
        }
        return Some(value.to_owned());
    }
    None
}

/// The XDG application directories, in precedence order.
///
/// Deliberately the same shape as [`crate::icon_theme`]'s `default_dirs`,
/// minus `/usr/share/pixmaps`: that is a flat *icon* directory and holds
/// no desktop entries.
fn default_dirs() -> Vec<PathBuf> {
    /// What `$XDG_DATA_DIRS` means when it is unset, per the basedir
    /// spec. The same default `icon_theme` uses.
    const DEFAULT_DATA_DIRS: &str = "/usr/local/share:/usr/share";

    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        Some(d) if d.is_absolute() => dirs.push(d.join("applications")),
        _ => {
            if let Some(h) = &home {
                dirs.push(h.join(".local").join("share").join("applications"));
            }
        }
    }
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_DATA_DIRS.into());
    for d in std::env::split_paths(&data_dirs).filter(|p| p.is_absolute()) {
        dirs.push(d.join("applications"));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nitro-desktop-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        dir
    }

    fn put(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write a fixture entry");
    }

    #[test]
    fn an_entrys_icon_is_found_by_its_basename() {
        let dir = fixture("basic");
        put(
            &dir,
            "nitro-calc.desktop",
            "[Desktop Entry]\nType=Application\nName=Calculator\nIcon=calculator\n",
        );
        let index = DesktopIndex::with_dirs(vec![dir]);
        assert_eq!(index.icon("nitro-calc"), Some("calculator"));
        assert_eq!(index.len(), 1);
        assert!(!index.is_empty());
        // The name is the basename, not the `Name=`, not the `Exec=`.
        assert_eq!(index.icon("Calculator"), None);
        assert_eq!(index.icon("calculator"), None);
    }

    #[test]
    fn only_the_desktop_entry_group_counts() {
        // The trap the launcher's parser records: a file's later groups
        // are actions with their own keys, and taking the last `Icon=`
        // would give the application its "open a new window" artwork.
        let dir = fixture("groups");
        put(
            &dir,
            "browser.desktop",
            "[Desktop Entry]\nIcon=browser\nActions=new-window;\n\n\
             [Desktop Action new-window]\nName=New Window\nIcon=browser-new\n",
        );
        put(
            &dir,
            "actiononly.desktop",
            "[Desktop Action solo]\nIcon=not-the-app\n",
        );
        let index = DesktopIndex::with_dirs(vec![dir]);
        assert_eq!(index.icon("browser"), Some("browser"));
        assert_eq!(
            index.icon("actiononly"),
            None,
            "an Icon= outside [Desktop Entry] is not the entry's icon"
        );
    }

    #[test]
    fn a_localized_key_never_shadows_the_plain_one() {
        let dir = fixture("locale");
        put(
            &dir,
            "a.desktop",
            "[Desktop Entry]\nIcon[de]=rechner\nIcon=calculator\nIcon[fr]=calculatrice\n",
        );
        // And a file with *only* a localized key has no icon at all,
        // rather than the last translation to be parsed.
        put(&dir, "b.desktop", "[Desktop Entry]\nIcon[de]=rechner\n");
        let index = DesktopIndex::with_dirs(vec![dir]);
        assert_eq!(index.icon("a"), Some("calculator"));
        assert_eq!(index.icon("b"), None);
    }

    #[test]
    fn an_earlier_directory_wins_and_a_missing_one_is_not_an_error() {
        let user = fixture("prec-user");
        let system = fixture("prec-system");
        put(&user, "foo.desktop", "[Desktop Entry]\nIcon=mine\n");
        put(&system, "foo.desktop", "[Desktop Entry]\nIcon=packaged\n");
        put(&system, "bar.desktop", "[Desktop Entry]\nIcon=theirs\n");
        let index = DesktopIndex::with_dirs(vec![
            user,
            PathBuf::from("/nonexistent-nitro-applications"),
            system,
        ]);
        assert_eq!(index.icon("foo"), Some("mine"), "the user's entry wins");
        assert_eq!(index.icon("bar"), Some("theirs"));
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn nothing_but_a_desktop_file_is_read_and_a_hostile_name_is_refused() {
        let dir = fixture("hostile");
        put(&dir, "real.desktop", "[Desktop Entry]\nIcon=real\n");
        put(&dir, "notes.txt", "[Desktop Entry]\nIcon=nope\n");
        put(&dir, ".desktop", "[Desktop Entry]\nIcon=empty-stem\n");
        put(&dir, "empty.desktop", "[Desktop Entry]\nIcon=\n");
        put(&dir, "novalue.desktop", "[Desktop Entry]\nIcon\n");
        std::fs::create_dir_all(dir.join("adir.desktop")).expect("a directory that looks like one");
        let index = DesktopIndex::with_dirs(vec![dir]);
        assert_eq!(index.icon("real"), Some("real"));
        assert_eq!(index.len(), 1, "one real entry: {:?}", index.icons);
        // A name that could only come from a path, refused at the door.
        assert_eq!(index.icon("../real"), None);
        assert_eq!(index.icon("sub/real"), None);
        assert_eq!(index.icon(""), None);
        assert_eq!(index.icon("re\0al"), None);
    }

    #[test]
    fn an_absolute_icon_path_is_passed_through_unexamined() {
        // The desktop-entry spec allows a path in `Icon=`, and deciding
        // whether it is readable is the icon lookup's business, not this
        // index's — it is a string map, and one that validated paths
        // would have two opinions about the same file.
        let dir = fixture("abspath");
        put(
            &dir,
            "thing.desktop",
            "[Desktop Entry]\nIcon=/opt/x/i.png\n",
        );
        let index = DesktopIndex::with_dirs(vec![dir]);
        assert_eq!(index.icon("thing"), Some("/opt/x/i.png"));
    }

    #[test]
    fn comments_blank_lines_and_whitespace_are_tolerated() {
        let dir = fixture("messy");
        put(
            &dir,
            "messy.desktop",
            "# a comment\n\n  [Desktop Entry]  \n# another\nType=Application\n\
             Icon  =  spaced  \nName=Messy\n",
        );
        let index = DesktopIndex::with_dirs(vec![dir]);
        assert_eq!(index.icon("messy"), Some("spaced"));
    }

    #[test]
    fn an_empty_search_path_is_an_empty_index() {
        let index = DesktopIndex::with_dirs(Vec::new());
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
        assert_eq!(index.icon("anything"), None);
        assert!(index.dirs().is_empty());
    }
}

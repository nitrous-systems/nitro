//! `.desktop` files: the parser, the search path, and the rules about
//! which entries a launcher may show.
//!
//! The format is the freedesktop Desktop Entry Specification, and this is
//! a **deliberately partial** reader of it: the launcher needs a name, a
//! command and three booleans, and every other key is skipped without
//! being understood. That is not laziness — a parser that understood
//! keys it does not use would have opinions about files it has no reason
//! to have opinions about, and the failure mode of a launcher is "an
//! application is missing", which is much easier to debug when the reader
//! is small enough to read.
//!
//! The whole of the format that is honoured here:
//!
//! | key | what the launcher does with it |
//! |---|---|
//! | `Name` | the entry's label, and what the query matches against |
//! | `Exec` | the command, with the `%f`/`%u`/… field codes stripped |
//! | `NoDisplay=true` | the entry is skipped |
//! | `Hidden=true` | the entry is skipped ("deleted" in the spec) |
//! | `Terminal=true` | kept, but marked: see [`Entry::terminal`] |
//! | `Type` | anything but `Application` is skipped |
//!
//! Everything else — `Icon`, `Categories`, `MimeType`, `Actions`, the
//! whole `X-` namespace — is read past. Icons are M4: the toolkit has an
//! `Image` widget but no icon *theme* lookup, and half an icon theme is
//! worse than none.
//!
//! # Two rules that are easy to get wrong
//!
//! **Only the `[Desktop Entry]` group counts.** A file's later groups are
//! `[Desktop Action New]` and friends, which have their own `Name` and
//! `Exec`. A parser that ignored group headers would happily take the
//! *last* `Exec` in the file and launch "open a new private window"
//! whenever the user asked for the browser.
//!
//! **A localized key is not the key.** `Name[de]=Rechner` is a different
//! key from `Name`, and a parser that split on `=` and trimmed would
//! overwrite the name with whichever translation came last. The launcher
//! runs in one locale — the user's — and picking the right translation is
//! a feature with a `LANG`-parsing tail on it; showing `Name` is correct
//! for the C locale and predictable everywhere else, which is the M3
//! trade. Recorded in the crate README under *Limitations*.

use std::path::{Path, PathBuf};

/// One launchable thing: a parsed `.desktop` file, or one of the nitro
/// binaries the launcher finds next to itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What the user sees and what the query matches against.
    pub name: String,
    /// The command, already split into argv with the field codes gone.
    /// Never empty for an entry that made it out of the parser.
    pub argv: Vec<String>,
    /// `Terminal=true`: the program wants a terminal emulator to run in.
    ///
    /// Kept rather than dropped, and **not run** in M3: there is no
    /// `nitro-term` yet, and spawning a terminal application with its
    /// stdio on `/dev/null` produces a process the user cannot see and
    /// cannot type at, which looks exactly like a launcher that did
    /// nothing. [`Entry::runnable`] is the predicate.
    pub terminal: bool,
    /// Where it came from, for diagnostics and for the tests.
    pub source: Source,
}

/// Where an [`Entry`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A `.desktop` file at this path.
    Desktop(PathBuf),
    /// A nitro binary sitting next to the launcher.
    ///
    /// The launcher lists these so the box works on a machine with no
    /// desktop files at all — which is exactly the test box, and exactly
    /// the state a freshly deployed `~/nitro-bin` is in.
    Builtin,
}

impl Entry {
    /// Whether the launcher will actually run this entry.
    ///
    /// `Terminal=true` is not runnable in M3; see [`Entry::terminal`].
    #[must_use]
    pub fn runnable(&self) -> bool {
        !self.terminal && !self.argv.is_empty()
    }

    /// The program, i.e. `argv[0]`.
    #[must_use]
    pub fn program(&self) -> &str {
        self.argv.first().map_or("", String::as_str)
    }
}

/// Parse one `.desktop` file's text.
///
/// `None` when the file is not a launchable application entry: no
/// `[Desktop Entry]` group, a `Type` that is not `Application`, no usable
/// `Exec`, no `Name`, or `NoDisplay`/`Hidden` set.
///
/// The path is carried through into [`Source::Desktop`] and is not read.
#[must_use]
pub fn parse(text: &str, path: &Path) -> Option<Entry> {
    let mut in_entry = false;
    let mut name = None::<String>;
    let mut exec = None::<String>;
    let mut kind = None::<String>;
    let mut terminal = false;
    let mut hidden = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(group) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // A group header ends the previous group, so everything after
            // `[Desktop Action …]` is somebody else's `Exec`.
            in_entry = group == "Desktop Entry";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        // The spec allows spaces around `=`; the *key* may not contain
        // them, so trimming is safe here and the value is trimmed of the
        // same whitespace the file's author could not see either.
        let key = key.trim();
        let value = value.trim();
        // `Name[de]` is a different key from `Name` and must not
        // overwrite it. Rejecting anything with a bracket is enough: no
        // key this parser wants has one.
        if key.contains('[') {
            continue;
        }
        match key {
            "Name" => name = Some(value.to_owned()),
            "Exec" => exec = Some(value.to_owned()),
            "Type" => kind = Some(value.to_owned()),
            "Terminal" => terminal = is_true(value),
            "NoDisplay" | "Hidden" => hidden |= is_true(value),
            _ => {}
        }
    }
    if hidden {
        return None;
    }
    // A missing `Type` is taken as `Application`: it is required by the
    // spec, so a file without one is malformed, and the malformed files
    // in the wild are overwhelmingly application entries with a typo
    // rather than links or directories in disguise.
    if let Some(k) = &kind
        && k != "Application"
    {
        return None;
    }
    let name = name?;
    let argv = exec_argv(&exec?);
    if name.is_empty() || argv.is_empty() {
        return None;
    }
    Some(Entry {
        name,
        argv,
        terminal,
        source: Source::Desktop(path.to_path_buf()),
    })
}

/// Whether a desktop-entry boolean is true.
///
/// The spec says `true`/`false` exactly. `1` is accepted as well because
/// it is common in old files, and the cost of being wrong is a hidden
/// entry rather than a wrong command.
fn is_true(v: &str) -> bool {
    v.eq_ignore_ascii_case("true") || v == "1"
}

/// Turn an `Exec` value into argv: split on whitespace honouring the
/// spec's quoting, then drop the field codes.
///
/// The field codes (`%f`, `%F`, `%u`, `%U`, `%d`, `%D`, `%n`, `%N`,
/// `%i`, `%c`, `%k`, `%v`, `%m`) are where a file to open or an icon
/// would be substituted. A launcher that starts a program with no
/// document has nothing to substitute, so they are **removed** rather
/// than passed through: `%U` handed to the program literally is an
/// argument it will try to open, and `firefox %U` would open a file
/// called `%U`.
///
/// `%%` is an escaped percent and becomes `%`.
#[must_use]
pub fn exec_argv(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in split_exec(exec) {
        let Some(arg) = strip_field_codes(&raw) else {
            continue;
        };
        out.push(arg);
    }
    out
}

/// Split an `Exec` value into words, honouring `"…"` quoting and the
/// backslash escapes the spec allows inside quotes.
fn split_exec(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => {
                // Inside quotes the spec escapes `"`, `` ` ``, `$` and
                // `\`; anything else keeps its backslash rather than
                // vanishing, which is the forgiving direction.
                match chars.next() {
                    Some(e @ ('"' | '`' | '$' | '\\')) => cur.push(e),
                    Some(other) => {
                        cur.push('\\');
                        cur.push(other);
                    }
                    None => cur.push('\\'),
                }
            }
            c if c.is_whitespace() && !quoted => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Remove the field codes from one already-split argument.
///
/// `None` when the argument *was* a field code and nothing is left of it,
/// so `firefox %u` becomes `["firefox"]` rather than `["firefox", ""]` —
/// an empty argument is not nothing, and `execvp` would pass it on.
fn strip_field_codes(arg: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = arg.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // Every field code is one letter, and an unknown one is
            // dropped too: a code this parser does not know is still a
            // substitution the launcher cannot make. `%%` is the escape,
            // and a trailing `%` is not a code at all.
            Some(c) if c != '%' => {}
            _ => out.push('%'),
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_owned())
}

/// The directories `.desktop` files are looked for in, **most-specific
/// last** — which is the reverse of the order `XDG_DATA_DIRS` is written
/// in, and the reversal is the whole subtlety here.
///
/// The XDG base-directory spec says "the first directory listed is the
/// most important", and the Desktop Entry spec resolves a desktop-file
/// ID to the **first** file found along that path. [`scan`] implements
/// precedence the other way round — later directory wins, because it
/// overwrites as it goes — so the two only agree if the list handed to it
/// is reversed. Hence [`dirs_from`]'s `.rev()`.
///
/// Getting this backwards is not a theoretical complaint: it makes
/// `/usr/share/applications` shadow `/usr/local/share/applications`, so a
/// locally installed program is hidden by the distribution's copy of the
/// same file — exactly backwards, and silent.
///
/// The defaults are the spec's: `/usr/local/share:/usr/share` for
/// `XDG_DATA_DIRS` and `~/.local/share` for `XDG_DATA_HOME`. The home
/// directory is appended **after** the reversal, so it outranks every
/// system directory.
///
/// `NITRO_LAUNCHER_DIRS` overrides the whole list, which is how the tests
/// point the launcher at a fixture directory instead of at whatever the
/// machine running them happens to have installed. It is taken in the
/// order written, because it is not `XDG_DATA_DIRS` and a test that has
/// to reason about a reversal is a test about the wrong thing.
#[must_use]
pub fn search_dirs() -> Vec<PathBuf> {
    if let Some(over) = env_nonempty("NITRO_LAUNCHER_DIRS") {
        return over
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    let data_dirs =
        env_nonempty("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    let home = env_nonempty("XDG_DATA_HOME")
        .or_else(|| env_nonempty("HOME").map(|h| format!("{h}/.local/share")));
    dirs_from(&data_dirs, home.as_deref())
}

/// The pure half of [`search_dirs`]: the search path for a given
/// `XDG_DATA_DIRS` and `XDG_DATA_HOME`, most-specific last.
///
/// Split out so the precedence rule can be **tested**. `search_dirs`
/// reads the process environment, and a test that set it would race every
/// other test in the binary — so the earlier version of this test asserted
/// on a `Vec` it built itself, which is to say it asserted nothing and
/// missed the inverted order this function now pins down.
#[must_use]
pub fn dirs_from(data_dirs: &str, data_home: Option<&str>) -> Vec<PathBuf> {
    // `.rev()`: `XDG_DATA_DIRS` is most-important-**first** and `scan`
    // wants most-important-last. See [`search_dirs`].
    let mut out: Vec<PathBuf> = data_dirs
        .split(':')
        .filter(|s| !s.trim().is_empty())
        .map(|d| Path::new(d.trim()).join("applications"))
        .rev()
        .collect();
    if let Some(h) = data_home.map(str::trim).filter(|h| !h.is_empty()) {
        out.push(Path::new(h).join("applications"));
    }
    out
}

/// An environment variable, if it is set and not empty.
///
/// The spec says an empty `XDG_DATA_DIRS` means "use the default", not
/// "search nowhere", and treating it as the latter is how a launcher ends
/// up empty on a machine where something cleared the variable.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Read every `.desktop` file under `dirs` and return the entries.
///
/// Entries are deduplicated **by file name**, earliest directory losing:
/// that is the spec's precedence rule, and it is why `~/.local/share`
/// comes last in [`search_dirs`] — a user's own `firefox.desktop`
/// replaces the system's rather than appearing beside it.
///
/// Directories that do not exist, files that cannot be read and files
/// that do not parse are skipped silently. A launcher is not the right
/// place to report that somebody's package shipped a malformed entry, and
/// a launcher that refused to start over one would be strictly worse than
/// one missing an item.
#[must_use]
pub fn scan(dirs: &[PathBuf]) -> Vec<Entry> {
    // Keyed by file name, so the later directory wins; the insertion
    // order is not the output order, which is sorted below anyway.
    let mut by_id: Vec<(String, Entry)> = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for file in rd.filter_map(Result::ok) {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "desktop") {
                continue;
            }
            let Some(id) = path.file_name().map(|f| f.to_string_lossy().into_owned()) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(entry) = parse(&text, &path) else {
                // A file that parsed to nothing still *shadows* an
                // earlier one of the same name: a user who put a
                // `NoDisplay=true` firefox.desktop in their own directory
                // meant to hide the system one, and re-showing it would
                // be exactly backwards.
                by_id.retain(|(k, _)| k != &id);
                continue;
            };
            if let Some(slot) = by_id.iter_mut().find(|(k, _)| k == &id) {
                slot.1 = entry;
            } else {
                by_id.push((id, entry));
            }
        }
    }
    let mut out: Vec<Entry> = by_id.into_iter().map(|(_, e)| e).collect();
    // Sorted by name so the list is stable between runs: `read_dir`
    // returns whatever order the filesystem feels like, and a launcher
    // whose results reshuffled between openings would be unusable with
    // muscle memory.
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

/// The newest modification time across `dirs`, as nanoseconds since the
/// epoch, plus the number of directories that exist.
///
/// This is the rescan trigger: a launcher that re-read every `.desktop`
/// file on the machine each time it opened would cost a few hundred
/// `open`/`read` pairs on a keystroke, and one that never re-read them
/// would not show an application installed since login. A directory's
/// mtime changes when a file in it is created, removed or renamed — which
/// is what installing or removing a package does — so the pair is a cheap
/// and sufficient "has anything been added?".
///
/// What it deliberately does *not* catch is a file **edited in place**,
/// which changes the file's mtime and not the directory's. That is rare
/// (packages replace files rather than editing them) and self-correcting
/// (the next install fixes it), and catching it would mean stat-ing every
/// file, which is the cost this exists to avoid. Recorded in the README.
#[must_use]
pub fn fingerprint(dirs: &[PathBuf]) -> (u64, usize) {
    let mut newest = 0u64;
    let mut present = 0usize;
    for dir in dirs {
        let Ok(md) = std::fs::metadata(dir) else {
            continue;
        };
        present += 1;
        if let Ok(t) = md.modified()
            && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
        {
            newest = newest.max(d.as_nanos() as u64);
        }
    }
    (newest, present)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> PathBuf {
        PathBuf::from("/x/test.desktop")
    }

    #[test]
    fn a_plain_entry_parses() {
        let e = parse(
            "[Desktop Entry]\nType=Application\nName=Calculator\nExec=nitro-calc\n",
            &p(),
        )
        .expect("an application entry");
        assert_eq!(e.name, "Calculator");
        assert_eq!(e.argv, vec!["nitro-calc".to_owned()]);
        assert!(!e.terminal);
        assert!(e.runnable());
        assert_eq!(e.source, Source::Desktop(p()));
    }

    #[test]
    fn field_codes_are_stripped_not_passed_on() {
        // The bug this prevents is concrete: `firefox %U` launched with
        // the code intact opens a file named `%U`.
        for exec in ["firefox %U", "firefox %u", "firefox %f", "firefox %F"] {
            let e =
                parse(&format!("[Desktop Entry]\nName=Web\nExec={exec}\n"), &p()).expect("parsed");
            assert_eq!(e.argv, vec!["firefox".to_owned()], "{exec}");
        }
        // A code in the middle of an argument leaves the rest.
        assert_eq!(
            exec_argv("prog --file=%f --flag"),
            vec!["prog".to_owned(), "--file=".to_owned(), "--flag".to_owned()]
        );
        // `%%` is a literal percent, not a code.
        assert_eq!(
            exec_argv("prog 50%%"),
            vec!["prog".to_owned(), "50%".to_owned()]
        );
    }

    #[test]
    fn quoted_arguments_survive_the_split() {
        assert_eq!(
            exec_argv(r#"/usr/bin/prog "two words" plain"#),
            vec![
                "/usr/bin/prog".to_owned(),
                "two words".to_owned(),
                "plain".to_owned()
            ]
        );
        assert_eq!(
            exec_argv(r#"prog "a\"b""#),
            vec!["prog".to_owned(), "a\"b".to_owned()]
        );
    }

    #[test]
    fn nodisplay_and_hidden_are_honoured() {
        assert!(
            parse(
                "[Desktop Entry]\nName=Setup\nExec=setup\nNoDisplay=true\n",
                &p()
            )
            .is_none()
        );
        assert!(
            parse(
                "[Desktop Entry]\nName=Setup\nExec=setup\nHidden=TRUE\n",
                &p()
            )
            .is_none()
        );
        // `NoDisplay=false` is not hidden, which is the case a sloppy
        // `contains("NoDisplay")` check would get wrong.
        assert!(
            parse(
                "[Desktop Entry]\nName=Setup\nExec=setup\nNoDisplay=false\n",
                &p()
            )
            .is_some()
        );
    }

    #[test]
    fn a_localized_name_does_not_overwrite_the_name() {
        // The bug: split on `=`, trim, and `Name[de]` lands in `Name`.
        let e = parse(
            "[Desktop Entry]\nName=Calculator\nName[de]=Rechner\nName[fr]=Calculatrice\nExec=calc\n",
            &p(),
        )
        .expect("parsed");
        assert_eq!(e.name, "Calculator");
        // And a localized comment does not become the name either.
        let e = parse(
            "[Desktop Entry]\nName[de]=Rechner\nName=Calculator\nGenericName[de]=Rechner\nExec=calc\n",
            &p(),
        )
        .expect("parsed");
        assert_eq!(e.name, "Calculator");
    }

    #[test]
    fn only_the_desktop_entry_group_is_read() {
        // A browser's actions group has its own Name and Exec; taking the
        // last one in the file launches "new private window" for every
        // search that matched the browser.
        let e = parse(
            "[Desktop Entry]\nName=Browser\nExec=browser\n\
             [Desktop Action NewPrivate]\nName=Private Window\nExec=browser --private\n",
            &p(),
        )
        .expect("parsed");
        assert_eq!(e.name, "Browser");
        assert_eq!(e.argv, vec!["browser".to_owned()]);
    }

    #[test]
    fn a_terminal_entry_is_parsed_but_not_runnable() {
        // Kept rather than dropped, because "htop is missing from the
        // launcher" and "htop is there and starts nothing you can see"
        // are different bugs and only the first is honest. M3 has no
        // terminal to run it in.
        let e = parse(
            "[Desktop Entry]\nName=htop\nExec=htop\nTerminal=true\n",
            &p(),
        )
        .expect("parsed");
        assert!(e.terminal);
        assert!(!e.runnable());
    }

    #[test]
    fn a_non_application_type_is_skipped() {
        assert!(
            parse(
                "[Desktop Entry]\nType=Link\nName=Docs\nExec=x\nURL=http://x\n",
                &p()
            )
            .is_none()
        );
        assert!(parse("[Desktop Entry]\nType=Directory\nName=Games\n", &p()).is_none());
        // A missing Type is taken as Application: malformed files in the
        // wild are overwhelmingly applications with a typo.
        assert!(parse("[Desktop Entry]\nName=X\nExec=x\n", &p()).is_some());
    }

    #[test]
    fn an_entry_without_a_name_or_an_exec_is_not_launchable() {
        assert!(parse("[Desktop Entry]\nExec=x\n", &p()).is_none());
        assert!(parse("[Desktop Entry]\nName=X\n", &p()).is_none());
        // An `Exec` that is nothing but a field code leaves no program.
        assert!(parse("[Desktop Entry]\nName=X\nExec=%f\n", &p()).is_none());
        // And a file with no group header at all is not an entry.
        assert!(parse("Name=X\nExec=x\n", &p()).is_none());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let e = parse(
            "# a comment\n\n[Desktop Entry]\n# another\nName=X\n\nExec=x\n",
            &p(),
        )
        .expect("parsed");
        assert_eq!(e.name, "X");
    }

    #[test]
    fn scanning_prefers_the_later_directory_and_sorts_by_name() {
        let dir = std::env::temp_dir().join(format!("nitro-launcher-scan-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(
            a.join("app.desktop"),
            "[Desktop Entry]\nName=System\nExec=system\n",
        )
        .unwrap();
        std::fs::write(
            b.join("app.desktop"),
            "[Desktop Entry]\nName=Mine\nExec=mine\n",
        )
        .unwrap();
        std::fs::write(
            a.join("zebra.desktop"),
            "[Desktop Entry]\nName=Zebra\nExec=zebra\n",
        )
        .unwrap();
        std::fs::write(a.join("notes.txt"), "not a desktop file").unwrap();

        let found = scan(&[a.clone(), b.clone()]);
        let names: Vec<&str> = found.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Mine", "Zebra"], "the user's file wins");

        // And a user's hidden override removes the system entry rather
        // than leaving it visible.
        std::fs::write(
            b.join("zebra.desktop"),
            "[Desktop Entry]\nName=Zebra\nExec=zebra\nNoDisplay=true\n",
        )
        .unwrap();
        let names: Vec<String> = scan(&[a, b]).into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["Mine".to_owned()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fingerprint_moves_when_a_file_is_added() {
        let dir = std::env::temp_dir().join(format!("nitro-launcher-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let before = fingerprint(std::slice::from_ref(&dir));
        assert_eq!(before.1, 1, "the directory exists");
        // The mtime has 1 ns resolution on tmpfs but the clock may not,
        // so wait long enough that a change is unambiguous.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("new.desktop"), "[Desktop Entry]\nName=N\nExec=n\n").unwrap();
        let after = fingerprint(std::slice::from_ref(&dir));
        assert_ne!(before, after, "adding a file changes the fingerprint");
        // A directory that is not there is not counted, so removing one
        // is a change too.
        assert_eq!(fingerprint(&[dir.join("nope")]), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_search_path_is_most_specific_last() {
        // The regression for an inverted precedence, and the reason
        // `dirs_from` exists as a separate function at all: the earlier
        // version of this test built a `Vec` locally and asserted that
        // `Path::join` works, which is to say it asserted nothing — and
        // missed the bug.
        //
        // `XDG_DATA_DIRS` is most-important-**first** (XDG basedir: "the
        // first directory listed is the most important"), and `scan`
        // gives precedence to the **last** directory it reads. So the
        // list has to come out reversed, or `/usr/share` shadows
        // `/usr/local/share` and a locally installed program is hidden by
        // the distribution's copy of the same file.
        let dirs = dirs_from("/usr/local/share:/usr/share", Some("/home/u/.local/share"));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/usr/share/applications"),
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/home/u/.local/share/applications"),
            ],
            "most-specific last: /usr/local/share must outrank /usr/share"
        );

        // And that really is what `scan` reads as precedence — asserted
        // against `scan` itself rather than restated here, so the two
        // cannot drift apart.
        let root = std::env::temp_dir().join(format!("nitro-launcher-prec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (sys, local) = (root.join("usr/share"), root.join("usr/local/share"));
        for (dir, name) in [(&sys, "System"), (&local, "Local")] {
            let apps = dir.join("applications");
            std::fs::create_dir_all(&apps).unwrap();
            std::fs::write(
                apps.join("prog.desktop"),
                format!("[Desktop Entry]\nName={name}\nExec=prog\n"),
            )
            .unwrap();
        }
        let path = dirs_from(&format!("{}:{}", local.display(), sys.display()), None);
        let found = scan(&path);
        assert_eq!(
            found.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["Local"],
            "the earlier entry in XDG_DATA_DIRS wins, as the spec says"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_home_directory_outranks_every_system_directory() {
        // A user's own `.desktop` file replaces the system's, whatever
        // `XDG_DATA_DIRS` holds — which is why the home directory is
        // appended *after* the reversal rather than being part of it.
        let dirs = dirs_from("/a:/b:/c", Some("/home/u/.local/share"));
        assert_eq!(
            dirs.last(),
            Some(&PathBuf::from("/home/u/.local/share/applications"))
        );
        // Reversed, so `/a` (the most important) is the latest of the
        // three system directories.
        assert_eq!(
            dirs[..3].to_vec(),
            vec![
                PathBuf::from("/c/applications"),
                PathBuf::from("/b/applications"),
                PathBuf::from("/a/applications"),
            ]
        );
    }

    #[test]
    fn an_absent_or_ragged_data_dirs_does_not_produce_junk_paths() {
        // No home is not an error: a daemon with no `HOME` still has a
        // system search path.
        assert_eq!(
            dirs_from("/usr/share", None),
            vec![PathBuf::from("/usr/share/applications")]
        );
        // Empty entries (a trailing `:`, a `::`) are dropped rather than
        // becoming `applications` relative to the working directory,
        // which is a real directory on somebody's machine.
        assert_eq!(
            dirs_from(":/usr/share::", None),
            vec![PathBuf::from("/usr/share/applications")]
        );
        assert!(dirs_from("", None).is_empty());
        assert!(dirs_from("  ", None).is_empty());
        // And an empty `XDG_DATA_HOME` is "unset", not "the root".
        assert!(dirs_from("", Some("")).is_empty());
    }
}

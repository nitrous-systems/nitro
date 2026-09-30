//! What opens a file.
//!
//! Three questions, answered in order, each with its own freedesktop
//! specification behind it and a deliberately partial reading of it here:
//!
//! 1. **What type is this?** The extension, against the system's
//!    `globs2` table or a small built-in one. That half lives in
//!    [`nitro_fs::mime`], shared with the toolkit's file picker, and is
//!    re-exported here ([`type_of`], [`Glob`], …) so callers keep one
//!    `mime` module.
//! 2. **What handles that type?** The `.desktop` id registered for it in
//!    `mimeapps.list` and `mimeinfo.cache`.
//! 3. **How do I run that?** The entry's `Exec`, parsed by the
//!    launcher's `.desktop` reader, with the path appended.
//!
//! # Nothing here reads the environment except [`Assoc::from_env`]
//!
//! Every path this module consults is a value handed to it, so a test
//! can point it at a temp directory without touching `$XDG_*`. That
//! matters more than usual here: the test binary is threaded, so
//! `std::env::set_var` would race every other test in it, and the
//! alternative — testing against whatever the developer has installed —
//! is a test that passes on one machine.

use std::path::{Path, PathBuf};

pub use nitro_fs::mime::{Glob, builtin_type, icon_for, load_globs2, parse_globs2, type_of};

/// Where the MIME associations live.
///
/// Paths only, resolved once and then read on demand: an association
/// lookup happens when the user opens a file, which is rare enough that
/// caching the *contents* would mostly mean showing stale answers after
/// the user installed something.
///
/// Injectable ([`Assoc::at`]) so tests need no environment mutation; see
/// the module documentation for why that matters in a threaded test
/// binary.
#[derive(Debug, Clone)]
pub struct Assoc {
    /// Directories that may hold a `mimeapps.list`, most important
    /// first: the config roots, then each data root's `applications`.
    lists: Vec<PathBuf>,
    /// Directories holding `.desktop` files (and a `mimeinfo.cache`),
    /// most important first.
    apps: Vec<PathBuf>,
    /// `$XDG_CURRENT_DESKTOP`, lowercased and split on `:`; each names a
    /// `{desktop}-mimeapps.list` read before a directory's plain one.
    desktops: Vec<String>,
}

impl Assoc {
    /// The association files this machine has, from the XDG variables.
    ///
    /// `$XDG_CONFIG_HOME/mimeapps.list` (default `~/.config`) first,
    /// then each `$XDG_CONFIG_DIRS` entry, then the `applications`
    /// directory of `$XDG_DATA_HOME` (default `~/.local/share`) and of
    /// each `$XDG_DATA_DIRS` entry. That is the order the spec gives,
    /// most important **first** — the opposite of the order
    /// `nitro_launcher::desktop::search_dirs` returns, and for a reason:
    /// the launcher's scan overwrites as it walks, so it wants the
    /// winner last, while this walks until it finds an answer and stops.
    #[must_use]
    pub fn from_env() -> Assoc {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let config_home = env_path("XDG_CONFIG_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".config")))
            .into_iter();
        let config_dirs = env_paths("XDG_CONFIG_DIRS", "/etc/xdg");
        let data_home = env_path("XDG_DATA_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")))
            .into_iter();
        let data_dirs = env_paths("XDG_DATA_DIRS", "/usr/local/share:/usr/share");
        Assoc::at(
            config_home.chain(config_dirs).collect(),
            data_home.chain(data_dirs).collect(),
        )
        .with_desktops(desktops(
            std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
        ))
    }

    /// The association files under the given roots, most important
    /// first.
    ///
    /// `config_dirs` are searched for `mimeapps.list`; `data_dirs` for
    /// `applications/mimeapps.list`, `applications/mimeinfo.cache` and
    /// the `.desktop` files themselves. Both are directory roots, not
    /// file paths, so a caller passes `/usr/share` and not
    /// `/usr/share/applications`.
    #[must_use]
    pub fn at(config_dirs: Vec<PathBuf>, data_dirs: Vec<PathBuf>) -> Assoc {
        let apps: Vec<PathBuf> = data_dirs
            .into_iter()
            .map(|d| d.join("applications"))
            .collect();
        // The data directories carry a `mimeapps.list` too, ranked below
        // every config one: it is where a distribution states its
        // defaults, and a user's `~/.config` must outrank it.
        let mut lists = config_dirs;
        lists.extend(apps.iter().cloned());
        Assoc {
            lists,
            apps,
            desktops: Vec::new(),
        }
    }

    /// The same roots, with the desktop names whose
    /// `{desktop}-mimeapps.list` files count (lowercase, most important
    /// first — `$XDG_CURRENT_DESKTOP`'s order).
    ///
    /// Within each directory the spec reads every desktop-specific list
    /// before the plain `mimeapps.list`. So `nitro-mimeapps.list`, which
    /// `just install` puts in a *data* directory, states nitro's
    /// defaults above the distribution's `mimeapps.list` beside it and
    /// every package cache, yet below anything in the user's
    /// `~/.config`.
    #[must_use]
    pub fn with_desktops(mut self, desktops: Vec<String>) -> Assoc {
        self.desktops = desktops;
        self
    }

    /// Every `mimeapps.list` path, most important first.
    fn mimeapps(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for dir in &self.lists {
            for d in &self.desktops {
                out.push(dir.join(format!("{d}-mimeapps.list")));
            }
            out.push(dir.join("mimeapps.list"));
        }
        out
    }

    /// The `.desktop` id registered for `mime`, in the spec's order.
    ///
    /// Every `mimeapps.list`'s `[Default Applications]` first — that is
    /// the user's explicit choice, or the desktop's or distribution's —
    /// then every list's `[Added Associations]`, then, per
    /// `applications` directory in priority order, that directory's
    /// `mimeinfo.cache` followed by the `MimeType=` keys of its
    /// `.desktop` files. The cache is only ever what those keys said
    /// when `update-desktop-database` last ran; reading the keys too
    /// means a `.desktop` file copied into `~/.local/share/applications`
    /// without regenerating the cache still outranks `/usr/share`, as
    /// the directory order says it should. An id
    /// named in a `[Removed Associations]` group is skipped wherever it
    /// would otherwise have been found.
    ///
    /// The spec scopes a removal to the files *below* the one that
    /// states it; this treats a removal as global, which is a
    /// simplification with one visible consequence: a user who removed an
    /// association in `~/.config` cannot have a system file put it back.
    /// That is the direction a user's own file should win in anyway, and
    /// the alternative is a per-file merge whose only observable effect
    /// is the case where two files disagree about a removal.
    ///
    /// A value may list several ids separated by `;`; they are tried in
    /// order, and the first that is not removed wins. Whether the id
    /// resolves to a file that exists is [`Assoc::argv_for`]'s question.
    #[must_use]
    pub fn handler_for(&self, mime: &str) -> Option<String> {
        let lists: Vec<Ini> = self.mimeapps().iter().map(|p| Ini::read(p)).collect();
        let removed: Vec<&str> = lists
            .iter()
            .flat_map(|l| l.values("Removed Associations", mime))
            .collect();
        let pick = |group: &str| -> Option<String> {
            lists
                .iter()
                .flat_map(|l| l.values(group, mime))
                .find(|id| !removed.contains(id))
                .map(str::to_owned)
        };
        pick("Default Applications")
            .or_else(|| pick("Added Associations"))
            .or_else(|| {
                self.apps.iter().find_map(|dir| {
                    let cache = Ini::read(&dir.join("mimeinfo.cache"));
                    cache
                        .values("MIME Cache", mime)
                        .find(|id| !removed.contains(id))
                        .map(str::to_owned)
                        .or_else(|| {
                            declared_in(dir, mime).find(|id| !removed.contains(&id.as_str()))
                        })
                })
            })
    }

    /// A `.desktop` id, resolved to a file and turned into an argv that
    /// opens `path`.
    ///
    /// The id is a file name relative to an `applications` directory.
    /// The spec also allows a `-` in an id to mean a subdirectory
    /// (`kde-konsole.desktop` may live at `kde/konsole.desktop`), so that
    /// is tried too, once, after the plain name — twice `stat`ing a path
    /// is cheaper than missing the application.
    ///
    /// The path is **appended** rather than substituted because
    /// `nitro_launcher::desktop::exec_argv` *strips* the `%f`/`%u` field
    /// codes: it is the launcher's parser, and a launcher opens a program
    /// with no document, so it removes the placeholders rather than
    /// leaving `%U` to be opened as a file called `%U`. Appending gives
    /// the same argv the substitution would have for the overwhelmingly
    /// common `Exec=prog %U` and `Exec=prog %f`; what it gets wrong is an
    /// entry whose field code is not last (`Exec=prog %f --flag`), where
    /// the path lands after the flag instead of before it. Sharing the
    /// launcher's parser — with its `[Desktop Action …]` and `Name[de]`
    /// handling already right and already tested — is worth that.
    ///
    /// `None` when no `applications` directory holds the id, or the file
    /// does not parse as a launchable application entry.
    #[must_use]
    pub fn argv_for(&self, id: &str, path: &Path) -> Option<Vec<String>> {
        let alt = id.replacen('-', "/", 1);
        for dir in &self.apps {
            for candidate in [dir.join(id), dir.join(&alt)] {
                let Ok(text) = std::fs::read_to_string(&candidate) else {
                    continue;
                };
                let Some(entry) = nitro_launcher::desktop::parse(&text, &candidate) else {
                    continue;
                };
                let mut argv = entry.argv;
                argv.push(path.to_string_lossy().into_owned());
                return Some(argv);
            }
        }
        None
    }
}

/// What should open a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Open {
    /// A registered handler, as a ready-to-spawn argv.
    Argv(Vec<String>),
    /// The text fallback: a terminal running an editor on the file.
    ///
    /// A separate case from [`Open::Argv`] even though it is also an
    /// argv, because the caller may want to say something different
    /// about it in the status line ("opening in vi" is worth saying; "
    /// opening in the program you configured" is not) and because it is
    /// the case a future "always ask" prompt would attach to.
    Editor(Vec<String>),
    /// Nothing claims this file.
    ///
    /// Not an error: a `.iso` on a machine with no image mounter is a
    /// file with no handler, and the honest answer is to say so in the
    /// status line rather than to invent one.
    None,
}

/// What opens `path`: type, then handler, then the text fallback.
///
/// The fallback is the useful half of this function. A `text/*` file with
/// no registered handler opens in `term` running `$EDITOR`, or `vi` when
/// that is unset — which is the one program a Unix machine is close to
/// guaranteed to have, and the reason `vi` and not `nano` or the user's
/// taste. It applies to `text/*` only: an editor started on a PDF shows
/// its bytes, which is a worse answer than "nothing opens this".
///
/// The argv is `term -e EDITOR path`, xterm's convention and every
/// terminal emulator's since. `nitro-term` does not honour `-e` yet, so
/// this is the shape that will work the moment it does; today it opens a
/// terminal in which the user can type the command themselves. Recorded
/// in `docs/files.md` under *Limitations*.
#[must_use]
pub fn open_with(path: &Path, globs: &[Glob], assoc: &Assoc, term: &str) -> Open {
    let Some(mime) = type_of(path, globs) else {
        return Open::None;
    };
    if let Some(id) = assoc.handler_for(&mime)
        && let Some(argv) = assoc.argv_for(&id, path)
    {
        return Open::Argv(argv);
    }
    if mime.starts_with("text/") {
        return Open::Editor(editor_argv(
            term,
            path,
            std::env::var("EDITOR").ok().as_deref(),
        ));
    }
    Open::None
}

/// The terminal-plus-editor argv, with `$EDITOR` handed in.
///
/// Split out so the fallback can be tested without setting a variable
/// the whole test binary shares, and so the `vi` default is pinned
/// somewhere rather than being an `unwrap_or` in the middle of a
/// function. An `$EDITOR` that is set but empty counts as unset: an
/// empty program name is not a program.
#[must_use]
pub fn editor_argv(term: &str, path: &Path, editor: Option<&str>) -> Vec<String> {
    let editor = editor
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .unwrap_or("vi");
    vec![
        term.to_owned(),
        "-e".to_owned(),
        editor.to_owned(),
        path.to_string_lossy().into_owned(),
    ]
}

/// The ids of the `.desktop` files directly in `dir` whose
/// `[Desktop Entry]` `MimeType=` lists `mime`, in file-name order.
///
/// What `update-desktop-database` would have written to the directory's
/// `mimeinfo.cache`. Read on every open that gets this far, uncached,
/// for the reason [`Assoc`] gives; an `applications` directory holds a
/// few hundred small files at most. Subdirectories are not walked: the
/// cache step covers the rare vendor-prefixed entry that lives in one.
fn declared_in(dir: &Path, mime: &str) -> impl Iterator<Item = String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".desktop"))
        .collect();
    names.sort();
    let mime = mime.to_owned();
    let dir = dir.to_owned();
    names.into_iter().filter(move |n| {
        Ini::read(&dir.join(n))
            .values("Desktop Entry", "MimeType")
            .any(|m| m == mime)
    })
}

/// `$XDG_CURRENT_DESKTOP` as the list [`Assoc::with_desktops`] takes:
/// split on `:`, lowercased (the spec's file names are lowercase), empty
/// parts dropped.
#[must_use]
pub fn desktops(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// An environment variable as a path, if set and not empty.
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// A `:`-separated environment variable as paths, with the spec's
/// default when it is unset or empty.
fn env_paths(key: &str, default: &str) -> Vec<PathBuf> {
    let value = std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_owned());
    value
        .split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The two-level key-value files this module reads.
///
/// `mimeapps.list` and `mimeinfo.cache` are both desktop-entry-format
/// files: `[Group]` headers and `key=value` lines, where the value is a
/// `;`-separated list. A third copy of the launcher's parser is not
/// needed — this one keeps the whole file rather than four keys of it,
/// which the launcher's does not do and does not want to.
#[derive(Debug, Default)]
struct Ini {
    /// `(group, key, values)`, in file order.
    entries: Vec<(String, String, Vec<String>)>,
}

impl Ini {
    /// Read and parse a file; a missing or unreadable one is empty.
    ///
    /// Missing is the normal case — most machines have two of the four
    /// files this module looks for — so it is not an error and not
    /// reported.
    fn read(path: &Path) -> Ini {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ini::default();
        };
        Ini::parse(&text)
    }

    /// Parse the group/key/value structure.
    fn parse(text: &str) -> Ini {
        let mut entries = Vec::new();
        let mut group = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(g) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                g.trim().clone_into(&mut group);
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let values: Vec<String> = value
                .split(';')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .collect();
            if values.is_empty() {
                continue;
            }
            entries.push((group.clone(), key.trim().to_owned(), values));
        }
        Ini { entries }
    }

    /// The values of `key` in `group`, across every occurrence of it.
    ///
    /// Every occurrence, because a file that repeats a key is malformed
    /// and the forgiving reading — both lists, in order — is the one
    /// that loses nothing.
    fn values<'a>(&'a self, group: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> {
        self.entries
            .iter()
            .filter(move |(g, k, _)| g == group && k == key)
            .flat_map(|(_, _, v)| v.iter().map(String::as_str))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own; see `dir::tests::scratch`.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nitro-files-mime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(path, text).expect("write");
    }

    /// A config root and a data root holding the given files.
    fn assoc_fixture(dir: &Path) -> Assoc {
        Assoc::at(vec![dir.join("config")], vec![dir.join("data")])
    }

    #[test]
    fn the_default_application_wins_over_an_added_one() {
        let dir = scratch("default");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\ntext/plain=chosen.desktop\n\
             [Added Associations]\ntext/plain=other.desktop\n",
        );
        let assoc = assoc_fixture(&dir);
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("chosen.desktop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_config_list_outranks_the_system_list_and_the_cache() {
        let dir = scratch("priority");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\ntext/plain=mine.desktop\n",
        );
        write(
            &dir.join("data/applications/mimeapps.list"),
            "[Default Applications]\ntext/plain=distro.desktop\n",
        );
        write(
            &dir.join("data/applications/mimeinfo.cache"),
            "[MIME Cache]\ntext/plain=package.desktop\n",
        );
        let assoc = assoc_fixture(&dir);
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("mine.desktop")
        );

        // With the user's file gone, the distribution's default wins;
        // with that gone too, the cache answers.
        std::fs::remove_file(dir.join("config/mimeapps.list")).expect("rm");
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("distro.desktop")
        );
        std::fs::remove_file(dir.join("data/applications/mimeapps.list")).expect("rm");
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("package.desktop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_removed_association_is_skipped_wherever_it_is_offered() {
        let dir = scratch("removed");
        write(
            &dir.join("config/mimeapps.list"),
            "[Removed Associations]\ntext/plain=bad.desktop\n",
        );
        write(
            &dir.join("data/applications/mimeapps.list"),
            "[Default Applications]\ntext/plain=bad.desktop;good.desktop\n",
        );
        let assoc = assoc_fixture(&dir);
        // The second id in the list is taken: a removal skips the entry
        // rather than the whole line.
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("good.desktop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_type_nobody_registered_has_no_handler() {
        let dir = scratch("nohandler");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\nimage/png=viewer.desktop\n",
        );
        assert_eq!(assoc_fixture(&dir).handler_for("application/pdf"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_id_resolves_to_an_argv_with_the_path_appended() {
        let dir = scratch("argv");
        write(
            &dir.join("data/applications/viewer.desktop"),
            "[Desktop Entry]\nType=Application\nName=Viewer\nExec=viewer --fullscreen %U\n",
        );
        let assoc = assoc_fixture(&dir);
        let argv = assoc
            .argv_for("viewer.desktop", Path::new("/tmp/a.png"))
            .expect("an argv");
        // `%U` is gone (the launcher's parser strips it) and the path is
        // appended in its place.
        assert_eq!(
            argv,
            vec![
                "viewer".to_owned(),
                "--fullscreen".to_owned(),
                "/tmp/a.png".to_owned()
            ]
        );
        // A dashed id may name a subdirectory.
        write(
            &dir.join("data/applications/kde/konsole.desktop"),
            "[Desktop Entry]\nName=Konsole\nExec=konsole\n",
        );
        assert_eq!(
            assoc
                .argv_for("kde-konsole.desktop", Path::new("/tmp/x"))
                .expect("an argv")[0],
            "konsole"
        );
        // An id nothing on disk answers to is `None`, not a panic.
        assert_eq!(assoc.argv_for("ghost.desktop", Path::new("/tmp/x")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unlaunchable_desktop_file_yields_no_argv() {
        let dir = scratch("unlaunchable");
        // Hidden, and so not a thing to open a file with.
        write(
            &dir.join("data/applications/hidden.desktop"),
            "[Desktop Entry]\nName=X\nExec=x\nHidden=true\n",
        );
        // A link entry has no command.
        write(
            &dir.join("data/applications/link.desktop"),
            "[Desktop Entry]\nType=Link\nName=L\nURL=http://x\n",
        );
        let assoc = assoc_fixture(&dir);
        assert_eq!(assoc.argv_for("hidden.desktop", Path::new("/tmp/x")), None);
        assert_eq!(assoc.argv_for("link.desktop", Path::new("/tmp/x")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_a_registered_file_gives_the_handlers_argv() {
        let dir = scratch("open-argv");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\nimage/png=viewer.desktop\n",
        );
        write(
            &dir.join("data/applications/viewer.desktop"),
            "[Desktop Entry]\nName=Viewer\nExec=viewer %f\n",
        );
        let file = dir.join("shot.png");
        write(&file, "");
        let open = open_with(&file, &[], &assoc_fixture(&dir), "nitro-term");
        assert_eq!(
            open,
            Open::Argv(vec![
                "viewer".to_owned(),
                file.to_string_lossy().into_owned()
            ])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_text_file_with_no_handler_falls_back_to_the_editor() {
        let dir = scratch("open-editor");
        let file = dir.join("notes.txt");
        write(&file, "hello");
        let open = open_with(&file, &[], &assoc_fixture(&dir), "nitro-term");
        let Open::Editor(argv) = open else {
            panic!("a text file with no handler opens in the editor");
        };
        assert_eq!(argv[0], "nitro-term");
        assert_eq!(argv[1], "-e");
        assert_eq!(argv[3], file.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_editor_defaults_to_vi_and_honours_a_set_one() {
        let path = Path::new("/tmp/notes.txt");
        assert_eq!(
            editor_argv("nitro-term", path, None),
            vec![
                "nitro-term".to_owned(),
                "-e".to_owned(),
                "vi".to_owned(),
                "/tmp/notes.txt".to_owned()
            ]
        );
        assert_eq!(editor_argv("nitro-term", path, Some("nvim"))[2], "nvim");
        // Set but empty is not a program name.
        assert_eq!(editor_argv("nitro-term", path, Some("  "))[2], "vi");
    }

    #[test]
    fn a_non_text_file_with_no_handler_opens_nothing() {
        let dir = scratch("open-none");
        let pdf = dir.join("book.pdf");
        write(&pdf, "");
        // An editor on a PDF shows its bytes, which is worse than
        // saying nothing can open it.
        assert_eq!(
            open_with(&pdf, &[], &assoc_fixture(&dir), "nitro-term"),
            Open::None
        );
        // And a file of no known type is `None` as well.
        let odd = dir.join("thing.qqq");
        write(&odd, "");
        assert_eq!(
            open_with(&odd, &[], &assoc_fixture(&dir), "nitro-term"),
            Open::None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_handler_whose_desktop_file_is_missing_falls_through() {
        // A stale `mimeapps.list` naming an uninstalled program must not
        // swallow the text fallback: the user still gets an editor.
        let dir = scratch("stale");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\ntext/plain=uninstalled.desktop\n",
        );
        let file = dir.join("notes.txt");
        write(&file, "");
        assert!(matches!(
            open_with(&file, &[], &assoc_fixture(&dir), "nitro-term"),
            Open::Editor(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ini_file_keeps_group_scope_and_repeated_keys() {
        let ini = Ini::parse("# comment\n[A]\nk=1;2\n[B]\nk=3\n[A]\nk=4\nragged line\nempty=;;\n");
        assert_eq!(
            ini.values("A", "k").collect::<Vec<_>>(),
            vec!["1", "2", "4"]
        );
        assert_eq!(ini.values("B", "k").collect::<Vec<_>>(), vec!["3"]);
        assert_eq!(ini.values("A", "empty").count(), 0);
        assert_eq!(ini.values("C", "k").count(), 0);
    }

    #[test]
    fn a_desktop_files_mime_type_counts_with_no_cache() {
        let dir = scratch("scan");
        write(
            &dir.join("data/applications/b-player.desktop"),
            "[Desktop Entry]\nName=B\nExec=b\nMimeType=audio/ogg;audio/mpeg;\n",
        );
        write(
            &dir.join("data/applications/a-player.desktop"),
            "[Desktop Entry]\nName=A\nExec=a\nMimeType=audio/mpeg;\n\
             [Desktop Action x]\nMimeType=audio/ogg;\n",
        );
        let assoc = assoc_fixture(&dir);
        // Only `[Desktop Entry]`'s key counts, and file-name order breaks
        // a tie.
        assert_eq!(
            assoc.handler_for("audio/ogg").as_deref(),
            Some("b-player.desktop")
        );
        assert_eq!(
            assoc.handler_for("audio/mpeg").as_deref(),
            Some("a-player.desktop")
        );
        assert_eq!(assoc.handler_for("video/mp4"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_user_desktop_file_outranks_a_system_cache() {
        let dir = scratch("scan-order");
        write(
            &dir.join("user/applications/nitro-amp.desktop"),
            "[Desktop Entry]\nName=amp\nExec=nitro-amp %F\nMimeType=audio/ogg;\n",
        );
        write(
            &dir.join("system/applications/mimeinfo.cache"),
            "[MIME Cache]\naudio/ogg=totem.desktop;\n",
        );
        let assoc = Assoc::at(vec![], vec![dir.join("user"), dir.join("system")]);
        assert_eq!(
            assoc.handler_for("audio/ogg").as_deref(),
            Some("nitro-amp.desktop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn within_one_directory_the_cache_comes_before_the_scan() {
        let dir = scratch("cache-first");
        write(
            &dir.join("data/applications/mimeinfo.cache"),
            "[MIME Cache]\naudio/ogg=cached.desktop;\n",
        );
        write(
            &dir.join("data/applications/aaa.desktop"),
            "[Desktop Entry]\nName=A\nExec=a\nMimeType=audio/ogg;\n",
        );
        let assoc = assoc_fixture(&dir);
        assert_eq!(
            assoc.handler_for("audio/ogg").as_deref(),
            Some("cached.desktop")
        );
        // A removed cache entry falls through to the scan.
        write(
            &dir.join("config/mimeapps.list"),
            "[Removed Associations]\naudio/ogg=cached.desktop\n",
        );
        assert_eq!(
            assoc.handler_for("audio/ogg").as_deref(),
            Some("aaa.desktop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_desktop_list_counts_only_on_that_desktop_and_below_the_users() {
        let dir = scratch("desktop-list");
        write(
            &dir.join("data/applications/nitro-mimeapps.list"),
            "[Default Applications]\naudio/ogg=nitro-amp.desktop\n",
        );
        write(
            &dir.join("data/applications/mimeapps.list"),
            "[Default Applications]\naudio/ogg=distro.desktop\n",
        );
        let plain = assoc_fixture(&dir);
        assert_eq!(
            plain.handler_for("audio/ogg").as_deref(),
            Some("distro.desktop")
        );
        let nitro = assoc_fixture(&dir).with_desktops(desktops(Some("Nitro:GNOME")));
        assert_eq!(
            nitro.handler_for("audio/ogg").as_deref(),
            Some("nitro-amp.desktop")
        );
        // The user's own list still wins.
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\naudio/ogg=mine.desktop\n",
        );
        assert_eq!(
            nitro.handler_for("audio/ogg").as_deref(),
            Some("mine.desktop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_desktop_splits_and_lowercases() {
        assert_eq!(desktops(Some("nitro:GNOME")), vec!["nitro", "gnome"]);
        assert_eq!(desktops(Some(" : ")), Vec::<String>::new());
        assert_eq!(desktops(None), Vec::<String>::new());
    }

    #[test]
    fn an_ogg_opens_in_the_player_that_declares_audio_ogg() {
        let dir = scratch("open-ogg");
        write(
            &dir.join("data/applications/nitro-amp.desktop"),
            "[Desktop Entry]\nName=amp\nExec=nitro-amp %F\nMimeType=audio/ogg;\n",
        );
        write(
            &dir.join("data/applications/nitro-video.desktop"),
            "[Desktop Entry]\nName=video\nExec=nitro-video %f\nMimeType=video/x-theora+ogg;\n",
        );
        let globs =
            parse_globs2("50:audio/ogg:*.ogg\n50:video/ogg:*.ogg\n50:video/x-theora+ogg:*.ogg\n");
        let file = dir.join("song.ogg");
        write(&file, "");
        assert_eq!(
            open_with(&file, &globs, &assoc_fixture(&dir), "nitro-term"),
            Open::Argv(vec![
                "nitro-amp".to_owned(),
                file.to_string_lossy().into_owned()
            ])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

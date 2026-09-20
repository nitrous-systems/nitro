//! The sidebar's **places**: the directories a file manager lists on the
//! left so the common ones are one click away.
//!
//! Pure: the list is computed from a home directory, an optional
//! `user-dirs.dirs` and a trash root, and nothing here touches a widget
//! — so it is tested without a display, like [`dir`](crate::dir) and
//! [`trash`](crate::trash).
//!
//! What is listed, in order: **Home**, then the six XDG user directories
//! that exist (Desktop, Documents, Downloads, Music, Pictures, Videos —
//! from `$XDG_CONFIG_HOME/user-dirs.dirs` when it names them, else the
//! conventional `~/Name`), then, after a separator, **Root** (`/`) and
//! **Trash** (the trash's `files/` directory, listed even before it
//! exists, since navigating to a missing directory already reports in
//! the status line). A directory that does not exist is left out rather
//! than shown greyed: a sidebar of places you cannot go is a sidebar of
//! disappointments, and GNOME and macOS both hide them.

use std::path::{Path, PathBuf};

/// Which run of the sidebar a place sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Home and the XDG user directories, under a "Places" header.
    Places,
    /// Root and the trash, after a separator.
    System,
}

/// One sidebar row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// A stable key for the row's addressing name: `places/place_<key>`.
    pub key: &'static str,
    /// What the row says.
    pub label: String,
    /// The symbolic icon it carries (`docs/icons.md`).
    pub icon: &'static str,
    /// Where it goes.
    pub path: PathBuf,
    /// Which run it belongs to.
    pub section: Section,
}

impl Place {
    fn new(
        key: &'static str,
        label: &str,
        icon: &'static str,
        path: PathBuf,
        section: Section,
    ) -> Self {
        Self {
            key,
            label: label.to_owned(),
            icon,
            path,
            section,
        }
    }
}

/// The six XDG user directories: the `user-dirs.dirs` key, the row's
/// key and label, the conventional folder name, and the icon.
///
/// `music-note-beamed` cannot be imported (it mixes fill rules) and
/// `image` spills past the grid, so Music is `headphones` and Pictures
/// is `images` — `docs/icons.md` records both substitutions.
const XDG_DIRS: [(&str, &str, &str, &str); 6] = [
    ("XDG_DESKTOP_DIR", "desktop", "Desktop", "folder-fill"),
    ("XDG_DOCUMENTS_DIR", "documents", "Documents", "folder-fill"),
    ("XDG_DOWNLOAD_DIR", "downloads", "Downloads", "download"),
    ("XDG_MUSIC_DIR", "music", "Music", "headphones"),
    ("XDG_PICTURES_DIR", "pictures", "Pictures", "images"),
    ("XDG_VIDEOS_DIR", "videos", "Videos", "film"),
];

/// Parse a `user-dirs.dirs` file: `XDG_X_DIR="$HOME/Name"` lines, with
/// `$HOME` expanded against `home`. Comments and anything unparseable
/// are skipped, which is what `xdg-user-dir` does too.
#[must_use]
pub fn parse_user_dirs(text: &str, home: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !key.starts_with("XDG_") || !key.ends_with("_DIR") {
            continue;
        }
        let value = value.trim().trim_matches('"');
        let path = if let Some(rest) = value.strip_prefix("$HOME") {
            home.join(rest.trim_start_matches('/'))
        } else if value.starts_with('/') {
            PathBuf::from(value)
        } else {
            continue;
        };
        out.push((key.to_owned(), path));
    }
    out
}

/// The places for `home` (when there is one), using `user_dirs` — the
/// parsed `user-dirs.dirs`, or empty — and `trash_files` for the trash
/// row. Only directories that exist are listed, except the trash.
#[must_use]
pub fn places(
    home: Option<&Path>,
    user_dirs: &[(String, PathBuf)],
    trash_files: &Path,
) -> Vec<Place> {
    let mut out = Vec::new();
    if let Some(home) = home.filter(|h| h.is_dir()) {
        out.push(Place::new(
            "home",
            "Home",
            "house",
            home.to_path_buf(),
            Section::Places,
        ));
        for (var, key, name, icon) in XDG_DIRS {
            let path = user_dirs
                .iter()
                .find(|(k, _)| k == var)
                .map_or_else(|| home.join(name), |(_, p)| p.clone());
            // `$HOME` itself is what `xdg-user-dirs` writes for a
            // directory the user removed; it is Home already.
            if path == home || !path.is_dir() {
                continue;
            }
            out.push(Place::new(key, name, icon, path, Section::Places));
        }
    }
    out.push(Place::new(
        "root",
        "Root",
        "hdd",
        PathBuf::from("/"),
        Section::System,
    ));
    out.push(Place::new(
        "trash",
        "Trash",
        "trash3",
        trash_files.to_path_buf(),
        Section::System,
    ));
    out
}

/// The places for the real environment: `$HOME`,
/// `$XDG_CONFIG_HOME/user-dirs.dirs` (or `~/.config/user-dirs.dirs`),
/// and the trash's `files/`.
#[must_use]
pub fn from_env(trash_root: &Path) -> Vec<Place> {
    let home = nitro_launcher::spawn::home_dir();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let user_dirs = match (&home, config) {
        (Some(home), Some(config)) => std::fs::read_to_string(config.join("user-dirs.dirs"))
            .map(|t| parse_user_dirs(&t, home))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    places(home.as_deref(), &user_dirs, &trash_root.join("files"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nitro-places-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");
        root
    }

    #[test]
    fn user_dirs_expand_home_and_skip_what_they_cannot_read() {
        let home = Path::new("/home/x");
        let text = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"/mnt/dl\"\n\
                    XDG_MUSIC_DIR=\"relative\"\nnot a key\nFOO=\"$HOME/bar\"\n";
        let got = parse_user_dirs(text, home);
        assert_eq!(
            got,
            vec![
                (
                    "XDG_DESKTOP_DIR".to_owned(),
                    PathBuf::from("/home/x/Desktop")
                ),
                ("XDG_DOWNLOAD_DIR".to_owned(), PathBuf::from("/mnt/dl")),
            ]
        );
    }

    #[test]
    fn only_directories_that_exist_are_listed_and_the_order_is_stable() {
        let root = scratch("places");
        let home = root.join("home");
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        std::fs::create_dir_all(home.join("dl")).unwrap();
        // A file, not a directory: not a place.
        std::fs::write(home.join("Pictures"), "x").unwrap();
        let user_dirs = vec![
            ("XDG_DOWNLOAD_DIR".to_owned(), home.join("dl")),
            // Removed by the user: `xdg-user-dirs` writes `$HOME` back.
            ("XDG_VIDEOS_DIR".to_owned(), home.clone()),
        ];
        let trash = root.join("Trash/files");
        let got = places(Some(&home), &user_dirs, &trash);
        let keys: Vec<&str> = got.iter().map(|p| p.key).collect();
        assert_eq!(keys, ["home", "documents", "downloads", "root", "trash"]);
        assert_eq!(got[2].path, home.join("dl"), "user-dirs wins over the name");
        assert_eq!(got[2].label, "Downloads");
        assert_eq!(got[4].path, trash, "the trash is listed before it exists");
        assert!(got[..3].iter().all(|p| p.section == Section::Places));
        assert!(got[3..].iter().all(|p| p.section == Section::System));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_home_means_root_and_trash_only() {
        let got = places(None, &[], Path::new("/nowhere/files"));
        let keys: Vec<&str> = got.iter().map(|p| p.key).collect();
        assert_eq!(keys, ["root", "trash"]);
        let got = places(Some(Path::new("/does/not/exist")), &[], Path::new("/t"));
        assert_eq!(got.len(), 2);
    }
}

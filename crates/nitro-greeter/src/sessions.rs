//! What the greeter can start: nitro first, then the machine's
//! `/usr/share/wayland-sessions/*.desktop` entries.
//!
//! The `.desktop` reader is `nitro-launcher`'s (`docs/greeter.md`,
//! decision 4): the same `Exec=` splitting and field-code removal, and
//! the same limits.

use std::path::{Path, PathBuf};

/// Where display managers look for Wayland sessions.
pub const WAYLAND_SESSIONS: &str = "/usr/share/wayland-sessions";

/// One session the user can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// What the picker shows, and what the state file remembers.
    pub name: String,
    /// The command greetd runs as the user.
    pub cmd: Vec<String>,
    /// `XDG_SESSION_DESKTOP`: the file's basename, `nitro` for ours.
    pub desktop: String,
    /// `XDG_CURRENT_DESKTOP`: `DesktopNames=` in spirit; the name
    /// `nitro` for ours, the basename otherwise.
    pub current_desktop: String,
}

impl SessionEntry {
    /// The environment sent with `start_session`.
    #[must_use]
    pub fn env(&self) -> Vec<String> {
        vec![
            format!("XDG_SESSION_DESKTOP={}", self.desktop),
            format!("XDG_CURRENT_DESKTOP={}", self.current_desktop),
            "XDG_SESSION_TYPE=wayland".to_owned(),
        ]
    }
}

/// Where `systemd-cat` is looked for.
const SYSTEMD_CAT: &str = "/usr/bin/systemd-cat";

/// The built-in entry: `nitro-session` next to this executable (an
/// install puts them in one `$BINDIR`, the box in `~/nitro-bin`), else
/// on `PATH`.
#[must_use]
pub fn builtin() -> SessionEntry {
    let program = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("nitro-session")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("nitro-session"));
    let cat = Path::new(SYSTEMD_CAT);
    SessionEntry {
        name: "Nitro".to_owned(),
        cmd: builtin_cmd(&program, cat.is_file().then_some(cat)),
        desktop: "nitro".to_owned(),
        current_desktop: "nitro".to_owned(),
    }
}

/// The built-in command. greetd gives the session the VT as its stdio,
/// where nothing can read the log once the compositor has the screen,
/// so with `systemd-cat` present the session's log goes to the journal
/// under `nitro-session`, as it does under `nitro-dev.service`.
fn builtin_cmd(session: &Path, systemd_cat: Option<&Path>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(c) = systemd_cat {
        v.push(c.display().to_string());
        v.push("--identifier=nitro-session".to_owned());
    }
    v.push(session.display().to_string());
    v
}

/// `builtin` first, then every parseable `*.desktop` in `dir`, sorted by
/// file name. A file whose `Exec` runs `nitro-session` is skipped: it is
/// the built-in, which knows where the deployed binary is.
#[must_use]
pub fn sessions_in(dir: &Path, builtin: SessionEntry) -> Vec<SessionEntry> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "desktop"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let mut out = vec![builtin];
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let Some(e) = nitro_launcher::desktop::parse(&text, &f) else {
            continue;
        };
        let prog = Path::new(e.program())
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if prog == "nitro-session" {
            continue;
        }
        let base = f
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        out.push(SessionEntry {
            name: e.name,
            cmd: e.argv,
            current_desktop: base.clone(),
            desktop: base,
        });
    }
    out
}

/// The machine's sessions.
#[must_use]
pub fn sessions() -> Vec<SessionEntry> {
    sessions_in(Path::new(WAYLAND_SESSIONS), builtin())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nitro-greeter-sessions-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn nitro() -> SessionEntry {
        SessionEntry {
            name: "Nitro".into(),
            cmd: vec!["/opt/nitro-session".into()],
            desktop: "nitro".into(),
            current_desktop: "nitro".into(),
        }
    }

    #[test]
    fn the_builtin_comes_first_then_the_files_sorted() {
        let d = dir("sorted");
        std::fs::write(
            d.join("sway.desktop"),
            "[Desktop Entry]\nName=Sway\nExec=sway --unsupported-gpu\nType=Application\n",
        )
        .unwrap();
        std::fs::write(
            d.join("labwc.desktop"),
            "[Desktop Entry]\nName=labwc\nExec=labwc\n",
        )
        .unwrap();
        std::fs::write(d.join("broken.desktop"), "no group here\n").unwrap();
        std::fs::write(d.join("README"), "[Desktop Entry]\nName=x\nExec=x\n").unwrap();
        std::fs::write(
            d.join("nitro.desktop"),
            "[Desktop Entry]\nName=Nitro\nExec=/usr/local/bin/nitro-session\n",
        )
        .unwrap();
        let got = sessions_in(&d, nitro());
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Nitro", "labwc", "Sway"]);
        assert_eq!(got[0].cmd, ["/opt/nitro-session"], "ours, not the file's");
        assert_eq!(got[2].cmd, ["sway", "--unsupported-gpu"]);
        assert_eq!(got[2].desktop, "sway");
        assert!(
            got[2]
                .env()
                .contains(&"XDG_SESSION_DESKTOP=sway".to_owned())
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_builtin_logs_to_the_journal_when_it_can() {
        let s = Path::new("/opt/nitro-session");
        assert_eq!(builtin_cmd(s, None), ["/opt/nitro-session"]);
        assert_eq!(
            builtin_cmd(s, Some(Path::new("/usr/bin/systemd-cat"))),
            [
                "/usr/bin/systemd-cat",
                "--identifier=nitro-session",
                "/opt/nitro-session"
            ]
        );
    }

    #[test]
    fn no_directory_is_just_the_builtin() {
        let got = sessions_in(Path::new("/nonexistent/wayland-sessions"), nitro());
        assert_eq!(got, [nitro()]);
    }
}

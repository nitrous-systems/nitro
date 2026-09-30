//! The remembered defaults: the last user and session.
//!
//! `/var/cache/nitro-greeter/state` (`NITRO_GREETER_STATE` overrides it),
//! two lines, `user=…` and `session=…`. Read and written best-effort:
//! losing the file only loses a default (`docs/greeter.md`, decision 4).

use std::path::PathBuf;

/// The default location. Must be writable by the greeter user.
pub const DEFAULT_PATH: &str = "/var/cache/nitro-greeter/state";

/// What the greeter remembers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Remembered {
    /// The last user who logged in.
    pub user: Option<String>,
    /// The name of the session they picked.
    pub session: Option<String>,
}

impl Remembered {
    /// Parse the file's text; unknown lines and garbage are ignored.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut r = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            if v.is_empty() || v.chars().any(char::is_control) {
                continue;
            }
            match k.trim() {
                "user" => r.user = Some(v.to_owned()),
                "session" => r.session = Some(v.to_owned()),
                _ => {}
            }
        }
        r
    }

    /// The file's text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::new();
        for (k, v) in [("user", &self.user), ("session", &self.session)] {
            if let Some(v) = v.as_deref().filter(|v| !v.contains('\n')) {
                s.push_str(k);
                s.push('=');
                s.push_str(v);
                s.push('\n');
            }
        }
        s
    }
}

/// Where the state lives.
#[must_use]
pub fn path() -> PathBuf {
    std::env::var_os("NITRO_GREETER_STATE")
        .filter(|p| !p.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_PATH), PathBuf::from)
}

/// Read the state; nothing if it is missing or unreadable.
#[must_use]
pub fn load() -> Remembered {
    std::fs::read_to_string(path())
        .map(|t| Remembered::parse(&t))
        .unwrap_or_default()
}

/// Write the state to `p`; a failure is a warning.
pub fn save(p: &std::path::Path, r: &Remembered) {
    let tmp = p.with_extension("tmp");
    let res = std::fs::write(&tmp, r.render()).and_then(|()| std::fs::rename(&tmp, p));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("nitro-greeter: remembering the login in {}: {e}", p.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_round_trips() {
        let r = Remembered {
            user: Some("alice".into()),
            session: Some("Nitro".into()),
        };
        assert_eq!(Remembered::parse(&r.render()), r);
        assert_eq!(Remembered::default().render(), "");
    }

    #[test]
    fn save_then_load_through_a_file() {
        let d = std::env::temp_dir().join(format!("nitro-greeter-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("state");
        let r = Remembered {
            user: Some("alice".into()),
            session: None,
        };
        save(&p, &r);
        assert_eq!(Remembered::parse(&std::fs::read_to_string(&p).unwrap()), r);
        // An unwritable place is a warning, not a panic.
        save(std::path::Path::new("/nonexistent/dir/state"), &r);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn garbage_is_ignored() {
        let r = Remembered::parse("\u{0}\u{1}junk\nuser=\nsession=Sway\nfoo=bar\nuser=bob\n");
        assert_eq!(
            r,
            Remembered {
                user: Some("bob".into()),
                session: Some("Sway".into())
            }
        );
    }
}

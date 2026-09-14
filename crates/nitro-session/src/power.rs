//! The session socket: `$XDG_RUNTIME_DIR/nitro/session.sock`, a line
//! protocol, and the power actions behind it.
//!
//! # Why not D-Bus
//!
//! `DESIGN.md` says session policy that talks to logind lives in one side
//! daemon and is "the only place a D-Bus client is allowed". This is that
//! daemon, and it still does not speak D-Bus — the *permission* was
//! spent, not the requirement. Three reasons, in order of weight:
//!
//! 1. **`zbus` is ~40 crates.** The whole tree is 35 distinct external
//!    crates today (`DEPENDENCIES.md`). Doubling it to send four method
//!    calls a user makes twice a day is the single worst dependency trade
//!    available in this repo.
//! 2. **`systemctl` is already there.** It is part of the same systemd
//!    that owns logind; a box that has logind has it. `systemctl suspend`
//!    *is* a logind call — it goes to `org.freedesktop.login1.Manager`,
//!    with the same polkit check, the same inhibitor handling and the
//!    same "another session is active" refusal. We are not avoiding
//!    logind; we are letting the tool that ships with it do the IPC.
//! 3. **The thing D-Bus would buy is not wanted yet.** What a real
//!    client gets over `systemctl` is *events*: `PrepareForSleep`,
//!    `Lock`/`Unlock` signals, idle hints, and an inhibitor fd held
//!    across a suspend so a lock screen can paint before the machine goes
//!    down. Every one of those is M4 work — and M4 is when
//!    [`Command::Lock`] stops returning `err`. **When a lock screen needs
//!    to paint before suspend, this decision gets revisited**, because
//!    that is the first requirement `systemctl` genuinely cannot meet.
//!
//! The cost, recorded honestly: a `systemctl suspend` is a fork, an exec
//! and a D-Bus round trip inside someone else's process (~20 ms rather
//! than ~2 ms), it can fail for reasons we can only report as text, and
//! the session cannot be told the machine is *about to* sleep.
//!
//! # The protocol
//!
//! One request per line, ASCII, `\n`-terminated; the reply is `ok\n` or
//! `err <reason>\n`. It is `nitro-server`'s control socket again
//! (`crates/nitro-server/src/protocol.rs`) down to the `MAX_LINE`
//! overflow rule, because the bar will eventually speak both and a
//! desktop with two hand-rolled line protocols that differ in their
//! details is a desktop with one of them written wrong.
//!
//! | request | effect |
//! |---|---|
//! | `lock` | M4. Answers `err not implemented …` today. |
//! | `suspend` | `systemctl suspend` |
//! | `poweroff` | `systemctl poweroff` |
//! | `reboot` | `systemctl reboot` |
//! | `logout` | orderly teardown of the session, exit 0 |
//! | `status` | `ok` + one `name pid` line per running piece + a blank line |
//!
//! `status` is not in the spec and is two lines of code: it is what makes
//! "is the bar actually running, or is it restarting?" answerable over
//! ssh without parsing `ps`, and it is the only request that has a body.

use std::fmt;

/// Longest request line accepted, matching the server's control socket.
pub const MAX_LINE: usize = 4096;

/// A parsed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Lock the session. M4; refused for now.
    Lock,
    /// Suspend the machine.
    Suspend,
    /// Power the machine off.
    PowerOff,
    /// Reboot the machine.
    Reboot,
    /// End the session: teardown, exit 0.
    Logout,
    /// Report what is running.
    Status,
}

impl Command {
    /// Parse one request line. Unknown verbs and trailing arguments are
    /// both errors: a command with an argument is a command from a
    /// client that thinks this protocol is a different one.
    ///
    /// # Errors
    /// [`ParseError`] for a blank line, an unknown verb, or a known verb
    /// with anything after it.
    pub fn parse(line: &str) -> Result<Self, ParseError> {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        let Some(verb) = parts.next() else {
            return Err(ParseError::Empty);
        };
        if parts.next().is_some() {
            return Err(ParseError::Arguments(verb.to_owned()));
        }
        match verb {
            "lock" => Ok(Self::Lock),
            "suspend" => Ok(Self::Suspend),
            "poweroff" => Ok(Self::PowerOff),
            "reboot" => Ok(Self::Reboot),
            "logout" => Ok(Self::Logout),
            "status" => Ok(Self::Status),
            other => Err(ParseError::Unknown(other.to_owned())),
        }
    }

    /// The `systemctl` verb this command runs, if any.
    #[must_use]
    pub fn systemctl_verb(self) -> Option<&'static str> {
        match self {
            Self::Suspend => Some("suspend"),
            Self::PowerOff => Some("poweroff"),
            Self::Reboot => Some("reboot"),
            Self::Lock | Self::Logout | Self::Status => None,
        }
    }
}

/// Why a request line was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A blank line.
    Empty,
    /// A verb this protocol does not have.
    Unknown(String),
    /// A known verb with something after it.
    Arguments(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty request"),
            Self::Unknown(v) => write!(f, "unknown request {v:?}"),
            Self::Arguments(v) => write!(f, "{v} takes no arguments"),
        }
    }
}

/// A reply, ready to be written.
#[must_use]
pub fn ok() -> Vec<u8> {
    b"ok\n".to_vec()
}

/// An `err <reason>` reply.
///
/// The reason is flattened to one line: a `systemctl` failure can be
/// several, and a line protocol whose error message contains a newline
/// is a line protocol with a framing bug.
#[must_use]
pub fn err(reason: &str) -> Vec<u8> {
    let flat: String = reason
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    format!("err {}\n", flat.trim()).into_bytes()
}

/// The `status` body: `ok\n`, one `name pid\n` per piece, a blank line.
#[must_use]
pub fn status_reply(pieces: &[(String, Option<u32>)]) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut out = String::from("ok\n");
    for (name, pid) in pieces {
        match pid {
            // Writing into a `String` cannot fail.
            Some(pid) => {
                let _ = writeln!(out, "{name} {pid}");
            }
            None => {
                let _ = writeln!(out, "{name} -");
            }
        }
    }
    out.push('\n');
    out.into_bytes()
}

/// Run `systemctl <verb>` and wait for it.
///
/// Synchronous on purpose. `systemctl suspend` returns as soon as logind
/// has *accepted* the request, not when the machine wakes up, so this
/// blocks for a D-Bus round trip and nothing more — and blocking is what
/// lets the answer on the socket be the real one instead of an
/// unconditional `ok`. A caller who is told `ok` by a session that never
/// checked has no way to find out that polkit said no.
///
/// # Errors
/// The text of whatever went wrong, for an `err` line.
pub fn run_systemctl(verb: &str) -> Result<(), String> {
    let out = std::process::Command::new("systemctl")
        .arg(verb)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("systemctl {verb}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let reason = stderr.trim();
    if reason.is_empty() {
        Err(format!("systemctl {verb} failed: {}", out.status))
    } else {
        Err(format!("systemctl {verb}: {reason}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_verb_parses_and_nothing_else_does() {
        assert_eq!(Command::parse("lock"), Ok(Command::Lock));
        assert_eq!(Command::parse("suspend"), Ok(Command::Suspend));
        assert_eq!(Command::parse("poweroff"), Ok(Command::PowerOff));
        assert_eq!(Command::parse("reboot"), Ok(Command::Reboot));
        assert_eq!(Command::parse("logout"), Ok(Command::Logout));
        assert_eq!(Command::parse("status"), Ok(Command::Status));
        assert_eq!(Command::parse("  reboot \r\n"), Ok(Command::Reboot));

        assert_eq!(Command::parse(""), Err(ParseError::Empty));
        assert_eq!(
            Command::parse("halt"),
            Err(ParseError::Unknown("halt".to_owned()))
        );
        // A verb with an argument is refused rather than being taken as
        // the verb: `poweroff now` from a client that thinks this is a
        // shell must not power the box off.
        assert_eq!(
            Command::parse("poweroff now"),
            Err(ParseError::Arguments("poweroff".to_owned()))
        );
    }

    /// The mapping onto `systemctl`, spelled out: this is the table a
    /// reader checks when they want to know what `poweroff` really does,
    /// and a typo here is a reboot when the user asked for a shutdown.
    #[test]
    fn the_systemctl_verbs_are_exactly_these_three() {
        assert_eq!(Command::Suspend.systemctl_verb(), Some("suspend"));
        assert_eq!(Command::PowerOff.systemctl_verb(), Some("poweroff"));
        assert_eq!(Command::Reboot.systemctl_verb(), Some("reboot"));
        assert_eq!(Command::Lock.systemctl_verb(), None);
        assert_eq!(Command::Logout.systemctl_verb(), None);
        assert_eq!(Command::Status.systemctl_verb(), None);
    }

    #[test]
    fn an_error_reply_is_always_exactly_one_line() {
        let multi = err("Failed to suspend:\nInteractive authentication required.\n");
        assert_eq!(
            String::from_utf8(multi).unwrap(),
            "err Failed to suspend: Interactive authentication required.\n"
        );
        assert_eq!(String::from_utf8(ok()).unwrap(), "ok\n");
    }

    #[test]
    fn the_status_body_ends_in_a_blank_line() {
        let body = status_reply(&[
            ("nitro-server".to_owned(), Some(42)),
            ("nitro-bar".to_owned(), None),
        ]);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "ok\nnitro-server 42\nnitro-bar -\n\n"
        );
    }

    /// `systemctl` failures are reported rather than swallowed. Asked of
    /// a unit that cannot exist, so the test says nothing about whether
    /// the machine has systemd beyond "the binary is or is not there".
    #[test]
    fn a_failing_systemctl_is_reported() {
        let e = run_systemctl("no-such-verb-ever").expect_err("not a systemctl verb");
        assert!(e.contains("systemctl"), "{e}");
        assert!(!e.contains('\n'), "the reason is one line: {e}");
    }
}

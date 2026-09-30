//! Who the conversation is with.
//!
//! [`Backend`] is the seam between the pure state machine and a process
//! that runs PAM. Lock mode uses [`AuthHelper`], which spawns
//! `nitro-auth` and speaks to it over pipes. The greeter (plan step 5 in
//! `docs/greeter.md`) will add a second implementation over greetd's
//! `$GREETD_SOCK` `UnixStream`: the same codec ([`nitro_login::ipc`]),
//! the same non-blocking reads, a different descriptor.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use nitro_login::ipc::{self, Framer};
use nitro_login::{Request, Response};

/// An authenticator the greeter can talk to.
pub trait Backend {
    /// Send one request. Starts the authenticator if it is not running.
    ///
    /// # Errors
    /// The authenticator could not be started or has gone away.
    fn send(&mut self, req: &Request) -> io::Result<()>;

    /// The descriptor to watch for responses, while connected.
    fn fd(&self) -> Option<BorrowedFd<'_>>;

    /// Every complete response that has arrived, without blocking
    /// (`Ok(vec![])` when nothing has).
    ///
    /// # Errors
    /// End of stream or a malformed frame: the authenticator is gone,
    /// and is restarted by the next [`Backend::send`]. The caller must
    /// stop watching the old [`Backend::fd`].
    fn read(&mut self) -> io::Result<Vec<Response>>;

    /// Hang up: the conversation is over. For the helper, closing its
    /// stdin is its signal to exit, and it is waited for.
    fn close(&mut self) {}
}

/// A running `nitro-auth`.
struct Running {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    framer: Framer,
}

/// `nitro-auth`, spawned on first use and again after it died.
pub struct AuthHelper {
    program: PathBuf,
    running: Option<Running>,
}

impl AuthHelper {
    /// The helper at `program`.
    #[must_use]
    pub fn new(program: PathBuf) -> Self {
        Self {
            program,
            running: None,
        }
    }

    /// `$NITRO_AUTH` if set; else `nitro-auth` next to this executable
    /// (an install puts them in one `$BINDIR`, a build in one
    /// `target/<profile>`); else `nitro-auth` on `PATH`.
    #[must_use]
    pub fn locate() -> PathBuf {
        if let Some(p) = std::env::var_os("NITRO_AUTH").filter(|p| !p.is_empty()) {
            return PathBuf::from(p);
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            let beside = dir.join("nitro-auth");
            if beside.is_file() {
                return beside;
            }
        }
        PathBuf::from("nitro-auth")
    }

    fn start(&mut self) -> io::Result<&mut Running> {
        if self.running.is_none() {
            let mut child = Command::new(&self.program)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;
            let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other("no pipes to the helper"));
            };
            let flags = rustix::fs::fcntl_getfl(&stdout)?;
            rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK)?;
            self.running = Some(Running {
                child,
                stdin,
                stdout,
                framer: Framer::new(),
            });
        }
        self.running
            .as_mut()
            .ok_or_else(|| io::Error::other("helper not running"))
    }

    /// Forget the helper, reaping it. Dropping its stdin is what tells a
    /// live one to exit.
    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            let Running {
                mut child, stdin, ..
            } = r;
            drop(stdin);
            let _ = child.wait();
        }
    }
}

impl Backend for AuthHelper {
    fn send(&mut self, req: &Request) -> io::Result<()> {
        let mut frame = ipc::encode_request(req);
        let res = self.start().and_then(|r| r.stdin.write_all(&frame));
        ipc::wipe(&mut frame);
        if res.is_err() {
            self.stop();
        }
        res
    }

    fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.running.as_ref().map(|r| r.stdout.as_fd())
    }

    fn read(&mut self) -> io::Result<Vec<Response>> {
        let Some(r) = self.running.as_mut() else {
            return Ok(Vec::new());
        };
        let mut buf = [0u8; 4096];
        let mut eof = false;
        loop {
            match r.stdout.read(&mut buf) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => r.framer.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.stop();
                    return Err(e);
                }
            }
        }
        let mut out = Vec::new();
        loop {
            match r.framer.next_frame() {
                Ok(Some(body)) => match ipc::decode_response(&body) {
                    Ok(resp) => out.push(resp),
                    Err(e) => {
                        self.stop();
                        return Err(io::Error::new(io::ErrorKind::InvalidData, e));
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    self.stop();
                    return Err(io::Error::new(io::ErrorKind::InvalidData, e));
                }
            }
        }
        if eof {
            // Frames that arrived before the end are still delivered;
            // the caller sees `fd()` gone and stops watching.
            self.stop();
            if out.is_empty() {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
        }
        Ok(out)
    }

    fn close(&mut self) {
        self.stop();
    }
}

impl Drop for AuthHelper {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_read(b: &mut AuthHelper) -> io::Result<Vec<Response>> {
        for _ in 0..500 {
            match b.read() {
                Ok(v) if v.is_empty() && b.fd().is_some() => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                other => return other,
            }
        }
        panic!("the helper never answered");
    }

    #[test]
    fn a_helper_that_exits_is_an_error_and_is_respawned_on_the_next_send() {
        let mut b = AuthHelper::new(PathBuf::from("/bin/true"));
        assert!(b.fd().is_none(), "spawned lazily");
        let _ = b.send(&Request::CancelSession);
        assert!(wait_read(&mut b).is_err());
        assert!(b.fd().is_none());
        let _ = b.send(&Request::CancelSession);
        assert!(b.fd().is_some(), "respawned");
    }

    #[test]
    fn frames_come_back_through_the_pipes() {
        // `cat` echoes the request frame: not a response, so it is a
        // protocol error — which is what proves the bytes made the
        // round trip through the framer.
        let mut b = AuthHelper::new(PathBuf::from("cat"));
        b.send(&Request::CancelSession).unwrap();
        let err = wait_read(&mut b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_missing_helper_fails_to_send() {
        let mut b = AuthHelper::new(PathBuf::from("/nonexistent/nitro-auth"));
        assert!(b.send(&Request::CancelSession).is_err());
    }
}

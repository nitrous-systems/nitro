//! The greetd backend: greetd's IPC socket, `$GREETD_SOCK`.
//!
//! The codec is [`nitro_login::ipc`], shared with `nitro-auth`; this is
//! only the transport. The stream is connected on the first
//! [`Backend::send`] and again after it was lost. Writes stay
//! **blocking**: frames are a few hundred bytes to a local root daemon
//! that reads them at once. Reads never block: `recv(DONTWAIT)` in a
//! loop, from the loop's fd hook, because greetd answers
//! `create_session` only once PAM asks its first question, which on a
//! fingerprint stack takes seconds, and the UI keeps painting meanwhile.

use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use nitro_login::ipc::{self, Framer};
use nitro_login::{Request, Response};
use rustix::net::RecvFlags;

use crate::backend::Backend;

/// The variable greetd sets for its greeter.
pub const SOCK_ENV: &str = "GREETD_SOCK";

/// A connection to greetd.
#[derive(Debug)]
pub struct Greetd {
    path: PathBuf,
    stream: Option<UnixStream>,
    framer: Framer,
}

impl Greetd {
    /// greetd at `path`; nothing is connected yet.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            stream: None,
            framer: Framer::new(),
        }
    }

    /// greetd at `$GREETD_SOCK`.
    ///
    /// # Errors
    /// The variable is unset or empty: this is not running under greetd.
    pub fn from_env() -> Result<Self, String> {
        std::env::var_os(SOCK_ENV)
            .filter(|p| !p.is_empty())
            .map(|p| Self::new(PathBuf::from(p)))
            .ok_or_else(|| format!("${SOCK_ENV} is not set: greeter mode runs under greetd"))
    }

    fn connect(&mut self) -> io::Result<&mut UnixStream> {
        if self.stream.is_none() {
            self.stream = Some(UnixStream::connect(&self.path)?);
            self.framer = Framer::new();
        }
        self.stream
            .as_mut()
            .ok_or_else(|| io::Error::other("not connected"))
    }

    fn drop_stream(&mut self) {
        self.stream = None;
        self.framer = Framer::new();
    }
}

impl Backend for Greetd {
    fn send(&mut self, req: &Request) -> io::Result<()> {
        use std::io::Write as _;
        let mut frame = ipc::encode_request(req);
        let res = self.connect().and_then(|s| s.write_all(&frame));
        ipc::wipe(&mut frame);
        if res.is_err() {
            self.drop_stream();
        }
        res
    }

    fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.stream.as_ref().map(AsFd::as_fd)
    }

    fn read(&mut self) -> io::Result<Vec<Response>> {
        let Some(stream) = self.stream.as_ref() else {
            return Ok(Vec::new());
        };
        let mut buf = [0u8; 4096];
        let mut eof = false;
        loop {
            match rustix::net::recv(stream, &mut buf, RecvFlags::DONTWAIT) {
                Ok((0, _)) => {
                    eof = true;
                    break;
                }
                Ok((n, _)) => self.framer.push(&buf[..n]),
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => {
                    self.drop_stream();
                    return Err(e.into());
                }
            }
        }
        let mut out = Vec::new();
        loop {
            match self.framer.next_frame() {
                Ok(Some(body)) => match ipc::decode_response(&body) {
                    Ok(r) => out.push(r),
                    Err(e) => {
                        self.drop_stream();
                        return Err(io::Error::new(io::ErrorKind::InvalidData, e));
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    self.drop_stream();
                    return Err(io::Error::new(io::ErrorKind::InvalidData, e));
                }
            }
        }
        if eof {
            self.drop_stream();
            if out.is_empty() {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
        }
        Ok(out)
    }

    fn close(&mut self) {
        self.drop_stream();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixListener;

    use nitro_login::ErrorKind;

    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir()
                .join(format!("nitro-greeter-greetd-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake(tag: &str) -> (Dir, UnixListener, Greetd) {
        let d = Dir::new(tag);
        let path = d.0.join("greetd.sock");
        let l = UnixListener::bind(&path).unwrap();
        (d, l, Greetd::new(path))
    }

    /// Read one request frame from the fake greetd's side.
    fn recv_request(s: &mut UnixStream) -> Request {
        let mut head = [0u8; 4];
        s.read_exact(&mut head).unwrap();
        let mut body = vec![0u8; u32::from_ne_bytes(head) as usize];
        s.read_exact(&mut body).unwrap();
        ipc::decode_request(&body).unwrap()
    }

    fn wait_read(g: &mut Greetd, want: usize) -> io::Result<Vec<Response>> {
        let mut got = Vec::new();
        for _ in 0..500 {
            got.extend(g.read()?);
            if got.len() >= want {
                return Ok(got);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("greetd never answered; got {got:?}");
    }

    #[test]
    fn requests_arrive_framed() {
        let (_d, l, mut g) = fake("framed");
        assert!(g.fd().is_none(), "connected lazily");
        g.send(&Request::CreateSession {
            username: "al\"ice".into(),
        })
        .unwrap();
        let (mut s, _) = l.accept().unwrap();
        assert_eq!(
            recv_request(&mut s),
            Request::CreateSession {
                username: "al\"ice".into()
            }
        );
        g.send(&Request::StartSession {
            cmd: vec!["nitro-session".into()],
            env: vec!["XDG_SESSION_DESKTOP=nitro".into()],
        })
        .unwrap();
        assert!(matches!(recv_request(&mut s), Request::StartSession { .. }));
    }

    #[test]
    fn a_split_response_is_reassembled_and_two_in_one_write_both_arrive() {
        let (_d, l, mut g) = fake("split");
        g.send(&Request::CancelSession).unwrap();
        let (mut s, _) = l.accept().unwrap();
        recv_request(&mut s);
        let frame = ipc::encode_response(&Response::Success);
        s.write_all(&frame[..3]).unwrap();
        assert!(g.read().unwrap().is_empty(), "half a frame is nothing yet");
        s.write_all(&frame[3..]).unwrap();
        assert_eq!(wait_read(&mut g, 1).unwrap(), [Response::Success]);

        let mut two = ipc::encode_response(&Response::Error {
            kind: ErrorKind::AuthError,
            description: "no".into(),
        });
        two.extend(ipc::encode_response(&Response::Success));
        s.write_all(&two).unwrap();
        let got = wait_read(&mut g, 2).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], Response::Success);
    }

    #[test]
    fn eof_is_an_error_and_the_next_send_reconnects() {
        let (_d, l, mut g) = fake("eof");
        g.send(&Request::CancelSession).unwrap();
        let (mut s, _) = l.accept().unwrap();
        recv_request(&mut s);
        drop(s);
        assert_eq!(
            wait_read(&mut g, 1).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(g.fd().is_none());
        g.send(&Request::CancelSession).unwrap();
        assert!(g.fd().is_some(), "reconnected");
        let (mut s, _) = l.accept().unwrap();
        assert_eq!(recv_request(&mut s), Request::CancelSession);
    }

    #[test]
    fn an_oversize_frame_is_invalid_data() {
        let (_d, l, mut g) = fake("oversize");
        g.send(&Request::CancelSession).unwrap();
        let (mut s, _) = l.accept().unwrap();
        s.write_all(&u32::MAX.to_ne_bytes()).unwrap();
        let e = wait_read(&mut g, 1).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(g.fd().is_none());
    }

    #[test]
    fn no_socket_is_a_send_error() {
        let d = Dir::new("none");
        let mut g = Greetd::new(d.0.join("absent.sock"));
        assert!(g.send(&Request::CancelSession).is_err());
        assert!(g.fd().is_none());
    }

    #[test]
    fn from_env_needs_the_variable() {
        // The test process is not run by greetd; if it somehow were,
        // there is nothing to assert.
        if std::env::var_os(SOCK_ENV).is_none() {
            assert!(Greetd::from_env().is_err());
        }
    }
}

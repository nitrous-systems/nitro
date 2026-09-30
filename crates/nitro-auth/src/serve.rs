//! The helper's loop: greetd's protocol on a byte stream, over any
//! [`Authenticator`].
//!
//! Generic so it can be tested with a scripted authenticator: the tests
//! never touch real PAM, which could trip `pam_faillock` on the machine
//! running them.

use std::io::{self, Read, Write};

use nitro_login::ipc::{self, ErrorKind, MAX_FRAME, MessageKind, Request, Response};

/// The user gave up on the question: `cancel_session`, EOF, or a
/// malformed frame arrived instead of an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// How to ask the user something, from inside an authentication.
pub trait Prompter {
    /// Show `text` as `kind` and wait for the answer: the typed text for
    /// `Visible`/`Secret`, `None` for `Info`/`Error`.
    ///
    /// # Errors
    /// [`Cancelled`] if the conversation was abandoned; the
    /// authenticator should fail the transaction and return.
    fn ask(&mut self, kind: MessageKind, text: &str) -> Result<Option<String>, Cancelled>;
}

/// Why an authentication failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Not authenticated (a wrong password, a locked account): the
    /// greeter says so and asks again. The text is PAM's.
    Auth(String),
    /// Something broke (no PAM service file, a module error).
    Other(String),
}

/// Something that can check a user's identity by a conversation.
pub trait Authenticator {
    /// Authenticate `user`, asking through `conv`.
    ///
    /// # Errors
    /// Any [`Failure`]. A cancelled conversation is reported however
    /// the backend reports it; the loop knows it was a cancel.
    fn authenticate(&mut self, user: &str, conv: &mut dyn Prompter) -> Result<(), Failure>;
}

/// Why the loop stopped with an error.
#[derive(Debug)]
pub enum ServeError {
    /// Reading or writing the stream failed.
    Io(io::Error),
    /// A frame that is not the protocol.
    Protocol(ipc::ProtocolError),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o: {e}"),
            Self::Protocol(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ServeError {}

impl From<io::Error> for ServeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Read one frame's body; `None` at a clean end of stream (between
/// frames).
fn read_frame(r: &mut dyn Read) -> Result<Option<Vec<u8>>, ServeError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(ServeError::Protocol(ipc::ProtocolError(format!(
            "frame of {len} bytes"
        ))));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

fn read_request(r: &mut dyn Read) -> Result<Option<Request>, ServeError> {
    let Some(mut body) = read_frame(r)? else {
        return Ok(None);
    };
    let req = ipc::decode_request(&body);
    ipc::wipe(&mut body);
    req.map(Some).map_err(ServeError::Protocol)
}

fn send(w: &mut dyn Write, resp: &Response) -> Result<(), ServeError> {
    w.write_all(&ipc::encode_response(resp))?;
    w.flush()?;
    Ok(())
}

fn error(kind: ErrorKind, description: impl Into<String>) -> Response {
    Response::Error {
        kind,
        description: description.into(),
    }
}

/// How a conversation ended, other than by the authenticator returning.
enum Interrupt {
    /// `cancel_session`: answer `success` and wait for the next request.
    Cancel,
    /// The stream ended or broke: stop the loop with this result.
    Stop(Result<(), ServeError>),
}

/// The [`Prompter`] over the stream: every question is an
/// `auth_message`, every answer the next `post_auth_message_response`.
struct Stream<'a> {
    r: &'a mut dyn Read,
    w: &'a mut dyn Write,
    interrupt: Option<Interrupt>,
}

impl Prompter for Stream<'_> {
    fn ask(&mut self, kind: MessageKind, text: &str) -> Result<Option<String>, Cancelled> {
        if self.interrupt.is_some() {
            return Err(Cancelled);
        }
        let msg = Response::AuthMessage {
            kind,
            text: text.to_owned(),
        };
        if let Err(e) = send(self.w, &msg) {
            self.interrupt = Some(Interrupt::Stop(Err(e)));
            return Err(Cancelled);
        }
        loop {
            let req = match read_request(self.r) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    self.interrupt = Some(Interrupt::Stop(Ok(())));
                    return Err(Cancelled);
                }
                Err(e) => {
                    self.interrupt = Some(Interrupt::Stop(Err(e)));
                    return Err(Cancelled);
                }
            };
            let refusal = match req {
                Request::PostAuthMessageResponse { response } => {
                    return Ok(match kind {
                        MessageKind::Visible | MessageKind::Secret => {
                            Some(response.unwrap_or_default())
                        }
                        // An answer to a line is ignored, not refused:
                        // there is nothing it could mean.
                        MessageKind::Info | MessageKind::Error => None,
                    });
                }
                Request::CancelSession => {
                    self.interrupt = Some(Interrupt::Cancel);
                    return Err(Cancelled);
                }
                // greetd's rule: one conversation at a time.
                Request::CreateSession { .. } => "a session is already being created",
                Request::StartSession { .. } => "not supported by nitro-auth",
            };
            if let Err(e) = send(self.w, &error(ErrorKind::Error, refusal)) {
                self.interrupt = Some(Interrupt::Stop(Err(e)));
                return Err(Cancelled);
            }
        }
    }
}

/// Serve requests from `r`, answering on `w`, until end of stream.
///
/// Only `owner` may be authenticated: a `create_session` for anyone else
/// is refused without a PAM transaction. A new `create_session` after an
/// attempt finished starts a new transaction, which is how a wrong
/// password is retried.
///
/// # Errors
/// An I/O failure or a malformed frame. The greeter treats either as
/// the helper having died, and respawns it.
pub fn serve(
    r: &mut dyn Read,
    w: &mut dyn Write,
    owner: &str,
    auth: &mut dyn Authenticator,
) -> Result<(), ServeError> {
    while let Some(req) = read_request(r)? {
        let reply = match req {
            Request::CreateSession { username } if username != owner => error(
                ErrorKind::Error,
                format!("only {owner} can unlock this session"),
            ),
            Request::CreateSession { username } => {
                let mut stream = Stream {
                    r: &mut *r,
                    w: &mut *w,
                    interrupt: None,
                };
                let result = auth.authenticate(&username, &mut stream);
                match stream.interrupt {
                    Some(Interrupt::Stop(end)) => return end,
                    // greetd answers a cancel with `success`.
                    Some(Interrupt::Cancel) => Response::Success,
                    None => match result {
                        Ok(()) => Response::Success,
                        Err(Failure::Auth(d)) => error(ErrorKind::AuthError, d),
                        Err(Failure::Other(d)) => error(ErrorKind::Error, d),
                    },
                }
            }
            Request::PostAuthMessageResponse { .. } => {
                error(ErrorKind::Error, "no authentication in progress")
            }
            Request::StartSession { .. } => error(ErrorKind::Error, "not supported by nitro-auth"),
            Request::CancelSession => Response::Success,
        };
        send(w, &reply)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_login::ipc::{Framer, decode_response, encode_request};

    /// An authenticator that runs a closure per attempt.
    struct Scripted<F>(F, u32);

    impl<F: FnMut(u32, &str, &mut dyn Prompter) -> Result<(), Failure>> Authenticator for Scripted<F> {
        fn authenticate(&mut self, user: &str, conv: &mut dyn Prompter) -> Result<(), Failure> {
            self.1 += 1;
            (self.0)(self.1, user, conv)
        }
    }

    fn input(reqs: &[Request]) -> Vec<u8> {
        reqs.iter().flat_map(encode_request).collect()
    }

    fn run(
        reqs: &[Request],
        f: impl FnMut(u32, &str, &mut dyn Prompter) -> Result<(), Failure>,
    ) -> (Result<(), ServeError>, Vec<Response>, u32) {
        let bytes = input(reqs);
        let mut r = &bytes[..];
        let mut w = Vec::new();
        let mut a = Scripted(f, 0);
        let outcome = serve(&mut r, &mut w, "alice", &mut a);
        let mut fr = Framer::new();
        fr.push(&w);
        let mut out = Vec::new();
        while let Some(b) = fr.next_frame().unwrap() {
            out.push(decode_response(&b).unwrap());
        }
        (outcome, out, a.1)
    }

    fn create(u: &str) -> Request {
        Request::CreateSession { username: u.into() }
    }
    fn answer(s: &str) -> Request {
        Request::PostAuthMessageResponse {
            response: Some(s.into()),
        }
    }
    fn msg(kind: MessageKind, text: &str) -> Response {
        Response::AuthMessage {
            kind,
            text: text.into(),
        }
    }

    /// The usual PAM stack: one password prompt.
    fn password(_n: u32, user: &str, c: &mut dyn Prompter) -> Result<(), Failure> {
        assert_eq!(user, "alice");
        match c.ask(MessageKind::Secret, "Password: ") {
            Ok(Some(p)) if p == "hunter2" => Ok(()),
            Ok(_) => Err(Failure::Auth("Authentication failure".into())),
            Err(Cancelled) => Err(Failure::Other("conversation error".into())),
        }
    }

    #[test]
    fn a_right_password_succeeds() {
        let (res, out, n) = run(&[create("alice"), answer("hunter2")], password);
        res.unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            out,
            [msg(MessageKind::Secret, "Password: "), Response::Success]
        );
    }

    #[test]
    fn a_one_time_code_after_the_password() {
        let (res, out, _) = run(
            &[create("alice"), answer("hunter2"), answer("123456")],
            |_, _, c| {
                assert_eq!(
                    c.ask(MessageKind::Secret, "Password: ")?.unwrap(),
                    "hunter2"
                );
                assert_eq!(
                    c.ask(MessageKind::Visible, "Verification code: ")?.unwrap(),
                    "123456"
                );
                Ok(())
            },
        );
        res.unwrap();
        assert_eq!(
            out,
            [
                msg(MessageKind::Secret, "Password: "),
                msg(MessageKind::Visible, "Verification code: "),
                Response::Success
            ]
        );
    }

    impl From<Cancelled> for Failure {
        fn from(_: Cancelled) -> Self {
            Self::Other("cancelled".into())
        }
    }

    #[test]
    fn an_info_line_is_answered_with_null() {
        let (res, out, _) = run(
            &[
                create("alice"),
                Request::PostAuthMessageResponse { response: None },
                answer("hunter2"),
            ],
            |_, _, c| {
                assert_eq!(c.ask(MessageKind::Info, "Touch your key")?, None);
                assert_eq!(
                    c.ask(MessageKind::Secret, "Password: ")?.unwrap(),
                    "hunter2"
                );
                Ok(())
            },
        );
        res.unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], msg(MessageKind::Info, "Touch your key"));
        assert_eq!(out[2], Response::Success);
    }

    #[test]
    fn a_wrong_password_is_an_auth_error_and_can_be_retried() {
        let (res, out, n) = run(
            &[
                create("alice"),
                answer("wrong"),
                create("alice"),
                answer("hunter2"),
            ],
            password,
        );
        res.unwrap();
        assert_eq!(n, 2, "a new transaction per attempt");
        assert_eq!(
            out,
            [
                msg(MessageKind::Secret, "Password: "),
                Response::Error {
                    kind: ErrorKind::AuthError,
                    description: "Authentication failure".into()
                },
                msg(MessageKind::Secret, "Password: "),
                Response::Success,
            ]
        );
    }

    #[test]
    fn another_user_is_refused_without_a_transaction() {
        let (res, out, n) = run(&[create("mallory")], password);
        res.unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            out,
            [Response::Error {
                kind: ErrorKind::Error,
                description: "only alice can unlock this session".into()
            }]
        );
    }

    #[test]
    fn a_cancel_mid_prompt_abandons_the_attempt_with_success() {
        let (res, out, n) = run(
            &[
                create("alice"),
                Request::CancelSession,
                create("alice"),
                answer("hunter2"),
            ],
            password,
        );
        res.unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            out,
            [
                msg(MessageKind::Secret, "Password: "),
                Response::Success,
                msg(MessageKind::Secret, "Password: "),
                Response::Success,
            ]
        );
    }

    #[test]
    fn create_while_in_flight_is_refused_and_the_question_still_stands() {
        let (res, out, _) = run(
            &[create("alice"), create("alice"), answer("hunter2")],
            password,
        );
        res.unwrap();
        assert_eq!(out.len(), 3);
        assert!(matches!(
            out[1],
            Response::Error {
                kind: ErrorKind::Error,
                ..
            }
        ));
        assert_eq!(out[2], Response::Success);
    }

    #[test]
    fn eof_ends_the_loop_cleanly_even_mid_conversation() {
        let (res, out, _) = run(&[], password);
        res.unwrap();
        assert!(out.is_empty());
        let (res, out, _) = run(&[create("alice")], password);
        res.unwrap();
        assert_eq!(out, [msg(MessageKind::Secret, "Password: ")]);
    }

    #[test]
    fn requests_outside_a_conversation_are_answered() {
        let (res, out, _) = run(
            &[
                answer("x"),
                Request::StartSession {
                    cmd: vec!["sh".into()],
                    env: vec![],
                },
                Request::CancelSession,
            ],
            password,
        );
        res.unwrap();
        assert!(matches!(
            out[0],
            Response::Error {
                kind: ErrorKind::Error,
                ..
            }
        ));
        assert!(matches!(
            out[1],
            Response::Error {
                kind: ErrorKind::Error,
                ..
            }
        ));
        assert_eq!(out[2], Response::Success);
    }

    #[test]
    fn a_malformed_frame_is_an_error_not_a_panic() {
        let mut bytes = input(&[create("alice")]);
        let bad = br#"{"type":7}"#;
        bytes.extend_from_slice(&u32::try_from(bad.len()).unwrap().to_ne_bytes());
        bytes.extend_from_slice(bad);
        let mut r = &bytes[..];
        let mut w = Vec::new();
        let res = serve(&mut r, &mut w, "alice", &mut Scripted(password, 0));
        assert!(matches!(res, Err(ServeError::Protocol(_))));
        // Before the first request, and a torn length.
        let mut r = &bad[..];
        assert!(serve(&mut r, &mut Vec::new(), "alice", &mut Scripted(password, 0)).is_err());
        let mut r = &[1u8, 0][..];
        assert!(serve(&mut r, &mut Vec::new(), "alice", &mut Scripted(password, 0)).is_err());
        let mut r = &u32::MAX.to_ne_bytes()[..];
        assert!(serve(&mut r, &mut Vec::new(), "alice", &mut Scripted(password, 0)).is_err());
    }
}

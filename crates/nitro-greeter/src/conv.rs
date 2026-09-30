//! The conversation: PAM's questions, as a pure state machine.
//!
//! No I/O and no widgets. Every method takes an event (the user typed
//! something, the authenticator said something) and returns the
//! [`Request`]s to send, so the whole of decision 4 in
//! `docs/greeter.md` is tested against scripted conversations without a
//! server or a PAM stack.
//!
//! ```text
//!   User ──submit_user──▶ Waiting ──auth_message(secret|visible)──▶ Prompt
//!    ▲                     │  ▲ info|error: null reply, stay           │
//!    │                     │  └──────────────answer────────────────────┘
//!    └──error / cancel─────┤
//!                          └──success──▶ Authenticated
//! ```
//!
//! greetd answers every request with exactly one response, in order.
//! The machine keeps the queue of requests still unanswered, so a
//! response that belongs to a conversation it has already cancelled
//! (and the `success` that answers the `cancel_session` itself) is
//! recognised and dropped rather than taken for the end of the new
//! one.
//!
//! The greeter (plan step 5) will add a `Session` step after `success`
//! (choose a session, send `start_session`); in lock mode `success` is
//! the end.

use std::collections::VecDeque;

use nitro_login::{ErrorKind, MessageKind, Request, Response};

/// Where the conversation is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Asking for a user name; nothing in flight.
    User,
    /// Waiting for the authenticator (show a spinner).
    Waiting,
    /// PAM asked a question: `text`, answered masked or not per `kind`.
    Prompt {
        /// [`MessageKind::Secret`] or [`MessageKind::Visible`].
        kind: MessageKind,
        /// The question, from PAM.
        text: String,
    },
    /// The user is authenticated.
    Authenticated,
}

/// What an unanswered request was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    /// `create_session` or `post_auth_message_response` of the live
    /// conversation: its answer drives the machine.
    Live,
    /// A request of a conversation since cancelled, or the
    /// `cancel_session` itself: its answer is dropped.
    Stale,
}

/// The conversation state machine.
#[derive(Debug, Clone)]
pub struct Conversation {
    state: State,
    username: String,
    /// Whether the authenticator has a conversation open: from
    /// `create_session` until its `success`/`error`.
    open: bool,
    outstanding: VecDeque<Sent>,
    notices: Vec<(MessageKind, String)>,
    last_error: Option<(ErrorKind, String)>,
}

impl Default for Conversation {
    fn default() -> Self {
        Self::new()
    }
}

impl Conversation {
    /// Waiting for a user name.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::User,
            username: String::new(),
            open: false,
            outstanding: VecDeque::new(),
            notices: Vec::new(),
            last_error: None,
        }
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The name of the current (or last) attempt. Kept across a failure,
    /// so a mistyped password costs one field, not two.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The `info`/`error` lines PAM sent during this attempt, in order.
    #[must_use]
    pub fn notices(&self) -> &[(MessageKind, String)] {
        &self.notices
    }

    /// Why the last attempt failed, until the user answers again.
    #[must_use]
    pub fn last_error(&self) -> Option<&(ErrorKind, String)> {
        self.last_error.as_ref()
    }

    /// Whether requests are awaiting answers.
    #[must_use]
    pub fn busy(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// Abandon a conversation in flight, if any: `cancel_session`.
    fn cancel_open(&mut self, out: &mut Vec<Request>) {
        if self.open {
            for s in &mut self.outstanding {
                *s = Sent::Stale;
            }
            self.outstanding.push_back(Sent::Stale);
            out.push(Request::CancelSession);
            self.open = false;
        }
    }

    /// Start an attempt for `name`. Sends `cancel_session` first if a
    /// conversation is in flight: greetd refuses a second
    /// `create_session` otherwise.
    ///
    /// The last error stays until the user answers, so the automatic
    /// retry after a wrong password still shows why.
    pub fn submit_user(&mut self, name: &str) -> Vec<Request> {
        let mut out = Vec::new();
        self.cancel_open(&mut out);
        name.clone_into(&mut self.username);
        self.notices.clear();
        self.open = true;
        self.outstanding.push_back(Sent::Live);
        out.push(Request::CreateSession {
            username: name.to_owned(),
        });
        self.state = State::Waiting;
        out
    }

    /// Answer the question on screen. Ignored outside [`State::Prompt`].
    pub fn answer(&mut self, text: String) -> Vec<Request> {
        if !matches!(self.state, State::Prompt { .. }) {
            return Vec::new();
        }
        self.last_error = None;
        self.outstanding.push_back(Sent::Live);
        self.state = State::Waiting;
        vec![Request::PostAuthMessageResponse {
            response: Some(text),
        }]
    }

    /// Give up on the attempt and go back to the user name.
    pub fn cancel(&mut self) -> Vec<Request> {
        let mut out = Vec::new();
        self.cancel_open(&mut out);
        if self.state != State::Authenticated {
            self.state = State::User;
        }
        out
    }

    /// The authenticator went away (the helper died, the socket closed):
    /// nothing is in flight any more, and the attempt failed.
    pub fn lost(&mut self, description: &str) {
        self.outstanding.clear();
        self.open = false;
        if self.state != State::Authenticated {
            self.state = State::User;
            self.last_error = Some((ErrorKind::Error, description.to_owned()));
        }
    }

    /// Take one response; returns the requests it calls for (the `null`
    /// reply to an `info`/`error` line).
    pub fn on_response(&mut self, resp: Response) -> Vec<Request> {
        match self.outstanding.pop_front() {
            Some(Sent::Live) => {}
            // Stale, or unsolicited: nothing of ours is waiting for it.
            Some(Sent::Stale) | None => return Vec::new(),
        }
        match resp {
            Response::AuthMessage {
                kind: kind @ (MessageKind::Secret | MessageKind::Visible),
                text,
            } => {
                self.state = State::Prompt { kind, text };
                Vec::new()
            }
            Response::AuthMessage { kind, text } => {
                self.notices.push((kind, text));
                self.outstanding.push_back(Sent::Live);
                vec![Request::PostAuthMessageResponse { response: None }]
            }
            Response::Success => {
                self.open = false;
                self.last_error = None;
                self.state = State::Authenticated;
                Vec::new()
            }
            Response::Error { kind, description } => {
                self.open = false;
                self.last_error = Some((kind, description));
                self.state = State::User;
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One step of a scripted conversation.
    enum Step {
        /// The user submits a name; exactly these requests go out.
        User(&'static str, Vec<Request>),
        /// The user answers; exactly these go out.
        Answer(&'static str, Vec<Request>),
        /// The user cancels.
        Cancel(Vec<Request>),
        /// The authenticator says this; exactly these go out, and the
        /// machine lands in the given state.
        Recv(Response, Vec<Request>, State),
    }

    fn play(script: Vec<Step>) -> Conversation {
        let mut c = Conversation::new();
        for (i, step) in script.into_iter().enumerate() {
            match step {
                Step::User(n, want) => assert_eq!(c.submit_user(n), want, "step {i}"),
                Step::Answer(a, want) => assert_eq!(c.answer(a.into()), want, "step {i}"),
                Step::Cancel(want) => assert_eq!(c.cancel(), want, "step {i}"),
                Step::Recv(r, want, state) => {
                    assert_eq!(c.on_response(r), want, "step {i}");
                    assert_eq!(c.state(), &state, "step {i}");
                }
            }
        }
        c
    }

    fn create(n: &str) -> Request {
        Request::CreateSession { username: n.into() }
    }
    fn post(a: &str) -> Request {
        Request::PostAuthMessageResponse {
            response: Some(a.into()),
        }
    }
    fn null() -> Request {
        Request::PostAuthMessageResponse { response: None }
    }
    fn ask(kind: MessageKind, t: &str) -> Response {
        Response::AuthMessage {
            kind,
            text: t.into(),
        }
    }
    fn prompt(kind: MessageKind, t: &str) -> State {
        State::Prompt {
            kind,
            text: t.into(),
        }
    }
    fn auth_error(d: &str) -> Response {
        Response::Error {
            kind: ErrorKind::AuthError,
            description: d.into(),
        }
    }
    use MessageKind::{Info, Secret, Visible};
    use Step::{Answer, Cancel, Recv, User};

    #[test]
    fn a_password() {
        let c = play(vec![
            User("alice", vec![create("alice")]),
            Recv(
                ask(Secret, "Password: "),
                vec![],
                prompt(Secret, "Password: "),
            ),
            Answer("hunter2", vec![post("hunter2")]),
            Recv(Response::Success, vec![], State::Authenticated),
        ]);
        assert!(!c.busy());
    }

    #[test]
    fn a_one_time_code_after_the_password() {
        play(vec![
            User("alice", vec![create("alice")]),
            Recv(
                ask(Secret, "Password: "),
                vec![],
                prompt(Secret, "Password: "),
            ),
            Answer("hunter2", vec![post("hunter2")]),
            Recv(ask(Visible, "Code: "), vec![], prompt(Visible, "Code: ")),
            Answer("123456", vec![post("123456")]),
            Recv(Response::Success, vec![], State::Authenticated),
        ]);
    }

    #[test]
    fn an_info_line_is_answered_with_null_and_kept() {
        let c = play(vec![
            User("alice", vec![create("alice")]),
            Recv(ask(Info, "Touch the key"), vec![null()], State::Waiting),
            Recv(ask(Secret, "PIN: "), vec![], prompt(Secret, "PIN: ")),
        ]);
        assert_eq!(c.notices(), [(Info, "Touch the key".to_owned())]);
    }

    #[test]
    fn a_wrong_password_keeps_the_name_and_a_retry_succeeds() {
        let c = play(vec![
            User("alice", vec![create("alice")]),
            Recv(
                ask(Secret, "Password: "),
                vec![],
                prompt(Secret, "Password: "),
            ),
            Answer("wrong", vec![post("wrong")]),
            Recv(auth_error("Authentication failure"), vec![], State::User),
        ]);
        assert_eq!(c.username(), "alice");
        assert_eq!(
            c.last_error(),
            Some(&(ErrorKind::AuthError, "Authentication failure".into()))
        );
        let mut c = c;
        // No cancel: the failed conversation is already over.
        assert_eq!(c.submit_user("alice"), [create("alice")]);
        // The reason stays up while the password is asked again...
        c.on_response(ask(Secret, "Password: "));
        assert!(c.last_error().is_some());
        // ...until the user answers.
        c.answer("hunter2".into());
        assert!(c.last_error().is_none());
        c.on_response(Response::Success);
        assert_eq!(c.state(), &State::Authenticated);
    }

    #[test]
    fn a_cancel_during_a_prompt_then_a_new_name_cancels_first() {
        let c = play(vec![
            User("alice", vec![create("alice")]),
            Recv(
                ask(Secret, "Password: "),
                vec![],
                prompt(Secret, "Password: "),
            ),
            Cancel(vec![Request::CancelSession]),
            // The cancel's own `success` is not an authentication.
            Recv(Response::Success, vec![], State::User),
            User("bob", vec![create("bob")]),
            Recv(
                ask(Secret, "Password: "),
                vec![],
                prompt(Secret, "Password: "),
            ),
        ]);
        assert_eq!(c.username(), "bob");
    }

    #[test]
    fn a_new_name_while_waiting_sends_cancel_before_create_and_drops_the_old_answers() {
        play(vec![
            User("alice", vec![create("alice")]),
            User("bob", vec![Request::CancelSession, create("bob")]),
            // Alice's first question arrives late, then the cancel's
            // success: both belong to the abandoned conversation.
            Recv(ask(Secret, "alice's password"), vec![], State::Waiting),
            Recv(Response::Success, vec![], State::Waiting),
            Recv(
                ask(Secret, "bob's password"),
                vec![],
                prompt(Secret, "bob's password"),
            ),
        ]);
    }

    #[test]
    fn cancel_with_nothing_open_sends_nothing() {
        play(vec![Cancel(vec![])]);
        play(vec![
            User("alice", vec![create("alice")]),
            Recv(auth_error("no"), vec![], State::User),
            Cancel(vec![]),
        ]);
    }

    #[test]
    fn an_error_error_returns_to_the_name_with_its_own_tone() {
        let c = play(vec![
            User("alice", vec![create("alice")]),
            Recv(
                Response::Error {
                    kind: ErrorKind::Error,
                    description: "pam_start: no service".into(),
                },
                vec![],
                State::User,
            ),
        ]);
        assert_eq!(c.last_error().map(|e| e.0), Some(ErrorKind::Error));
    }

    #[test]
    fn an_answer_outside_a_prompt_and_unsolicited_responses_are_ignored() {
        let mut c = Conversation::new();
        assert!(c.answer("x".into()).is_empty());
        assert!(c.on_response(Response::Success).is_empty());
        assert_eq!(c.state(), &State::User);
    }

    #[test]
    fn losing_the_authenticator_fails_the_attempt() {
        let mut c = Conversation::new();
        c.submit_user("alice");
        c.lost("authentication helper exited");
        assert_eq!(c.state(), &State::User);
        assert!(!c.busy());
        assert_eq!(c.submit_user("alice"), [create("alice")], "no stale cancel");
    }
}

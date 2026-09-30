//! The lock screen through a real server on the shell socket, with a
//! scripted authenticator in place of `nitro-auth` (real PAM in a test
//! could trip `pam_faillock` on the machine running it).

use std::cell::RefCell;
use std::io;
use std::os::fd::BorrowedFd;
use std::rc::Rc;

use nitro_greeter::{Backend, Greeter, State, build, names, on_response};
use nitro_login::{ErrorKind, MessageKind, Request, Response};
use nitro_ui::event::key;
use nitro_ui::introspect;
use nitro_ui::test::Harness;
use nitro_ui::widgets::SECRET_MASK;
use nitro_ui::{Size, WidgetId};

/// Records what the greeter sent; the test plays the authenticator.
#[derive(Clone, Default)]
struct Scripted(Rc<RefCell<Vec<Request>>>);

impl Backend for Scripted {
    fn send(&mut self, req: &Request) -> io::Result<()> {
        self.0.borrow_mut().push(req.clone());
        Ok(())
    }
    fn fd(&self) -> Option<BorrowedFd<'_>> {
        None
    }
    fn read(&mut self) -> io::Result<Vec<Response>> {
        Ok(Vec::new())
    }
}

fn lock_screen() -> (Harness<Greeter>, Scripted) {
    let b = Scripted::default();
    let mut h = Harness::shell(
        "nitro-greeter",
        Greeter::lock("alice", Box::new(b.clone()))
            .with_session_socket(std::env::temp_dir().join("nitro-greeter-test-no-such.sock")),
        nitro_greeter::surface(),
        Some(Size::new(320.0, 240.0)),
        build,
    );
    h.settle();
    (h, b)
}

fn recv(h: &mut Harness<Greeter>, r: Response) {
    let (ui, s) = h.parts();
    on_response(s, ui, r);
    h.settle();
}

fn sent(b: &Scripted) -> Vec<Request> {
    std::mem::take(&mut *b.0.borrow_mut())
}

fn named(h: &mut Harness<Greeter>, n: &str) -> WidgetId {
    introspect::resolve(h.ui(), &format!("window/{n}")).unwrap_or_else(|| panic!("no {n}"))
}

fn shown(h: &mut Harness<Greeter>, n: &str) -> bool {
    let id = named(h, n);
    h.ui().is_visible(id)
}

fn get(h: &mut Harness<Greeter>, n: &str) -> String {
    let v = introspect::get_prop(h.ui(), &format!("window/{n}"), "value").unwrap();
    // `hey` prints an empty value as `-`.
    if v == "-" { String::new() } else { v }
}

fn create(u: &str) -> Request {
    Request::CreateSession { username: u.into() }
}

fn secret(t: &str) -> Response {
    Response::AuthMessage {
        kind: MessageKind::Secret,
        text: t.into(),
    }
}

/// `asdf` on a US layout, then Enter.
fn type_asdf(h: &mut Harness<Greeter>) {
    for k in [30, 31, 32, 33] {
        h.key(k);
    }
    h.key(key::ENTER);
    h.settle();
}

#[test]
fn it_takes_the_lock_and_asks_for_the_owners_password_at_once() {
    let (mut h, b) = lock_screen();
    assert_eq!(h.server().stat("locked"), 1);
    assert_eq!(h.server().stat("lock_owned"), 1);
    assert_eq!(sent(&b), [create("alice")]);
    assert_eq!(get(&mut h, names::USER), "alice");
    assert_eq!(*h.state().conversation().state(), State::Waiting);
    assert!(shown(&mut h, names::STATUS));

    recv(&mut h, secret("Password: "));
    assert_eq!(get(&mut h, names::PROMPT), "Password: ");
    let answer = named(&mut h, names::ANSWER);
    assert_eq!(
        h.ui().focused(),
        Some(answer),
        "the prompt has the keyboard"
    );
    assert!(!shown(&mut h, names::STATUS));
}

#[test]
fn a_typed_password_is_sent_and_never_shown_and_success_unlocks() {
    let (mut h, b) = lock_screen();
    sent(&b);
    recv(&mut h, secret("Password: "));
    // Typed through real keys; before Enter, introspection sees a mask.
    for k in [30, 31, 32, 33] {
        h.key(k);
    }
    h.settle();
    let mask: String = std::iter::repeat_n(SECRET_MASK, 4).collect();
    assert_eq!(get(&mut h, names::ANSWER), mask);
    h.key(key::ENTER);
    h.settle();
    assert_eq!(
        sent(&b),
        [Request::PostAuthMessageResponse {
            response: Some("asdf".into())
        }]
    );
    assert_eq!(get(&mut h, names::ANSWER), "", "cleared once sent");

    recv(&mut h, Response::Success);
    assert!(h.state().unlocked());
    assert!(h.ui().should_quit());
    assert_eq!(h.server().stat("locked"), 0);
}

#[test]
fn a_wrong_password_shows_the_reason_and_asks_again() {
    let (mut h, b) = lock_screen();
    sent(&b);
    recv(&mut h, secret("Password: "));
    type_asdf(&mut h);
    sent(&b);
    recv(
        &mut h,
        Response::Error {
            kind: ErrorKind::AuthError,
            description: "Authentication failure".into(),
        },
    );
    // Restarted for the same name, without a second field to fill.
    assert_eq!(sent(&b), [create("alice")]);
    assert_eq!(get(&mut h, names::MESSAGE), "Authentication failure");
    recv(&mut h, secret("Password: "));
    assert_eq!(get(&mut h, names::MESSAGE), "Authentication failure");
    type_asdf(&mut h);
    assert_eq!(get(&mut h, names::MESSAGE), "", "gone once answered");
    assert_eq!(h.server().stat("locked"), 1);
}

#[test]
fn a_one_time_code_after_the_password_is_asked_in_the_clear() {
    let (mut h, b) = lock_screen();
    recv(&mut h, secret("Password: "));
    type_asdf(&mut h);
    recv(
        &mut h,
        Response::AuthMessage {
            kind: MessageKind::Visible,
            text: "Verification code: ".into(),
        },
    );
    assert_eq!(get(&mut h, names::PROMPT), "Verification code: ");
    let answer = named(&mut h, names::ANSWER);
    assert!(
        !h.widget::<nitro_ui::widgets::TextField<Greeter>>(answer)
            .is_secret()
    );
    type_asdf(&mut h);
    let all = sent(&b);
    assert_eq!(
        all.last(),
        Some(&Request::PostAuthMessageResponse {
            response: Some("asdf".into())
        })
    );
    recv(&mut h, Response::Success);
    assert!(h.state().unlocked());
}

#[test]
fn an_info_line_is_shown_and_answered_with_null() {
    let (mut h, b) = lock_screen();
    sent(&b);
    recv(
        &mut h,
        Response::AuthMessage {
            kind: MessageKind::Info,
            text: "Touch your security key".into(),
        },
    );
    assert_eq!(
        sent(&b),
        [Request::PostAuthMessageResponse { response: None }]
    );
    assert_eq!(get(&mut h, names::MESSAGE), "Touch your security key");
}

#[test]
fn another_name_starts_no_conversation_and_offers_logout() {
    let (mut h, b) = lock_screen();
    recv(&mut h, secret("Password: "));
    sent(&b);
    let logout = named(&mut h, names::LOGOUT);
    assert!(!h.ui().is_visible(logout));
    let path = format!("window/{}", names::USER);
    {
        let (ui, s) = h.parts();
        introspect::invoke(ui, s, &path, "set_value", Some("bob")).unwrap();
        introspect::invoke(ui, s, &path, "submit", None).unwrap();
    }
    h.settle();
    // The owner's conversation is cancelled, and bob's never starts.
    assert_eq!(sent(&b), [Request::CancelSession]);
    assert_eq!(h.state().other_user(), Some("bob"));
    assert!(h.ui().is_visible(logout));
    assert!(get(&mut h, names::MESSAGE).starts_with("Only alice can unlock this session"));
    assert!(!shown(&mut h, names::ANSWER));

    // No session socket here: the failure is on screen, not a crash.
    h.click(logout);
    h.settle();
    assert!(get(&mut h, names::MESSAGE).starts_with("Could not log out"));
    assert_eq!(h.server().stat("locked"), 1);
}

#[test]
fn the_helper_dying_is_an_error_and_a_retry_starts_over() {
    let (mut h, b) = lock_screen();
    sent(&b);
    {
        let (ui, s) = h.parts();
        on_response(
            s,
            ui,
            Response::Error {
                kind: ErrorKind::Error,
                description: nitro_greeter::HELPER_EXITED.into(),
            },
        );
    }
    h.settle();
    assert_eq!(get(&mut h, names::MESSAGE), nitro_greeter::HELPER_EXITED);
    // An `error{error}` is not retried by itself: the user resubmits.
    assert!(sent(&b).is_empty());
    let path = format!("window/{}", names::USER);
    {
        let (ui, s) = h.parts();
        introspect::invoke(ui, s, &path, "submit", None).unwrap();
    }
    h.settle();
    assert_eq!(sent(&b), [create("alice")]);
}

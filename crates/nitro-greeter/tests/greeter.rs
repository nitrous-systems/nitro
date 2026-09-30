//! Greeter mode through a real server on the shell socket, with a
//! scripted greetd in place of the real one.

use std::cell::RefCell;
use std::io;
use std::os::fd::BorrowedFd;
use std::rc::Rc;

use nitro_greeter::{Backend, Greeter, Remembered, SessionEntry, State, build, names, on_response};
use nitro_login::{ErrorKind, MessageKind, Request, Response};
use nitro_ui::event::key;
use nitro_ui::introspect;
use nitro_ui::test::Harness;
use nitro_ui::{Size, WidgetId};

/// Records what the greeter sent; the test plays greetd.
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

fn entry(name: &str, cmd: &str) -> SessionEntry {
    SessionEntry {
        name: name.into(),
        cmd: vec![cmd.into()],
        desktop: name.to_lowercase(),
        current_desktop: name.to_lowercase(),
    }
}

fn sessions() -> Vec<SessionEntry> {
    vec![
        entry("Nitro", "/opt/nitro/nitro-session"),
        entry("Sway", "sway"),
    ]
}

fn greeter(remembered: Remembered) -> (Harness<Greeter>, Scripted) {
    let b = Scripted::default();
    let mut h = Harness::shell(
        "nitro-greeter",
        Greeter::login(Box::new(b.clone()), sessions(), remembered)
            .with_session_socket(std::env::temp_dir().join("nitro-greeter-test-no-such.sock")),
        nitro_greeter::surface(),
        Some(Size::new(480.0, 640.0)),
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
    if v == "-" { String::new() } else { v }
}

fn type_name(h: &mut Harness<Greeter>, name: &str) {
    let path = format!("window/{}", names::USER);
    let (ui, s) = h.parts();
    introspect::invoke(ui, s, &path, "set_value", Some(name)).unwrap();
    introspect::invoke(ui, s, &path, "submit", None).unwrap();
    h.settle();
}

fn secret(t: &str) -> Response {
    Response::AuthMessage {
        kind: MessageKind::Secret,
        text: t.into(),
    }
}

fn create(u: &str) -> Request {
    Request::CreateSession { username: u.into() }
}

/// `asdf` on a US layout, then Enter.
fn type_asdf(h: &mut Harness<Greeter>) {
    for k in [30, 31, 32, 33] {
        h.key(k);
    }
    h.key(key::ENTER);
    h.settle();
}

fn start_of(e: &SessionEntry) -> Request {
    Request::StartSession {
        cmd: e.cmd.clone(),
        env: e.env(),
    }
}

#[test]
fn no_lock_is_taken_and_the_name_field_has_the_keyboard() {
    let (mut h, b) = greeter(Remembered::default());
    assert_eq!(h.server().stat("locked"), 0);
    assert_eq!(
        sent(&b),
        [Request::CancelSession],
        "only the reset until a name is typed"
    );
    let user = named(&mut h, names::USER);
    assert_eq!(h.ui().focused(), Some(user));
    assert!(shown(&mut h, names::SESSION));
    assert!(shown(&mut h, names::SUSPEND));
    assert!(shown(&mut h, names::POWEROFF));
    assert!(!shown(&mut h, names::LOGOUT));
    assert_eq!(get(&mut h, names::SESSION), "Nitro");
}

#[test]
fn a_login_starts_the_nitro_session_and_quits() {
    let (mut h, b) = greeter(Remembered::default());
    sent(&b);
    // greetd's answer to the start-up reset.
    recv(&mut h, Response::Success);
    type_name(&mut h, "alice");
    assert_eq!(sent(&b), [create("alice")]);
    recv(&mut h, secret("Password: "));
    let answer = named(&mut h, names::ANSWER);
    assert_eq!(h.ui().focused(), Some(answer));
    type_asdf(&mut h);
    assert_eq!(
        sent(&b),
        [Request::PostAuthMessageResponse {
            response: Some("asdf".into())
        }]
    );
    recv(&mut h, Response::Success);
    assert_eq!(sent(&b), [start_of(&sessions()[0])]);
    assert_eq!(*h.state().conversation().state(), State::Starting);
    assert!(!h.state().started());
    recv(&mut h, Response::Success);
    assert!(h.state().started());
    assert_eq!(
        h.state().remembered(),
        &Remembered {
            user: Some("alice".into()),
            session: Some("Nitro".into())
        }
    );
}

#[test]
fn the_remembered_user_is_asked_for_at_once() {
    let (mut h, b) = greeter(Remembered {
        user: Some("bob".into()),
        session: Some("Sway".into()),
    });
    assert_eq!(sent(&b), [Request::CancelSession, create("bob")]);
    assert_eq!(get(&mut h, names::USER), "bob");
    assert_eq!(get(&mut h, names::SESSION), "Sway");
}

#[test]
fn a_wrong_password_keeps_the_name_and_asks_again() {
    let (mut h, b) = greeter(Remembered::default());
    recv(&mut h, Response::Success);
    type_name(&mut h, "alice");
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
    assert_eq!(sent(&b), [create("alice")]);
    assert_eq!(get(&mut h, names::MESSAGE), "Authentication failure");
    assert_eq!(get(&mut h, names::USER), "alice");
}

#[test]
fn the_session_button_cycles_and_the_chosen_command_is_sent() {
    let (mut h, b) = greeter(Remembered::default());
    recv(&mut h, Response::Success);
    let pick = |h: &mut Harness<Greeter>| {
        let path = format!("window/{}", names::SESSION);
        let (ui, s) = h.parts();
        introspect::invoke(ui, s, &path, "click", None).unwrap();
        h.settle();
    };
    pick(&mut h);
    assert_eq!(get(&mut h, names::SESSION), "Sway");
    pick(&mut h);
    assert_eq!(get(&mut h, names::SESSION), "Nitro", "round");
    pick(&mut h);
    type_name(&mut h, "alice");
    recv(&mut h, secret("Password: "));
    type_asdf(&mut h);
    sent(&b);
    recv(&mut h, Response::Success);
    assert_eq!(sent(&b), [start_of(&sessions()[1])]);
}

#[test]
fn a_start_session_error_is_shown_and_cancelled() {
    let (mut h, b) = greeter(Remembered::default());
    recv(&mut h, Response::Success);
    type_name(&mut h, "alice");
    recv(&mut h, secret("Password: "));
    type_asdf(&mut h);
    recv(&mut h, Response::Success);
    sent(&b);
    recv(
        &mut h,
        Response::Error {
            kind: ErrorKind::Error,
            description: "could not exec".into(),
        },
    );
    assert_eq!(sent(&b), [Request::CancelSession]);
    assert_eq!(get(&mut h, names::MESSAGE), "could not exec");
    assert!(!h.state().started());
    assert_eq!(*h.state().conversation().state(), State::User);
}

#[test]
fn a_power_button_without_a_session_shows_the_error() {
    let (mut h, _b) = greeter(Remembered::default());
    let p = named(&mut h, names::SUSPEND);
    h.click(p);
    h.settle();
    assert!(get(&mut h, names::MESSAGE).contains("no session at"));
}

#[test]
fn lock_mode_hides_the_greeter_controls() {
    let b = Scripted::default();
    let mut h = Harness::shell(
        "nitro-greeter",
        Greeter::lock("alice", Box::new(b)),
        nitro_greeter::surface(),
        Some(Size::new(480.0, 640.0)),
        build,
    );
    h.settle();
    for n in [
        names::SESSION,
        names::SUSPEND,
        names::REBOOT,
        names::POWEROFF,
    ] {
        assert!(!shown(&mut h, n), "{n} is greeter-only");
    }
}

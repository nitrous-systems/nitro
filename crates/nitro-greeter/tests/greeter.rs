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
use nitro_ui::{MenuButton, Size, WidgetId, WindowId};

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

/// Click the menu button `n` and return its open popup.
fn open(h: &mut Harness<Greeter>, n: &str) -> WindowId {
    let b = named(h, n);
    h.click(b);
    h.settle();
    h.ui()
        .widget::<MenuButton<Greeter>>(b)
        .unwrap()
        .popup()
        .unwrap_or_else(|| panic!("{n}'s menu did not open"))
}

fn is_open(h: &mut Harness<Greeter>, n: &str) -> bool {
    let b = named(h, n);
    h.ui()
        .widget::<MenuButton<Greeter>>(b)
        .unwrap()
        .popup()
        .is_some()
}

/// A row of the open menu: its value (checked or not).
fn row_value(h: &mut Harness<Greeter>, id: &str) -> String {
    introspect::get_prop(h.ui(), &format!("window[1]/{id}"), "value")
        .unwrap_or_else(|e| panic!("no menu row {id}: {e}"))
}

fn click_row(h: &mut Harness<Greeter>, id: &str) {
    let r = introspect::resolve(h.ui(), &format!("window[1]/{id}"))
        .unwrap_or_else(|| panic!("no menu row {id}"));
    h.click(r);
    h.settle();
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
    assert!(shown(&mut h, names::SESSION_NAME));
    assert!(shown(&mut h, names::POWER));
    assert!(!shown(&mut h, names::LOGOUT));
    assert_eq!(get(&mut h, names::SESSION_NAME), "Nitro");
    assert_eq!(get(&mut h, names::SESSION), "closed");
    // The bar sits under the card: session bottom-left, power
    // bottom-right.
    let (s, p, ans) = (
        named(&mut h, names::SESSION),
        named(&mut h, names::POWER),
        named(&mut h, names::USER),
    );
    let (sb, pb, ub) = (
        h.ui().window_bounds(s),
        h.ui().window_bounds(p),
        h.ui().window_bounds(ans),
    );
    assert!(
        sb.x < ub.x && pb.right() > ub.right(),
        "{sb:?} {pb:?} {ub:?}"
    );
    assert!(
        sb.y > ub.bottom() && pb.y > ub.bottom(),
        "{sb:?} {pb:?} {ub:?}"
    );
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
    assert_eq!(get(&mut h, names::SESSION_NAME), "Sway");
    open(&mut h, names::SESSION);
    assert_eq!(row_value(&mut h, "sway"), "true");
    assert_eq!(row_value(&mut h, "nitro"), "false");
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
fn the_session_menu_picks_and_the_chosen_command_is_sent() {
    let (mut h, b) = greeter(Remembered::default());
    recv(&mut h, Response::Success);
    let pop = open(&mut h, names::SESSION);
    assert_eq!(get(&mut h, names::SESSION), "open");
    assert_eq!(row_value(&mut h, "nitro"), "true");
    click_row(&mut h, "sway");
    assert!(!h.ui().has_window(pop), "picking closes the menu");
    assert!(!is_open(&mut h, names::SESSION));
    assert_eq!(get(&mut h, names::SESSION_NAME), "Sway");
    // And back, and forth again: the mark follows the choice.
    open(&mut h, names::SESSION);
    assert_eq!(row_value(&mut h, "sway"), "true");
    assert_eq!(row_value(&mut h, "nitro"), "false");
    click_row(&mut h, "nitro");
    assert_eq!(get(&mut h, names::SESSION_NAME), "Nitro");
    open(&mut h, names::SESSION);
    click_row(&mut h, "sway");
    type_name(&mut h, "alice");
    recv(&mut h, secret("Password: "));
    type_asdf(&mut h);
    sent(&b);
    recv(&mut h, Response::Success);
    assert_eq!(sent(&b), [start_of(&sessions()[1])]);
    recv(&mut h, Response::Success);
    assert_eq!(
        h.state().remembered().session.as_deref(),
        Some("Sway"),
        "the picked session is remembered"
    );
}

#[test]
fn the_menus_are_reachable_with_tab_and_driven_by_keys() {
    let (mut h, _b) = greeter(Remembered::default());
    let user = named(&mut h, names::USER);
    let session = named(&mut h, names::SESSION);
    let power = named(&mut h, names::POWER);
    assert_eq!(h.ui().focused(), Some(user));
    let mut seen = Vec::new();
    for _ in 0..8 {
        h.key(key::TAB);
        h.settle();
        if let Some(f) = h.ui().focused() {
            seen.push(f);
        }
    }
    assert!(seen.contains(&session), "Tab reaches the session button");
    assert!(seen.contains(&power), "Tab reaches the power button");
    // Down opens with the chosen session highlighted; Down, Enter picks
    // the next.
    h.ui().focus(session);
    h.settle();
    h.key(key::DOWN);
    h.settle();
    assert!(is_open(&mut h, names::SESSION), "Down opens");
    h.key(key::DOWN);
    h.key(key::ENTER);
    h.settle();
    assert!(!is_open(&mut h, names::SESSION));
    assert_eq!(get(&mut h, names::SESSION_NAME), "Sway");
    assert_eq!(h.state().chosen_session().unwrap().name, "Sway");
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
    let pop = open(&mut h, names::POWER);
    for id in [names::SUSPEND, names::REBOOT, names::POWEROFF] {
        assert!(
            introspect::resolve(h.ui(), &format!("window[1]/{id}")).is_some(),
            "no {id} in the power menu"
        );
    }
    click_row(&mut h, names::SUSPEND);
    assert!(!h.ui().has_window(pop));
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
    for n in [names::SESSION, names::SESSION_NAME, names::POWER] {
        assert!(!shown(&mut h, n), "{n} is greeter-only");
    }
}

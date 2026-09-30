//! `nitro-greeter`: the lock screen, and later the greetd greeter.
//!
//! One app, two backends (`docs/greeter.md`, decision 6). The screen
//! renders PAM's conversation, whatever it asks, through the pure state
//! machine in [`conv`]. It talks to an authenticator through [`backend`]:
//! `nitro-auth` in lock mode, greetd in greeter mode (plan step 5, not
//! built yet).
//!
//! ```text
//! ┌──────────────────────────────┐
//! │            14:05             │  clock
//! │  [ alice                  ]  │  user     (Enter submits)
//! │  Password:                   │  prompt   (PAM's text)
//! │  [ ••••••                 ]  │  answer   (masked for `secret`)
//! │  Authentication failure      │  message  (notices and errors)
//! │  Checking…                   │  status   (while waiting)
//! │  [ Log out ]                 │  logout   (another user's name only)
//! └──────────────────────────────┘
//! ```
//!
//! Every widget is named, so `hey nitro-greeter get window/message value`
//! works; `window/answer`'s `value` is the mask, never the text.
//!
//! **Lock mode** (`nitro-greeter --lock`, what `nitro-session --locked`
//! and the bar's Lock action run): the window is [`Surface::lock`]. Right
//! after it opens the app sends `Lock`, which takes over an ownerless
//! lock (a server started with `NITRO_LOCKED=1`, or a crashed
//! predecessor), and starts a conversation for the session's owner so
//! the password prompt has the keyboard at once. A wrong password shows
//! PAM's reason and asks again; success sends `Unlock` and exits 0.
//! A different name starts no conversation at all: another user never
//! types a password into this session (decision 6). They are told to log
//! out, and offered the button.

pub mod backend;
pub mod conv;

use std::path::PathBuf;

use nitro_bar::clock::{self, Zone};
use nitro_login::{ErrorKind, MessageKind, Request};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::widgets::{Label, TextField, button, column, label, panel, text_field};
use nitro_ui::{App, ColorRole, CrossAlign, FdToken, MainAlign, Size, Surface, Ui, WidgetId};

pub use backend::{AuthHelper, Backend};
pub use conv::{Conversation, State};

/// The app's name: its introspection socket and its `hey` address.
pub const APP_NAME: &str = "nitro-greeter";

/// The widget names, for `hey` and the tests.
pub mod names {
    /// The clock label.
    pub const CLOCK: &str = "clock";
    /// The user-name field.
    pub const USER: &str = "user";
    /// PAM's question.
    pub const PROMPT: &str = "prompt";
    /// The answer field: masked for a `secret` question.
    pub const ANSWER: &str = "answer";
    /// Notices and errors.
    pub const MESSAGE: &str = "message";
    /// "Checking…" while the authenticator works.
    pub const STATUS: &str = "status";
    /// Log out, offered when someone else's name was typed.
    pub const LOGOUT: &str = "logout";
}

/// What the screen is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Unlock the owner's session: `nitro-auth`, `Lock`/`Unlock`.
    Lock,
}

/// Said when the helper goes away mid-conversation.
pub const HELPER_EXITED: &str = "authentication helper exited";

#[derive(Debug, Clone, Copy)]
struct Ids {
    clock: WidgetId,
    user: WidgetId,
    prompt: WidgetId,
    answer: WidgetId,
    message: WidgetId,
    status: WidgetId,
    logout: WidgetId,
}

/// The app's state.
pub struct Greeter {
    mode: Mode,
    owner: String,
    conv: Conversation,
    backend: Box<dyn Backend>,
    watch: Option<FdToken>,
    ids: Option<Ids>,
    zone: Zone,
    /// Someone else's name was typed: the logout path is on screen.
    other_user: Option<String>,
    /// What the logout button reported, if it failed.
    logout_error: Option<String>,
    session_socket: Option<PathBuf>,
    unlocked: bool,
}

impl Greeter {
    /// A lock screen for `owner`'s session, authenticating through
    /// `backend`.
    #[must_use]
    pub fn lock(owner: impl Into<String>, backend: Box<dyn Backend>) -> Self {
        Self {
            mode: Mode::Lock,
            owner: owner.into(),
            conv: Conversation::new(),
            backend,
            watch: None,
            ids: None,
            zone: Zone::local(),
            other_user: None,
            logout_error: None,
            session_socket: None,
            unlocked: false,
        }
    }

    /// Use this `session.sock` for the logout button instead of the
    /// session's default.
    #[must_use]
    pub fn with_session_socket(mut self, path: PathBuf) -> Self {
        self.session_socket = Some(path);
        self
    }

    /// The conversation.
    #[must_use]
    pub fn conversation(&self) -> &Conversation {
        &self.conv
    }

    /// Whose session this is.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the session was unlocked (the app is quitting).
    #[must_use]
    pub fn unlocked(&self) -> bool {
        self.unlocked
    }

    /// The name on the logout path, if another user's name was typed.
    #[must_use]
    pub fn other_user(&self) -> Option<&str> {
        self.other_user.as_deref()
    }
}

/// Wall clock, milliseconds since the epoch.
fn now_ms() -> i64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Realtime);
    t.tv_sec
        .saturating_mul(1_000)
        .saturating_add(t.tv_nsec / 1_000_000)
}

const TEXT: f32 = 15.0;
const CARD_W: f32 = 320.0;

/// Build the tree: a full-screen background with one centred card.
///
/// Public so the tests build what the binary builds.
///
/// # Panics
/// Never in practice: every `attach` names an id just built.
pub fn build(ui: &mut Ui<Greeter>) -> WidgetId {
    let clock = ui.build(label("").name(names::CLOCK).size(40.0).weight(300));
    let user = ui.build(
        text_field("")
            .name(names::USER)
            .placeholder("User name")
            .size(TEXT)
            .width_percent(1.0)
            .on_submit(|_s: &mut Greeter, ui: &mut Ui<Greeter>, t: &str| {
                let name = t.trim().to_owned();
                ui.defer(move |s, ui| submit_user(s, ui, &name));
            }),
    );
    let prompt = ui.build(label("").name(names::PROMPT).size(TEXT));
    let answer = ui.build(
        text_field("")
            .name(names::ANSWER)
            .secret()
            .size(TEXT)
            .width_percent(1.0)
            .on_submit(|_s: &mut Greeter, ui: &mut Ui<Greeter>, t: &str| {
                // The field is out of its slot during its own callback,
                // so clearing it waits for the deferred step.
                let text = t.to_owned();
                ui.defer(move |s, ui| submit_answer(s, ui, text));
            }),
    );
    let message = ui.build(label("").name(names::MESSAGE).size(TEXT - 2.0));
    let status = ui.build(
        label("Checking…")
            .name(names::STATUS)
            .size(TEXT - 2.0)
            .color_role(ColorRole::TextDim),
    );
    let logout = ui.build(
        button("Log out")
            .name(names::LOGOUT)
            .on_click(|s: &mut Greeter, ui: &mut Ui<Greeter>| logout(s, ui)),
    );
    let card = ui.build(
        panel()
            .background_role(ColorRole::Surface)
            .radius(12.0)
            .padding(24.0)
            .gap(10.0)
            .cross_align(CrossAlign::Center)
            .width(CARD_W),
    );
    for c in [clock, user, prompt, answer, message, status, logout] {
        ui.attach(card, c).unwrap();
    }
    let root = ui.build(
        column()
            .main_align(MainAlign::Center)
            .cross_align(CrossAlign::Center)
            .width_percent(1.0)
            .height_percent(1.0),
    );
    ui.attach(root, card).unwrap();
    let ids = Ids {
        clock,
        user,
        prompt,
        answer,
        message,
        status,
        logout,
    };
    // The state is not reachable from `build`; the loop runs a zero
    // timer on its first turn, before the first frame is presented.
    ui.set_timer(0, move |s: &mut Greeter, ui: &mut Ui<Greeter>| {
        s.ids = Some(ids);
        start(s, ui);
    });
    root
}

/// Take the lock and ask for the owner's password.
fn start(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    tick_clock(s, ui);
    match s.mode {
        Mode::Lock => {
            if let Err(e) = ui.lock_session() {
                eprintln!("nitro-greeter: Lock: {e}");
            }
            let owner = s.owner.clone();
            if let Some(ids) = s.ids
                && let Ok(mut f) = ui.widget_mut::<TextField<Greeter>>(ids.user)
            {
                f.set_text(owner.clone());
            }
            submit_user(s, ui, &owner);
        }
    }
}

fn tick_clock(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    let now = now_ms();
    if let Some(ids) = s.ids
        && let Ok(mut l) = ui.widget_mut::<Label>(ids.clock)
    {
        l.set_text(clock::format_hm(now.div_euclid(1_000), &s.zone));
    }
    ui.set_timer(clock::ms_to_next_minute(now), tick_clock);
}

/// Send requests; a backend that fails loses the conversation.
fn send_all(s: &mut Greeter, ui: &mut Ui<Greeter>, reqs: Vec<Request>) {
    for mut req in reqs {
        let sent = s.backend.send(&req);
        if let Request::PostAuthMessageResponse {
            response: Some(r), ..
        } = &mut req
        {
            nitro_login::ipc::wipe_string(r);
        }
        if let Err(e) = sent {
            unwatch(s, ui);
            s.conv.lost(&format!("{HELPER_EXITED}: {e}"));
            return;
        }
    }
    watch(s, ui);
}

/// Register the backend's descriptor with the loop, once.
fn watch(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    if s.watch.is_some() {
        return;
    }
    let Some(fd) = s.backend.fd() else { return };
    match ui.add_fd(fd, readable) {
        Ok(t) => s.watch = Some(t),
        Err(e) => eprintln!("nitro-greeter: watching the helper: {e}"),
    }
}

fn unwatch(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    if let Some(t) = s.watch.take() {
        ui.remove_fd(t);
    }
}

/// The backend's descriptor is readable.
fn readable(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    let Ok(resps) = s.backend.read() else {
        unwatch(s, ui);
        s.conv.lost(HELPER_EXITED);
        render(s, ui);
        return;
    };
    // It answered and then exited. Stop watching its descriptor (at EOF
    // it would wake the loop for ever) *before* the responses run: one
    // may respawn the helper, whose new descriptor must be watched.
    let gone = s.backend.fd().is_none();
    if gone {
        unwatch(s, ui);
    }
    for r in resps {
        on_response(s, ui, r);
    }
    if gone && s.backend.fd().is_none() && s.conv.busy() {
        s.conv.lost(HELPER_EXITED);
        render(s, ui);
    }
}

/// One response from the authenticator. The fd hook calls it, and so
/// do the tests, with a scripted backend.
pub fn on_response(s: &mut Greeter, ui: &mut Ui<Greeter>, resp: nitro_login::Response) {
    let reqs = s.conv.on_response(resp);
    send_all(s, ui, reqs);
    match s.conv.state() {
        State::Authenticated => {
            unlock(s, ui);
            return;
        }
        // A wrong password: the reason is on screen, and the same name
        // is asked again at once. One field, not two. Only when the user
        // actually answered something: a failure PAM reached on its own
        // (a locked or expired account) would otherwise loop create ->
        // error with no input, logging and bumping faillock each time.
        // Then the error stays up and Enter in the name field retries.
        State::User
            if s.conv
                .last_error()
                .is_some_and(|e| e.0 == ErrorKind::AuthError)
                && s.conv.answered()
                && s.conv.username() == s.owner
                && !s.conv.busy() =>
        {
            let reqs = s.conv.submit_user(&s.owner.clone());
            send_all(s, ui, reqs);
        }
        _ => {}
    }
    render(s, ui);
}

/// The user submitted a name.
fn submit_user(s: &mut Greeter, ui: &mut Ui<Greeter>, name: &str) {
    s.logout_error = None;
    if s.mode == Mode::Lock && name != s.owner {
        let reqs = s.conv.cancel();
        send_all(s, ui, reqs);
        s.other_user = (!name.is_empty()).then(|| name.to_owned());
        render(s, ui);
        return;
    }
    s.other_user = None;
    let reqs = s.conv.submit_user(name);
    send_all(s, ui, reqs);
    render(s, ui);
}

/// The user answered PAM's question.
fn submit_answer(s: &mut Greeter, ui: &mut Ui<Greeter>, mut text: String) {
    if let Some(ids) = s.ids
        && let Ok(mut f) = ui.widget_mut::<TextField<Greeter>>(ids.answer)
    {
        f.set_text("");
    }
    let reqs = s.conv.answer(std::mem::take(&mut text));
    nitro_login::ipc::wipe_string(&mut text);
    send_all(s, ui, reqs);
    render(s, ui);
}

fn unlock(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    if let Err(e) = ui.unlock_session() {
        eprintln!("nitro-greeter: Unlock: {e}");
    }
    let _ = ui.flush();
    unwatch(s, ui);
    s.backend.close();
    s.unlocked = true;
    ui.quit();
}

fn logout(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    let path = s
        .session_socket
        .clone()
        .or_else(nitro_system::session::default_socket_path);
    let res = match path {
        Some(p) => nitro_system::session::request(&p, nitro_system::session::Action::Logout),
        None => Err("no session socket".to_owned()),
    };
    s.logout_error = res.err().map(|e| format!("Could not log out: {e}"));
    render(s, ui);
}

/// The message line: what is wrong, or what PAM said.
fn message(s: &Greeter) -> (String, ColorRole) {
    if let Some(e) = &s.logout_error {
        return (e.clone(), ColorRole::Danger);
    }
    if let Some(name) = &s.other_user {
        return (
            format!(
                "Only {} can unlock this session. Log out to sign in as {name} — unsaved work will be lost.",
                s.owner
            ),
            ColorRole::Warning,
        );
    }
    if let Some((kind, text)) = s.conv.last_error() {
        let role = match kind {
            ErrorKind::AuthError => ColorRole::Danger,
            ErrorKind::Error => ColorRole::Warning,
        };
        return (text.clone(), role);
    }
    match s.conv.notices().last() {
        Some((MessageKind::Error, t)) => (t.clone(), ColorRole::Danger),
        Some((_, t)) => (t.clone(), ColorRole::Text),
        None => (String::new(), ColorRole::Text),
    }
}

/// Bring the widgets in line with the state.
fn render(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    let Some(ids) = s.ids else { return };
    let prompting = match s.conv.state() {
        State::Prompt { kind, text } => Some((*kind, text.clone())),
        _ => None,
    };
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.prompt) {
        l.set_text(prompting.as_ref().map_or("", |p| p.1.as_str()));
    }
    if let Some((kind, _)) = &prompting
        && let Ok(mut f) = ui.widget_mut::<TextField<Greeter>>(ids.answer)
    {
        f.set_secret(*kind == MessageKind::Secret);
    }
    ui.set_collapsed(ids.prompt, prompting.is_none());
    ui.set_collapsed(ids.answer, prompting.is_none());
    let (text, role) = message(s);
    ui.set_collapsed(ids.message, text.is_empty());
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.message) {
        l.set_text(text);
        l.set_color_role(role);
    }
    ui.set_collapsed(ids.status, *s.conv.state() != State::Waiting);
    ui.set_collapsed(ids.logout, s.other_user.is_none());
    if prompting.is_some() {
        ui.focus(ids.answer);
    } else if *s.conv.state() == State::User {
        ui.focus(ids.user);
    }
}

/// The lock screen's shell surface.
#[must_use]
pub fn surface() -> Surface {
    Surface::lock()
}

/// Run lock mode: `nitro-greeter --lock`.
///
/// # Errors
/// No shell socket, or any wire failure. The session stays locked
/// either way: that is the server's rule, not this app's.
pub fn run_lock() -> Result<(), Box<dyn std::error::Error>> {
    let owner = nitro_login::owner().ok_or("cannot tell whose session this is")?;
    let helper = AuthHelper::new(AuthHelper::locate());
    App::shell(APP_NAME)?
        .title("nitro-greeter")
        .surface(surface())
        .size(Size::new(640.0, 480.0))
        .run(Greeter::lock(owner, Box::new(helper)), build)?;
    Ok(())
}

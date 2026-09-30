//! `nitro-greeter`: the lock screen, and greetd's greeter.
//!
//! One app, two backends (`docs/greeter.md`, decision 6). The screen
//! renders PAM's conversation, whatever it asks, through the pure state
//! machine in [`conv`]. It talks to an authenticator through [`backend`]:
//! `nitro-auth` in lock mode, greetd ([`greetd::Greetd`]) in greeter mode.
//!
//! ```text
//! ┌──────────────────────────────────────────┐
//! │     ┌──────────────────────────────┐     │
//! │     │            14:05             │     │  clock
//! │     │  [ alice                  ]  │     │  user     (Enter submits)
//! │     │  Password:                   │     │  prompt   (PAM's text)
//! │     │  [ ••••••                 ]  │     │  answer   (masked for `secret`)
//! │     │  Authentication failure      │     │  message  (notices and errors)
//! │     │  Checking…                   │     │  status   (while waiting)
//! │     │  [ Log out ]                 │     │  logout   (another user's name only)
//! │     └──────────────────────────────┘     │
//! │ (▭) Nitro                            (⏻) │  session · session-name · power
//! └──────────────────────────────────────────┘
//! ```
//!
//! The bottom bar is greeter-only. `session` (bottom-left) is an icon
//! button opening a menu of the sessions, the chosen one marked;
//! `session-name` shows the choice. `power` (bottom-right) opens a menu
//! of Suspend / Restart / Power off. Both are reachable with Tab and
//! their menus are keyboard-driven ([`nitro_ui::menu`]).
//!
//! Every widget is named, so `hey nitro-greeter get window/message value`
//! works; `window/answer`'s `value` is the mask, never the text. An open
//! menu is the popup window `window[1]`, its rows named by item id:
//! `window[1]/sway`, `window[1]/poweroff`.
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
//!
//! **Greeter mode** (`nitro-greeter` with no flag, what `nitro-session
//! --greeter` runs under greetd): the same window ([`Surface::lock`]:
//! full output, focusable, and nothing else is on screen), no `Lock`.
//! The remembered user is prefilled and asked for at once; otherwise the
//! name field has the keyboard. After `success` the chosen session's
//! command goes to greetd as `start_session`; its `success` saves the
//! remembered user and session and exits 0, which ends the greeter's
//! whole session so greetd can start the user's.

pub mod backend;
pub mod conv;
pub mod greetd;
pub mod sessions;
pub mod state;

use std::path::PathBuf;

use nitro_bar::clock::{self, Zone};
use nitro_login::{ErrorKind, MessageKind, Request};
use nitro_system::session::Action;
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::widgets::{Label, TextField, button, column, label, panel, row, spacer, text_field};
use nitro_ui::{
    App, ColorRole, CrossAlign, FdToken, MainAlign, MenuButton, MenuEntry, MenuItem, Size, Surface,
    Ui, WidgetId, menu_button,
};

pub use backend::{AuthHelper, Backend};
pub use conv::{Conversation, State};
pub use sessions::SessionEntry;
pub use state::Remembered;

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
    /// The session menu button (greeter). Its menu's rows are named by
    /// [`sessions::menu_ids`](crate::sessions::menu_ids) (`window[1]/sway`),
    /// their value is whether the session is the chosen one.
    pub const SESSION: &str = "session";
    /// The chosen session's name, next to the session button (greeter).
    pub const SESSION_NAME: &str = "session-name";
    /// The power menu button (greeter).
    pub const POWER: &str = "power";
    /// Suspend: a power menu item, `window[1]/suspend` while it is open.
    pub const SUSPEND: &str = "suspend";
    /// Reboot: a power menu item, `window[1]/reboot` while it is open.
    pub const REBOOT: &str = "reboot";
    /// Power off: a power menu item, `window[1]/poweroff` while it is
    /// open.
    pub const POWEROFF: &str = "poweroff";
}

/// What the screen is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Unlock the owner's session: `nitro-auth`, `Lock`/`Unlock`.
    Lock,
    /// greetd's greeter: log somebody in and start their session.
    Greeter,
}

/// Said when the helper goes away mid-conversation.
pub const HELPER_EXITED: &str = "authentication helper exited";

/// Said when greetd's socket goes away mid-conversation.
pub const GREETD_LOST: &str = "greetd connection lost";

#[derive(Debug, Clone, Copy)]
struct Ids {
    clock: WidgetId,
    user: WidgetId,
    prompt: WidgetId,
    answer: WidgetId,
    message: WidgetId,
    status: WidgetId,
    logout: WidgetId,
    session: WidgetId,
    session_name: WidgetId,
    power: WidgetId,
    bar: WidgetId,
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
    /// Greeter: what can be started, and which is chosen.
    sessions: Vec<SessionEntry>,
    /// The session menu's item ids, one per entry.
    session_ids: Vec<String>,
    chosen: usize,
    remembered: Remembered,
    /// Greeter: greetd accepted the session; the app is quitting.
    started: bool,
    /// What a power button reported, if it failed.
    power_error: Option<String>,
    /// Where to remember the login; `None` (the tests) remembers nothing.
    state_file: Option<PathBuf>,
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
            sessions: Vec::new(),
            session_ids: Vec::new(),
            chosen: 0,
            remembered: Remembered::default(),
            started: false,
            power_error: None,
            state_file: None,
        }
    }

    /// greetd's greeter over `backend`, offering `sessions` (never
    /// empty in practice: [`sessions::sessions`] puts nitro first), with
    /// the last login's user and session as defaults.
    #[must_use]
    pub fn login(
        backend: Box<dyn Backend>,
        sessions: Vec<SessionEntry>,
        remembered: Remembered,
    ) -> Self {
        let chosen = remembered
            .session
            .as_ref()
            .and_then(|n| sessions.iter().position(|e| &e.name == n))
            .unwrap_or(0);
        let mut g = Self::lock(String::new(), backend);
        g.mode = Mode::Greeter;
        g.session_ids = sessions::menu_ids(&sessions);
        g.sessions = sessions;
        g.chosen = chosen;
        g.remembered = remembered;
        g
    }

    /// Remember each login in `path` ([`state::save`]).
    #[must_use]
    pub fn with_state_file(mut self, path: PathBuf) -> Self {
        self.state_file = Some(path);
        self
    }

    /// The session that would be started (greeter).
    #[must_use]
    pub fn chosen_session(&self) -> Option<&SessionEntry> {
        self.sessions.get(self.chosen)
    }

    /// Whether greetd accepted the session (the app is quitting).
    #[must_use]
    pub fn started(&self) -> bool {
        self.started
    }

    /// What should be remembered now: the user and session of the
    /// login greetd just accepted.
    #[must_use]
    pub fn remembered(&self) -> &Remembered {
        &self.remembered
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

/// Build the tree: a full-screen background with one centred card and,
/// under it, the greeter's bar of session and power menus.
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
    let (bar, session, session_name, power) = bottom_bar(ui);
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
    let above = ui.build(spacer().grow(1.0));
    let below = ui.build(spacer().grow(1.0));
    for c in [above, card, below, bar] {
        ui.attach(root, c).unwrap();
    }
    let ids = Ids {
        clock,
        user,
        prompt,
        answer,
        message,
        status,
        logout,
        session,
        session_name,
        power,
        bar,
    };
    // The state is not reachable from `build`; the loop runs a zero
    // timer on its first turn, before the first frame is presented.
    ui.set_timer(0, move |s: &mut Greeter, ui: &mut Ui<Greeter>| {
        s.ids = Some(ids);
        start(s, ui);
    });
    root
}

/// The greeter's bar: the session menu and its name bottom-left, the
/// power menu bottom-right. Returns the bar, session, session-name and
/// power ids.
fn bottom_bar(ui: &mut Ui<Greeter>) -> (WidgetId, WidgetId, WidgetId, WidgetId) {
    let session = ui.build(
        menu_button("display")
            .name(names::SESSION)
            .label("Session")
            .on_select(|_s: &mut Greeter, ui: &mut Ui<Greeter>, id: &str| {
                let id = id.to_owned();
                ui.defer(move |s, ui| pick_session(s, ui, &id));
            }),
    );
    let session_name = ui.build(
        label("")
            .name(names::SESSION_NAME)
            .size(TEXT - 2.0)
            .color_role(ColorRole::TextDim),
    );
    let power = ui.build(
        menu_button("power")
            .name(names::POWER)
            .label("Power")
            .align_right(true)
            .item(MenuItem::new(names::SUSPEND, "Suspend").icon("moon"))
            .item(MenuItem::new(names::REBOOT, "Restart").icon("bootstrap-reboot"))
            .item(MenuItem::new(names::POWEROFF, "Power off").icon("power"))
            .on_select(|_s: &mut Greeter, ui: &mut Ui<Greeter>, id: &str| {
                let action = match id {
                    names::SUSPEND => Action::Suspend,
                    names::REBOOT => Action::Reboot,
                    names::POWEROFF => Action::Poweroff,
                    _ => return,
                };
                ui.defer(move |s, ui| power(s, ui, action));
            }),
    );
    let bar = ui.build(
        row()
            .width_percent(1.0)
            .padding(16.0)
            .gap(8.0)
            .cross_align(CrossAlign::Center),
    );
    let fill = ui.build(spacer().grow(1.0));
    for c in [session, session_name, fill, power] {
        ui.attach(bar, c).unwrap();
    }
    (bar, session, session_name, power)
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
        Mode::Greeter => {
            // A crashed predecessor may have left greetd mid-conversation.
            let reqs = s.conv.reset();
            send_all(s, ui, reqs);
            let user = s.remembered.user.clone().filter(|u| !u.is_empty());
            if let Some(ids) = s.ids {
                if let Ok(mut f) = ui.widget_mut::<TextField<Greeter>>(ids.user) {
                    f.set_text(user.clone().unwrap_or_default());
                }
                ui.set_collapsed(ids.logout, true);
                let items: Vec<MenuEntry> = s
                    .sessions
                    .iter()
                    .zip(&s.session_ids)
                    .enumerate()
                    .map(|(i, (e, id))| {
                        MenuItem::new(id.clone(), e.name.clone())
                            .radio(i == s.chosen)
                            .into()
                    })
                    .collect();
                if let Ok(mut b) = ui.widget_mut::<MenuButton<Greeter>>(ids.session) {
                    b.set_items(items);
                }
            }
            match user {
                Some(u) => submit_user(s, ui, &u),
                None => render(s, ui),
            }
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
            s.conv.lost(&format!("{}: {e}", lost_text(s)));
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
        s.conv.lost(lost_text(s));
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
        s.conv.lost(lost_text(s));
        render(s, ui);
    }
}

fn lost_text(s: &Greeter) -> &'static str {
    match s.mode {
        Mode::Lock => HELPER_EXITED,
        Mode::Greeter => GREETD_LOST,
    }
}

/// One response from the authenticator. The fd hook calls it, and so
/// do the tests, with a scripted backend.
pub fn on_response(s: &mut Greeter, ui: &mut Ui<Greeter>, resp: nitro_login::Response) {
    let reqs = s.conv.on_response(resp);
    send_all(s, ui, reqs);
    match s.conv.state() {
        State::Authenticated if s.mode == Mode::Lock => {
            unlock(s, ui);
            return;
        }
        State::Authenticated => {
            if let Some(e) = s.sessions.get(s.chosen).cloned() {
                let reqs = s.conv.start_session(e.cmd.clone(), e.env());
                send_all(s, ui, reqs);
            }
        }
        State::Started => {
            handed_off(s, ui);
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
                && (s.mode == Mode::Greeter || s.conv.username() == s.owner)
                && !s.conv.busy() =>
        {
            let name = s.conv.username().to_owned();
            let reqs = s.conv.submit_user(&name);
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
    if s.mode == Mode::Greeter && name.is_empty() {
        render(s, ui);
        return;
    }
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

/// greetd accepted the session: remember the login, hang up, exit 0.
fn handed_off(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    s.remembered = Remembered {
        user: Some(s.conv.username().to_owned()),
        session: s.sessions.get(s.chosen).map(|e| e.name.clone()),
    };
    if let Some(p) = &s.state_file {
        state::save(p, &s.remembered);
    }
    unwatch(s, ui);
    s.backend.close();
    s.started = true;
    ui.quit();
}

/// The session menu: `id` was picked.
fn pick_session(s: &mut Greeter, ui: &mut Ui<Greeter>, id: &str) {
    if let Some(i) = s.session_ids.iter().position(|x| x == id) {
        s.chosen = i;
    }
    render(s, ui);
}

/// The power menu: ask our own `nitro-session`.
fn power(s: &mut Greeter, ui: &mut Ui<Greeter>, action: Action) {
    let path = s
        .session_socket
        .clone()
        .or_else(nitro_system::session::default_socket_path);
    let res = match path {
        Some(p) => nitro_system::session::request(&p, action),
        None => Err("no session socket".to_owned()),
    };
    s.power_error = res.err();
    render(s, ui);
}

fn logout(s: &mut Greeter, ui: &mut Ui<Greeter>) {
    let path = s
        .session_socket
        .clone()
        .or_else(nitro_system::session::default_socket_path);
    let res = match path {
        Some(p) => nitro_system::session::request(&p, Action::Logout),
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
    if let Some(e) = &s.power_error {
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
    let greeter = s.mode == Mode::Greeter;
    for id in [ids.bar, ids.session, ids.session_name, ids.power] {
        ui.set_collapsed(id, !greeter);
    }
    if greeter && let Some(e) = s.sessions.get(s.chosen) {
        if let Ok(mut l) = ui.widget_mut::<Label>(ids.session_name) {
            l.set_text(e.name.clone());
        }
        if let (Some(id), Ok(mut b)) = (
            s.session_ids.get(s.chosen),
            ui.widget_mut::<MenuButton<Greeter>>(ids.session),
        ) {
            b.set_checked(id, true);
        }
    }
    if prompting.is_some() {
        ui.focus(ids.answer);
    } else if *s.conv.state() == State::User {
        ui.focus(ids.user);
    }
}

/// The shell surface, for both modes: an overlay covering the output,
/// focusable. The greeter has nothing else on screen, so it needs no
/// other kind of window.
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

/// Run greeter mode: `nitro-greeter` under greetd. Returns once greetd
/// accepted a session.
///
/// # Errors
/// Not under greetd (`$GREETD_SOCK` unset), no shell socket, or a wire
/// failure. `nitro-session --greeter` restarts the greeter on any of
/// them.
pub fn run_greeter() -> Result<(), Box<dyn std::error::Error>> {
    let backend = greetd::Greetd::from_env()?;
    let greeter = Greeter::login(Box::new(backend), sessions::sessions(), state::load())
        .with_state_file(state::path());
    App::shell(APP_NAME)?
        .title("nitro-greeter")
        .surface(surface())
        .size(Size::new(640.0, 480.0))
        .run(greeter, build)?;
    Ok(())
}

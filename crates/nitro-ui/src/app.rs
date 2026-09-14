//! [`App`]: connect, open a window, and run the epoll loop.
//!
//! The loop is the same shape as the server's — level-triggered epoll
//! over the connection fd plus whatever the app registered with
//! [`Ui::add_fd`] — and it has the same property: with nothing happening
//! it blocks in `epoll_wait` and no bytes move. After every batch of
//! events it calls [`Ui::flush`], which sends a commit only if a pass
//! produced a mutation.

use std::os::fd::AsFd;

use nitro_core::Size;
use nitro_wire::client::Connection;
use rustix::event::epoll::{self, EventData, EventFlags};

use crate::arena::WidgetId;
use crate::error::Error;
use crate::theme::Theme;
use crate::ui::Ui;

/// An app: one connection, one window, one widget tree.
///
/// ```no_run
/// use nitro_ui::{App, Ui};
/// use nitro_ui::widgets::{column, label};
/// use nitro_ui::build::ContainerBuilder as _;
///
/// # fn main() -> Result<(), nitro_ui::Error> {
/// App::new("hello")?.run(0u32, |ui: &mut Ui<u32>| {
///     ui.build(column().child(label("Hello")))
/// })
/// # }
/// ```
pub struct App {
    conn: Connection,
    title: String,
    theme: Theme,
    size: Option<Size>,
    backdrop: bool,
    introspect: bool,
    name: String,
    surface: Option<crate::shell::Surface>,
}

impl App {
    /// Connect to the server named by `NITRO_SOCKET` (or the default
    /// path) and announce ourselves as `name`.
    ///
    /// # Errors
    /// Any connection or handshake failure.
    pub fn new(name: &str) -> Result<Self, Error> {
        Ok(Self::with_connection(
            Connection::connect_default(name)?,
            name,
        ))
    }

    /// Connect to the **shell** socket named by `NITRO_SHELL_SOCKET` (or
    /// the default path) and announce ourselves as `name`.
    ///
    /// A connection made here is privileged: it carries `caps::SHELL` and
    /// may send the shell ops. The socket *is* the capability — a client
    /// is privileged because of where it connected, not because of
    /// anything it sent (`docs/shell.md`).
    ///
    /// This **fails** rather than falling back to the ordinary socket. A
    /// bar that silently downgraded would come up looking right and then
    /// be killed by the first shell op it sent, which is a much harder
    /// failure to read than "could not connect".
    ///
    /// # Errors
    /// Any connection or handshake failure — in particular a server too
    /// old to have the shell socket, or a wrong `XDG_RUNTIME_DIR`.
    pub fn shell(name: &str) -> Result<Self, Error> {
        Ok(Self::with_connection(
            Connection::connect_shell(name)?,
            name,
        ))
    }

    /// Use an already-connected socket. What the test harness does.
    #[must_use]
    pub fn with_connection(conn: Connection, title: &str) -> Self {
        Self {
            conn,
            title: title.to_owned(),
            theme: Theme::default(),
            size: None,
            backdrop: true,
            introspect: true,
            name: title.to_owned(),
            surface: None,
        }
    }

    /// Open the window as a shell surface — a bar, dock, launcher or
    /// wallpaper — rather than as an ordinary application window.
    ///
    /// The layer, the flags, the anchor and the exclusive zone are all
    /// applied in the window's **first commit**; see
    /// [`crate::shell::Surface`].
    ///
    /// It does not connect anything: pair it with [`App::shell`], which
    /// is where the privilege comes from.
    #[must_use]
    pub fn surface(mut self, surface: crate::shell::Surface) -> Self {
        self.surface = Some(surface);
        self
    }

    /// Do not paint the theme's background behind the tree.
    ///
    /// By default the window root paints one, because the root widget
    /// paints nothing of its own and a transparent window puts the
    /// theme's dark text straight onto the desktop. An app that wants
    /// the desktop to show through — a HUD, a shaped window — asks for
    /// it here.
    #[must_use]
    pub fn transparent(mut self) -> Self {
        self.backdrop = false;
        self
    }

    /// Turn the introspection socket on or off (default: on).
    ///
    /// See `docs/introspection.md`: the socket is what makes an app
    /// scriptable from outside, and it is on by default because an app
    /// that has to opt in is an app nothing can drive. Turning it off
    /// costs the app its `hey` interface and, later, its accessibility.
    #[must_use]
    pub fn introspect(mut self, on: bool) -> Self {
        self.introspect = on;
        self
    }

    /// Set the window title (default: the app's name).
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Override the theme.
    #[must_use]
    pub fn theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Ask for a specific window size; without one the window is sized to
    /// the root's measured size.
    #[must_use]
    pub fn size(mut self, size: Size) -> Self {
        self.size = Some(size);
        self
    }

    /// Build the tree into a `Ui`, without running the loop. The harness
    /// and tests drive the result themselves.
    ///
    /// # Errors
    /// A wire failure, or [`Error::NoRoot`] if `build` returned a stale
    /// id.
    pub fn build<S: 'static>(
        self,
        build: impl FnOnce(&mut Ui<S>) -> WidgetId,
    ) -> Result<Ui<S>, Error> {
        let Self {
            conn,
            title,
            theme,
            size,
            backdrop,
            introspect: _,
            name,
            surface,
        } = self;
        let mut ui = Ui::new(conn, theme);
        ui.set_backdrop(backdrop);
        ui.set_app_id(&name);
        if let Some(s) = surface {
            ui.set_surface(s);
        }
        let root = build(&mut ui);
        ui.set_root(root)?;
        ui.open_window(&title, size)?;
        ui.flush()?;
        Ok(ui)
    }

    /// Build the tree and run the event loop until the app quits or the
    /// window closes.
    ///
    /// # Errors
    /// Any wire or `epoll` failure. They are all fatal.
    pub fn run<S: 'static>(
        self,
        mut state: S,
        build: impl FnOnce(&mut Ui<S>) -> WidgetId,
    ) -> Result<(), Error> {
        let (want_socket, name) = (self.introspect, self.name.clone());
        let mut ui = self.build(build)?;
        // The socket is best-effort: an app whose runtime directory is
        // unwritable is still an app, and refusing to start because
        // nothing can script it would be the wrong trade.
        let socket = if want_socket {
            crate::introspect::Socket::bind(&name).ok()
        } else {
            None
        };
        event_loop_with(&mut ui, &mut state, socket)
    }
}

/// Token for the connection fd in the epoll set. App fds use their own
/// raw number, which cannot collide: fd 0 is stdin and never registered.
const CONN_TOKEN: u64 = u64::MAX;
/// Token for the introspection listener.
const INTROSPECT_TOKEN: u64 = u64::MAX - 1;

/// Run `ui` until it quits.
///
/// # Errors
/// Any wire or `epoll` failure.
pub fn event_loop<S: 'static>(ui: &mut Ui<S>, state: &mut S) -> Result<(), Error> {
    event_loop_with(ui, state, None)
}

/// Run `ui` until it quits, serving `socket` alongside it.
///
/// The introspection listener and its clients live in the **same**
/// `epoll` set as the connection, and a request is executed between
/// events by this loop — never concurrently with the app's own code.
/// That is the `BeOS` property the design asks for: IPC and the app share
/// one message loop, so being scriptable costs neither a thread nor a
/// lock.
///
/// # Errors
/// Any wire or `epoll` failure.
pub fn event_loop_with<S: 'static>(
    ui: &mut Ui<S>,
    state: &mut S,
    socket: Option<crate::introspect::Socket>,
) -> Result<(), Error> {
    let epfd = epoll::create(epoll::CreateFlags::CLOEXEC)?;
    epoll::add(
        &epfd,
        ui.as_fd(),
        EventData::new_u64(CONN_TOKEN),
        EventFlags::IN,
    )?;
    let mut socket = socket;
    if let Some(s) = &socket {
        epoll::add(
            &epfd,
            s.as_fd(),
            EventData::new_u64(INTROSPECT_TOKEN),
            EventFlags::IN,
        )?;
    }
    let mut registered: Vec<u64> = Vec::new();
    // `epoll::Event` has no `Default`, so the buffer is built by hand.
    let mut events = [epoll::Event {
        flags: EventFlags::empty(),
        data: EventData::new_u64(0),
    }; 16];
    while !ui.should_quit() {
        sync_fds(&epfd, ui, &mut registered)?;
        // A connected introspection client is polled by the same loop,
        // so its readability has to be part of the wait. Registering
        // each client stream in the epoll set would mean tracking tokens
        // for sockets that come and go every second; the client count is
        // tiny and bounded, so they are polled with a short timeout
        // instead, and only while at least one is connected.
        let mut timeout = ui.next_timeout();
        if socket.as_ref().is_some_and(crate::introspect::Socket::busy) {
            timeout = Some(timeout.unwrap_or(10).min(10));
        }
        let timeout = timeout.map(|ms| rustix::time::Timespec {
            tv_sec: (ms / 1000).cast_signed(),
            tv_nsec: ((ms % 1000) * 1_000_000).cast_signed(),
        });
        let n = match epoll::wait(&epfd, &mut events[..], timeout.as_ref()) {
            Ok(n) => n,
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        };
        for e in &events[..n] {
            let token = e.data.u64();
            if token == CONN_TOKEN {
                ui.pump(state)?;
            } else if token == INTROSPECT_TOKEN {
                if let Some(s) = &mut socket {
                    s.accept();
                }
            } else {
                ui.run_fd(state, crate::ui::FdToken::from_raw(token));
            }
        }
        ui.run_timers(state);
        if let Some(s) = &mut socket {
            s.serve(ui, state);
        }
        ui.flush()?;
    }
    Ok(())
}

/// Bring the epoll set in line with the app's registered fds.
///
/// `registered` is keyed on the hook's **token**, not on its descriptor
/// number, and that is load-bearing: a descriptor number is recycled the
/// moment it is closed, so a hook removed and another added in the same
/// turn would take the same number and this function would decide it was
/// already in the set. It would not be — closing a descriptor removes it
/// from every `epoll` set — and the new hook would never fire. See
/// [`FdToken`](crate::FdToken) for the box run that found it.
fn sync_fds<S: 'static>(
    epfd: &impl AsFd,
    ui: &Ui<S>,
    registered: &mut Vec<u64>,
) -> Result<(), Error> {
    let want = ui.hook_fds();
    for (id, borrowed) in &want {
        if !registered.contains(id) {
            epoll::add(epfd, *borrowed, EventData::new_u64(*id), EventFlags::IN)?;
            registered.push(*id);
        }
    }
    // A hook that went away took its owned descriptor with it, and
    // closing a descriptor removes it from every epoll set: there is
    // nothing left to delete, only bookkeeping to drop.
    registered.retain(|id| want.iter().any(|(i, _)| i == id));
    Ok(())
}

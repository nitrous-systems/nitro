//! [`App`]: connect, open a window, and run the epoll loop.
//!
//! The loop is the same shape as the server's — level-triggered epoll
//! over the connection fd plus whatever the app registered with
//! [`Ui::add_fd`] — and it has the same property: with nothing happening
//! it blocks in `epoll_wait` and no bytes move. After every batch of
//! events it calls [`Ui::flush`], which sends a commit only if a pass
//! produced a mutation.

use std::os::fd::{AsFd, RawFd};

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
}

impl App {
    /// Connect to the server named by `NITRO_SOCKET` (or the default
    /// path) and announce ourselves as `name`.
    ///
    /// # Errors
    /// Any connection or handshake failure.
    pub fn new(name: &str) -> Result<Self, Error> {
        let conn = Connection::connect_default(name)?;
        Ok(Self {
            conn,
            title: name.to_owned(),
            theme: Theme::default(),
            size: None,
        })
    }

    /// Use an already-connected socket. What the test harness does.
    #[must_use]
    pub fn with_connection(conn: Connection, title: &str) -> Self {
        Self {
            conn,
            title: title.to_owned(),
            theme: Theme::default(),
            size: None,
        }
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
        } = self;
        let mut ui = Ui::new(conn, theme);
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
        let mut ui = self.build(build)?;
        event_loop(&mut ui, &mut state)
    }
}

/// Token for the connection fd in the epoll set. App fds use their own
/// raw number, which cannot collide: fd 0 is stdin and never registered.
const CONN_TOKEN: u64 = u64::MAX;

/// Run `ui` until it quits.
///
/// # Errors
/// Any wire or `epoll` failure.
pub fn event_loop<S: 'static>(ui: &mut Ui<S>, state: &mut S) -> Result<(), Error> {
    let epfd = epoll::create(epoll::CreateFlags::CLOEXEC)?;
    epoll::add(
        &epfd,
        ui.as_fd(),
        EventData::new_u64(CONN_TOKEN),
        EventFlags::IN,
    )?;
    let mut registered: Vec<RawFd> = Vec::new();
    // `epoll::Event` has no `Default`, so the buffer is built by hand.
    let mut events = [epoll::Event {
        flags: EventFlags::empty(),
        data: EventData::new_u64(0),
    }; 16];
    while !ui.should_quit() {
        sync_fds(&epfd, ui, &mut registered)?;
        let timeout = ui.next_timeout().map(|ms| rustix::time::Timespec {
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
            } else {
                ui.run_fd(state, crate::ui::FdToken::from_raw(token as RawFd));
            }
        }
        ui.run_timers(state);
        ui.flush()?;
    }
    Ok(())
}

/// Bring the epoll set in line with the app's registered fds.
fn sync_fds<S: 'static>(
    epfd: &impl AsFd,
    ui: &Ui<S>,
    registered: &mut Vec<RawFd>,
) -> Result<(), Error> {
    let want = ui.hook_fds();
    for (raw, borrowed) in &want {
        if !registered.contains(raw) {
            epoll::add(
                epfd,
                *borrowed,
                EventData::new_u64(*raw as u64),
                EventFlags::IN,
            )?;
            registered.push(*raw);
        }
    }
    // A hook that went away took its owned descriptor with it, and
    // closing a descriptor removes it from every epoll set: there is
    // nothing left to delete, only bookkeeping to drop.
    registered.retain(|raw| want.iter().any(|(r, _)| r == raw));
    Ok(())
}

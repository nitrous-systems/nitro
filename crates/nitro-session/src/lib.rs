//! **The session**: the process that turns "the box booted" into "there
//! is a desktop", and the one that takes it away again.
//!
//! `nitro-session` starts [`nitro-server`], waits for it to really be
//! answering, starts the wallpaper, the bar and the launcher, and then
//! sits in one `poll(2)` for the rest of the login. A shell piece that
//! dies is restarted with a backoff; the *server* exiting ends the
//! session, with the server's exit code. `SIGTERM` tears everything down
//! in reverse order.
//!
//! It also owns the power actions — `suspend`, `poweroff`, `reboot`,
//! `logout`, and an M4 `lock` — behind a line protocol on
//! `$XDG_RUNTIME_DIR/nitro/session.sock`. They are run through
//! `systemctl` rather than D-Bus; [`power`] argues that at length.
//!
//! # What each module is
//!
//! | module | question it answers |
//! |---|---|
//! | [`pieces`] | what runs, in what order, found where |
//! | [`child`] | how one process is started, watched and stopped |
//! | [`backoff`] | when a piece that died may be restarted |
//! | [`wait`] | when is the server *ready* (not: does the file exist) |
//! | [`socket`] | the session socket and its clients |
//! | [`power`] | the commands, and why `systemctl` and not `zbus` |
//! | [`session`] | the loop that ties them together |
//! | [`signals`] | SIGTERM → a descriptor |
//! | [`logging`] | `NITRO_LOG`, same levels and format as the server |
//!
//! # Why this is a separate process at all
//!
//! The server could have started the shell itself — it knows when its
//! sockets are up, it already has an event loop, and it would save a
//! process. It must not, for one reason: **a compositor that spawns its
//! own UI cannot be restarted without taking the UI with it, and cannot
//! be debugged by running it alone.** Keeping supervision out of the
//! server is also what keeps `just fake` honest — the server on the fake
//! backend is the same binary, with no shell attached, which is what
//! every test in this tree runs.
//!
//! The second reason is the power actions. `poweroff` is a privileged
//! request from a shell client, and the process that answers it should
//! not be the process holding DRM master and every client's buffers.
//!
//! [`nitro-server`]: https://example.invalid

pub mod backoff;
pub mod child;
pub mod logging;
pub mod pieces;
pub mod power;
pub mod session;
pub mod signals;
pub mod socket;
pub mod wait;

pub use session::{Config, Outcome, Session};

//! Turn the environment into a [`nitro_session::Config`] and run.
//!
//! - `NITRO_SOCKET` / `NITRO_SHELL_SOCKET` name the sockets the session
//!   waits for — the same variables the server binds and the clients
//!   connect to, resolved by the same `nitro-wire` functions, so the
//!   three agree without being told.
//! - `NITRO_SESSION_SOCKET` overrides the session socket; otherwise
//!   `$XDG_RUNTIME_DIR/nitro/session.sock`.
//! - `NITRO_SESSION_BIN_DIR` overrides where the pieces are looked for
//!   before `$PATH` (default: the directory `nitro-session` itself is
//!   in).
//! - `NITRO_SESSION_PIECES=a,b,c` overrides which shell pieces are
//!   started. The server is always first and is not in the list. An
//!   empty value starts the server alone, which is how you bisect "is
//!   this the compositor or the bar?" on the box without editing the
//!   unit.
//! - `NITRO_LOG=error|warn|info|debug`, as everywhere else.
//!
//! Everything else a piece needs it reads from the environment itself:
//! the session passes its own environment on unchanged, so
//! `NITRO_BACKEND=fake` in the unit reaches the server, and
//! `NITRO_FONT_DIRS` reaches the text stack, without this file knowing
//! either variable exists.

use std::path::PathBuf;
use std::process::ExitCode;

use nitro_session::pieces::{Piece, Role};
use nitro_session::{Config, Session, error, info, signals, socket, warn};

fn pieces_from_env() -> Vec<Piece> {
    let Ok(list) = std::env::var("NITRO_SESSION_PIECES") else {
        return nitro_session::pieces::PIECES.to_vec();
    };
    let mut out = vec![nitro_session::pieces::PIECES[0].clone()];
    for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        // Only the pieces this session knows how to supervise: the
        // variable is a *subset* selector, not a way to have the desktop
        // start an arbitrary program as root-adjacent as the compositor.
        match nitro_session::pieces::PIECES
            .iter()
            .find(|p| p.program == name && p.role == Role::Shell)
        {
            Some(p) => out.push(p.clone()),
            None => warn!("NITRO_SESSION_PIECES: {name:?} is not a shell piece; ignored"),
        }
    }
    out
}

fn config_from_env() -> Config {
    let wire_path =
        std::env::var_os("NITRO_SOCKET").map_or_else(nitro_wire::socket_path, PathBuf::from);
    let shell_path = std::env::var_os("NITRO_SHELL_SOCKET")
        .map_or_else(nitro_wire::shell_socket_path, PathBuf::from);
    let session = socket::resolve(
        std::env::var_os(socket::SOCKET_ENV)
            .map(PathBuf::from)
            .as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .as_deref(),
    );
    if let Some(w) = session.warning {
        warn!("{w}");
    }
    let mut config = Config::new(wire_path, shell_path, session.path);
    config.pieces = pieces_from_env();
    if let Some(dir) = std::env::var_os("NITRO_SESSION_BIN_DIR") {
        config.bin_dir = Some(PathBuf::from(dir));
    }
    config
}

fn main() -> ExitCode {
    let config = config_from_env();
    info!("nitro-session {} starting", env!("CARGO_PKG_VERSION"));
    // Installed *before* the first child, so a SIGTERM during start-up
    // is queued rather than killing a supervisor that has a compositor
    // holding the VT.
    let mut signals = match signals::Signals::install() {
        Ok(s) => s,
        Err(e) => {
            error!("signals: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut session = match Session::start(config) {
        Ok(s) => s,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    info!("session up; socket {}", session.socket_path().display());
    let outcome = session.run(Some(&mut signals));
    info!("session ended: {outcome:?}");
    ExitCode::from(outcome.code())
}

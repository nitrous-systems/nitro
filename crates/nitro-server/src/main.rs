//! Turn the environment into a [`nitro_server::Config`] and run.
//!
//! - `NITRO_BACKEND=fake` (`NITRO_FAKE_SIZE=WxH`, default 1280x720) or
//!   `drm` (default; `NITRO_DRM_CARD` picks the device).
//! - `NITRO_CONTROL` overrides the control socket path; otherwise
//!   `$XDG_RUNTIME_DIR/nitro/control.sock`.
//! - `NITRO_SOCKET` overrides the wire socket path; otherwise
//!   `$XDG_RUNTIME_DIR/nitro/wire.sock` (the path `nitro-wire` clients
//!   resolve to on their own, so the two agree without being told).
//! - `NITRO_INPUT_DIR` overrides where `event*` devices are looked for
//!   (default `/dev/input`); `NITRO_INPUT=off` disables input entirely,
//!   which is what a headless test wants.
//! - `NITRO_LOG=error|warn|info|debug`.

use std::path::PathBuf;
use std::process::ExitCode;

use nitro_server::{BackendKind, Config, control, error, info, warn};

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

fn config_from_env() -> Result<Config, String> {
    let backend = match std::env::var("NITRO_BACKEND").as_deref() {
        Ok("fake") => {
            let size = std::env::var("NITRO_FAKE_SIZE").ok();
            let (width, height) = match size.as_deref() {
                Some(s) => {
                    parse_size(s).ok_or_else(|| format!("NITRO_FAKE_SIZE={s:?}: want WxH"))?
                }
                None => (1280, 720),
            };
            BackendKind::Fake { width, height }
        }
        Ok("drm") | Err(_) => BackendKind::Drm {
            card: std::env::var_os("NITRO_DRM_CARD").map(PathBuf::from),
        },
        Ok(other) => return Err(format!("NITRO_BACKEND={other:?}: want drm or fake")),
    };
    let socket = control::resolve(
        std::env::var_os("NITRO_CONTROL")
            .map(PathBuf::from)
            .as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .as_deref(),
    );
    if let Some(w) = socket.warning {
        warn!("{w}");
    }
    // `nitro-wire` resolves the same path from the same variables, so a
    // client started in this environment finds this server without being
    // configured; `NITRO_SOCKET` overrides both ends at once.
    let wire_path =
        std::env::var_os("NITRO_SOCKET").map_or_else(nitro_wire::socket_path, PathBuf::from);
    // Input is on unless asked otherwise, and never on the fake backend,
    // which has no seat to open devices through.
    let input_dir = match std::env::var("NITRO_INPUT").as_deref() {
        Ok("off") => None,
        _ => Some(
            std::env::var_os("NITRO_INPUT_DIR")
                .map_or_else(|| PathBuf::from("/dev/input"), PathBuf::from),
        ),
    };
    Ok(Config {
        backend,
        control_path: socket.path,
        wire_path,
        handle_signals: true,
        fake_input: None,
        input_dir,
    })
}

fn main() -> ExitCode {
    let config = match config_from_env() {
        Ok(c) => c,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    info!("nitro-server {} starting", env!("CARGO_PKG_VERSION"));
    match nitro_server::run(config) {
        Ok(()) => {
            info!("exited cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

//! Turn the environment into a [`nitro_server::Config`] and run.
//!
//! - `NITRO_BACKEND=fake` (`NITRO_FAKE_SIZE=WxH`, default 1280x720) or
//!   `drm` (default; `NITRO_DRM_CARD` picks the device).
//! - `NITRO_DEMO=static` (default: bar stops after 3 s) or `moving`.
//! - `NITRO_CONTROL` overrides the control socket path; otherwise
//!   `$XDG_RUNTIME_DIR/nitro/control.sock`.
//! - `NITRO_LOG=error|warn|info|debug`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

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
    let bar_stop = match std::env::var("NITRO_DEMO").as_deref() {
        Ok("static") | Err(_) => Some(Duration::from_secs(3)),
        Ok("moving") => None,
        Ok(other) => return Err(format!("NITRO_DEMO={other:?}: want static or moving")),
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
    Ok(Config {
        backend,
        bar_stop,
        control_path: socket.path,
        handle_signals: true,
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

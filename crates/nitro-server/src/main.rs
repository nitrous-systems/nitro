//! Turn the environment into a [`nitro_server::Config`] and run.
//!
//! - `NITRO_BACKEND=fake` (`NITRO_FAKE_SIZE=WxH`, default 1280x720) or
//!   `drm` (default; `NITRO_DRM_CARD` picks the device).
//! - `NITRO_CONTROL` overrides the control socket path; otherwise
//!   `$XDG_RUNTIME_DIR/nitro/control.sock`.
//! - `NITRO_SOCKET` overrides the wire socket path; otherwise
//!   `$XDG_RUNTIME_DIR/nitro/wire.sock` (the path `nitro-wire` clients
//!   resolve to on their own, so the two agree without being told).
//! - `NITRO_SHELL_SOCKET` overrides the **shell** socket path; otherwise
//!   `$XDG_RUNTIME_DIR/nitro/shell.sock`. A client that connects there is
//!   privileged (`docs/shell.md`).
//! - `NITRO_INPUT_DIR` overrides where `event*` devices are looked for
//!   (default `/dev/input`); `NITRO_INPUT=off` disables input entirely,
//!   which is what a headless test wants.
//! - `NITRO_SCALE=<connector>=<f32>,…` overrides an output's scale; see
//!   `docs/wm.md`. It beats `server.conf`, which beats the EDID.
//! - `NITRO_MODE=<connector>=<WxH[@Hz]|max|fastest>,…` overrides an
//!   output's mode, and `NITRO_MODELINE=<connector>=<clock> <hdisp> …`
//!   drives one at timings the monitor does not advertise. Both beat
//!   `output.<c>.mode` in `server.conf`; see `docs/settings.md`.
//! - `NITRO_CONFIG` overrides where `server.conf` is read from; otherwise
//!   `$XDG_CONFIG_HOME/nitro/server.conf`, else
//!   `$HOME/.config/nitro/server.conf`. With neither variable set there is
//!   no file and no watch, and the server runs on its defaults. See
//!   `crates/nitro-server/src/config.rs` and `docs/settings.md`.
//! - `NITRO_SHADOW=0` paints straight into the scanout buffer instead of
//!   into a per-output heap shadow (the default). For A/B measurement on
//!   real hardware; see `crates/nitro-server/src/frame.rs`.
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
    // The privileged socket, resolved the same way: `nitro-wire`'s
    // `shell_socket_path` reads `NITRO_SHELL_SOCKET` and otherwise sits next
    // to the wire socket, so a shell client started in this environment
    // finds it without being configured either. See `docs/shell.md`.
    let shell_path = std::env::var_os("NITRO_SHELL_SOCKET")
        .map_or_else(nitro_wire::shell_socket_path, PathBuf::from);
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
        shell_path,
        handle_signals: true,
        fake_input: None,
        input_dir,
        scales: std::env::var("NITRO_SCALE")
            .map(|s| nitro_server::parse_scales(&s))
            .unwrap_or_default(),
        // `NITRO_MODE` is a comma-separated list like `NITRO_SCALE`;
        // `NITRO_MODELINE` holds one connector's raw timings, which
        // contain spaces and so cannot share that list. The modeline wins
        // where both name the same connector, because it is the more
        // specific instruction of the two.
        modes: {
            let mut m = std::env::var("NITRO_MODE")
                .map(|s| nitro_server::parse_modes(&s))
                .unwrap_or_default();
            if let Ok(s) = std::env::var("NITRO_MODELINE") {
                m.extend(nitro_server::parse_modelines(&s));
            }
            m
        },
        // Anything but `0` leaves the shadow on: this is a measurement
        // escape hatch, not a configuration surface, and the default is
        // the one that ships.
        shadow: std::env::var("NITRO_SHADOW").as_deref() != Ok("0"),
        // `NITRO_CONFIG`, else the XDG path. `None` — a service with
        // neither `$XDG_CONFIG_HOME` nor `$HOME` — means no file and no
        // watch rather than a guessed path the user cannot find.
        config_path: nitro_server::config::path(),
        // The real XDG icon search path. `NITRO_ICON_PATH` overrides it,
        // and it is read where the path is built rather than here: the
        // field exists for the tests, which cannot use an environment
        // variable because they run as threads of one process.
        icon_dirs: None,
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

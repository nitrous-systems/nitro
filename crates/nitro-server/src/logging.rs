//! Minimal leveled logging to stderr. No crate: the server has one thread,
//! one output (the journal, or a terminal) and no need for structured
//! sinks yet.
//!
//! The level comes from `NITRO_LOG` (`error`, `warn`, `info`, `debug`;
//! default `info`) and is read once, on first use. Lines look like
//! `[warn] control socket: ...`; journald adds the timestamp.

use std::fmt;
use std::io::Write as _;
use std::sync::atomic::{AtomicU8, Ordering};

/// Severity, most severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// Something failed that the server cannot recover from.
    Error = 0,
    /// Something failed but the server carries on.
    Warn = 1,
    /// Lifecycle: outputs, session state, clients.
    Info = 2,
    /// Per-frame / per-request chatter.
    Debug = 3,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" | "trace" => Some(Level::Debug),
            _ => None,
        }
    }
}

const UNSET: u8 = u8::MAX;
static MAX_LEVEL: AtomicU8 = AtomicU8::new(UNSET);

/// Override the level (tests, or a `--verbose` flag). Takes effect for
/// every subsequent line.
pub fn set_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The current maximum level, initialising it from `NITRO_LOG` on first
/// use.
pub fn level() -> Level {
    let raw = MAX_LEVEL.load(Ordering::Relaxed);
    if raw != UNSET {
        return from_raw(raw);
    }
    let level = std::env::var("NITRO_LOG")
        .ok()
        .and_then(|s| Level::parse(&s))
        .unwrap_or(Level::Info);
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
    level
}

fn from_raw(raw: u8) -> Level {
    match raw {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        _ => Level::Debug,
    }
}

/// True when a line at `level` would be printed.
pub fn enabled(level: Level) -> bool {
    level <= self::level()
}

/// Write one line. Prefer the [`log!`], [`error!`], [`warn!`], [`info!`]
/// and [`debug!`] macros.
pub fn write(level: Level, args: fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    // A failed write to stderr is not worth handling: there is nowhere
    // else to report it.
    let _ = writeln!(out, "[{}] {args}", level.name());
}

/// Log at an explicit level: `log!(Level::Warn, "x = {}", x)`.
#[macro_export]
macro_rules! log {
    ($level:expr, $($arg:tt)*) => {
        $crate::logging::write($level, format_args!($($arg)*))
    };
}

/// Log at `Level::Error`.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log!($crate::logging::Level::Error, $($arg)*) };
}

/// Log at `Level::Warn`.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log!($crate::logging::Level::Warn, $($arg)*) };
}

/// Log at `Level::Info`.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log!($crate::logging::Level::Info, $($arg)*) };
}

/// Log at `Level::Debug`.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log!($crate::logging::Level::Debug, $($arg)*) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_levels_case_insensitively() {
        assert_eq!(Level::parse("ERROR"), Some(Level::Error));
        assert_eq!(Level::parse(" warn "), Some(Level::Warn));
        assert_eq!(Level::parse("Info"), Some(Level::Info));
        assert_eq!(Level::parse("debug"), Some(Level::Debug));
        assert_eq!(Level::parse("loud"), None);
    }

    #[test]
    fn ordering_is_severity() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        assert_eq!(from_raw(Level::Warn as u8), Level::Warn);
    }
}

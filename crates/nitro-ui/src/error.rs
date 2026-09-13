//! What can go wrong in the toolkit.

use std::fmt;

/// A toolkit error.
///
/// Everything that can be caused by an app's own mistake — a stale id, a
/// wrong widget type, re-entering a widget — is an error value, never a
/// panic: a toolkit that aborts the process because a callback held on to
/// an id for one frame too long is not one you can write an app in.
#[derive(Debug)]
pub enum Error {
    /// The id names a widget that no longer exists (or an index that was
    /// recycled for a different one — generations catch that).
    StaleWidget,
    /// The widget is currently running one of its own methods, so it is
    /// out of its slot and cannot be borrowed again. Mutate *other*
    /// widgets from a callback; to change yourself, use the `&mut self`
    /// you already have.
    Busy,
    /// The widget exists but is not of the requested type.
    WrongType {
        /// The type that was asked for.
        expected: &'static str,
    },
    /// No root widget has been set.
    NoRoot,
    /// The connection failed, which is fatal: the wire protocol has no
    /// recoverable errors.
    Wire(nitro_wire::Error),
    /// A system call failed.
    Io(rustix::io::Errno),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::StaleWidget => write!(f, "stale widget id"),
            Error::Busy => write!(f, "widget is already borrowed (re-entrant access)"),
            Error::WrongType { expected } => write!(f, "widget is not a {expected}"),
            Error::NoRoot => write!(f, "no root widget"),
            Error::Wire(e) => write!(f, "wire: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Wire(e) => Some(e),
            _ => None,
        }
    }
}

impl From<nitro_wire::Error> for Error {
    fn from(e: nitro_wire::Error) -> Self {
        Error::Wire(e)
    }
}

impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        Error::Io(e)
    }
}

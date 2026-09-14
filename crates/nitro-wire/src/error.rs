//! Error types: [`DecodeError`] for anything read off the wire,
//! [`EncodeError`] for messages we refuse to write, and [`Error`] for the
//! socket-level API that can hit both plus the kernel.

use std::fmt;

use crate::types::ErrorCode;

/// Why a frame or message could not be decoded.
///
/// Decoding is *total*: every malformed byte sequence produces one of these
/// and never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The payload ended in the middle of a field.
    Truncated,
    /// Bytes were left over after the message's fields.
    Trailing,
    /// The op code is not part of this protocol version (or is a
    /// client op seen by a client / a server op seen by a server).
    UnknownOp(u16),
    /// A tagged field held a value outside its enumeration (bad node kind,
    /// fill tag, bool other than 0/1, …).
    BadValue,
    /// A `str` field was not valid UTF-8, or contained a NUL.
    BadUtf8,
    /// A length field exceeded the protocol maximum.
    TooLarge,
    /// The message needs a file descriptor and none was attached.
    MissingFd,
    /// File descriptors were attached that no message claimed.
    UnexpectedFd,
    /// A frame header used a reserved `flags` bit.
    BadFlags,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Truncated => "truncated message",
            Self::Trailing => "trailing bytes after message",
            Self::UnknownOp(_) => "unknown op code",
            Self::BadValue => "field value out of range",
            Self::BadUtf8 => "string is not valid UTF-8",
            Self::TooLarge => "length exceeds protocol maximum",
            Self::MissingFd => "missing file descriptor",
            Self::UnexpectedFd => "unexpected file descriptor",
            Self::BadFlags => "reserved frame flag set",
        };
        match self {
            Self::UnknownOp(op) => write!(f, "{s} 0x{op:04x}"),
            _ => f.write_str(s),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Why a message could not be encoded. Both cases mean the caller built a
/// message that cannot be represented on the wire; the [`Writer`] keeps the
/// buffer well-formed by dropping the offending frame.
///
/// [`Writer`]: crate::Writer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EncodeError {
    /// The payload would exceed [`MAX_PAYLOAD`](crate::MAX_PAYLOAD).
    TooLarge,
    /// More than [`MAX_FDS`](crate::MAX_FDS) descriptors on one frame.
    TooManyFds,
    /// A descriptor could not be duplicated for sending (`EMFILE`).
    Fd(rustix::io::Errno),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("message payload exceeds the protocol maximum"),
            Self::TooManyFds => f.write_str("too many file descriptors on one frame"),
            Self::Fd(e) => write!(f, "cannot duplicate file descriptor: {e}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Anything that can go wrong on a connection.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A syscall failed.
    Io(rustix::io::Errno),
    /// The peer sent something we could not decode. Fatal.
    Decode(DecodeError),
    /// We refused to encode a message. Not fatal, nothing was written.
    Encode(EncodeError),
    /// The peer closed the connection.
    Closed,
    /// The peer's protocol version differs from [`VERSION`](crate::VERSION).
    Version {
        /// The version we speak.
        ours: u32,
        /// The version the peer asked for.
        theirs: u32,
    },
    /// The peer sent a well-formed message that does not belong here (for
    /// example anything but `Welcome` in answer to `Hello`).
    Unexpected(&'static str),
    /// The server answered with a fatal `Error` message.
    Rejected {
        /// Error code from the server.
        code: ErrorCode,
        /// Human-readable detail; never interpreted.
        msg: String,
    },
    /// A message carrying file descriptors was about to go out on a
    /// **remote** socket, which cannot carry them.
    ///
    /// Raised on the sending side, before any byte leaves: a frame whose
    /// header declares descriptors and whose descriptors never arrive is
    /// a desynchronised stream, so the honest failure is here and not at
    /// the far end. It is *not* fatal to the connection — nothing was
    /// written and nothing was queued, so the connection is exactly as
    /// it was.
    ///
    /// That is what lets a caller treat it as "there is no buffer here"
    /// rather than as a failure: `nitro-ui` turns it into a `None` from
    /// `upload_image` and carries on drawing the rest of the tree. A
    /// caller that instead propagates it will stop, which for a paint
    /// pass means the app exits — see `docs/remote.md`.
    RemoteNoFds,
    /// `NITRO_SOCKET` (or `remote.listen`) named something unusable.
    BadEndpoint(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Decode(e) => write!(f, "decode: {e}"),
            Self::Encode(e) => write!(f, "encode: {e}"),
            Self::Closed => f.write_str("connection closed by peer"),
            Self::Version { ours, theirs } => {
                write!(f, "protocol version mismatch: ours {ours}, theirs {theirs}")
            }
            Self::Unexpected(what) => write!(f, "unexpected message: {what}"),
            Self::Rejected { code, msg } => write!(f, "server error {code:?}: {msg}"),
            Self::RemoteNoFds => {
                f.write_str("file descriptors cannot be passed over a remote link")
            }
            Self::BadEndpoint(what) => write!(f, "bad endpoint: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Decode(e) => Some(e),
            Self::Encode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        Self::Io(e)
    }
}

impl From<DecodeError> for Error {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

impl From<EncodeError> for Error {
    fn from(e: EncodeError) -> Self {
        Self::Encode(e)
    }
}

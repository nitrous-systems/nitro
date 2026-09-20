//! Errors returned by the mutation API.

use std::fmt;

/// Why a scene mutation was refused.
///
/// Every mutation is total: a bad key or a bad argument is an error value,
/// never a panic and never a silent write to the wrong node. The server maps
/// these onto protocol errors for the offending client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Error {
    /// The key names a slot that is empty or has been recycled.
    StaleKey,
    /// The client does not own the node or buffer it named.
    NotOwner,
    /// The property does not exist on this node kind (for example a fill on a
    /// group, or a transform on a rect).
    WrongKind,
    /// The requested parent is the node itself, one of its descendants, a node
    /// in another client's tree, or otherwise unusable.
    BadParent,
    /// The node is a window's root; it is owned by the window and cannot be
    /// reparented or destroyed on its own.
    RootNode,
    /// The sibling passed as `before` is not a child of the parent.
    BadSibling,
    /// The tree would become deeper than [`MAX_DEPTH`](crate::MAX_DEPTH).
    TooDeep,
    /// A buffer description or source rectangle is not usable: zero extent, a
    /// stride that does not cover the width, too little data, or a source rect
    /// that leaves the buffer.
    BadBuffer,
    /// No output with that id has been added.
    UnknownOutput,
    /// The buffer's pixels are a read-only mapping of the client's memory;
    /// the client writes them, the scene only reads.
    ReadOnly,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::StaleKey => "stale key",
            Self::NotOwner => "not the owner",
            Self::WrongKind => "wrong node kind",
            Self::BadParent => "invalid parent",
            Self::RootNode => "node is a window root",
            Self::BadSibling => "sibling is not a child of the parent",
            Self::TooDeep => "tree too deep",
            Self::BadBuffer => "invalid buffer",
            Self::UnknownOutput => "unknown output",
            Self::ReadOnly => "buffer is read-only",
        };
        f.write_str(s)
    }
}

impl std::error::Error for Error {}

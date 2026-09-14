//! The nitro file manager: a window onto one directory at a time.
//!
//! This is the placeholder crate root. The four modules under it are the
//! part of a file manager that has nothing to do with widgets — reading
//! and ordering a directory ([`dir`]), deciding what type a file is and
//! what opens it ([`mime`]), the `FreeDesktop` trash ([`trash`]), and the
//! rename/new-folder/copy operations ([`ops`]) — and each is testable
//! without a display server, which is why they are separate from the UI
//! that drives them.

pub mod dir;
pub mod mime;
pub mod ops;
pub mod trash;

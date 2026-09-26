//! `nitro-fs` — the directory model, with no widget in it.
//!
//! [`dir`] reads, sorts and formats a directory listing and scans a big
//! one on a background thread; [`mime`] maps a file name to a MIME type
//! through `globs2` and a type to an icon; [`places`] lists the XDG user
//! directories a sidebar offers.
//!
//! This code was written for `nitro-files` and lived there. It is its own
//! crate because the toolkit's file picker wants the same model, and
//! `nitro-files` depends on `nitro-ui`, so `nitro-ui` cannot use code
//! that lives in `nitro-files`. The half of MIME handling that needs the
//! launcher's `.desktop` reader (what *opens* a type), the trash and the
//! file operations stay in `nitro-files`. See `docs/files.md`.

pub mod dir;
pub mod mime;
pub mod places;

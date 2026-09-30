//! `text/uri-list` (RFC 2483) for the clipboard: `file://` URIs from paths
//! and back, plus the GNOME `x-special/gnome-copied-files` form that also
//! says whether the files were copied or cut.
//!
//! A path is OS bytes, not text, so encoding is bytewise: every byte
//! outside the unreserved set (and `/`) is percent-encoded, which makes
//! any name — spaces, newlines, invalid UTF-8 — survive the round trip.

use std::ffi::OsStr;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

/// `path` as a `file://` URI, percent-encoding every byte except the
/// unreserved characters and `/`.
#[must_use]
pub fn path_to_file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0xf)]));
        }
    }
    out
}

/// A `text/uri-list` of `paths`: one URI per line, each CRLF-terminated.
#[must_use]
pub fn uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        out.push_str(&path_to_file_uri(p));
        out.push_str("\r\n");
    }
    out
}

/// The local paths in a `text/uri-list`.
///
/// Comments (`#`) and blank lines are skipped; only `file:///…` and
/// `file://localhost/…` are accepted, every other scheme or host is
/// ignored. A malformed `%` escape is kept literally.
#[must_use]
pub fn parse_uri_list(list: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in list.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("file://") else {
            continue;
        };
        let path = if rest.starts_with('/') {
            rest
        } else if let Some(p) = rest.strip_prefix("localhost") {
            if !p.starts_with('/') {
                continue;
            }
            p
        } else {
            continue;
        };
        out.push(PathBuf::from(std::ffi::OsString::from_vec(percent_decode(
            path.as_bytes(),
        ))));
    }
    out
}

fn percent_decode(s: &[u8]) -> Vec<u8> {
    let hex = |b: u8| char::from(b).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && i + 2 < s.len()
            && let (Some(h), Some(l)) = (hex(s[i + 1]), hex(s[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// The GNOME file-manager clipboard type: an action line (`copy` or
/// `cut`) and then one `file://` URI per line. Nautilus, Nemo, Thunar,
/// `PCManFM` and Dolphin all read it; it is the only widely read way to say
/// "cut".
pub const GNOME_COPIED_FILES_MIME: &str = "x-special/gnome-copied-files";

/// KDE's cut marker: offered with the value `1` alongside a uri-list when
/// the files were cut rather than copied.
pub const KDE_CUT_MIME: &str = "application/x-kde-cutselection";

/// What a paste of the clipboard's files should do with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipOp {
    /// Copy them; the originals stay.
    #[default]
    Copy,
    /// Move them; the originals go.
    Cut,
}

/// An `x-special/gnome-copied-files` body: `copy` or `cut`, then one URI
/// per line, `\n`-separated with no trailing newline — the shape
/// Nautilus writes.
#[must_use]
pub fn gnome_copied_files(op: ClipOp, paths: &[PathBuf]) -> String {
    let mut out = String::from(match op {
        ClipOp::Copy => "copy",
        ClipOp::Cut => "cut",
    });
    for p in paths {
        out.push('\n');
        out.push_str(&path_to_file_uri(p));
    }
    out
}

/// Parse an `x-special/gnome-copied-files` body.
///
/// The first line is the action: `cut` is [`ClipOp::Cut`], anything else
/// (`copy`, an unknown word, nothing) is [`ClipOp::Copy`] — the safe
/// reading, since a wrong copy leaves a spare file and a wrong move loses
/// one from where the user left it. A writer that omits the action line
/// and starts with a URI is read as a copy of every line. The URIs go
/// through [`parse_uri_list`], so CRLF, comments and foreign schemes are
/// handled as there.
#[must_use]
pub fn parse_gnome_copied_files(s: &str) -> (ClipOp, Vec<PathBuf>) {
    let (first, rest) = s.split_once('\n').unwrap_or((s, ""));
    let action = first.trim_end_matches('\r').trim();
    if action.starts_with("file:") {
        return (ClipOp::Copy, parse_uri_list(s));
    }
    let op = if action == "cut" {
        ClipOp::Cut
    } else {
        ClipOp::Copy
    };
    (op, parse_uri_list(rest))
}

/// Paths joined by `\n`, for the plain-text form of a copy.
#[must_use]
pub fn plain_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(OsStr::as_bytes(p.as_os_str()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reserved_bytes() {
        assert_eq!(
            path_to_file_uri(Path::new("/tmp/a b/c%d#é")),
            "file:///tmp/a%20b/c%25d%23%C3%A9"
        );
        let odd = PathBuf::from(std::ffi::OsString::from_vec(b"/x/\xff\n".to_vec()));
        assert_eq!(path_to_file_uri(&odd), "file:///x/%FF%0A");
    }

    #[test]
    fn a_list_is_crlf_terminated() {
        let l = uri_list(&[PathBuf::from("/a"), PathBuf::from("/b c")]);
        assert_eq!(l, "file:///a\r\nfile:///b%20c\r\n");
    }

    #[test]
    fn round_trips() {
        let paths = vec![
            PathBuf::from("/tmp/a b"),
            PathBuf::from(std::ffi::OsString::from_vec(b"/x/\xff%\n".to_vec())),
        ];
        assert_eq!(parse_uri_list(&uri_list(&paths)), paths);
    }

    #[test]
    fn gnome_copied_files_has_the_nautilus_shape() {
        let paths = [PathBuf::from("/a"), PathBuf::from("/b c")];
        assert_eq!(
            gnome_copied_files(ClipOp::Copy, &paths),
            "copy\nfile:///a\nfile:///b%20c"
        );
        assert_eq!(
            gnome_copied_files(ClipOp::Cut, &paths[..1]),
            "cut\nfile:///a"
        );
    }

    #[test]
    fn gnome_copied_files_round_trips() {
        let paths = vec![
            PathBuf::from("/tmp/a b"),
            PathBuf::from(std::ffi::OsString::from_vec(b"/x/\xff%\n".to_vec())),
        ];
        for op in [ClipOp::Copy, ClipOp::Cut] {
            assert_eq!(
                parse_gnome_copied_files(&gnome_copied_files(op, &paths)),
                (op, paths.clone())
            );
        }
    }

    #[test]
    fn a_missing_or_unknown_action_is_a_copy() {
        assert_eq!(
            parse_gnome_copied_files("file:///a\nfile:///b"),
            (ClipOp::Copy, vec![PathBuf::from("/a"), PathBuf::from("/b")])
        );
        assert_eq!(
            parse_gnome_copied_files("link\nfile:///a"),
            (ClipOp::Copy, vec![PathBuf::from("/a")])
        );
        assert_eq!(parse_gnome_copied_files(""), (ClipOp::Copy, vec![]));
    }

    #[test]
    fn gnome_copied_files_tolerates_crlf_and_a_trailing_newline() {
        assert_eq!(
            parse_gnome_copied_files("cut\r\nfile:///a\r\nfile:///b\r\n"),
            (ClipOp::Cut, vec![PathBuf::from("/a"), PathBuf::from("/b")])
        );
    }

    #[test]
    fn parsing_skips_comments_other_schemes_and_hosts() {
        let l = "# a comment\r\n\r\nfile:///a\r\nhttp://x/y\r\nfile://otherhost/b\r\n\
                 file://localhost/c%20d\nfile:///bad%zz%4\n";
        assert_eq!(
            parse_uri_list(l),
            [
                PathBuf::from("/a"),
                PathBuf::from("/c d"),
                PathBuf::from("/bad%zz%4")
            ]
        );
    }
}

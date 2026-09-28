//! `text/uri-list` (RFC 2483) for the clipboard: `file://` URIs from paths
//! and back.
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

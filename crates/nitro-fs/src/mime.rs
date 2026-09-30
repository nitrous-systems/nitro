//! What type a file is, and which icon draws it.
//!
//! The type is the extension, against the system's `globs2` table when
//! the machine has one and against a small built-in table when it does
//! not. The icon is a family lookup on that type ([`icon_for`]).
//!
//! This is the first of the three questions `nitro_files::mime` answers
//! when a file is opened — *what type is this?* The other two, *what
//! handles that type?* and *how do I run that?*, need the launcher's
//! `.desktop` reader and stay in `nitro-files`. This half is shared with
//! the toolkit's file picker, which needs types for its filter and icons
//! for its rows and must not depend on the file manager.
//!
//! # Why the extension and not the content
//!
//! `shared-mime-info` also carries *magic* rules — byte patterns at
//! offsets, with priorities — and sniffing content is what `file(1)`
//! does. This does not: it would mean opening and reading every file in
//! a directory to draw the list, which is the cost we spend the whole of
//! [`crate::dir`] avoiding, and getting it wrong on a file the user
//! explicitly asked to open is recoverable in a way that a slow listing
//! is not. The visible consequence is that an extensionless script is
//! `None` rather than `text/x-shellscript`, so it falls through to the
//! editor fallback only if something else claims it. Recorded in
//! `docs/files.md` under *Limitations*.
//!
//! Nothing here reads the environment: the `globs2` path is handed in,
//! so a test points it at a temp directory.

use std::path::Path;

/// The extension table used when the machine has no `globs2`.
///
/// Thirty-odd entries, chosen for what a file manager on this desktop
/// will actually be asked to open rather than for coverage: the text and
/// source files the editor fallback exists for, the image formats the
/// toolkit's own `Image` widget and `nitro-shot` produce (`png`, `ppm`),
/// the documents and archives a browser or an archive manager claims, and
/// the handful of audio and video containers a media player registers
/// for. Everything else is `None` and falls to whatever `globs2` said, or
/// to nothing.
///
/// It is a table and not a database because the database is
/// `shared-mime-info`, it is already installed on every machine that has
/// a desktop, and this is the fallback for the machine that does not —
/// which is the test box with a bare rootfs, and the case where a
/// hundred-line table is the difference between "opens your notes" and
/// "does nothing".
///
/// The extension is matched case-insensitively (`.JPG` off a camera is a
/// JPEG) and without the dot. A name that is *only* an extension
/// (`.gz`) is a hidden file called `gz` and not an archive, so it has no
/// type here — the same rule [`type_of`] applies to the system table.
#[must_use]
pub fn builtin_type(name: &str) -> Option<&'static str> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let ext = ext.to_ascii_lowercase();
    let mime = match ext.as_str() {
        // Text, and the source files that are text with a syntax.
        "txt" | "log" | "text" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "rs" => "text/x-rust",
        "c" | "h" => "text/x-csrc",
        "sh" | "bash" => "application/x-shellscript",
        "py" => "text/x-python",
        "toml" => "application/toml",
        "json" => "application/json",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        // Images. `ppm` is here because it is what this tree's own
        // tools write.
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ppm" => "image/x-portable-pixmap",
        // Documents.
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        // Archives.
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "xz" => "application/x-xz",
        "zst" => "application/zstd",
        "tar" => "application/x-tar",
        // Audio and video.
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/x-wav",
        "opus" => "audio/opus",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "m3u" | "m3u8" => "audio/x-mpegurl",
        "pls" => "audio/x-scpls",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        _ => return None,
    };
    Some(mime)
}

/// One `*.ext` rule from a `globs2` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob {
    /// The rule's weight; the highest one that matches wins.
    pub weight: u32,
    /// The MIME type the rule assigns.
    pub mime: String,
    /// The extension, lowercased and without the leading `*.`. May
    /// contain dots itself (`tar.gz`).
    pub ext: String,
}

/// Parse the `weight:type:glob` lines of a freedesktop `globs2` file.
///
/// The format is one rule per line, `#` for comments, and an optional
/// fourth field of flags. Only the `*.ext` shape of glob is kept, and
/// that is the whole subtlety of this function:
///
/// * `*.tar.gz` is kept, with `tar.gz` as the extension — a suffix with
///   dots in it is still a suffix.
/// * `Makefile`, `*README*`, `core.[0-9]*` and the rest of the literal
///   and character-class shapes are **skipped**, because matching them
///   means implementing `fnmatch`, and the types they name (a makefile, a
///   core dump) are ones where guessing wrong costs a user nothing and
///   guessing at all costs us a glob engine.
/// * A rule with flags is skipped as well. The flag that actually
///   appears is `cs`, "case-sensitive", and it exists precisely to say
///   that `*.C` (C++) is not `*.c` (C) — a distinction we cannot honour
///   while matching case-insensitively, so the honest thing is to not
///   claim the rule rather than to apply it in the wrong case.
///
/// A malformed line is skipped rather than failing the parse: this file
/// belongs to the distribution, we are a reader of it, and a file manager
/// that refused to guess any type because line 900 was odd would be
/// strictly worse than one missing a rule.
#[must_use]
pub fn parse_globs2(text: &str) -> Vec<Glob> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(':');
        let (Some(weight), Some(mime), Some(glob)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // A fourth field is a flag we do not implement; see above.
        if fields.next().is_some() {
            continue;
        }
        let Ok(weight) = weight.trim().parse::<u32>() else {
            continue;
        };
        let Some(ext) = glob.strip_prefix("*.") else {
            continue;
        };
        // Anything left that is not a plain suffix is a glob shape we do
        // not match. `*` and `?` and `[` are the whole of it.
        if ext.is_empty() || ext.contains(['*', '?', '[', ']']) {
            continue;
        }
        if mime.trim().is_empty() {
            continue;
        }
        out.push(Glob {
            weight,
            mime: mime.trim().to_owned(),
            ext: ext.to_ascii_lowercase(),
        });
    }
    out
}

/// Read `globs2` from `path`, or nothing if it is not there.
///
/// Injectable rather than hard-coded to `/usr/share/mime/globs2` so the
/// tests can hand it a fixture; the app passes the real path, and a
/// machine without `shared-mime-info` gets an empty table and the
/// built-in one below it.
#[must_use]
pub fn load_globs2(path: &Path) -> Vec<Glob> {
    std::fs::read_to_string(path)
        .map(|text| parse_globs2(&text))
        .unwrap_or_default()
}

/// Whether `name` ends in `.{ext}`, with something in front of the dot.
///
/// Split out and written on bytes for one reason, and it is a measured
/// one: the obvious spelling is `name.ends_with(&format!(".{ext}"))`,
/// which allocates **once per rule per file**. That was invisible while
/// [`type_of`] was called when the user opened something; the icon column
/// calls it once per file per listing, and a `globs2` on this box holds
/// ~2 000 rules — so a thousand-row directory meant two million `String`s
/// and **75.7 ms**, an eighth of a second of the loop. The same listing
/// with this function is 3.6 ms. See `docs/files.md`.
///
/// A name that is *only* an extension (`.gz`) is a hidden file called
/// `gz` and not an archive, which is the `+ 2` below.
fn ends_with_dot_ext(name: &str, ext: &str) -> bool {
    let (n, e) = (name.as_bytes(), ext.as_bytes());
    if n.len() < e.len() + 2 {
        return false;
    }
    let dot = n.len() - e.len() - 1;
    n[dot] == b'.' && &n[dot + 1..] == e
}

/// The MIME type of `path`: the `globs2` table first, then the built-in
/// one.
///
/// The system table wins because it is the machine's own answer and
/// because it is two orders of magnitude bigger than ours; the built-in
/// table is the fallback for the machine that has none. Within the
/// system table the highest weight wins, and a tie is broken by the
/// **longer** extension, so a `.tar.gz` is gzip-compressed-tar rather
/// than plain gzip when both rules carry the default weight of 50.
/// A tie in both goes to the rule **first in the file**: shared-mime-info
/// writes the canonical type first, so `*.ogg` is `audio/ogg` rather
/// than the `video/x-theora+ogg` listed after it, and `*.m3u` is
/// `audio/x-mpegurl` rather than `application/vnd.apple.mpegurl`.
///
/// **This is called once per file per listing** (`dir::read_dir`), not
/// once per file the user opens, which is what [`ends_with_dot_ext`] is
/// about.
#[must_use]
pub fn type_of(path: &Path, globs: &[Glob]) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    // Not `max_by_key`: that returns the *last* of equal maxima, and the
    // first is the one shared-mime-info means.
    let best = globs
        .iter()
        .filter(|g| ends_with_dot_ext(&name, &g.ext))
        .fold(None::<&Glob>, |best, g| match best {
            Some(b) if (b.weight, b.ext.len()) >= (g.weight, g.ext.len()) => Some(b),
            _ => Some(g),
        });
    if let Some(g) = best {
        return Some(g.mime.clone());
    }
    builtin_type(&name).map(str::to_owned)
}

/// Whether a `text/x-*` type is prose, a subtitle track or a document
/// rather than source.
///
/// The **exception list**, and the polarity is the point. `text/x-…` is
/// `shared-mime-info`'s prefix for "a text format with a syntax", and on
/// this box's table 112 of its 145 `text/*` types carry it — of which the
/// overwhelming majority are programming languages, build files and
/// markup. Enumerating those was the first attempt and it was wrong on the
/// box: my list had 20 languages and the box's table names 112, so
/// `.hs`, `.kt`, `.scala`, `.vala`, `.ml`, `.ex`, `.f90` and eighty more
/// silently got the *document* icon. A map that has to be extended for
/// every language anybody installs is a map that is quietly wrong on every
/// machine.
///
/// So the default for `text/x-*` is **code**, and this is what is carved
/// back out: the READMEs and changelogs, the subtitle and playlist
/// formats, the typesetting sources, and the translation catalogues.
/// Wrong answers here are bounded and visible — a `.srt` showing a code
/// icon is a wrong icon — where wrong answers the other way were
/// unbounded and grew with the machine's package list.
fn is_prose_or_document(mime: &str) -> bool {
    matches!(
        mime,
        // Files a project ships to be read by a person.
        "text/x-authors"
            | "text/x-changelog"
            | "text/x-copying"
            | "text/x-credits"
            | "text/x-install"
            | "text/x-readme"
            | "text/x-todo-txt"
            | "text/x-log"
            | "text/x-nfo"
            | "text/x-mpl2"
            // Typesetting and markup meant as prose.
            | "text/x-tex"
            | "text/x-texinfo"
            | "text/x-bibtex"
            | "text/x-rst"
            | "text/x-setext"
            | "text/x-txt2tags"
            | "text/x-troff-me"
            | "text/x-troff-mm"
            | "text/x-troff-ms"
            // Subtitles and playlists: text, but nobody edits them as
            // source.
            | "text/x-ssa"
            | "text/x-subviewer"
            | "text/x-microdvd"
            | "text/x-mpsub"
            | "text/x-google-video-pointer"
            | "text/x-iMelody"
            // Data and catalogues.
            | "text/x-ldif"
            | "text/x-uuencode"
            | "text/x-ms-regedit"
            | "text/x-gettext-translation"
            | "text/x-gettext-translation-template"
            | "text/x-mrml"
            | "text/x-mup"
    )
}

/// Whether a type is source, a script or markup: **text with a syntax**,
/// which is what the code icon means.
///
/// Its own function rather than an arm of [`icon_for`] because it has to
/// be consulted *before* the `text/` family: a `.rs`, a `.c`, a `.sh` and
/// an `.html` are all `text/…`, and a code icon says more about them than
/// a document icon does. That ordering is the one real decision in this
/// map and it is visible at the call site.
///
/// Two spellings of everything, because a `globs2` is the machine's file
/// and machines disagree. The box's `shared-mime-info` says **`text/rust`**
/// where ours said `text/x-rust`, and our built-in fallback table says
/// `text/x-rust` where the box says `text/rust` — so a `.rs` drew a
/// document icon on the box and a code icon in the tests, which is exactly
/// the kind of divergence a test against a fixture cannot see. Both are
/// matched, in both directions, wherever the two families overlap.
fn is_code(mime: &str) -> bool {
    // `text/x-*` is source unless it is on the short prose list: see
    // `is_prose_or_document` for why the default runs this way round.
    if mime.starts_with("text/x-") {
        return !is_prose_or_document(mime);
    }
    matches!(
        mime,
        // Web and markup.
        "text/html"
            | "text/xml"
            | "text/css"
            | "text/javascript"
            | "text/sgml"
            | "text/cache-manifest"
            | "application/xhtml+xml"
            | "application/javascript"
            | "application/x-javascript"
            | "application/ecmascript"
            | "application/xslt+xml"
            | "application/xml-dtd"
            // Languages the modern table spells without `x-`. `text/rust`
            // is the one the box actually has, and the one that found
            // this whole family.
            | "text/rust"
            | "text/julia"
            | "text/tcl"
            | "text/vbscript"
            | "text/vbscript.encode"
            | "text/jscript.encode"
            | "text/vnd.wap.wmlscript"
            | "text/vnd.senx.warpscript"
            | "text/x.gcode"
            // Shells and scripting languages, `application/` half.
            | "application/x-shellscript"
            | "application/x-csh"
            | "application/x-fishscript"
            | "application/x-nuscript"
            | "application/x-powershell"
            | "application/x-awk"
            | "application/x-perl"
            | "application/x-python"
            | "application/x-ruby"
            | "application/x-php"
            | "application/x-gdscript"
            | "application/sql"
            | "application/vnd.coffeescript"
    )
}

/// Whether a type is structured text that is configuration, data or a
/// document rather than code.
fn is_document(mime: &str) -> bool {
    matches!(
        mime,
        "application/json"
            | "application/json5"
            | "application/ld+json"
            | "application/schema+json"
            | "application/xml"
            | "application/toml"
            | "application/x-toml"
            | "application/x-yaml"
            | "application/yaml"
            | "application/x-desktop"
            | "application/pdf"
            | "application/postscript"
            | "application/rtf"
            | "application/x-tex"
            | "application/vnd.oasis.opendocument.text"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    )
}

/// Whether a type is an archive or a compressed container.
///
/// The list is the shapes a `globs2` table actually produces for the
/// things on a desktop; the `-compressed-tar` spellings are
/// `shared-mime-info`'s own name for a `.tar.gz` and friends, and they are
/// what an installed table returns rather than `application/gzip`.
fn is_archive(mime: &str) -> bool {
    matches!(
        mime,
        "application/zip"
            | "application/gzip"
            | "application/zstd"
            | "application/x-tar"
            | "application/x-xz"
            | "application/x-lzma"
            | "application/x-lz4"
            | "application/x-bzip"
            | "application/x-bzip2"
            | "application/x-7z-compressed"
            | "application/x-rar"
            | "application/x-rar-compressed"
            | "application/vnd.rar"
            | "application/x-compressed-tar"
            | "application/x-bzip-compressed-tar"
            | "application/x-bzip2-compressed-tar"
            | "application/x-xz-compressed-tar"
            | "application/x-zstd-compressed-tar"
            | "application/x-lzma-compressed-tar"
            | "application/x-cpio"
            | "application/x-archive"
            | "application/x-deb"
            | "application/vnd.debian.binary-package"
            | "application/x-rpm"
            | "application/epub+zip"
            | "application/java-archive"
    )
}

/// Whether a type is a font.
///
/// `font/*` is the modern family (RFC 8081); the `application/x-font-*`
/// spellings predate it and are still what a 2010-vintage `globs2` on a
/// long-lived box says, so both are matched.
fn is_font(mime: &str) -> bool {
    mime.starts_with("font/")
        || mime.starts_with("application/x-font")
        || matches!(
            mime,
            "application/font-woff" | "application/vnd.ms-opentype"
        )
}

/// The symbolic icon name for a MIME type.
///
/// The whole map in one place, so that "what icon does a `.rs` file get"
/// is answered once and is testable without a listing, a server or a
/// window. The names are the `file-earmark-*` family of the server's
/// symbolic set (`crates/nitro-icons/icons.txt`, `docs/icons.md`).
///
/// The order below is the map: the specific full-type lists first
/// ([`is_code`] before everything, for the reason its doc comment gives),
/// then the media families by prefix, then `text/*` as the catch-all under
/// them.
///
/// A type nobody here claims gets the plain `file-earmark`, which is the
/// same answer a file with no type at all gets: the icon column then says
/// "a file", which is true, rather than guessing.
#[must_use]
pub fn icon_for(mime: &str) -> &'static str {
    // Stripped of any `; charset=…` parameter, then lowercased — a MIME
    // type is case-insensitive (RFC 2045 §5.1) and this string need not
    // come from a `globs2` table, which holds bare lowercase types.
    //
    // `split`/`trim` borrow, so the copy is paid **only** by a type that
    // is actually mixed-case: neither `builtin_type` nor a `globs2` table
    // produces one, and this is once per file per listing.
    let mime = mime.split(';').next().unwrap_or(mime).trim();
    if mime.bytes().any(|b| b.is_ascii_uppercase()) {
        return icon_for_lower(&mime.to_ascii_lowercase());
    }
    icon_for_lower(mime)
}

/// [`icon_for`]'s map, on a type already bare and lowercase.
fn icon_for_lower(mime: &str) -> &'static str {
    if is_code(mime) {
        return "file-earmark-code";
    }
    if is_document(mime) {
        return "file-earmark-text";
    }
    if is_archive(mime) {
        return "file-earmark-zip";
    }
    if is_font(mime) {
        return "file-earmark-font";
    }
    match mime.split_once('/') {
        Some(("image", _)) => "file-earmark-image",
        Some(("audio", _)) => "file-earmark-music",
        Some(("video", _)) => "file-earmark-play",
        Some(("text", _)) => "file-earmark-text",
        _ => "file-earmark",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A directory of this test's own; see `dir::tests::scratch`.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nitro-fs-mime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn the_builtin_table_knows_the_types_the_fallback_exists_for() {
        assert_eq!(builtin_type("notes.txt"), Some("text/plain"));
        assert_eq!(builtin_type("README.md"), Some("text/markdown"));
        assert_eq!(builtin_type("lib.rs"), Some("text/x-rust"));
        assert_eq!(builtin_type("shot.png"), Some("image/png"));
        assert_eq!(builtin_type("frame.ppm"), Some("image/x-portable-pixmap"));
        assert_eq!(builtin_type("book.pdf"), Some("application/pdf"));
        assert_eq!(builtin_type("song.flac"), Some("audio/flac"));
        assert_eq!(builtin_type("clip.mkv"), Some("video/x-matroska"));
        assert_eq!(builtin_type("track.m4a"), Some("audio/mp4"));
        assert_eq!(builtin_type("mix.m3u8"), Some("audio/x-mpegurl"));
        assert_eq!(builtin_type("radio.pls"), Some("audio/x-scpls"));
        assert_eq!(builtin_type("film.mov"), Some("video/quicktime"));
        // Case-insensitively: a camera writes `.JPG`.
        assert_eq!(builtin_type("DSC_0001.JPG"), Some("image/jpeg"));
        // The last extension is the one that counts.
        assert_eq!(builtin_type("archive.tar.gz"), Some("application/gzip"));
    }

    #[test]
    fn a_file_with_no_extension_has_no_builtin_type() {
        // No content sniffing: an extensionless script is unknown here.
        assert_eq!(builtin_type("Makefile"), None);
        assert_eq!(builtin_type("configure"), None);
        assert_eq!(builtin_type(""), None);
        // A dotfile with no second dot is a name, not an extension:
        // `.bashrc` is a hidden file called `bashrc`, and reading
        // `bashrc` as its extension would be a type claimed for a file
        // that has none.
        assert_eq!(builtin_type(".bashrc"), None);
        // Same rule, and the case that caught it: `.gz` is a hidden
        // file, not a gzip archive.
        assert_eq!(builtin_type(".gz"), None);
    }

    #[test]
    fn globs2_lines_parse_and_the_shapes_we_cannot_match_are_skipped() {
        let table = parse_globs2(
            "# comment\n\
             50:text/plain:*.txt\n\
             50:application/gzip:*.gz\n\
             60:application/x-compressed-tar:*.tar.gz\n\
             \n\
             50:text/x-csrc:*.c\n\
             50:text/x-c++src:*.C:cs\n\
             50:text/x-makefile:Makefile\n\
             50:application/x-core:core.[0-9]*\n\
             50:text/x-readme:*README*\n\
             not:a:rule:at:all\n\
             xx:text/plain:*.bad\n\
             50::*.empty\n",
        );
        let exts: Vec<&str> = table.iter().map(|g| g.ext.as_str()).collect();
        assert_eq!(exts, vec!["txt", "gz", "tar.gz", "c"]);
        assert_eq!(table[2].weight, 60);
        assert_eq!(table[2].mime, "application/x-compressed-tar");
    }

    #[test]
    fn the_system_table_outranks_the_builtin_one_and_the_longest_suffix_wins() {
        let table = parse_globs2(
            "50:application/gzip:*.gz\n\
             50:application/x-compressed-tar:*.tar.gz\n\
             80:text/x-nitro:*.txt\n",
        );
        // The longer extension breaks a tie at equal weight, so a
        // `.tar.gz` is not merely gzip.
        assert_eq!(
            type_of(Path::new("/a/backup.tar.gz"), &table).as_deref(),
            Some("application/x-compressed-tar")
        );
        // And the system's answer beats the built-in table's.
        assert_eq!(
            type_of(Path::new("/a/notes.txt"), &table).as_deref(),
            Some("text/x-nitro")
        );
        // With no system table the built-in one answers.
        assert_eq!(
            type_of(Path::new("/a/notes.txt"), &[]).as_deref(),
            Some("text/plain")
        );
        // A name that *is* the extension is not a match: `.gz` alone is
        // a hidden file called `gz`, not a gzip archive.
        assert_eq!(type_of(Path::new("/a/.gz"), &table), None);
        assert_eq!(type_of(Path::new("/a/unknown.qqq"), &table), None);
    }

    #[test]
    fn a_full_tie_goes_to_the_rule_first_in_the_file() {
        // The real table's order: the canonical type first, the
        // subclasses after it at the same weight.
        let table = parse_globs2(
            "50:audio/ogg:*.ogg\n\
             50:video/ogg:*.ogg\n\
             50:video/x-theora+ogg:*.ogg\n\
             50:audio/x-mpegurl:*.m3u\n\
             50:application/vnd.apple.mpegurl:*.m3u\n\
             60:audio/x-nitro:*.m3u\n",
        );
        assert_eq!(
            type_of(Path::new("/a/song.ogg"), &table).as_deref(),
            Some("audio/ogg")
        );
        // A higher weight still beats file order.
        assert_eq!(
            type_of(Path::new("/a/mix.m3u"), &table).as_deref(),
            Some("audio/x-nitro")
        );
    }

    #[test]
    fn a_missing_globs2_is_an_empty_table_rather_than_an_error() {
        let dir = scratch("globs-missing");
        assert!(load_globs2(&dir.join("globs2")).is_empty());
        write(&dir.join("globs2"), "50:text/plain:*.txt\n");
        assert_eq!(load_globs2(&dir.join("globs2")).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_icon_table_maps_every_family_it_claims_and_nothing_else() {
        // The whole type → icon map as a table, so a rule can be read
        // and checked without a listing, a server or a window.
        let cases = [
            // Plain text and the structured-data types that are
            // configuration rather than code.
            ("text/plain", "file-earmark-text"),
            ("text/markdown", "file-earmark-text"),
            ("text/csv", "file-earmark-text"),
            ("application/json", "file-earmark-text"),
            ("application/xml", "file-earmark-text"),
            ("application/toml", "file-earmark-text"),
            ("application/x-yaml", "file-earmark-text"),
            ("application/pdf", "file-earmark-text"),
            ("application/postscript", "file-earmark-text"),
            // The two office formats worth naming, because they are the
            // ones a user has: everything else under `vnd.` is generic.
            (
                "application/vnd.oasis.opendocument.text",
                "file-earmark-text",
            ),
            // Source, scripts and markup: text with a *syntax*. These are
            // matched before the `text/` family, which is the one
            // ordering decision in the function.
            ("text/x-csrc", "file-earmark-code"),
            ("text/rust", "file-earmark-code"),
            ("text/x-c++src", "file-earmark-code"),
            ("text/x-chdr", "file-earmark-code"),
            ("text/x-rust", "file-earmark-code"),
            ("text/x-python", "file-earmark-code"),
            ("text/x-shellscript", "file-earmark-code"),
            ("application/x-shellscript", "file-earmark-code"),
            ("text/html", "file-earmark-code"),
            ("text/css", "file-earmark-code"),
            ("text/javascript", "file-earmark-code"),
            ("application/javascript", "file-earmark-code"),
            // Media, by family.
            ("image/png", "file-earmark-image"),
            ("image/jpeg", "file-earmark-image"),
            ("image/svg+xml", "file-earmark-image"),
            ("image/x-portable-pixmap", "file-earmark-image"),
            ("audio/mpeg", "file-earmark-music"),
            ("audio/flac", "file-earmark-music"),
            ("audio/x-wav", "file-earmark-music"),
            ("video/mp4", "file-earmark-play"),
            ("video/x-matroska", "file-earmark-play"),
            ("video/webm", "file-earmark-play"),
            // Fonts, both spellings: `font/*` is RFC 8081 and
            // `application/x-font-*` is what an older `globs2` says.
            ("font/ttf", "file-earmark-font"),
            ("font/woff2", "file-earmark-font"),
            ("application/x-font-ttf", "file-earmark-font"),
            ("application/vnd.ms-opentype", "file-earmark-font"),
            // Archives, including `shared-mime-info`'s
            // `-compressed-tar` spellings for a `.tar.gz`.
            ("application/zip", "file-earmark-zip"),
            ("application/gzip", "file-earmark-zip"),
            ("application/zstd", "file-earmark-zip"),
            ("application/x-tar", "file-earmark-zip"),
            ("application/x-xz", "file-earmark-zip"),
            ("application/x-bzip2", "file-earmark-zip"),
            ("application/x-7z-compressed", "file-earmark-zip"),
            ("application/vnd.rar", "file-earmark-zip"),
            ("application/x-compressed-tar", "file-earmark-zip"),
            ("application/epub+zip", "file-earmark-zip"),
            // Everything else is honestly "a file". A guess here would
            // be worse than the plain icon: the column would assert a
            // type the table does not know.
            ("application/octet-stream", "file-earmark"),
            ("application/x-executable", "file-earmark"),
            ("model/gltf+json", "file-earmark"),
            ("", "file-earmark"),
        ];
        for (mime, want) in cases {
            assert_eq!(icon_for(mime), want, "icon_for({mime:?})");
        }
    }

    #[test]
    fn both_spellings_of_a_language_get_the_code_icon() {
        // The defect the **box** found, and no fixture could have.
        //
        // Our built-in fallback table says `text/x-rust`; the test box's
        // installed `shared-mime-info` says **`text/rust`**. The first
        // version of this map knew only the `x-` spelling, so a `.rs`
        // drew a *document* icon on the box and a code icon in every test
        // here — a divergence invisible to a test that supplies its own
        // table, because it is a disagreement between two real machines.
        for (a, b) in [
            ("text/x-rust", "text/rust"),
            ("text/x-tcl", "text/tcl"),
            ("text/x-julia", "text/julia"),
        ] {
            assert_eq!(icon_for(a), "file-earmark-code", "{a}");
            assert_eq!(icon_for(b), "file-earmark-code", "{b}");
        }
    }

    #[test]
    fn an_unenumerated_text_x_language_is_code_and_the_prose_ones_are_not() {
        // The polarity, which is the other half of the same finding: our
        // list had 20 languages and the box's table names 112, so
        // enumerating them meant being quietly wrong on every machine
        // with a language we had not thought of. `text/x-*` therefore
        // defaults to **code** and the prose formats are carved out.
        //
        // The languages below are deliberately ones this tree never
        // mentions: if the rule regressed to an enumeration, every one of
        // them would fail.
        for m in [
            "text/x-haskell",
            "text/x-kotlin",
            "text/x-scala",
            "text/x-vala",
            "text/x-ocaml",
            "text/x-elixir",
            "text/x-fortran",
            "text/x-cobol",
            "text/x-verilog",
            "text/x-lilypond",
            "text/x-some-language-invented-next-year",
        ] {
            assert_eq!(icon_for(m), "file-earmark-code", "{m}");
        }
        // And the carve-outs: a README is prose and a subtitle track is
        // not source, however `x-` they are spelled.
        for m in [
            "text/x-readme",
            "text/x-changelog",
            "text/x-authors",
            "text/x-copying",
            "text/x-tex",
            "text/x-rst",
            "text/x-ssa",
            "text/x-microdvd",
            "text/x-gettext-translation",
        ] {
            assert_eq!(icon_for(m), "file-earmark-text", "{m}");
        }
    }

    #[test]
    fn the_test_boxs_own_globs2_table_resolves_the_types_it_names() {
        // A corpus rather than a fixture: the 823 distinct MIME types the
        // test box's `/usr/share/mime/globs2` actually names, checked
        // family by family. The table is not vendored — that would be
        // 38 KB of somebody else's data in this repo — so the assertions
        // are the *invariants* it revealed, applied to the spellings it
        // uses. What the box's run measured, and what regressing any of
        // these would break:
        //
        //   image/* 94 → -image, audio/* 59 → -music,
        //   video/* 31 → -play, font/* 5 (+7 x-font) → -font,
        //   text/* 145 → 111 code + 34 text, **0 generic**.
        //
        // The last one is the load-bearing number: before the fix, 82 of
        // those 111 were generic or document icons.
        let sample = [
            // Every `text/*` type must get *some* text-ish icon; none may
            // fall through to the generic file icon.
            "text/plain",
            "text/rust",
            "text/markdown",
            "text/x-csrc",
            "text/x-haskell",
            "text/x-readme",
            "text/vcard",
            "text/vnd.graphviz",
            "text/x.gcode",
        ];
        for m in sample {
            let icon = icon_for(m);
            assert!(
                icon == "file-earmark-text" || icon == "file-earmark-code",
                "a text/* type fell to {icon}: {m}"
            );
        }
        // The media families, in the box's own spellings.
        for (m, want) in [
            ("image/vnd.zbrush.pcx", "file-earmark-image"),
            ("image/x-xpixmap", "file-earmark-image"),
            ("audio/x-voc", "file-earmark-music"),
            ("audio/vnd.dts", "file-earmark-music"),
            ("video/x-ogm+ogg", "file-earmark-play"),
            ("video/vnd.rn-realvideo", "file-earmark-play"),
            ("font/collection", "file-earmark-font"),
            ("font/otf", "file-earmark-font"),
            ("application/x-font-pcf", "file-earmark-font"),
        ] {
            assert_eq!(icon_for(m), want, "{m}");
        }
    }

    #[test]
    fn a_type_is_matched_case_insensitively_and_without_its_parameters() {
        // A MIME type is case-insensitive (RFC 2045 §5.1) and may carry
        // parameters. A `globs2` table holds neither shape, so this is
        // robustness rather than a case in use — but a column that drew
        // the generic icon for `text/plain; charset=utf-8` would be a
        // bug nobody would think to look for.
        assert_eq!(icon_for("TEXT/PLAIN"), "file-earmark-text");
        assert_eq!(icon_for("Image/PNG"), "file-earmark-image");
        assert_eq!(icon_for("text/plain; charset=utf-8"), "file-earmark-text");
        assert_eq!(icon_for("  text/x-rust  "), "file-earmark-code");
    }

    #[test]
    fn with_no_globs2_the_builtin_table_still_gives_sensible_icons() {
        // The case the built-in table exists for, joined to the icon map:
        // a box with no `shared-mime-info` (the test box, a bare rootfs)
        // must still show a photo as a photo. The glob list is **empty**
        // here, so every answer comes from `builtin_type`.
        let icon = |name: &str| icon_for(&type_of(Path::new(name), &[]).unwrap_or_default());
        assert_eq!(icon("notes.txt"), "file-earmark-text");
        assert_eq!(icon("README.md"), "file-earmark-text");
        assert_eq!(icon("main.rs"), "file-earmark-code");
        assert_eq!(icon("build.sh"), "file-earmark-code");
        assert_eq!(icon("page.html"), "file-earmark-code");
        assert_eq!(icon("photo.PNG"), "file-earmark-image");
        assert_eq!(icon("shot.ppm"), "file-earmark-image");
        assert_eq!(icon("song.mp3"), "file-earmark-music");
        assert_eq!(icon("clip.mp4"), "file-earmark-play");
        assert_eq!(icon("bundle.zip"), "file-earmark-zip");
        assert_eq!(icon("notes.tar"), "file-earmark-zip");
        // Two honest gaps, recorded rather than papered over: the
        // built-in table has no font extensions and no `.toml`-adjacent
        // oddities, so `.ttf` is the generic file icon *without* a
        // `globs2`. Adding them to `builtin_type` would be an
        // improvement to that table and is not this map's business.
        assert_eq!(icon("Vera.ttf"), "file-earmark");
        assert_eq!(icon("mystery.qqq"), "file-earmark");
        assert_eq!(icon("no-extension"), "file-earmark");
        // And with a `globs2` that does know fonts, the same name is a
        // font — which is the half that shows the map is doing the work
        // rather than the extension table.
        let globs = parse_globs2("50:font/ttf:*.ttf\n");
        let with = icon_for(&type_of(Path::new("Vera.ttf"), &globs).unwrap_or_default());
        assert_eq!(with, "file-earmark-font");
    }
}

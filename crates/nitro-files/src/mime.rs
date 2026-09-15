//! What type a file is, and what opens it.
//!
//! Three questions, answered in order, each with its own freedesktop
//! specification behind it and a deliberately partial reading of it here:
//!
//! 1. **What type is this?** The extension, against the system's
//!    `globs2` table when the machine has one and against a small
//!    built-in table when it does not.
//! 2. **What handles that type?** The `.desktop` id registered for it in
//!    `mimeapps.list` and `mimeinfo.cache`.
//! 3. **How do I run that?** The entry's `Exec`, parsed by the
//!    launcher's `.desktop` reader, with the path appended.
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
//! # Nothing here reads the environment except [`Assoc::from_env`]
//!
//! Every path this module consults is a value handed to it, so a test
//! can point it at a temp directory without touching `$XDG_*`. That
//! matters more than usual here: the test binary is threaded, so
//! `std::env::set_var` would race every other test in it, and the
//! alternative — testing against whatever the developer has installed —
//! is a test that passes on one machine.

use std::path::{Path, PathBuf};

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
        "ogg" => "audio/ogg",
        "wav" => "audio/x-wav",
        "opus" => "audio/opus",
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
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
///
/// **This is called once per file per listing** (`dir::read_dir`), not
/// once per file the user opens, which is what [`ends_with_dot_ext`] is
/// about.
#[must_use]
pub fn type_of(path: &Path, globs: &[Glob]) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    let best = globs
        .iter()
        .filter(|g| ends_with_dot_ext(&name, &g.ext))
        .max_by_key(|g| (g.weight, g.ext.len()));
    if let Some(g) = best {
        return Some(g.mime.clone());
    }
    builtin_type(&name).map(str::to_owned)
}

/// Whether a type is source, a script or markup: **text with a syntax**,
/// which is what the code icon means.
///
/// Its own function rather than an arm of [`icon_for`] because the list is
/// long, and because it has to be consulted *before* the `text/` family:
/// a `.rs`, a `.c`, a `.sh` and an `.html` are all `text/…`, and a code
/// icon says more about them than a document icon does. That ordering is
/// the one real decision in this map and it is visible at the call site.
fn is_code(mime: &str) -> bool {
    matches!(
        mime,
        "text/html"
            | "text/xml"
            | "text/css"
            | "text/javascript"
            | "text/x-rust"
            | "text/x-python"
            | "text/x-python3"
            | "text/x-java"
            | "text/x-java-source"
            | "text/x-go"
            | "text/x-lua"
            | "text/x-perl"
            | "text/x-ruby"
            | "text/x-sql"
            | "text/x-shellscript"
            | "text/x-makefile"
            | "text/x-patch"
            | "text/x-diff"
            | "application/javascript"
            | "application/x-javascript"
            | "application/ecmascript"
            | "application/x-shellscript"
            | "application/x-perl"
            | "application/x-python"
            | "application/x-ruby"
            | "application/x-php"
            | "application/xhtml+xml"
    ) || mime.starts_with("text/x-c")
        || mime.starts_with("text/x-script")
}

/// Whether a type is structured text that is configuration, data or a
/// document rather than code.
fn is_document(mime: &str) -> bool {
    matches!(
        mime,
        "application/json"
            | "application/ld+json"
            | "application/xml"
            | "application/toml"
            | "application/x-toml"
            | "application/x-yaml"
            | "application/yaml"
            | "application/x-desktop"
            | "application/pdf"
            | "application/rtf"
            | "application/x-tex"
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
    // Lowercased and stripped of any `; charset=…` parameter: a `globs2`
    // table holds bare types, but a MIME type is case-insensitive
    // (RFC 2045 §5.1) and this string need not always come from there.
    let mime = mime.split(';').next().unwrap_or(mime).trim();
    let lower = mime.to_ascii_lowercase();
    let mime = lower.as_str();
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

/// Where the MIME associations live.
///
/// Paths only, resolved once and then read on demand: an association
/// lookup happens when the user opens a file, which is rare enough that
/// caching the *contents* would mostly mean showing stale answers after
/// the user installed something.
///
/// Injectable ([`Assoc::at`]) so tests need no environment mutation; see
/// the module documentation for why that matters in a threaded test
/// binary.
#[derive(Debug, Clone)]
pub struct Assoc {
    /// `mimeapps.list` files, most important first.
    mimeapps: Vec<PathBuf>,
    /// `mimeinfo.cache` files, most important first.
    caches: Vec<PathBuf>,
    /// Directories holding `.desktop` files, most important first.
    apps: Vec<PathBuf>,
}

impl Assoc {
    /// The association files this machine has, from the XDG variables.
    ///
    /// `$XDG_CONFIG_HOME/mimeapps.list` (default `~/.config`) first,
    /// then each `$XDG_CONFIG_DIRS` entry, then the `applications`
    /// directory of `$XDG_DATA_HOME` (default `~/.local/share`) and of
    /// each `$XDG_DATA_DIRS` entry. That is the order the spec gives,
    /// most important **first** — the opposite of the order
    /// `nitro_launcher::desktop::search_dirs` returns, and for a reason:
    /// the launcher's scan overwrites as it walks, so it wants the
    /// winner last, while this walks until it finds an answer and stops.
    #[must_use]
    pub fn from_env() -> Assoc {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let config_home = env_path("XDG_CONFIG_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".config")))
            .into_iter();
        let config_dirs = env_paths("XDG_CONFIG_DIRS", "/etc/xdg");
        let data_home = env_path("XDG_DATA_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")))
            .into_iter();
        let data_dirs = env_paths("XDG_DATA_DIRS", "/usr/local/share:/usr/share");
        Assoc::at(
            config_home.chain(config_dirs).collect(),
            data_home.chain(data_dirs).collect(),
        )
    }

    /// The association files under the given roots, most important
    /// first.
    ///
    /// `config_dirs` are searched for `mimeapps.list`; `data_dirs` for
    /// `applications/mimeapps.list`, `applications/mimeinfo.cache` and
    /// the `.desktop` files themselves. Both are directory roots, not
    /// file paths, so a caller passes `/usr/share` and not
    /// `/usr/share/applications`.
    #[must_use]
    pub fn at(config_dirs: Vec<PathBuf>, data_dirs: Vec<PathBuf>) -> Assoc {
        let mut mimeapps: Vec<PathBuf> = config_dirs
            .into_iter()
            .map(|d| d.join("mimeapps.list"))
            .collect();
        let apps: Vec<PathBuf> = data_dirs
            .into_iter()
            .map(|d| d.join("applications"))
            .collect();
        // The data directories carry a `mimeapps.list` too, ranked below
        // every config one: it is where a distribution states its
        // defaults, and a user's `~/.config` must outrank it.
        mimeapps.extend(apps.iter().map(|d| d.join("mimeapps.list")));
        Assoc {
            mimeapps,
            caches: apps.iter().map(|d| d.join("mimeinfo.cache")).collect(),
            apps,
        }
    }

    /// The `.desktop` id registered for `mime`, in the spec's order.
    ///
    /// Every `mimeapps.list`'s `[Default Applications]` first — that is
    /// the user's explicit choice, or the distribution's — then every
    /// list's `[Added Associations]`, then the `mimeinfo.cache` files,
    /// which are what a package's own `MimeType=` key ends up in. An id
    /// named in a `[Removed Associations]` group is skipped wherever it
    /// would otherwise have been found.
    ///
    /// The spec scopes a removal to the files *below* the one that
    /// states it; this treats a removal as global, which is a
    /// simplification with one visible consequence: a user who removed an
    /// association in `~/.config` cannot have a system file put it back.
    /// That is the direction a user's own file should win in anyway, and
    /// the alternative is a per-file merge whose only observable effect
    /// is the case where two files disagree about a removal.
    ///
    /// A value may list several ids separated by `;`; they are tried in
    /// order, and the first that is not removed wins. Whether the id
    /// resolves to a file that exists is [`Assoc::argv_for`]'s question.
    #[must_use]
    pub fn handler_for(&self, mime: &str) -> Option<String> {
        let lists: Vec<Ini> = self.mimeapps.iter().map(|p| Ini::read(p)).collect();
        let removed: Vec<&str> = lists
            .iter()
            .flat_map(|l| l.values("Removed Associations", mime))
            .collect();
        let pick = |group: &str| -> Option<String> {
            lists
                .iter()
                .flat_map(|l| l.values(group, mime))
                .find(|id| !removed.contains(id))
                .map(str::to_owned)
        };
        pick("Default Applications")
            .or_else(|| pick("Added Associations"))
            .or_else(|| {
                self.caches.iter().map(|p| Ini::read(p)).find_map(|cache| {
                    cache
                        .values("MIME Cache", mime)
                        .find(|id| !removed.contains(id))
                        .map(str::to_owned)
                })
            })
    }

    /// A `.desktop` id, resolved to a file and turned into an argv that
    /// opens `path`.
    ///
    /// The id is a file name relative to an `applications` directory.
    /// The spec also allows a `-` in an id to mean a subdirectory
    /// (`kde-konsole.desktop` may live at `kde/konsole.desktop`), so that
    /// is tried too, once, after the plain name — twice `stat`ing a path
    /// is cheaper than missing the application.
    ///
    /// The path is **appended** rather than substituted because
    /// `nitro_launcher::desktop::exec_argv` *strips* the `%f`/`%u` field
    /// codes: it is the launcher's parser, and a launcher opens a program
    /// with no document, so it removes the placeholders rather than
    /// leaving `%U` to be opened as a file called `%U`. Appending gives
    /// the same argv the substitution would have for the overwhelmingly
    /// common `Exec=prog %U` and `Exec=prog %f`; what it gets wrong is an
    /// entry whose field code is not last (`Exec=prog %f --flag`), where
    /// the path lands after the flag instead of before it. Sharing the
    /// launcher's parser — with its `[Desktop Action …]` and `Name[de]`
    /// handling already right and already tested — is worth that.
    ///
    /// `None` when no `applications` directory holds the id, or the file
    /// does not parse as a launchable application entry.
    #[must_use]
    pub fn argv_for(&self, id: &str, path: &Path) -> Option<Vec<String>> {
        let alt = id.replacen('-', "/", 1);
        for dir in &self.apps {
            for candidate in [dir.join(id), dir.join(&alt)] {
                let Ok(text) = std::fs::read_to_string(&candidate) else {
                    continue;
                };
                let Some(entry) = nitro_launcher::desktop::parse(&text, &candidate) else {
                    continue;
                };
                let mut argv = entry.argv;
                argv.push(path.to_string_lossy().into_owned());
                return Some(argv);
            }
        }
        None
    }
}

/// What should open a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Open {
    /// A registered handler, as a ready-to-spawn argv.
    Argv(Vec<String>),
    /// The text fallback: a terminal running an editor on the file.
    ///
    /// A separate case from [`Open::Argv`] even though it is also an
    /// argv, because the caller may want to say something different
    /// about it in the status line ("opening in vi" is worth saying; "
    /// opening in the program you configured" is not) and because it is
    /// the case a future "always ask" prompt would attach to.
    Editor(Vec<String>),
    /// Nothing claims this file.
    ///
    /// Not an error: a `.iso` on a machine with no image mounter is a
    /// file with no handler, and the honest answer is to say so in the
    /// status line rather than to invent one.
    None,
}

/// What opens `path`: type, then handler, then the text fallback.
///
/// The fallback is the useful half of this function. A `text/*` file with
/// no registered handler opens in `term` running `$EDITOR`, or `vi` when
/// that is unset — which is the one program a Unix machine is close to
/// guaranteed to have, and the reason `vi` and not `nano` or the user's
/// taste. It applies to `text/*` only: an editor started on a PDF shows
/// its bytes, which is a worse answer than "nothing opens this".
///
/// The argv is `term -e EDITOR path`, xterm's convention and every
/// terminal emulator's since. `nitro-term` does not honour `-e` yet, so
/// this is the shape that will work the moment it does; today it opens a
/// terminal in which the user can type the command themselves. Recorded
/// in `docs/files.md` under *Limitations*.
#[must_use]
pub fn open_with(path: &Path, globs: &[Glob], assoc: &Assoc, term: &str) -> Open {
    let Some(mime) = type_of(path, globs) else {
        return Open::None;
    };
    if let Some(id) = assoc.handler_for(&mime)
        && let Some(argv) = assoc.argv_for(&id, path)
    {
        return Open::Argv(argv);
    }
    if mime.starts_with("text/") {
        return Open::Editor(editor_argv(
            term,
            path,
            std::env::var("EDITOR").ok().as_deref(),
        ));
    }
    Open::None
}

/// The terminal-plus-editor argv, with `$EDITOR` handed in.
///
/// Split out so the fallback can be tested without setting a variable
/// the whole test binary shares, and so the `vi` default is pinned
/// somewhere rather than being an `unwrap_or` in the middle of a
/// function. An `$EDITOR` that is set but empty counts as unset: an
/// empty program name is not a program.
#[must_use]
pub fn editor_argv(term: &str, path: &Path, editor: Option<&str>) -> Vec<String> {
    let editor = editor
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .unwrap_or("vi");
    vec![
        term.to_owned(),
        "-e".to_owned(),
        editor.to_owned(),
        path.to_string_lossy().into_owned(),
    ]
}

/// An environment variable as a path, if set and not empty.
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// A `:`-separated environment variable as paths, with the spec's
/// default when it is unset or empty.
fn env_paths(key: &str, default: &str) -> Vec<PathBuf> {
    let value = std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_owned());
    value
        .split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The two-level key-value files this module reads.
///
/// `mimeapps.list` and `mimeinfo.cache` are both desktop-entry-format
/// files: `[Group]` headers and `key=value` lines, where the value is a
/// `;`-separated list. A third copy of the launcher's parser is not
/// needed — this one keeps the whole file rather than four keys of it,
/// which the launcher's does not do and does not want to.
#[derive(Debug, Default)]
struct Ini {
    /// `(group, key, values)`, in file order.
    entries: Vec<(String, String, Vec<String>)>,
}

impl Ini {
    /// Read and parse a file; a missing or unreadable one is empty.
    ///
    /// Missing is the normal case — most machines have two of the four
    /// files this module looks for — so it is not an error and not
    /// reported.
    fn read(path: &Path) -> Ini {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ini::default();
        };
        Ini::parse(&text)
    }

    /// Parse the group/key/value structure.
    fn parse(text: &str) -> Ini {
        let mut entries = Vec::new();
        let mut group = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(g) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                g.trim().clone_into(&mut group);
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let values: Vec<String> = value
                .split(';')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .collect();
            if values.is_empty() {
                continue;
            }
            entries.push((group.clone(), key.trim().to_owned(), values));
        }
        Ini { entries }
    }

    /// The values of `key` in `group`, across every occurrence of it.
    ///
    /// Every occurrence, because a file that repeats a key is malformed
    /// and the forgiving reading — both lists, in order — is the one
    /// that loses nothing.
    fn values<'a>(&'a self, group: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> {
        self.entries
            .iter()
            .filter(move |(g, k, _)| g == group && k == key)
            .flat_map(|(_, _, v)| v.iter().map(String::as_str))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own; see `dir::tests::scratch`.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nitro-files-mime-{name}-{}", std::process::id()));
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
            // Source, scripts and markup: text with a *syntax*. These are
            // matched before the `text/` family, which is the one
            // ordering decision in the function.
            ("text/x-csrc", "file-earmark-code"),
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
            ("application/vnd.oasis.opendocument.text", "file-earmark"),
            ("model/gltf+json", "file-earmark"),
            ("", "file-earmark"),
        ];
        for (mime, want) in cases {
            assert_eq!(icon_for(mime), want, "icon_for({mime:?})");
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

    /// A config root and a data root holding the given files.
    fn assoc_fixture(dir: &Path) -> Assoc {
        Assoc::at(vec![dir.join("config")], vec![dir.join("data")])
    }

    #[test]
    fn the_default_application_wins_over_an_added_one() {
        let dir = scratch("default");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\ntext/plain=chosen.desktop\n\
             [Added Associations]\ntext/plain=other.desktop\n",
        );
        let assoc = assoc_fixture(&dir);
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("chosen.desktop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_config_list_outranks_the_system_list_and_the_cache() {
        let dir = scratch("priority");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\ntext/plain=mine.desktop\n",
        );
        write(
            &dir.join("data/applications/mimeapps.list"),
            "[Default Applications]\ntext/plain=distro.desktop\n",
        );
        write(
            &dir.join("data/applications/mimeinfo.cache"),
            "[MIME Cache]\ntext/plain=package.desktop\n",
        );
        let assoc = assoc_fixture(&dir);
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("mine.desktop")
        );

        // With the user's file gone, the distribution's default wins;
        // with that gone too, the cache answers.
        std::fs::remove_file(dir.join("config/mimeapps.list")).expect("rm");
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("distro.desktop")
        );
        std::fs::remove_file(dir.join("data/applications/mimeapps.list")).expect("rm");
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("package.desktop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_removed_association_is_skipped_wherever_it_is_offered() {
        let dir = scratch("removed");
        write(
            &dir.join("config/mimeapps.list"),
            "[Removed Associations]\ntext/plain=bad.desktop\n",
        );
        write(
            &dir.join("data/applications/mimeapps.list"),
            "[Default Applications]\ntext/plain=bad.desktop;good.desktop\n",
        );
        let assoc = assoc_fixture(&dir);
        // The second id in the list is taken: a removal skips the entry
        // rather than the whole line.
        assert_eq!(
            assoc.handler_for("text/plain").as_deref(),
            Some("good.desktop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_type_nobody_registered_has_no_handler() {
        let dir = scratch("nohandler");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\nimage/png=viewer.desktop\n",
        );
        assert_eq!(assoc_fixture(&dir).handler_for("application/pdf"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_id_resolves_to_an_argv_with_the_path_appended() {
        let dir = scratch("argv");
        write(
            &dir.join("data/applications/viewer.desktop"),
            "[Desktop Entry]\nType=Application\nName=Viewer\nExec=viewer --fullscreen %U\n",
        );
        let assoc = assoc_fixture(&dir);
        let argv = assoc
            .argv_for("viewer.desktop", Path::new("/tmp/a.png"))
            .expect("an argv");
        // `%U` is gone (the launcher's parser strips it) and the path is
        // appended in its place.
        assert_eq!(
            argv,
            vec![
                "viewer".to_owned(),
                "--fullscreen".to_owned(),
                "/tmp/a.png".to_owned()
            ]
        );
        // A dashed id may name a subdirectory.
        write(
            &dir.join("data/applications/kde/konsole.desktop"),
            "[Desktop Entry]\nName=Konsole\nExec=konsole\n",
        );
        assert_eq!(
            assoc
                .argv_for("kde-konsole.desktop", Path::new("/tmp/x"))
                .expect("an argv")[0],
            "konsole"
        );
        // An id nothing on disk answers to is `None`, not a panic.
        assert_eq!(assoc.argv_for("ghost.desktop", Path::new("/tmp/x")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unlaunchable_desktop_file_yields_no_argv() {
        let dir = scratch("unlaunchable");
        // Hidden, and so not a thing to open a file with.
        write(
            &dir.join("data/applications/hidden.desktop"),
            "[Desktop Entry]\nName=X\nExec=x\nHidden=true\n",
        );
        // A link entry has no command.
        write(
            &dir.join("data/applications/link.desktop"),
            "[Desktop Entry]\nType=Link\nName=L\nURL=http://x\n",
        );
        let assoc = assoc_fixture(&dir);
        assert_eq!(assoc.argv_for("hidden.desktop", Path::new("/tmp/x")), None);
        assert_eq!(assoc.argv_for("link.desktop", Path::new("/tmp/x")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_a_registered_file_gives_the_handlers_argv() {
        let dir = scratch("open-argv");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\nimage/png=viewer.desktop\n",
        );
        write(
            &dir.join("data/applications/viewer.desktop"),
            "[Desktop Entry]\nName=Viewer\nExec=viewer %f\n",
        );
        let file = dir.join("shot.png");
        write(&file, "");
        let open = open_with(&file, &[], &assoc_fixture(&dir), "nitro-term");
        assert_eq!(
            open,
            Open::Argv(vec![
                "viewer".to_owned(),
                file.to_string_lossy().into_owned()
            ])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_text_file_with_no_handler_falls_back_to_the_editor() {
        let dir = scratch("open-editor");
        let file = dir.join("notes.txt");
        write(&file, "hello");
        let open = open_with(&file, &[], &assoc_fixture(&dir), "nitro-term");
        let Open::Editor(argv) = open else {
            panic!("a text file with no handler opens in the editor");
        };
        assert_eq!(argv[0], "nitro-term");
        assert_eq!(argv[1], "-e");
        assert_eq!(argv[3], file.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_editor_defaults_to_vi_and_honours_a_set_one() {
        let path = Path::new("/tmp/notes.txt");
        assert_eq!(
            editor_argv("nitro-term", path, None),
            vec![
                "nitro-term".to_owned(),
                "-e".to_owned(),
                "vi".to_owned(),
                "/tmp/notes.txt".to_owned()
            ]
        );
        assert_eq!(editor_argv("nitro-term", path, Some("nvim"))[2], "nvim");
        // Set but empty is not a program name.
        assert_eq!(editor_argv("nitro-term", path, Some("  "))[2], "vi");
    }

    #[test]
    fn a_non_text_file_with_no_handler_opens_nothing() {
        let dir = scratch("open-none");
        let pdf = dir.join("book.pdf");
        write(&pdf, "");
        // An editor on a PDF shows its bytes, which is worse than
        // saying nothing can open it.
        assert_eq!(
            open_with(&pdf, &[], &assoc_fixture(&dir), "nitro-term"),
            Open::None
        );
        // And a file of no known type is `None` as well.
        let odd = dir.join("thing.qqq");
        write(&odd, "");
        assert_eq!(
            open_with(&odd, &[], &assoc_fixture(&dir), "nitro-term"),
            Open::None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_handler_whose_desktop_file_is_missing_falls_through() {
        // A stale `mimeapps.list` naming an uninstalled program must not
        // swallow the text fallback: the user still gets an editor.
        let dir = scratch("stale");
        write(
            &dir.join("config/mimeapps.list"),
            "[Default Applications]\ntext/plain=uninstalled.desktop\n",
        );
        let file = dir.join("notes.txt");
        write(&file, "");
        assert!(matches!(
            open_with(&file, &[], &assoc_fixture(&dir), "nitro-term"),
            Open::Editor(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ini_file_keeps_group_scope_and_repeated_keys() {
        let ini = Ini::parse("# comment\n[A]\nk=1;2\n[B]\nk=3\n[A]\nk=4\nragged line\nempty=;;\n");
        assert_eq!(
            ini.values("A", "k").collect::<Vec<_>>(),
            vec!["1", "2", "4"]
        );
        assert_eq!(ini.values("B", "k").collect::<Vec<_>>(), vec!["3"]);
        assert_eq!(ini.values("A", "empty").count(), 0);
        assert_eq!(ini.values("C", "k").count(), 0);
    }
}

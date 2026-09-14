//! `hey`: drive any nitro app from the command line.
//!
//! Every nitro app opens an introspection socket (see
//! `docs/introspection.md`) and answers a line protocol on it. `hey` is
//! the thing that speaks it — which makes "click the OK button" a shell
//! command, and makes every app testable and scriptable without the app
//! knowing anything about it.
//!
//! ```text
//! hey                                    list running apps
//! hey <app> list [path]                  the widget tree
//! hey <app> get <path> [prop]            one widget's properties
//! hey <app> set <path> <prop> <value>    change one
//! hey <app> do <path> <action> [arg]     invoke an action
//! hey <app> watch [path|*]               stream changes until Ctrl-C
//! hey <app> shot [-o file.png]           screenshot that window
//! hey <app> quit                         ask the app to exit
//! ```
//!
//! `<app>` matches by name prefix, by pid, or by `<name>.<pid>` when two
//! copies of the same app are up. Exit codes: 0 ok, 1 the app answered
//! `err` (or something else went wrong), 2 no such app.
//!
//! It depends on `std` and `rustix` and nothing else, on purpose: this
//! is the tool you reach for when something is wrong.

mod png;

use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Exit code for "no app matched".
const NO_APP: u8 = 2;

const USAGE: &str = "\
usage: hey                                       list running apps
       hey <app> list [path]                     the widget tree
       hey <app> tree [path]                     the same, indented
       hey <app> get <path> [prop]               one widget's properties
       hey <app> set <path> <prop> <value>       change one
       hey <app> do <path> <action> [arg]        invoke an action
                 e.g. hey calc do window/container[1]/7 click
       hey <app> watch [path|*]                  stream changes
       hey <app> shot [-o FILE]                  screenshot the window
       hey <app> quit                            ask the app to exit

<app> matches by name prefix, pid, or <name>.<pid> when two copies of
the same app are up. Exit: 0 ok, 1 error, 2 no such app.";

// ---------------------------------------------------------------------
// finding apps
// ---------------------------------------------------------------------

/// One app socket: name, pid, path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct App {
    name: String,
    pid: u32,
    path: PathBuf,
}

/// Where app sockets live. Mirrors `nitro_ui::introspect::socket_dir`;
/// duplicated rather than depended on, because linking the toolkit into
/// this CLI would pull the whole client half of nitro in for one path.
fn socket_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("NITRO_APPS_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        let d = PathBuf::from(d);
        if d.is_absolute() {
            return d.join("nitro").join("apps");
        }
    }
    let uid = rustix::process::getuid().as_raw();
    PathBuf::from(format!("/tmp/nitro-{uid}")).join("apps")
}

/// Split `<name>.<pid>.sock`.
fn parse_socket_name(file: &str) -> Option<(String, u32)> {
    let stem = file.strip_suffix(".sock")?;
    let (name, pid) = stem.rsplit_once('.')?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_owned(), pid.parse().ok()?))
}

/// Every app socket in `dir`, sorted by name then pid — live or not.
///
/// [`prune`] is what separates the two, and every caller that is about
/// to *decide* something runs it first.
fn list_apps(dir: &Path) -> Vec<App> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let file = e.file_name();
        let Some(file) = file.to_str() else { continue };
        if let Some((name, pid)) = parse_socket_name(file) {
            out.push(App {
                name,
                pid,
                path: e.path(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    out
}

/// Whether a process with `pid` still exists.
///
/// `/proc/<pid>` rather than `kill(pid, 0)`: nothing is signalled, no
/// permission is needed, and there is no signal number to get wrong.
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Whether anything is listening on the socket at `path`.
///
/// The half of the test a pid check cannot do. Two cases get past the
/// pid: a **zombie** — a dead app not yet reaped keeps its `/proc/<pid>`
/// entry but has closed its fds, which is every app killed under a
/// supervisor for as long as the reap takes — and **pid reuse**. Both
/// refuse the connection with `ECONNREFUSED`, which means no listener,
/// which means a leftover file. It is not a liveness *timeout* either:
/// `listen(2)` queues the connection in the kernel whether or not the
/// app is in `accept`, so a busy app is never mistaken for a dead one.
///
/// **Only `ECONNREFUSED` and `ENOENT` prove staleness** (issue #548). A
/// `false` here means "stale", and [`prune`] *unlinks* what is stale, so
/// every other error — `EMFILE`/`ENFILE` from a process out of descriptors,
/// `EACCES` on the directory, anything transient — is "don't know", and the
/// answer is `true`: leave the file alone. Otherwise a loaded caller unlinks
/// the socket of a healthy app, which keeps running and keeps its listener
/// but becomes invisible and unreachable until it restarts. Keeping a ghost
/// one round longer is the cheaper mistake.
///
/// Deliberately not handled: `connect(2)` on AF_UNIX *blocks* against an app
/// whose backlog is full, so one wedged app could hang every `hey`
/// invocation — `prune` connects to every socket in the directory. See
/// `docs/introspection.md`.
///
/// This is a copy of `nitro_ui::introspect::responds` and the two must stay
/// in step; `hey` deliberately does not depend on `nitro-ui` (see this
/// crate's README).
fn responds(path: &Path) -> bool {
    match UnixStream::connect(path) {
        Ok(_) => true,
        Err(e) => !matches!(
            e.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
        ),
    }
}

/// Drop, and unlink, every socket that is not a running app.
///
/// An app killed with `SIGTERM` or `SIGKILL` never runs its cleanup and
/// leaves its socket behind. Under `nitro-session`, which *restarts* a
/// shell piece that dies, those accumulate by themselves, and `hey
/// nitro-bar list` then refuses to run — "`nitro-bar` is ambiguous"
/// between one live bar and seven ghosts (#536, #544). The tool you
/// reach for when the bar has misbehaved must not be the tool that
/// stops working once it has.
///
/// So the ghosts go here, before anything is decided: before ambiguity
/// is judged, and before a listing is printed, because a listing that
/// names dead apps is the same lie in a friendlier voice. Unlinking is
/// best effort and silent — a ghost that will not unlink is still
/// excluded, which is the part that matters.
fn prune(apps: Vec<App>) -> Vec<App> {
    let mut out = Vec::with_capacity(apps.len());
    for a in apps {
        if pid_alive(a.pid) && responds(&a.path) {
            out.push(a);
        } else {
            let _ = std::fs::remove_file(&a.path);
        }
    }
    out
}

/// Find the app `want` names: `<name>.<pid>`, a pid, an exact name, or
/// a unique name prefix.
///
/// Ambiguity is an error rather than "the first one": `hey n do …` that
/// silently picked one of two apps would be the worst kind of tool. The
/// candidates are the live ones — [`prune`] has already dropped the
/// leftovers — so two hits really are two apps, and `<name>.<pid>`,
/// which is what the error prints, is how you pick between them.
fn find<'a>(apps: &'a [App], want: &str) -> Result<&'a App, String> {
    // `<name>.<pid>`: the socket's own file name minus `.sock`, and the
    // one selector that can never be ambiguous. Tried first, but only
    // as an exact hit, so an app actually *named* `a.1` still wins by
    // the name rules below.
    if let Some((name, pid)) = want.rsplit_once('.')
        && let Ok(pid) = pid.parse::<u32>()
        && !apps.iter().any(|a| a.name == want)
        && let Some(a) = apps.iter().find(|a| a.name == name && a.pid == pid)
    {
        return Ok(a);
    }
    if let Ok(pid) = want.parse::<u32>()
        && let Some(a) = apps.iter().find(|a| a.pid == pid)
    {
        return Ok(a);
    }
    let exact: Vec<&App> = apps.iter().filter(|a| a.name == want).collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }
    // Two copies of one app are ambiguous between *themselves*, not
    // against every app the prefix would also have caught: naming one
    // of them exactly has already said which name you meant.
    let hits: Vec<&App> = if exact.is_empty() {
        apps.iter().filter(|a| a.name.starts_with(want)).collect()
    } else {
        exact
    };
    match hits.len() {
        0 => Err(format!("no app matching `{want}`")),
        1 => Ok(hits[0]),
        _ => {
            let names: Vec<String> = hits
                .iter()
                .map(|a| format!("{}.{}", a.name, a.pid))
                .collect();
            Err(format!(
                "`{want}` is ambiguous: {}; name one, e.g. `hey {} …`",
                names.join(", "),
                names[0]
            ))
        }
    }
}

// ---------------------------------------------------------------------
// the protocol, client side
// ---------------------------------------------------------------------

/// A connected app socket.
struct Conn(BufReader<UnixStream>);

impl Conn {
    fn open(path: &Path) -> io::Result<Self> {
        let s = UnixStream::connect(path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        Ok(Self(BufReader::new(s)))
    }

    /// Send one request line and read the status line back.
    fn request(&mut self, line: &str) -> io::Result<String> {
        self.0.get_mut().write_all(line.as_bytes())?;
        self.0.get_mut().write_all(b"\n")?;
        self.line()
    }

    fn line(&mut self) -> io::Result<String> {
        let mut s = String::new();
        if self.0.read_line(&mut s)? == 0 {
            return Err(io::Error::other("the app closed the connection"));
        }
        Ok(s.trim_end_matches('\n').to_owned())
    }

    /// Read the body after an `ok`: lines up to a blank one.
    fn body(&mut self) -> io::Result<String> {
        let mut out = String::new();
        loop {
            let mut line = String::new();
            if self.0.read_line(&mut line)? == 0 || line == "\n" {
                return Ok(out);
            }
            out.push_str(&line);
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.0.read_exact(buf)
    }
}

/// The status line of a reply, as `Ok(rest)` or an error carrying the
/// app's own message.
fn status(line: &str) -> io::Result<String> {
    if let Some(rest) = line.strip_prefix("ok") {
        Ok(rest.trim_start().to_owned())
    } else if let Some(msg) = line.strip_prefix("err ") {
        Err(io::Error::other(msg.to_owned()))
    } else {
        Err(io::Error::other(format!("malformed reply {line:?}")))
    }
}

/// Quote a request argument so the app sees it as one field.
fn quote(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

/// Undo the app's field escaping, for printing.
fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => {
                if other != '\\' {
                    out.push('\\');
                }
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

// ---------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------

/// What `hey` was asked to do.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// No app named: list them.
    Apps,
    /// A request whose reply is a text body, printed as-is.
    Text { app: String, request: String },
    /// `watch`: a status line then a stream until the peer hangs up.
    Watch { app: String, request: String },
    /// `shot`: pixels out, optionally as a PNG file.
    Shot {
        app: String,
        file: Option<PathBuf>,
        raw: bool,
    },
}

/// Parse the command line.
///
/// # Errors
/// A message to print, with the usage appended where it helps.
fn parse(args: &[String]) -> Result<Command, String> {
    if args.is_empty() {
        return Ok(Command::Apps);
    }
    if args[0] == "-h" || args[0] == "--help" {
        return Err(USAGE.to_owned());
    }
    let app = args[0].clone();
    let Some(verb) = args.get(1) else {
        // `hey <app>` with no verb lists that app's tree, which is what
        // you want nine times out of ten.
        return Ok(Command::Text {
            app,
            request: "list".to_owned(),
        });
    };
    let rest = &args[2..];
    match verb.as_str() {
        "list" | "tree" | "get" | "set" | "do" => {
            if verb == "get" && rest.is_empty() {
                return Err("get needs a path".to_owned());
            }
            if verb == "set" && rest.len() < 3 {
                return Err("set needs a path, a property and a value".to_owned());
            }
            if verb == "do" && rest.len() < 2 {
                return Err(
                    "do needs a path and an action, e.g. `hey calc do window/container[1]/7 click`"
                        .to_owned(),
                );
            }
            // The app takes everything after `set <path> <prop>` (or
            // `do <path> <action>`) verbatim, so a value with spaces in
            // it arrives whole; escaping is what keeps a tab or a
            // newline inside it from looking like a field break.
            let mut request = verb.clone();
            for a in rest {
                request.push(' ');
                request.push_str(&quote(a));
            }
            Ok(Command::Text { app, request })
        }
        "quit" => Ok(Command::Text {
            app,
            request: "quit".to_owned(),
        }),
        "watch" => {
            let path = rest.first().cloned().unwrap_or_else(|| "*".to_owned());
            Ok(Command::Watch {
                app,
                request: format!("watch {path}"),
            })
        }
        "shot" => {
            let mut file = None;
            let mut raw = false;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-o" | "--out" => {
                        file = Some(PathBuf::from(it.next().ok_or("-o needs a FILE")?.clone()));
                    }
                    "--raw" => raw = true,
                    other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
                }
            }
            Ok(Command::Shot { app, file, raw })
        }
        other => Err(format!("unknown command `{other}`\n{USAGE}")),
    }
}

fn print_apps(apps: &[App]) {
    let width = apps.iter().map(|a| a.name.len()).max().unwrap_or(0);
    let mut out = io::stdout().lock();
    for a in apps {
        let _ = writeln!(out, "{:width$}  {}", a.name, a.pid, width = width);
    }
}

fn write_out(file: Option<&PathBuf>, bytes: &[u8]) -> io::Result<()> {
    if let Some(p) = file {
        std::fs::write(p, bytes)
    } else {
        let mut out = io::stdout().lock();
        out.write_all(bytes)?;
        out.flush()
    }
}

fn run(cmd: Command) -> Result<(), (u8, String)> {
    let dir = socket_dir();
    // Pruned before anything is decided: the leftovers of killed apps
    // are neither listed nor counted towards ambiguity. See [`prune`].
    let apps = prune(list_apps(&dir));
    let fail = |e: io::Error| (1u8, e.to_string());
    match cmd {
        Command::Apps => {
            print_apps(&apps);
            Ok(())
        }
        Command::Text { app, request } => {
            let app = find(&apps, &app).map_err(|e| (NO_APP, e))?;
            let mut conn = Conn::open(&app.path).map_err(fail)?;
            let line = conn.request(&request).map_err(fail)?;
            status(&line).map_err(fail)?;
            let body = conn.body().map_err(fail)?;
            let mut out = io::stdout().lock();
            for l in body.lines() {
                let _ = writeln!(out, "{}", unescape(l));
            }
            Ok(())
        }
        Command::Watch { app, request } => {
            let app = find(&apps, &app).map_err(|e| (NO_APP, e))?;
            let mut conn = Conn::open(&app.path).map_err(fail)?;
            let line = conn.request(&request).map_err(fail)?;
            status(&line).map_err(fail)?;
            // One line per change, until the app exits or we are killed.
            // Flushed per line: a watcher piped into `grep` is useless
            // if it buffers.
            loop {
                match conn.line() {
                    Ok(l) => {
                        let mut out = io::stdout().lock();
                        let _ = writeln!(out, "{}", unescape(&l));
                        let _ = out.flush();
                    }
                    Err(_) => return Ok(()),
                }
            }
        }
        Command::Shot { app, file, raw } => {
            let app = find(&apps, &app).map_err(|e| (NO_APP, e))?;
            let mut conn = Conn::open(&app.path).map_err(fail)?;
            let line = conn.request("shot").map_err(fail)?;
            let header = status(&line).map_err(fail)?;
            let fields: Vec<u32> = header
                .split_whitespace()
                .map(|f| f.parse::<u32>().ok())
                .collect::<Option<_>>()
                .ok_or_else(|| (1u8, format!("bad shot header {header:?}")))?;
            let [width, height, stride] = fields[..] else {
                return Err((1, format!("bad shot header {header:?}")));
            };
            let mut data = vec![0u8; stride as usize * height as usize];
            conn.read_exact(&mut data).map_err(fail)?;
            let bytes = if raw {
                data
            } else {
                png::encode_xrgb(width, height, stride, &data)
            };
            write_out(file.as_ref(), &bytes).map_err(fail)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    match run(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, msg)) => {
            eprintln!("hey: {msg}");
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    fn app(name: &str, pid: u32) -> App {
        App {
            name: name.to_owned(),
            pid,
            path: PathBuf::from(format!("/tmp/{name}.{pid}.sock")),
        }
    }

    #[test]
    fn no_arguments_lists_apps() {
        assert_eq!(parse(&[]).unwrap(), Command::Apps);
    }

    #[test]
    fn a_bare_app_name_lists_its_tree() {
        assert_eq!(
            parse(&args("calc")).unwrap(),
            Command::Text {
                app: "calc".to_owned(),
                request: "list".to_owned()
            }
        );
    }

    #[test]
    fn requests_are_assembled_from_the_arguments() {
        assert_eq!(
            parse(&args("calc do window/ok click")).unwrap(),
            Command::Text {
                app: "calc".to_owned(),
                request: "do window/ok click".to_owned()
            }
        );
        assert_eq!(
            parse(&args("calc get window/x value")).unwrap(),
            Command::Text {
                app: "calc".to_owned(),
                request: "get window/x value".to_owned()
            }
        );
        assert_eq!(
            parse(&args("calc list window/row[0]")).unwrap(),
            Command::Text {
                app: "calc".to_owned(),
                request: "list window/row[0]".to_owned()
            }
        );
    }

    #[test]
    fn incomplete_requests_are_rejected_before_connecting() {
        assert!(parse(&args("calc get")).is_err());
        assert!(parse(&args("calc set window/f text")).is_err());
        assert!(parse(&args("calc do window/ok")).is_err());
        assert!(parse(&args("calc frobnicate")).is_err());
        assert!(parse(&args("--help")).is_err());
    }

    #[test]
    fn watch_defaults_to_the_whole_tree() {
        assert_eq!(
            parse(&args("calc watch")).unwrap(),
            Command::Watch {
                app: "calc".to_owned(),
                request: "watch *".to_owned()
            }
        );
        assert_eq!(
            parse(&args("calc watch window/ok")).unwrap(),
            Command::Watch {
                app: "calc".to_owned(),
                request: "watch window/ok".to_owned()
            }
        );
    }

    #[test]
    fn shot_takes_an_output_file() {
        assert_eq!(
            parse(&args("calc shot -o a.png")).unwrap(),
            Command::Shot {
                app: "calc".to_owned(),
                file: Some(PathBuf::from("a.png")),
                raw: false
            }
        );
        assert_eq!(
            parse(&args("calc shot --raw")).unwrap(),
            Command::Shot {
                app: "calc".to_owned(),
                file: None,
                raw: true
            }
        );
        assert!(parse(&args("calc shot -o")).is_err());
    }

    #[test]
    fn apps_match_by_pid_exact_name_and_unique_prefix() {
        let apps = [app("calc", 10), app("calculator", 11), app("editor", 12)];
        assert_eq!(find(&apps, "12").unwrap().name, "editor");
        // An exact name wins over a prefix that would be ambiguous.
        assert_eq!(find(&apps, "calc").unwrap().pid, 10);
        assert_eq!(find(&apps, "ed").unwrap().name, "editor");
        assert_eq!(find(&apps, "calcu").unwrap().name, "calculator");
        assert!(find(&apps, "ca").is_err(), "ambiguous prefix");
        assert!(find(&apps, "nope").is_err());
        assert!(find(&apps, "999").is_err());
    }

    #[test]
    fn two_copies_of_one_app_are_told_apart_by_name_dot_pid() {
        let apps = [app("bar", 10), app("bar", 11), app("editor", 12)];
        assert_eq!(find(&apps, "bar.11").unwrap().pid, 11);
        assert_eq!(find(&apps, "bar.10").unwrap().pid, 10);
        // And the error says so, rather than leaving you to guess that
        // the thing in the parentheses is usable.
        let e = find(&apps, "bar").unwrap_err();
        assert!(e.contains("bar.10, bar.11"), "{e}");
        assert!(e.contains("hey bar.10"), "{e}");
        // A pid that belongs to another app is not a `bar`.
        assert!(find(&apps, "bar.12").is_err());
        assert!(find(&apps, "bar.999").is_err());
        // An app really named `a.1` is matched by its name, not read as
        // a selector for an app `a` with pid 1.
        let dotted = [app("a.1", 5), app("a", 1)];
        assert_eq!(find(&dotted, "a.1").unwrap().pid, 5);
    }

    #[test]
    fn a_dead_app_is_pruned_and_stops_making_a_name_ambiguous() {
        // A pid that cannot exist: far above the kernel's maximum, so
        // `/proc/<pid>` is certainly absent.
        const DEAD: u32 = u32::MAX - 7;
        let me = std::process::id();
        let dir = std::env::temp_dir().join(format!("hey-prune-{me}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A live app, a ghost of the same name, and a socket whose pid
        // is alive but which nothing listens on — pid 1, which is alive
        // by definition, so only the `connect` can tell. That is the
        // pid-reuse case: a plain file refuses with ECONNREFUSED.
        let live = dir.join(format!("bar.{me}.sock"));
        let _listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
        let ghost = dir.join(format!("bar.{DEAD}.sock"));
        std::fs::write(&ghost, b"").unwrap();
        let reused = dir.join("bar.1.sock");
        std::fs::write(&reused, b"").unwrap();
        assert!(pid_alive(1), "pid 1 is alive, so only `connect` can tell");

        assert_eq!(list_apps(&dir).len(), 3, "all three are files");
        let apps = prune(list_apps(&dir));
        assert_eq!(apps.len(), 1, "but only one is an app: {apps:?}");
        assert_eq!(apps[0].pid, me);
        assert!(!ghost.exists(), "the ghost's socket is unlinked");
        assert!(!reused.exists(), "and so is the one nothing answers on");
        assert!(live.exists(), "the live one is left alone");
        // Which is the whole point: the name resolves again.
        assert_eq!(find(&apps, "bar").unwrap().pid, me);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zombie_is_pruned_although_its_proc_entry_survives() {
        // The case the pid check cannot see, and the common one rather
        // than the exotic one: a killed app is a zombie until its
        // parent reaps it, and a zombie keeps `/proc/<pid>` while having
        // closed every fd. So the pid says "alive" and only the
        // `connect` — refused, because the listener is gone with the
        // process — tells the truth. Every app killed under a
        // supervisor passes through this state.
        //
        // Reproduced exactly rather than simulated: fork a child that
        // exits, and simply never wait for it.
        let me = std::process::id();
        let dir = std::env::temp_dir().join(format!("hey-zombie-{me}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn a child to zombify");
        let zpid = child.id();
        // Wait for it to die without reaping it: `try_wait` would reap,
        // so poll `/proc/<pid>/stat` for the `Z` state instead.
        let mut zombie = false;
        for _ in 0..200 {
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{zpid}/stat"))
                && stat
                    .rsplit(')')
                    .next()
                    .is_some_and(|s| s.split_whitespace().next() == Some("Z"))
            {
                zombie = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(zombie, "child {zpid} never became a zombie");
        assert!(
            pid_alive(zpid),
            "a zombie keeps its /proc entry, so the pid check alone is fooled"
        );

        let ghost = dir.join(format!("bar.{zpid}.sock"));
        std::fs::write(&ghost, b"").unwrap();
        let apps = prune(list_apps(&dir));
        assert!(apps.is_empty(), "the zombie is not an app: {apps:?}");
        assert!(!ghost.exists(), "and its socket is unlinked");

        child.wait().expect("reap");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_directory_is_not_proof_of_death() {
        // Issue #548: only ECONNREFUSED and ENOENT prove staleness. Any other
        // errno is "don't know", and `prune` must leave the file alone --
        // otherwise a loaded or unlucky caller unlinks a healthy app's socket
        // and the app stays running but invisible until it restarts.
        //
        // EACCES is the one that can be produced without privileges: chmod
        // the containing directory to 000 and `connect` cannot even reach the
        // inode. `list_apps` reads the parent, so the socket goes in a
        // subdirectory and the path is built by hand.
        use std::os::unix::fs::PermissionsExt as _;

        let me = std::process::id();
        let dir = std::env::temp_dir().join(format!("hey-eacces-{me}"));
        let _ = std::fs::remove_dir_all(&dir);
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        let sock = locked.join(format!("bar.{me}.sock"));
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let verdict = responds(&sock);
        let err = std::os::unix::net::UnixStream::connect(&sock)
            .map(|_| ())
            .err();

        // Undo before asserting, so a failure still leaves a removable tree.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);

        match err.map(|e| e.kind()) {
            Some(io::ErrorKind::PermissionDenied) => assert!(
                verdict,
                "EACCES is not proof of death: the socket must be left alone"
            ),
            // Running as root, or on a filesystem that ignores the mode: the
            // connect succeeded, which `responds` must also report as live.
            None => assert!(verdict, "a connect that succeeded is a live app"),
            other => panic!("unexpected connect error {other:?}"),
        }
    }

    #[test]
    fn socket_names_round_trip() {
        assert_eq!(
            parse_socket_name("hello-dialog.1234.sock"),
            Some(("hello-dialog".to_owned(), 1234))
        );
        assert_eq!(parse_socket_name("nope.sock"), None);
        assert_eq!(parse_socket_name("x.sock.1"), None);
    }

    #[test]
    fn status_lines_split_ok_from_err() {
        assert_eq!(status("ok").unwrap(), "");
        assert_eq!(status("ok 4 5 6").unwrap(), "4 5 6");
        assert_eq!(
            status("err no such widget").unwrap_err().to_string(),
            "no such widget"
        );
        assert!(status("garbage").is_err());
    }

    #[test]
    fn escaping_round_trips() {
        for s in ["plain", "two\twords", "a\nb", "back\\slash"] {
            assert_eq!(unescape(&quote(s)), s, "{s:?}");
        }
    }

    #[test]
    fn listing_a_directory_finds_sockets_and_ignores_the_rest() {
        let dir = std::env::temp_dir().join(format!("hey-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.7.sock"), b"").unwrap();
        std::fs::write(dir.join("b.3.sock"), b"").unwrap();
        std::fs::write(dir.join("notes.txt"), b"").unwrap();
        let apps = list_apps(&dir);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "a");
        assert_eq!(apps[1].pid, 3);
        assert!(list_apps(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

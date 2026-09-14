//! The introspection socket: every nitro app is scriptable from outside.
//!
//! An app opens one extra Unix socket and answers a line-based text
//! protocol on it — `list`, `get`, `set`, `do`, `watch`, `shot`. Through
//! it another process can read the widget tree, change it, invoke its
//! actions and screenshot it, with no cooperation from the app's own
//! code beyond having widgets at all. `crates/nitro-hey` (`hey`) is the
//! reference consumer; an agent or an accessibility bridge is the same
//! consumer wearing a different hat.
//!
//! Two properties are the whole point, and both are structural:
//!
//! * **There is one tree.** The requests are served out of
//!   [`Ui::introspect`](crate::Ui::introspect), which walks the same
//!   arena the layout and paint passes walk. Nothing is mirrored, so a
//!   widget cannot be in the drawn tree and missing from this one.
//! * **Requests run on the app thread, between events.** The listener
//!   lives in the app's own `epoll` set and
//!   [`Socket::serve`] is called by the app loop, so a `do … click` runs
//!   the real callback with the real `&mut S` and the real `&mut Ui<S>`,
//!   and no request can observe a half-laid-out tree. That is the `BeOS`
//!   property: IPC and the application share one message loop.
//!
//! The protocol, the path grammar and the roles/actions table are in
//! `docs/introspection.md`.

use std::io::{ErrorKind, Read as _, Write as _};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use nitro_core::Rect;

use crate::arena::WidgetId;
use crate::ui::{Node, Ui};
use crate::widget::Role;

/// Longest request line accepted before the connection is dropped.
pub const MAX_LINE: usize = 4096;

/// Environment variable overriding the socket directory.
pub const DIR_ENV: &str = "NITRO_APPS_DIR";

/// The root widget's path segment.
pub const ROOT: &str = "window";

// ---------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------

/// Whether `name` can be used as a path segment.
///
/// A label's accessible name is its text, which is a sentence; an
/// addressable name is an identifier. Rejecting the rest here is what
/// keeps `window/ok` unambiguous without a second "addressing name"
/// field on every widget.
#[must_use]
pub fn addressable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '[' || c == ']' || c == '\t')
}

/// The path of `id` in `ui`'s tree, or `None` if the id is stale.
///
/// The root is always `window`; below it a widget with an addressable
/// name is that name, and one without is `role[i]` counting among the
/// siblings of the same role.
#[must_use]
pub fn path_of<S: 'static>(ui: &Ui<S>, id: WidgetId) -> Option<String> {
    let root = ui.root()?;
    let mut segments = Vec::new();
    let mut cur = id;
    loop {
        if cur == root {
            segments.push(ROOT.to_owned());
            break;
        }
        let parent = ui.parent(cur)?;
        segments.push(segment_of(ui, parent, cur)?);
        cur = parent;
    }
    segments.reverse();
    Some(segments.join("/"))
}

/// One path segment: `child`'s name, or `role[i]` among `parent`'s
/// children of the same role.
fn segment_of<S: 'static>(ui: &Ui<S>, parent: WidgetId, child: WidgetId) -> Option<String> {
    let role = ui.role(child).ok()?;
    if let Some(name) = ui.address_name(child)
        && addressable(&name)
    {
        return Some(name);
    }
    let mut index = 0;
    for c in ui.children(parent) {
        if c == child {
            return Some(format!("{}[{index}]", role.name()));
        }
        if ui.role(c).ok() == Some(role) {
            index += 1;
        }
    }
    None
}

/// Resolve a path against the live tree.
///
/// An empty path, `window` and `/` all name the root. Names are matched
/// before `role[i]` segments, so `window/ok` finds the widget named `ok`
/// whatever its role index would have been.
///
/// A segment that matches no child is then looked for **anywhere in that
/// child's subtree, by name, provided the name occurs exactly once**. So
/// `window/ok` finds the button named `ok` even when it sits three
/// containers down, and an app does not have to spell out
/// `window/container[1]/container[0]/ok` — a path that names the
/// *layout* rather than the widget, and that a refactor of the layout
/// silently breaks. Ambiguity is refused rather than guessed: a name
/// used twice resolves to neither, and the caller gets `no such widget`
/// rather than a coin flip. An explicit `role[i]` segment always wins,
/// so nothing that resolved before resolves differently now.
#[must_use]
pub fn resolve<S: 'static>(ui: &Ui<S>, path: &str) -> Option<WidgetId> {
    let root = ui.root()?;
    let path = path.trim().trim_matches('/');
    if path.is_empty() || path == ROOT {
        return Some(root);
    }
    let mut segments = path.split('/');
    // An absolute path starts at `window`; a relative one is taken from
    // the root anyway, because there is nowhere else to start.
    let first = segments.next()?;
    let mut cur = if first == ROOT {
        root
    } else {
        child_by_segment(ui, root, first).or_else(|| unique_in_subtree(ui, root, first))?
    };
    for seg in segments {
        cur = child_by_segment(ui, cur, seg).or_else(|| unique_in_subtree(ui, cur, seg))?;
    }
    Some(cur)
}

/// The one widget under `from` whose addressing name is `name`, or
/// `None` when there is no such widget **or more than one**.
///
/// Pre-order, and it keeps walking after a hit rather than returning the
/// first: the whole value of the rule is that it refuses an ambiguous
/// name, and a search that stopped early could not know whether the name
/// was ambiguous.
fn unique_in_subtree<S: 'static>(ui: &Ui<S>, from: WidgetId, name: &str) -> Option<WidgetId> {
    if !addressable(name) {
        return None;
    }
    let mut found = None;
    let mut stack = ui.children(from);
    while let Some(id) = stack.pop() {
        if ui.address_name(id).as_deref() == Some(name) {
            if found.is_some() {
                return None;
            }
            found = Some(id);
        }
        stack.extend(ui.children(id));
    }
    found
}

fn child_by_segment<S: 'static>(ui: &Ui<S>, parent: WidgetId, seg: &str) -> Option<WidgetId> {
    let children = ui.children(parent);
    for c in &children {
        if let Some(name) = ui.address_name(*c)
            && addressable(&name)
            && name == seg
        {
            return Some(*c);
        }
    }
    let (role_name, index) = parse_indexed(seg)?;
    let mut n = 0;
    for c in &children {
        let Ok(role) = ui.role(*c) else { continue };
        if role.name() == role_name {
            if n == index {
                return Some(*c);
            }
            n += 1;
        }
    }
    None
}

/// `role[i]` → `("role", i)`; a bare `role` means index 0.
fn parse_indexed(seg: &str) -> Option<(&str, usize)> {
    match seg.split_once('[') {
        Some((role, rest)) => {
            let index = rest.strip_suffix(']')?.parse().ok()?;
            Some((role, index))
        }
        None => Some((seg, 0)),
    }
}

// ---------------------------------------------------------------------
// escaping
// ---------------------------------------------------------------------

/// Escape a field for the tab-separated line protocol.
#[must_use]
pub fn escape(s: &str) -> String {
    if !s.contains(['\t', '\n', '\r', '\\']) {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

/// Undo [`escape`]. An unknown escape keeps its backslash, so nothing is
/// silently lost.
#[must_use]
pub fn unescape(s: &str) -> String {
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
// where the socket lives
// ---------------------------------------------------------------------

/// Sanitise an app name into a file name component.
#[must_use]
pub fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "app".to_owned() } else { s }
}

/// The directory app sockets live in, given the environment: the
/// override, else `<runtime>/nitro/apps`, else `/tmp/nitro-<uid>/apps`.
///
/// Taken as arguments rather than read here, for the same reason
/// `nitro_server::control::resolve` does: it is the only way to test the
/// rule without mutating the process's environment, which is `unsafe`
/// and races every other test in the binary.
#[must_use]
pub fn resolve_dir(override_dir: Option<&Path>, runtime_dir: Option<&Path>, uid: u32) -> PathBuf {
    if let Some(d) = override_dir {
        return d.to_path_buf();
    }
    match runtime_dir {
        Some(d) if d.is_absolute() => d.join("nitro").join("apps"),
        _ => PathBuf::from(format!("/tmp/nitro-{uid}")).join("apps"),
    }
}

/// The directory app sockets live in: `$NITRO_APPS_DIR`, else
/// `$XDG_RUNTIME_DIR/nitro/apps`, else `/tmp/nitro-<uid>/apps`.
///
/// The same rule `nitro-server`'s control socket uses, one level deeper,
/// so `hey` can find every app by reading one directory.
#[must_use]
pub fn socket_dir() -> PathBuf {
    let over = std::env::var_os(DIR_ENV).map(PathBuf::from);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    resolve_dir(
        over.as_deref(),
        runtime.as_deref(),
        rustix::process::getuid().as_raw(),
    )
}

/// The socket path for `name` in this process: `<dir>/<name>.<pid>.sock`.
#[must_use]
pub fn socket_path(name: &str) -> PathBuf {
    let pid = rustix::process::getpid().as_raw_nonzero().get();
    socket_dir().join(format!("{}.{pid}.sock", sanitize(name)))
}

/// One app socket found in the directory: its name, pid and path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSocket {
    /// The app's (sanitised) name.
    pub name: String,
    /// The app's process id.
    pub pid: u32,
    /// Full path of the socket.
    pub path: PathBuf,
}

/// Split `<name>.<pid>.sock` back into its parts.
#[must_use]
pub fn parse_socket_name(file: &str) -> Option<(String, u32)> {
    let stem = file.strip_suffix(".sock")?;
    let (name, pid) = stem.rsplit_once('.')?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_owned(), pid.parse().ok()?))
}

/// Whether a process with `pid` still exists.
///
/// `/proc/<pid>` rather than `kill(pid, 0)`: nothing is signalled and no
/// permission is needed. It is only ever half the test — a zombie and a
/// reused pid both read as alive here, and [`responds`] is what catches
/// them.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Whether anything is listening on the socket at `path`.
///
/// The honest test, and the complement of [`pid_alive`]: `listen(2)`
/// queues the connection whether or not the app ever calls `accept`, so
/// a `connect` that succeeds proves a live listener rather than a
/// prompt one, and a `connect` that is refused proves the file is a
/// leftover — including for a zombie, which still has a `/proc` entry
/// but no open fds. The connection is dropped immediately; the app
/// accepts it, reads nothing and closes it, which costs it one loop
/// turn.
///
/// **Only `ECONNREFUSED` and `ENOENT` prove staleness** (issue #548). A
/// `false` here means "stale", and stale means the file gets *unlinked*, so
/// every other error — `EMFILE`/`ENFILE` from a process that has run out of
/// descriptors, `EACCES` on the directory, anything transient — is "don't
/// know", and the answer is `true`: leave the file alone. Otherwise a loaded
/// caller unlinks the sockets of healthy apps, and a live app that keeps its
/// listener becomes invisible and unreachable until it restarts. Erring
/// toward keeping a ghost one round longer is already the principle
/// [`sweep_dead`] documents for its pid-only check.
///
/// One thing this deliberately does **not** handle: `connect(2)` on AF_UNIX
/// *blocks* against a live-but-not-serving app whose backlog is full (128
/// queued connections against a peer that has stopped calling `accept`), so
/// one wedged app could hang a caller that walks the whole directory. The fix
/// is a non-blocking connect with a timeout, which is a fair amount of
/// machinery for a case a long way from anything seen; recorded in
/// `docs/introspection.md` rather than fixed.
#[must_use]
pub fn responds(path: &Path) -> bool {
    match UnixStream::connect(path) {
        Ok(_) => true,
        Err(e) => !matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound),
    }
}

/// Whether `sock` is a leftover rather than a running app.
///
/// The pid check is the cheap one and runs first; the `connect` catches
/// the two cases it cannot. A **zombie** — a dead app not yet reaped by
/// its parent — still has a `/proc/<pid>` entry but has closed its fds,
/// so only the `connect` refuses it; that is every app killed under a
/// supervisor, for as long as the reap takes. **Pid reuse** is the
/// other: a dead app's number handed to something else.
#[must_use]
pub fn stale(sock: &AppSocket) -> bool {
    !pid_alive(sock.pid) || !responds(&sock.path)
}

/// Unlink every `<name>.<pid>.sock` in `dir` whose process is gone, and
/// report how many went.
///
/// Called on [`Socket::bind`], which is the cheapest place to notice: an
/// app killed with `SIGKILL` — or with `SIGTERM`, since the toolkit
/// installs no handler — never runs its `Drop` and leaves its socket
/// behind, and after a few restarts `hey <name>` is ambiguous between
/// one live app and several ghosts (#536, #544). Restarting the app is
/// exactly when that stops being true.
///
/// Only the pid is consulted, because there is no listener to ask yet.
/// That makes the sweep deliberately conservative: a sibling that is
/// still a zombie, or whose pid has been reused, is left alone here and
/// removed by the next [`list_apps`] or `hey`, both of which do the
/// `connect` half too (see [`stale`]). Leaving a ghost one round too
/// long is harmless; unlinking a live app's socket would not be.
///
/// Best effort: a file that will not unlink (another copy of the app
/// sweeping the same directory, say) is skipped silently. This is
/// hygiene, not a transaction.
pub fn sweep_dead(dir: &Path, name: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let file = e.file_name();
        let Some(file) = file.to_str() else { continue };
        let Some((got, pid)) = parse_socket_name(file) else {
            continue;
        };
        if got == name && !pid_alive(pid) && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Every **live** app socket in `dir`, sorted by name then pid; the
/// leftovers of apps that are gone are unlinked on the way past.
///
/// A socket whose process is gone, or whose file nothing is listening
/// on, is not an app — it is what a killed app left behind. Returning
/// it would make `hey <name>` call a name ambiguous between one app and
/// three ghosts, which is what `nitro-session` restarting the shell
/// automatically turned from a curiosity into a daily failure (#544).
/// Pruning here rather than at every call site is what makes *every*
/// reader of the directory truthful, listings included.
#[must_use]
pub fn list_apps(dir: &Path) -> Vec<AppSocket> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let file = e.file_name();
        let Some(file) = file.to_str() else { continue };
        if let Some((name, pid)) = parse_socket_name(file) {
            let sock = AppSocket {
                name,
                pid,
                path: e.path(),
            };
            if stale(&sock) {
                let _ = std::fs::remove_file(&sock.path);
                continue;
            }
            out.push(sock);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    out
}

// ---------------------------------------------------------------------
// the listener
// ---------------------------------------------------------------------

/// The app's introspection listener plus its connected clients.
///
/// Owned by the app loop, which calls [`Socket::accept`] when the
/// listener is readable and [`Socket::serve`] once per turn. Dropping it
/// unlinks the socket file.
#[derive(Debug)]
pub struct Socket {
    listener: UnixListener,
    path: PathBuf,
    clients: Vec<Client>,
    /// Snapshot of the last introspection walk, for `watch`. Only taken
    /// while at least one client is watching, so an unwatched app pays
    /// nothing.
    snapshot: Vec<(WidgetId, String, bool)>,
    scratch: Vec<Node>,
    /// Widgets activated since the last drain, for `click` events.
    activated: Vec<WidgetId>,
}

/// One connected introspection client.
#[derive(Debug)]
struct Client {
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
    /// The path this client is watching, if it sent `watch`.
    watching: Option<String>,
    /// Set when the connection is finished with (hangup, overflow, or a
    /// `quit` that has been answered).
    done: bool,
}

impl Socket {
    /// Create the socket directory (`0700`), sweep the sockets earlier
    /// runs of this app left behind and bind a non-blocking listener
    /// for `name`.
    ///
    /// # Errors
    /// Directory creation or bind failure.
    pub fn bind(name: &str) -> std::io::Result<Self> {
        Self::bind_at(&socket_path(name))
    }

    /// As [`Socket::bind`], at an explicit path. What the tests use.
    ///
    /// # Errors
    /// Directory creation or bind failure.
    pub fn bind_at(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            match std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
            {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        // The sockets of *earlier* runs of this app, whose processes are
        // gone. A restart is the cheapest moment to notice, and the one
        // that makes the box recover on its own rather than accumulating
        // ghosts until someone runs `hey` and is told a name is
        // ambiguous. See [`sweep_dead`].
        if let (Some(dir), Some(file)) = (path.parent(), path.file_name().and_then(|f| f.to_str()))
            && let Some((name, _)) = parse_socket_name(file)
        {
            sweep_dead(dir, &name);
        }
        // And our own path, if a previous process with our pid left one.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            clients: Vec::new(),
            snapshot: Vec::new(),
            scratch: Vec::new(),
            activated: Vec::new(),
        })
    }

    /// The listener, for the app loop's `epoll` set.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.listener.as_fd()
    }

    /// Where the socket is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether anything is connected, so the loop knows to poll them.
    #[must_use]
    pub fn busy(&self) -> bool {
        !self.clients.is_empty()
    }

    /// How many clients are connected.
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Accept everything pending on the listener.
    pub fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_ok() {
                        self.clients.push(Client {
                            stream,
                            input: Vec::new(),
                            output: Vec::new(),
                            written: 0,
                            watching: None,
                            done: false,
                        });
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }

    /// Read, execute and answer every pending request, then push queued
    /// events at the watchers.
    ///
    /// Called by the app loop **between events**, so a request runs with
    /// the tree settled and the app's state free: `do … click` invokes
    /// the same callback a real click would, with the same `&mut S`.
    pub fn serve<S: 'static>(&mut self, ui: &mut Ui<S>, state: &mut S) {
        for i in 0..self.clients.len() {
            self.read_client(i);
            while let Some(line) = take_line(&mut self.clients[i].input) {
                let reply = self.handle(ui, state, i, &line);
                self.clients[i].output.extend_from_slice(&reply);
            }
        }
        self.emit_events(ui);
        for c in &mut self.clients {
            c.flush();
        }
        self.clients.retain(|c| !c.done);
        if !self.clients.iter().any(|c| c.watching.is_some()) {
            self.snapshot.clear();
            self.snapshot.shrink_to_fit();
        }
    }

    fn read_client(&mut self, i: usize) {
        let c = &mut self.clients[i];
        let mut buf = [0u8; 1024];
        loop {
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    c.done = true;
                    return;
                }
                Ok(n) => {
                    c.input.extend_from_slice(&buf[..n]);
                    if c.input.len() > MAX_LINE && !c.input.contains(&b'\n') {
                        c.output.extend_from_slice(b"err line too long\n\n");
                        c.done = true;
                        return;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    c.done = true;
                    return;
                }
            }
        }
    }

    /// Execute one request line and return its whole reply.
    fn handle<S: 'static>(
        &mut self,
        ui: &mut Ui<S>,
        state: &mut S,
        client: usize,
        line: &str,
    ) -> Vec<u8> {
        let line = line.trim_end_matches(['\r', '\n']);
        let mut words = line.split_whitespace();
        let Some(cmd) = words.next() else {
            return b"err empty request\n\n".to_vec();
        };
        match cmd {
            "list" | "tree" => {
                let path = words.next().unwrap_or(ROOT);
                match list(ui, path, cmd == "tree") {
                    Ok(body) => ok_body(&body),
                    Err(e) => err(&e),
                }
            }
            "get" => {
                let Some(path) = words.next() else {
                    return err("get needs a path");
                };
                match get(ui, path, words.next()) {
                    Ok(body) => ok_body(&body),
                    Err(e) => err(&e),
                }
            }
            "set" => {
                let (Some(path), Some(prop)) = (words.next(), words.next()) else {
                    return err("set needs a path, a property and a value");
                };
                // The value is the rest of the line, verbatim: a text
                // field's contents have spaces in them.
                let value = rest_after(line, &[cmd, path, prop]);
                match set(ui, state, path, prop, &unescape(&value)) {
                    Ok(()) => ok_empty(),
                    Err(e) => err(&e),
                }
            }
            "do" => {
                let (Some(path), Some(action)) = (words.next(), words.next()) else {
                    return err(
                        "do needs a path and an action, e.g. `do window/container[1]/7 click`",
                    );
                };
                let arg = rest_after(line, &[cmd, path, action]);
                let arg = unescape(&arg);
                let arg = if arg.is_empty() { None } else { Some(arg) };
                match invoke(ui, state, path, action, arg.as_deref()) {
                    Ok(()) => ok_empty(),
                    Err(e) => err(&e),
                }
            }
            "watch" => {
                let path = words.next().unwrap_or("*").to_owned();
                if resolve_watch(ui, &path).is_none() {
                    return err("no such widget");
                }
                self.clients[client].watching = Some(path);
                self.take_snapshot(ui);
                b"ok\n".to_vec()
            }
            "shot" => shot(ui),
            "quit" => {
                ui.quit();
                self.clients[client].done = true;
                ok_empty()
            }
            other => err(&format!("unknown request `{other}`")),
        }
    }

    /// Walk the tree and hand every watcher the changes since last time.
    fn emit_events<S: 'static>(&mut self, ui: &mut Ui<S>) {
        // The activation list is drained whether or not anyone is
        // watching, so an unwatched app cannot accumulate one.
        let mut activated = std::mem::take(&mut self.activated);
        ui.take_activations(&mut activated);
        if !self.clients.iter().any(|c| c.watching.is_some()) {
            activated.clear();
            self.activated = activated;
            return;
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        ui.introspect(&mut scratch);
        let mut events: Vec<(String, &'static str, String)> = Vec::new();
        for id in &activated {
            if let Some(path) = path_of(ui, *id) {
                events.push((path, "click", String::new()));
            }
        }
        for node in &scratch {
            let value = node.access.value.clone().unwrap_or_default();
            let focused = node.focused;
            let previous = self.snapshot.iter().find(|(id, _, _)| *id == node.id);
            let Some((_, old_value, old_focus)) = previous else {
                continue;
            };
            if *old_value != value || *old_focus != focused {
                let Some(path) = path_of(ui, node.id) else {
                    continue;
                };
                if *old_value != value {
                    events.push((path.clone(), "value", value.clone()));
                }
                if *old_focus != focused {
                    events.push((path, "focus", focused.to_string()));
                }
            }
        }
        self.snapshot.clear();
        self.snapshot.extend(
            scratch
                .iter()
                .map(|n| (n.id, n.access.value.clone().unwrap_or_default(), n.focused)),
        );
        scratch.clear();
        self.scratch = scratch;
        activated.clear();
        self.activated = activated;
        if events.is_empty() {
            return;
        }
        for c in &mut self.clients {
            let Some(watch) = c.watching.clone() else {
                continue;
            };
            for (path, kind, value) in &events {
                if !watch_matches(&watch, path) {
                    continue;
                }
                let line = format!("event {} {kind} {}\n", escape(path), escape(value));
                c.output.extend_from_slice(line.as_bytes());
            }
        }
    }

    fn take_snapshot<S: 'static>(&mut self, ui: &Ui<S>) {
        let mut scratch = std::mem::take(&mut self.scratch);
        ui.introspect(&mut scratch);
        self.snapshot.clear();
        self.snapshot.extend(
            scratch
                .iter()
                .map(|n| (n.id, n.access.value.clone().unwrap_or_default(), n.focused)),
        );
        scratch.clear();
        self.scratch = scratch;
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // Leaving the file behind would make `hey` list an app that is
        // not there; the connect would fail, but confusingly.
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Client {
    fn flush(&mut self) {
        while self.written < self.output.len() {
            match self.stream.write(&self.output[self.written..]) {
                Ok(0) => {
                    self.done = true;
                    return;
                }
                Ok(n) => self.written += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    self.done = true;
                    return;
                }
            }
        }
        self.output.clear();
        self.written = 0;
    }
}

/// Whether a `watch` path covers `path`: `*` covers everything, and a
/// path covers itself and its descendants.
#[must_use]
pub fn watch_matches(watch: &str, path: &str) -> bool {
    watch == "*"
        || watch == path
        || path
            .strip_prefix(watch)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn resolve_watch<S: 'static>(ui: &Ui<S>, path: &str) -> Option<WidgetId> {
    if path == "*" {
        return ui.root();
    }
    resolve(ui, path)
}

/// Split the first complete line off `buf`.
fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let nl = buf.iter().position(|&b| b == b'\n')?;
    let line: Vec<u8> = buf.drain(..=nl).collect();
    Some(String::from_utf8_lossy(&line[..nl]).into_owned())
}

/// Everything in `line` after the given leading words, trimmed.
///
/// Each word is stripped in turn rather than the line being split, so
/// the value keeps its internal spacing: `set f text hello  world` sets
/// two spaces, not one. Leading and trailing whitespace *is* trimmed —
/// the protocol is whitespace-separated and has no way to quote a
/// leading space, and `\t` is there for anything that needs to be
/// literal.
fn rest_after(line: &str, words: &[&str]) -> String {
    let mut rest = line.trim_start();
    for w in words {
        rest = rest.trim_start();
        rest = rest.strip_prefix(*w).unwrap_or(rest);
    }
    rest.trim().to_owned()
}

fn err(msg: &str) -> Vec<u8> {
    format!("err {}\n\n", msg.replace(['\n', '\r'], " ")).into_bytes()
}

fn ok_empty() -> Vec<u8> {
    b"ok\n\n".to_vec()
}

fn ok_body(body: &str) -> Vec<u8> {
    let mut out = String::with_capacity(body.len() + 5);
    out.push_str("ok\n");
    out.push_str(body);
    out.push('\n');
    out.into_bytes()
}

// ---------------------------------------------------------------------
// the read-only requests
// ---------------------------------------------------------------------

/// `list`: one line per widget in tree order, rooted at `path`.
fn list<S: 'static>(ui: &Ui<S>, path: &str, indent: bool) -> Result<String, String> {
    let id = resolve(ui, path).ok_or("no such widget")?;
    let mut out = String::new();
    list_into(ui, id, 0, indent, &mut out);
    Ok(out)
}

fn list_into<S: 'static>(ui: &Ui<S>, id: WidgetId, depth: usize, indent: bool, out: &mut String) {
    if let Some(line) = describe(ui, id) {
        if indent {
            for _ in 0..depth {
                out.push_str("  ");
            }
        }
        out.push_str(&line);
        out.push('\n');
    }
    for c in ui.children(id) {
        list_into(ui, c, depth + 1, indent, out);
    }
}

/// One `list` line: path, role, name, value, bounds, flags.
fn describe<S: 'static>(ui: &Ui<S>, id: WidgetId) -> Option<String> {
    let node = ui.introspect_node(id).ok()?;
    let path = path_of(ui, id)?;
    let name = ui
        .address_name(id)
        .filter(|n| addressable(n))
        .unwrap_or_else(|| "-".to_owned());
    let value = node.access.value.as_deref().unwrap_or("-");
    Some(format!(
        "{}\t{}\t{}\t{}\t{}\t{}",
        escape(&path),
        node.role.name(),
        escape(&name),
        escape(value),
        format_bounds(node.bounds),
        flags(ui, &node)
    ))
}

/// `x,y,w,h`, rounded: the protocol is for scripts and humans, and a
/// fractional pixel helps neither.
#[must_use]
pub fn format_bounds(r: Rect) -> String {
    format!(
        "{},{},{},{}",
        r.x.round() as i32,
        r.y.round() as i32,
        r.w.round() as i32,
        r.h.round() as i32
    )
}

fn flags<S: 'static>(ui: &Ui<S>, node: &Node) -> String {
    let mut out = Vec::new();
    if node.focused {
        out.push("focused");
    }
    if ui.is_hovered(node.id) {
        out.push("hovered");
    }
    if node.focusable {
        out.push("focusable");
    }
    if crate::introspect::enabled(ui, node.id) {
        out.push("enabled");
    }
    if out.is_empty() {
        "-".to_owned()
    } else {
        out.join(",")
    }
}

/// One introspection property of one widget, as `hey … get <path>
/// <prop>` serves it.
///
/// The protocol's own `get` is private because it formats a reply; this
/// is the same lookup without the framing, so a test can assert on the
/// property a script would actually read rather than on the widget's
/// `Access` — which is a different thing, and the box run for M4-A found
/// exactly that gap: `value` was right and `text` was empty.
///
/// # Errors
/// `no such widget` for an unresolvable path, or `unknown property`.
pub fn get_prop<S: 'static>(ui: &Ui<S>, path: &str, prop: &str) -> Result<String, String> {
    get(ui, path, Some(prop)).map(|s| s.trim_end_matches('\n').to_owned())
}

/// `get`: every property, or one.
fn get<S: 'static>(ui: &Ui<S>, path: &str, prop: Option<&str>) -> Result<String, String> {
    let id = resolve(ui, path).ok_or("no such widget")?;
    let node = ui.introspect_node(id).map_err(|e| e.to_string())?;
    let own_path = path_of(ui, id).unwrap_or_default();
    let value = node.access.value.clone().unwrap_or_default();
    let name = ui
        .address_name(id)
        .filter(|n| addressable(n))
        .unwrap_or_default();
    // `text` is the value of the widgets whose value *is* text, which
    // is what lets a caller ask "what does this read" without knowing
    // the role. A terminal's screen belongs in that set for the same
    // reason a label's string does — and it is the whole of how
    // `nitro-term` is driven from outside, since reading the screen as
    // text is what replaces a font, a screenshot and an OCR step. A
    // list is the same shape one dimension down: its value is the rows
    // it is showing, one per line, which is how `hey nitro-files get
    // list text` reads a directory without a screenshot.
    let text = if matches!(
        node.role,
        Role::Label | Role::Button | Role::TextField | Role::Terminal | Role::List
    ) {
        value.clone()
    } else {
        String::new()
    };
    let props: Vec<(&str, String)> = vec![
        ("path", own_path),
        ("role", node.role.name().to_owned()),
        ("name", name),
        ("value", value),
        ("text", text),
        ("bounds", format_bounds(node.bounds)),
        ("enabled", enabled(ui, id).to_string()),
        ("focused", node.focused.to_string()),
        ("focusable", node.focusable.to_string()),
        ("hovered", ui.is_hovered(id).to_string()),
        ("children", node.children.len().to_string()),
        ("actions", node.access.actions.join(",")),
    ];
    if let Some(want) = prop {
        return props
            .iter()
            .find(|(k, _)| *k == want)
            .map(|(_, v)| {
                // The single-property form is one line and the reply
                // ends at a blank one, so an empty value would be
                // indistinguishable from no reply at all. `-` is what
                // `list` already prints for an absent value, and using
                // it here keeps the two forms reading the same. The
                // full form is tab-separated and has no such ambiguity.
                if v.is_empty() {
                    "-\n".to_owned()
                } else {
                    format!("{}\n", escape(v))
                }
            })
            .ok_or_else(|| format!("unknown property `{want}`"));
    }
    let mut out = String::new();
    for (k, v) in &props {
        out.push_str(k);
        out.push('\t');
        out.push_str(&escape(v));
        out.push('\n');
    }
    Ok(out)
}

/// Whether the widget at `id` reacts to input.
#[must_use]
pub fn enabled<S: 'static>(ui: &Ui<S>, id: WidgetId) -> bool {
    ui.is_enabled(id)
}

/// `do`: invoke an action through the widget's own `action` method.
///
/// The callbacks fire, because [`Ui::action`] uses the same take-out
/// dispatch a real event does.
///
/// # Errors
/// A message suitable for an `err` reply.
pub fn invoke<S: 'static>(
    ui: &mut Ui<S>,
    state: &mut S,
    path: &str,
    action: &str,
    arg: Option<&str>,
) -> Result<(), String> {
    let id = resolve(ui, path).ok_or("no such widget")?;
    match ui.action(state, id, action, arg) {
        Ok(h) if h.is_handled() => Ok(()),
        Ok(_) => Err(format!("unknown action `{action}`")),
        Err(e) => Err(e.to_string()),
    }
}

/// `set`: change a property through the same setter the app would use.
///
/// Framework properties (`name`, `focused`) are the framework's;
/// everything else is `set_<prop>` on the widget, which is how a widget
/// gets a settable property without registering anything.
///
/// # Errors
/// A message suitable for an `err` reply.
pub fn set<S: 'static>(
    ui: &mut Ui<S>,
    state: &mut S,
    path: &str,
    prop: &str,
    value: &str,
) -> Result<(), String> {
    let id = resolve(ui, path).ok_or("no such widget")?;
    match prop {
        "name" => {
            ui.action(state, id, "set_name", Some(value))
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        "focused" => {
            if value == "true" {
                ui.action(state, id, "focus", None)
                    .map_err(|e| e.to_string())?;
            } else {
                ui.blur(state);
            }
            Ok(())
        }
        "path" | "role" | "bounds" | "children" | "actions" | "hovered" | "focusable" => {
            Err(format!("`{prop}` is read-only"))
        }
        other => {
            let action = format!("set_{other}");
            match ui.action(state, id, &action, Some(value)) {
                Ok(h) if h.is_handled() => Ok(()),
                Ok(_) => Err(format!("no settable property `{other}`")),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

/// `shot`: the server's screenshot of the output, cropped to this
/// window.
fn shot<S: 'static>(ui: &Ui<S>) -> Vec<u8> {
    let size = ui.window_size();
    let origin = ui.window_position();
    let scale = ui.scale();
    let path = ui.control_path();
    match crate::shot::window_shot_at(&path, origin, size, scale) {
        Ok(img) => {
            let mut out = format!("ok {} {} {}\n", img.width, img.height, img.stride).into_bytes();
            out.extend_from_slice(&img.data);
            out
        }
        Err(e) => err(&format!("shot: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressable_names_are_identifier_like() {
        assert!(addressable("ok"));
        assert!(addressable("first-name"));
        assert!(!addressable(""));
        assert!(!addressable("Hello, nitro"));
        assert!(!addressable("a/b"));
        assert!(!addressable("a[0]"));
    }

    #[test]
    fn escaping_round_trips() {
        for s in ["plain", "two\twords", "a\nb", "back\\slash", ""] {
            assert_eq!(unescape(&escape(s)), s, "{s:?}");
        }
        // Unknown escapes keep their backslash rather than vanishing.
        assert_eq!(unescape("\\q"), "\\q");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn indexed_segments_parse() {
        assert_eq!(parse_indexed("button[2]"), Some(("button", 2)));
        assert_eq!(parse_indexed("label"), Some(("label", 0)));
        assert_eq!(parse_indexed("button[x]"), None);
        assert_eq!(parse_indexed("button[2"), None);
    }

    #[test]
    fn an_unreadable_directory_is_not_proof_of_death() {
        // Issue #548: only ECONNREFUSED and ENOENT prove staleness. Any other
        // errno is "don't know" and the socket file must be left alone --
        // `stale()` feeding `list_apps`/`sweep_dead` is what *unlinks* it, so
        // a wrong `false` here makes a healthy app invisible and unreachable
        // until it restarts. The trigger to watch for is a long-running
        // in-process caller of `list_apps`, where EMFILE stops being
        // theoretical.
        //
        // EACCES is the one reproducible without privileges: chmod the
        // containing directory to 000 and `connect` cannot reach the inode.
        use std::os::unix::fs::PermissionsExt as _;

        let me = rustix::process::getpid().as_raw_nonzero().get() as u32;
        let dir = std::env::temp_dir().join(format!("nitro-eacces-{me}-{:?}", {
            std::thread::current().id()
        }));
        let _ = std::fs::remove_dir_all(&dir);
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        let sock = locked.join(format!("ghost.{me}.sock"));
        let listener = UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let verdict = responds(&sock);
        let err = UnixStream::connect(&sock).map(|_| ()).err();

        // Undo before asserting, so a failure still leaves a removable tree.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);

        match err.map(|e| e.kind()) {
            Some(ErrorKind::PermissionDenied) => assert!(
                verdict,
                "EACCES is not proof of death: the socket must be left alone"
            ),
            // Root, or a filesystem that ignores the mode: the connect
            // succeeded, which `responds` must also report as live.
            None => assert!(verdict, "a connect that succeeded is a live app"),
            other => panic!("unexpected connect error {other:?}"),
        }
    }

    #[test]
    fn a_socket_whose_process_is_gone_is_stale() {
        // A pid that cannot exist: the kernel's maximum is far below
        // this, so `/proc/<pid>` is certainly absent.
        const DEAD: u32 = u32::MAX - 7;
        assert!(!pid_alive(DEAD));
        let me = rustix::process::getpid().as_raw_nonzero().get() as u32;
        assert!(pid_alive(me), "this process is running");

        let dir = std::env::temp_dir().join(format!("nitro-stale-{me}-{:?}", {
            std::thread::current().id()
        }));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A leftover of a dead app, and a real listener of a live one.
        let dead = dir.join(format!("ghost.{DEAD}.sock"));
        std::fs::write(&dead, b"").unwrap();
        let live = dir.join(format!("ghost.{me}.sock"));
        let _listener = UnixListener::bind(&live).unwrap();

        assert!(stale(&AppSocket {
            name: "ghost".to_owned(),
            pid: DEAD,
            path: dead.clone(),
        }));
        assert!(!stale(&AppSocket {
            name: "ghost".to_owned(),
            pid: me,
            path: live.clone(),
        }));

        // A file with a *live* pid that nothing listens on is stale too:
        // that is the pid-reuse case the `connect` exists to catch.
        let reused = dir.join(format!("other.{me}.sock"));
        std::fs::write(&reused, b"").unwrap();
        assert!(!responds(&reused));
        assert!(stale(&AppSocket {
            name: "other".to_owned(),
            pid: me,
            path: reused.clone(),
        }));

        // `list_apps` drops both and unlinks them.
        let apps = list_apps(&dir);
        assert_eq!(apps.len(), 1, "only the live app: {apps:?}");
        assert_eq!(apps[0].pid, me);
        assert!(!dead.exists(), "the ghost's socket is unlinked");
        assert!(!reused.exists(), "and so is the one nothing answers on");
        assert!(live.exists(), "the live one is left alone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binding_sweeps_the_dead_siblings_of_the_same_name() {
        const DEAD: u32 = u32::MAX - 9;
        let me = rustix::process::getpid().as_raw_nonzero().get() as u32;
        let dir = std::env::temp_dir().join(format!("nitro-sweep-{me}-{:?}", {
            std::thread::current().id()
        }));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ghost = dir.join(format!("bar.{DEAD}.sock"));
        std::fs::write(&ghost, b"").unwrap();
        // Another app's ghost, and a file that is not a socket name at
        // all: neither is ours to remove.
        let other = dir.join(format!("calc.{DEAD}.sock"));
        std::fs::write(&other, b"").unwrap();
        let notes = dir.join("notes.txt");
        std::fs::write(&notes, b"").unwrap();

        let socket = Socket::bind_at(&dir.join(format!("bar.{me}.sock"))).unwrap();
        assert!(!ghost.exists(), "the dead bar's socket is swept on bind");
        assert!(other.exists(), "another app's socket is not ours to sweep");
        assert!(notes.exists());
        assert!(socket.path().exists());

        drop(socket);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn socket_names_round_trip() {
        assert_eq!(
            parse_socket_name("hello-dialog.1234.sock"),
            Some(("hello-dialog".to_owned(), 1234))
        );
        assert_eq!(parse_socket_name("nope.sock"), None);
        assert_eq!(parse_socket_name("hello.sock.1234"), None);
        assert_eq!(sanitize("my app/1"), "my_app_1");
        assert_eq!(sanitize(""), "app");
    }

    #[test]
    fn watch_covers_a_subtree_but_not_a_prefix_of_a_name() {
        assert!(watch_matches("*", "window/ok"));
        assert!(watch_matches("window", "window/ok"));
        assert!(watch_matches("window/row[0]", "window/row[0]/ok"));
        assert!(watch_matches("window/ok", "window/ok"));
        // `window/ok` must not match `window/okay`.
        assert!(!watch_matches("window/ok", "window/okay"));
        assert!(!watch_matches("window/ok", "window/cancel"));
    }

    #[test]
    fn the_socket_directory_follows_the_environment() {
        let over = Path::new("/tmp/nitro-apps-test");
        assert_eq!(
            resolve_dir(Some(over), Some(Path::new("/run/user/1")), 7),
            over
        );
        assert_eq!(
            resolve_dir(None, Some(Path::new("/run/user/1")), 7),
            PathBuf::from("/run/user/1/nitro/apps")
        );
        // A relative runtime dir is not usable, and falls back.
        assert_eq!(
            resolve_dir(None, Some(Path::new("relative")), 7),
            PathBuf::from("/tmp/nitro-7/apps")
        );
        assert_eq!(
            resolve_dir(None, None, 7),
            PathBuf::from("/tmp/nitro-7/apps")
        );
    }

    #[test]
    fn take_line_splits_and_keeps_the_rest() {
        let mut buf = b"first\nsecond".to_vec();
        assert_eq!(take_line(&mut buf).as_deref(), Some("first"));
        assert_eq!(take_line(&mut buf), None);
        assert_eq!(buf, b"second");
    }

    #[test]
    fn rest_after_keeps_spaces_in_the_value() {
        assert_eq!(
            rest_after(
                "set window/f text hello  world",
                &["set", "window/f", "text"]
            ),
            "hello  world"
        );
        assert_eq!(
            rest_after("do window/ok click", &["do", "window/ok", "click"]),
            ""
        );
        // The value may start with the property's own name — each word is
        // stripped once, not every occurrence.
        assert_eq!(
            rest_after("set w text texture", &["set", "w", "text"]),
            "texture"
        );
        assert_eq!(rest_after("set w text text", &["set", "w", "text"]), "text");
        // A value with a tab in it arrives escaped and comes back whole.
        assert_eq!(
            unescape(&rest_after(
                &format!("set w text {}", escape("a\tb")),
                &["set", "w", "text"]
            )),
            "a\tb"
        );
    }
}

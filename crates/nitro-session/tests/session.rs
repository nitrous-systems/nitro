//! The session driven end to end, against stub children.
//!
//! Everything here runs a **real** [`Session`] — real `fork`/`exec`, real
//! pidfds, the real `poll` loop, the real readiness probe over
//! `nitro-wire` — with `examples/stub_child.rs` standing in for the
//! compositor and the shell pieces. The one thing that is not real is
//! what the children *do*, and that is the point: a test that started
//! `nitro-server` would be measuring KMS and text, and would not be able
//! to ask for a crash.
//!
//! The stubs write a line per start and per SIGTERM into one file, so
//! restart counts, restart *delays* and teardown **order** are all read
//! off timestamps the children wrote themselves rather than inferred by
//! the test from a pid table it would have to race.
//!
//! # One environment note, and it is load-bearing
//!
//! A **signal mask is inherited across `fork` and `exec`**, so in a
//! sandbox that blocks `SIGTERM` — some CI containers do, and
//! `deploy/size.sh` already carries a comment about the same trap — every
//! stub child is born unable to receive the signal the session sends it.
//! Teardown still completes there, through the `SIGKILL` at the deadline,
//! but the `term` marks never appear.
//!
//! [`sigterm_deliverable`] detects that case the only honest way: by
//! actually sending a stub a `SIGTERM` and seeing whether it says so. The
//! order assertions are then made on whichever evidence exists — the
//! children's own `term` lines when signals work, and the session's
//! `stopping` log order is not a substitute, so those tests report
//! themselves as skipped rather than passing vacuously. Everything else
//! in this file runs either way.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nitro_session::backoff::Backoff;
use nitro_session::pieces::{Piece, Role};
use nitro_session::{Config, Outcome, Session};

/// The stub binary cargo built for this test.
///
/// `CARGO_BIN_EXE_*` exists for `[[bin]]` targets only, and the stub is
/// deliberately an example so that `cargo build --release --bins` — what
/// `just deploy` runs — never puts a test helper next to the shipped
/// binaries. So it is found the way the session finds its own pieces:
/// relative to the running executable. The test harness lives in
/// `target/<profile>/deps/`, and examples in `target/<profile>/examples/`.
fn stub() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let profile = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test>");
    let stub = profile.join("examples").join("stub_child");
    assert!(
        stub.exists(),
        "{} is missing; run the tests with `cargo test`, which builds the examples",
        stub.display()
    );
    stub
}

/// A private directory per test: the sockets and the mark files.
struct Env {
    dir: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nitro-session-it-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A bin dir with the stub under each piece's name, so the
        // session's own `resolve` (sibling before `$PATH`) is what finds
        // them — the production path, exercised by every test here.
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in [
            "nitro-server",
            "nitro-wallpaper",
            "nitro-bar",
            "nitro-launcher",
        ] {
            let link = bin.join(name);
            std::os::unix::fs::symlink(stub(), &link).unwrap();
        }
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn marks(&self, name: &str) -> Vec<(String, u64, u128)> {
        let text = std::fs::read_to_string(self.path(name)).unwrap_or_default();
        text.lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                Some((
                    f.next()?.to_owned(),
                    f.next()?.parse().ok()?,
                    f.next()?.parse().ok()?,
                ))
            })
            .collect()
    }

    /// The session's configuration, with stub arguments per piece.
    fn config(&self, args: Vec<(String, Vec<String>)>) -> Config {
        let mut c = Config::new(
            self.path("wire.sock"),
            self.path("shell.sock"),
            self.path("session.sock"),
        );
        c.bin_dir = Some(self.dir.join("bin"));
        c.args = args;
        c.ready_timeout = Duration::from_secs(10);
        c.handle_signals = false;
        // Short enough that a test can watch two restarts, long enough
        // that the doubling is visible: 100 ms → 200 ms → 400 ms, capped.
        c.backoff = Backoff::new(Duration::from_millis(100), Duration::from_millis(400));
        c
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The arguments that make a stub act as the server: bind both sockets,
/// and mark.
fn server_args(env: &Env) -> Vec<String> {
    vec![
        "--mark".to_owned(),
        env.path("nitro-server.marks").display().to_string(),
        "--serve".to_owned(),
        format!(
            "{},{}",
            env.path("wire.sock").display(),
            env.path("shell.sock").display()
        ),
    ]
}

fn shell_args(env: &Env, name: &str, extra: &[&str]) -> Vec<String> {
    let mut v = vec![
        "--mark".to_owned(),
        env.path(&format!("{name}.marks")).display().to_string(),
    ];
    v.extend(extra.iter().map(|s| (*s).to_owned()));
    v
}

/// Whether `SIGTERM` can actually be delivered to a child here.
///
/// Not "is SIGTERM blocked for *us*" — the mask a child is born with is
/// the one that matters, and the only way to know it is to try. One stub
/// is started, signalled, and asked whether it noticed.
fn sigterm_deliverable() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("nitro-session-sigprobe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mark = dir.join("m");
        let mut child = std::process::Command::new(stub())
            .args(["--mark", mark.to_str().unwrap()])
            .spawn()
            .expect("the stub starts");
        // Wait for it to have installed its handler: the `start` mark is
        // written before `register`, so give it a moment either way.
        std::thread::sleep(Duration::from_millis(300));
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_term = false;
        while Instant::now() < deadline {
            if std::fs::read_to_string(&mark)
                .unwrap_or_default()
                .contains("term")
            {
                saw_term = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        if !saw_term {
            eprintln!(
                "note: SIGTERM is not deliverable to children in this environment \
                 (an inherited signal mask); the teardown-order assertions are skipped"
            );
        }
        saw_term
    })
}

/// Step the session's loop until `done`, or panic after `timeout`.
///
/// Bounded steps, because most of the conditions here — a child having
/// written its mark file, a reply having arrived — are things the
/// session's own descriptors never become readable for. An unbounded
/// `poll_once` would park the test in the kernel forever waiting for a
/// wakeup nothing is going to send.
fn pump(session: &mut Session, timeout: Duration, mut done: impl FnMut(&Session) -> bool) {
    let deadline = Instant::now() + timeout;
    while !done(session) {
        assert!(
            Instant::now() < deadline,
            "the condition never became true within {timeout:?}"
        );
        session.poll_once_for(None, Some(Duration::from_millis(10)));
    }
}

/// Whether a pid still exists, via signal 0.
///
/// A pid can in principle be reused, but not inside a test's lifetime,
/// and these processes were ours so nothing else reaped them.
fn pid_alive(pid: u32) -> bool {
    rustix::process::test_kill_process(
        rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap(),
    )
    .is_ok()
}

fn connect(path: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return s,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "cannot connect to the session: {e}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Send one request and read the whole reply, pumping the session's loop
/// so it can answer — the test is single-threaded on purpose, so the
/// order of events is the test's rather than the scheduler's.
fn request(session: &mut Session, line: &str) -> String {
    let mut sock = connect(session.socket_path());
    // The session has to accept before anything is written, which it
    // does on its next pass.
    pump(session, Duration::from_secs(2), |_| true);
    sock.write_all(format!("{line}\n").as_bytes()).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();

    let mut got = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        session.poll_once_for(None, Some(Duration::from_millis(10)));
        let mut buf = [0u8; 1024];
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => {}
        }
        if !got.is_empty()
            && (got.ends_with("\n\n") || got.lines().count() == 1 && got.ends_with('\n'))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no reply to {line:?}: got {got:?}"
        );
    }
    got
}

/// The happy path, and what it proves: the session waits for the server
/// to *answer* before starting anything else.
///
/// The stub server binds and handshakes; the three shell pieces do not
/// serve anything. If the session started them before the server was
/// ready, the mark timestamps would not be ordered — and on a slower box
/// they would interleave, which is exactly the bug this ordering exists
/// to prevent.
#[test]
fn the_server_comes_up_first_and_then_the_shell_in_order() {
    let env = Env::new("startup");
    let config = env.config(vec![
        ("nitro-server".to_owned(), server_args(&env)),
        (
            "nitro-wallpaper".to_owned(),
            shell_args(&env, "nitro-wallpaper", &[]),
        ),
        ("nitro-bar".to_owned(), shell_args(&env, "nitro-bar", &[])),
        (
            "nitro-launcher".to_owned(),
            shell_args(&env, "nitro-launcher", &[]),
        ),
    ]);
    let mut session = Session::start(config).expect("the session starts");

    // Wait for each piece to have *written its mark*, not merely to have
    // been forked: `status` reports a pid the instant `spawn` returns,
    // and the child's first line of code has not run yet. Asserting on
    // the marks while racing the children is how this test would pass on
    // a fast machine and fail on the box.
    pump(&mut session, Duration::from_secs(10), |_| {
        [
            "nitro-server",
            "nitro-wallpaper",
            "nitro-bar",
            "nitro-launcher",
        ]
        .iter()
        .all(|n| !env.marks(&format!("{n}.marks")).is_empty())
    });

    // Every piece is running…
    let status = session.status();
    assert_eq!(
        status.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        vec![
            "nitro-server",
            "nitro-wallpaper",
            "nitro-bar",
            "nitro-launcher"
        ],
        "status reports the pieces in start order"
    );

    // …and the server started before any of them. The stub writes its
    // `start` line as its first statement, so these timestamps are the
    // real start order.
    let server_start = env.marks("nitro-server.marks")[0].2;
    for name in ["nitro-wallpaper", "nitro-bar", "nitro-launcher"] {
        let m = env.marks(&format!("{name}.marks"));
        assert_eq!(m.len(), 1, "{name} started exactly once");
        assert!(
            m[0].2 >= server_start,
            "{name} started at {} before the server at {server_start}",
            m[0].2
        );
    }

    session.teardown();
}

/// A shell piece that crashes is restarted, and the second crash costs
/// longer than the first.
///
/// The delay is read from the child's own start timestamps, because the
/// claim is about wall-clock time between restarts and nothing else can
/// witness it.
#[test]
fn a_crashed_shell_piece_is_restarted_with_a_growing_delay() {
    let env = Env::new("restart");
    let mut config = env.config(vec![
        ("nitro-server".to_owned(), server_args(&env)),
        (
            // Dies 50 ms after each start, with a non-zero code: the
            // crash loop the backoff is for.
            "nitro-bar".to_owned(),
            shell_args(
                &env,
                "nitro-bar",
                &["--exit-after", "50", "--exit-code", "3"],
            ),
        ),
    ]);
    config.pieces = vec![
        Piece {
            program: "nitro-server",
            role: Role::Server,
        },
        Piece {
            program: "nitro-bar",
            role: Role::Shell,
        },
    ];
    let mut session = Session::start(config).expect("the session starts");

    pump(&mut session, Duration::from_secs(20), |_| {
        env.marks("nitro-bar.marks").len() >= 3
    });
    let starts: Vec<u128> = env
        .marks("nitro-bar.marks")
        .iter()
        .filter(|(w, _, _)| w == "start")
        .map(|(_, _, t)| *t)
        .collect();
    assert!(starts.len() >= 3, "the bar was restarted twice: {starts:?}");

    // Each restart is at least its backoff delay after the previous
    // *exit*, which is 50 ms after the previous start. 100 ms then
    // 200 ms, with the stub's own lifetime added.
    let first_gap = starts[1] - starts[0];
    let second_gap = starts[2] - starts[1];
    assert!(
        first_gap >= 140,
        "the first restart waited ~150 ms (50 run + 100 backoff), got {first_gap} ms"
    );
    assert!(
        second_gap >= first_gap + 50,
        "the second restart waited longer than the first: {first_gap} then {second_gap} ms"
    );
    // Each restart really is a new process, not the same one re-marked.
    let pids: Vec<u64> = env
        .marks("nitro-bar.marks")
        .iter()
        .filter(|(w, _, _)| w == "start")
        .map(|(_, p, _)| *p)
        .collect();
    assert_eq!(
        pids.iter().collect::<std::collections::HashSet<_>>().len(),
        pids.len(),
        "every restart is a distinct process: {pids:?}"
    );

    session.teardown();
}

/// Teardown stops the pieces in **reverse** start order, and the
/// evidence is each child's own `term` line.
///
/// This is the test the ordering exists for: a session that `SIGTERM`ed
/// the server first would leave three clients talking to a socket that
/// had gone, and the journal would fill with connection errors during
/// what is supposed to be a clean stop.
#[test]
fn teardown_stops_the_pieces_in_reverse_order() {
    let env = Env::new("teardown");
    let config = env.config(vec![
        ("nitro-server".to_owned(), server_args(&env)),
        (
            "nitro-wallpaper".to_owned(),
            shell_args(&env, "nitro-wallpaper", &[]),
        ),
        ("nitro-bar".to_owned(), shell_args(&env, "nitro-bar", &[])),
        (
            "nitro-launcher".to_owned(),
            shell_args(&env, "nitro-launcher", &[]),
        ),
    ]);
    let mut session = Session::start(config).expect("the session starts");
    pump(&mut session, Duration::from_secs(10), |s| {
        s.status().iter().all(|(_, pid)| pid.is_some())
    });

    session.teardown();

    // Everything is gone either way; that part is checked below and is
    // what `SIGKILL` guarantees. The *order* can only be read off the
    // children's own marks, which need a deliverable SIGTERM.
    assert!(
        session.status().iter().all(|(_, pid)| pid.is_none()),
        "no piece survives teardown"
    );
    if !sigterm_deliverable() {
        return;
    }

    let term_at = |name: &str| -> u128 {
        let m = env.marks(&format!("{name}.marks"));
        m.iter()
            .find(|(w, _, _)| w == "term")
            .unwrap_or_else(|| panic!("{name} was never asked to stop: {m:?}"))
            .2
    };
    let launcher = term_at("nitro-launcher");
    let bar = term_at("nitro-bar");
    let wallpaper = term_at("nitro-wallpaper");
    let server = term_at("nitro-server");
    assert!(
        launcher <= bar && bar <= wallpaper && wallpaper <= server,
        "reverse order: launcher {launcher}, bar {bar}, wallpaper {wallpaper}, server {server}"
    );
}

/// A piece that ignores `SIGTERM` is killed, and the whole teardown is
/// still bounded — which is what keeps the session inside systemd's
/// `TimeoutStopSec=5`.
#[test]
fn a_piece_that_ignores_sigterm_is_killed_within_the_deadline() {
    let env = Env::new("stubborn");
    let mut config = env.config(vec![
        ("nitro-server".to_owned(), server_args(&env)),
        (
            "nitro-bar".to_owned(),
            shell_args(&env, "nitro-bar", &["--ignore-term"]),
        ),
    ]);
    config.pieces = vec![
        Piece {
            program: "nitro-server",
            role: Role::Server,
        },
        Piece {
            program: "nitro-bar",
            role: Role::Shell,
        },
    ];
    // A shorter deadline than the shipped 3 s, so the test asserts the
    // *shape* (wait, then kill, bounded) without spending three seconds
    // on it. The production value is what `TEARDOWN_TIMEOUT` says and is
    // checked against systemd's `TimeoutStopSec` in the unit, not here.
    config.teardown_timeout = Duration::from_millis(600);
    let mut session = Session::start(config).expect("the session starts");
    pump(&mut session, Duration::from_secs(10), |s| {
        s.status().iter().all(|(_, pid)| pid.is_some())
    });
    let bar_pid = session.status()[1].1.expect("the bar is running");

    let start = Instant::now();
    session.teardown();
    let took = start.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "teardown is bounded by its deadline, took {took:?}"
    );
    assert!(
        took >= Duration::from_millis(500),
        "…and it did wait for the piece before killing it: {took:?}"
    );
    assert!(
        session.status().iter().all(|(_, pid)| pid.is_none()),
        "the stubborn piece is gone"
    );
    assert!(
        !pid_alive(bar_pid),
        "pid {bar_pid} is still alive after teardown"
    );
}

/// The server exiting ends the session, with the server's exit code —
/// and the shell is torn down rather than restarted into a desktop with
/// no compositor.
#[test]
fn the_server_exiting_ends_the_session_with_its_code() {
    let env = Env::new("server-exit");
    let mut config = env.config(vec![
        ("nitro-server".to_owned(), {
            let mut a = server_args(&env);
            a.extend(["--exit-after".to_owned(), "300".to_owned()]);
            a.extend(["--exit-code".to_owned(), "9".to_owned()]);
            a
        }),
        ("nitro-bar".to_owned(), shell_args(&env, "nitro-bar", &[])),
    ]);
    config.pieces = vec![
        Piece {
            program: "nitro-server",
            role: Role::Server,
        },
        Piece {
            program: "nitro-bar",
            role: Role::Shell,
        },
    ];
    let mut session = Session::start(config).expect("the session starts");
    let outcome = session.run(None);

    assert_eq!(
        outcome,
        Outcome::ServerExited(nitro_session::child::Exit::Code(9))
    );
    assert_eq!(outcome.code(), 9, "the server's code is the session's");
    // The bar was stopped, not restarted.
    let bar = env.marks("nitro-bar.marks");
    assert_eq!(
        bar.iter().filter(|(w, _, _)| w == "start").count(),
        1,
        "the bar was not restarted on the way out: {bar:?}"
    );
    let bar_pid = u32::try_from(bar[0].1).unwrap();
    assert!(!pid_alive(bar_pid), "the bar was stopped: {bar:?}");
    if sigterm_deliverable() {
        assert!(
            bar.iter().any(|(w, _, _)| w == "term"),
            "the bar was asked to stop: {bar:?}"
        );
    }
}

/// The session socket: `status` lists what is running, `lock` is honestly
/// refused, an unknown verb is an error, and `logout` ends the session
/// *after* answering.
#[test]
fn the_session_socket_answers_status_lock_and_logout() {
    let env = Env::new("socket");
    let mut config = env.config(vec![
        ("nitro-server".to_owned(), server_args(&env)),
        ("nitro-bar".to_owned(), shell_args(&env, "nitro-bar", &[])),
    ]);
    config.pieces = vec![
        Piece {
            program: "nitro-server",
            role: Role::Server,
        },
        Piece {
            program: "nitro-bar",
            role: Role::Shell,
        },
    ];
    let mut session = Session::start(config).expect("the session starts");
    pump(&mut session, Duration::from_secs(10), |s| {
        s.status().iter().all(|(_, pid)| pid.is_some())
    });

    // `status`: a header, a line per piece with a real pid, a blank line.
    let status = request(&mut session, "status");
    let mut lines = status.lines();
    assert_eq!(lines.next(), Some("ok"));
    let server_line = lines.next().expect("a server line");
    assert!(server_line.starts_with("nitro-server "), "{server_line}");
    let pid: u32 = server_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(Some(pid), session.status()[0].1);
    assert!(lines.next().unwrap().starts_with("nitro-bar "));

    // `lock` is M4, and says so rather than pretending.
    let lock = request(&mut session, "lock");
    assert!(lock.starts_with("err "), "{lock}");
    assert!(lock.contains("not implemented"), "{lock}");
    assert_eq!(lock.lines().count(), 1, "one line: {lock:?}");

    // An unknown verb is refused, and the connection is closed after the
    // error — the same rule the wire protocol uses.
    let bad = request(&mut session, "halt");
    assert!(bad.starts_with("err "), "{bad}");
    assert!(bad.contains("halt"), "{bad}");

    // `poweroff now` must not power anything off: an argument makes it a
    // command this protocol does not have.
    let args = request(&mut session, "poweroff now");
    assert!(args.starts_with("err "), "{args}");
    assert!(args.contains("no arguments"), "{args}");

    // `logout` is answered, and *then* the session stops.
    let bye = request(&mut session, "logout");
    assert_eq!(bye, "ok\n");
    let outcome = session.run(None);
    assert_eq!(outcome, Outcome::Stopped);
    assert_eq!(outcome.code(), 0);
    let bar = env.marks("nitro-bar.marks");
    assert!(
        !pid_alive(u32::try_from(bar[0].1).unwrap()),
        "logout tore the session down: {bar:?}"
    );
    assert!(
        !env.path("session.sock").exists(),
        "the socket file is removed on the way out"
    );
}

/// A server that never becomes ready is a start failure, not a session
/// that carries on — and nothing is left running.
#[test]
fn a_server_that_never_answers_fails_the_start_and_leaves_nothing_behind() {
    let env = Env::new("never-ready");
    let mut config = env.config(vec![(
        // No `--serve`: the process runs happily and binds nothing.
        "nitro-server".to_owned(),
        vec![
            "--mark".to_owned(),
            env.path("nitro-server.marks").display().to_string(),
        ],
    )]);
    config.pieces = vec![Piece {
        program: "nitro-server",
        role: Role::Server,
    }];
    config.ready_timeout = Duration::from_millis(400);

    let e = Session::start(config).expect_err("the server never came up");
    assert!(e.contains("not ready"), "{e}");

    let marks = env.marks("nitro-server.marks");
    assert_eq!(marks.iter().filter(|(w, _, _)| w == "start").count(), 1);
    let pid = u32::try_from(marks[0].1).unwrap();
    assert!(
        !pid_alive(pid),
        "the half-started server (pid {pid}) is still running"
    );
    if sigterm_deliverable() {
        assert!(
            marks.iter().any(|(w, _, _)| w == "term"),
            "and it was asked politely first: {marks:?}"
        );
    }
    assert!(
        !env.path("session.sock").exists(),
        "and the socket was cleaned up"
    );
}

/// A server that *dies* while we wait for it ends the wait at once,
/// rather than sitting out the whole readiness timeout.
#[test]
fn a_server_that_dies_during_startup_is_noticed_at_once() {
    let env = Env::new("dies-early");
    let mut config = env.config(vec![(
        "nitro-server".to_owned(),
        vec![
            "--mark".to_owned(),
            env.path("nitro-server.marks").display().to_string(),
            "--exit-after".to_owned(),
            "100".to_owned(),
            "--exit-code".to_owned(),
            "1".to_owned(),
        ],
    )]);
    config.pieces = vec![Piece {
        program: "nitro-server",
        role: Role::Server,
    }];
    config.ready_timeout = Duration::from_secs(30);

    let start = Instant::now();
    let e = Session::start(config).expect_err("the server died");
    let took = start.elapsed();
    assert!(e.contains("exited before it was ready"), "{e}");
    assert!(
        took < Duration::from_secs(5),
        "the wait ended when the server died, not at the timeout: {took:?}"
    );
}

/// A client that half-closes after its request still gets the reply.
///
/// Found on the box: `printf 'status\n' | nc -U …/session.sock` printed
/// nothing, while a client that kept its socket open was answered fine.
/// `nc` shuts its write end down the instant stdin ends, so the request
/// line and the EOF arrive in the same wakeup — and the first version of
/// `service_client` returned on the hangup before parsing the bytes that
/// had already arrived.
///
/// It matters beyond `nc`: "send a request, half-close, read the reply"
/// is the most natural way to write a one-shot client, and `just
/// box-session status` is exactly that. The session must answer what it
/// received before it honours a hangup, which is the rule `nitro-wire`'s
/// own reader already follows.
#[test]
fn a_client_that_half_closes_after_its_request_still_gets_the_reply() {
    let env = Env::new("half-close");
    let mut config = env.config(vec![("nitro-server".to_owned(), server_args(&env))]);
    config.pieces = vec![Piece {
        program: "nitro-server",
        role: Role::Server,
    }];
    let mut session = Session::start(config).expect("the session starts");

    let mut sock = connect(session.socket_path());
    pump(&mut session, Duration::from_secs(2), |_| true);
    sock.write_all(b"status\n").unwrap();
    // Exactly what `nc` does at stdin EOF.
    sock.shutdown(std::net::Shutdown::Write).unwrap();

    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut got = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !got.ends_with("\n\n") {
        session.poll_once_for(None, Some(Duration::from_millis(10)));
        let mut buf = [0u8; 1024];
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "a half-closed client was never answered: got {got:?}"
        );
    }
    assert!(got.starts_with("ok\n"), "{got:?}");
    assert!(got.contains("nitro-server "), "{got:?}");

    session.teardown();
}

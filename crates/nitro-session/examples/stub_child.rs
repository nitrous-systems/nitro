//! A stand-in for a real piece, for `tests/session.rs`.
//!
//! The integration tests need a "server" that binds the two wire sockets
//! and a "shell piece" that can be told to crash — but a test that ran
//! the actual `nitro-server` would be measuring KMS, text and the scene
//! graph, not supervision. This is the smallest program that is
//! *supervisable*: it makes a mark when it starts, optionally binds the
//! sockets the session waits for, and exits when and how the test asks.
//!
//! ```text
//! stub_child --mark DIR/name [--serve wire.sock,shell.sock]
//!            [--exit-after MS] [--exit-code N] [--ignore-term]
//! ```
//!
//! Each start appends a line to `DIR/name` — `start <pid> <millis>` — and
//! a `SIGTERM` appends `term <pid> <millis>` before exiting 0. That file
//! is the test's whole observation surface: restart counts, restart
//! *delays* and teardown **order** are all read off the timestamps in it,
//! which is why the mark is written by the child rather than inferred by
//! the parent from a pid table it would have to race.
//!
//! `--serve` binds and *listens*, which is what the session's readiness
//! probe needs; the handshake it then speaks is the real `nitro-wire`
//! server side, because a probe that accepted anything would not prove
//! the session's check is the one a real shell piece performs.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

fn mark(path: &Path, what: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{what} {} {}", std::process::id(), now_millis());
        let _ = f.flush();
    }
}

struct Args {
    mark: Option<PathBuf>,
    serve: Vec<PathBuf>,
    exit_after: Option<Duration>,
    exit_code: i32,
    ignore_term: bool,
}

fn parse() -> Args {
    let mut args = Args {
        mark: None,
        serve: Vec::new(),
        exit_after: None,
        exit_code: 0,
        ignore_term: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mark" => args.mark = it.next().map(PathBuf::from),
            "--serve" => {
                args.serve = it
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect();
            }
            "--exit-after" => {
                args.exit_after = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .map(Duration::from_millis);
            }
            "--exit-code" => args.exit_code = it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            "--ignore-term" => args.ignore_term = true,
            other => {
                eprintln!("stub_child: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    args
}

fn main() {
    let args = parse();
    if let Some(m) = &args.mark {
        mark(m, "start");
    }

    // SIGTERM → a readable fd, the same self-pipe the session uses. A
    // stub that died on the default disposition could not write its
    // `term` line, and the teardown-order test is exactly that line.
    let (sig_read, sig_write) = std::os::unix::net::UnixDatagram::pair().expect("socketpair");
    sig_read.set_nonblocking(true).expect("nonblocking");
    if !args.ignore_term {
        signal_hook::low_level::pipe::register(signal_hook::consts::SIGTERM, sig_write)
            .expect("register");
    }
    // The registration probes the fd with an empty send; drop it.
    let mut buf = [0u8; 64];
    while sig_read.recv(&mut buf).is_ok() {}

    let listeners: Vec<nitro_wire::server::Listener> = args
        .serve
        .iter()
        .map(|p| nitro_wire::server::Listener::bind(p).expect("bind"))
        .collect();
    let mut clients: Vec<nitro_wire::server::ClientStream> = Vec::new();

    let deadline = args.exit_after.map(|d| Instant::now() + d);
    loop {
        if let Some(at) = deadline
            && Instant::now() >= at
        {
            std::process::exit(args.exit_code);
        }
        // Poll the signal pipe, the listeners and whatever connected.
        // The revents are read out into plain bools before anything is
        // mutated, so the descriptor table's borrows end here.
        let (sig_ready, listen_ready, client_ready) = {
            let sig_fd = {
                use std::os::fd::AsFd as _;
                sig_read.as_fd()
            };
            let listen_fds: Vec<_> = listeners
                .iter()
                .map(nitro_wire::server::Listener::as_fd)
                .collect();
            let client_fds: Vec<_> = clients
                .iter()
                .map(nitro_wire::server::ClientStream::as_fd)
                .collect();
            let mut fds = vec![rustix::event::PollFd::new(
                &sig_fd,
                rustix::event::PollFlags::IN,
            )];
            for fd in &listen_fds {
                fds.push(rustix::event::PollFd::new(fd, rustix::event::PollFlags::IN));
            }
            for fd in &client_fds {
                fds.push(rustix::event::PollFd::new(fd, rustix::event::PollFlags::IN));
            }
            let ts = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 20_000_000,
            };
            let _ = rustix::event::poll(&mut fds, Some(&ts));
            let sig = fds[0].revents().contains(rustix::event::PollFlags::IN);
            let listen: Vec<bool> = (0..listen_fds.len())
                .map(|i| fds[1 + i].revents().contains(rustix::event::PollFlags::IN))
                .collect();
            let client: Vec<bool> = (0..client_fds.len())
                .map(|i| !fds[1 + listen_fds.len() + i].revents().is_empty())
                .collect();
            (sig, listen, client)
        };
        let _ = &client_ready;

        if sig_ready {
            let mut got = false;
            while sig_read.recv(&mut buf).is_ok() {
                got = true;
            }
            if got {
                if let Some(m) = &args.mark {
                    mark(m, "term");
                }
                std::process::exit(0);
            }
        }
        for (i, l) in listeners.iter().enumerate() {
            if listen_ready[i] {
                while let Ok(Some(c)) = l.accept() {
                    clients.push(c);
                }
            }
        }
        // Speak the real handshake to whoever connected: read until a
        // `Hello`, answer `Welcome`, and drop clients that go away.
        clients.retain_mut(|c| {
            if c.read().is_err() {
                return false;
            }
            loop {
                match c.next_msg() {
                    Ok(Some(nitro_wire::msg::ClientMsg::Hello(_))) => {
                        let _ = c.welcome("stub_child", nitro_wire::types::caps::SHELL);
                        let _ = c.flush();
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => return true,
                    Err(_) => return false,
                }
            }
        });
    }
}

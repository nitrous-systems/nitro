//! The supervisor: one `poll(2)` loop over the pieces, the session
//! socket and the signal pipe.
//!
//! # The shape
//!
//! ```text
//! start server ──▶ wait for wire.sock + shell.sock ──▶ start the shell
//!                                                        │
//!    ┌───────────────────────────────────────────────────┘
//!    ▼
//!  poll( pidfd per child, session socket, connected clients, signal pipe )
//!    │
//!    ├─ a shell piece exited  ──▶ schedule a restart (backoff)
//!    ├─ the server exited     ──▶ tear down, exit with its code
//!    ├─ a command arrived     ──▶ lock/suspend/poweroff/reboot/logout/status
//!    ├─ a restart is due      ──▶ start the piece again
//!    └─ SIGTERM / SIGINT      ──▶ tear down in reverse order, exit 0
//! ```
//!
//! One thread, no timers except the restart deadline, and no wakeups at
//! all in the steady state: every edge above is a descriptor becoming
//! readable. That is the same claim the server makes, and it has to hold
//! here too — the session is the longest-lived process in the desktop.
//!
//! # Why the server is not restarted
//!
//! A shell piece can die and be replaced because its state is *derived*:
//! the bar re-reads the window list, the wallpaper re-paints, the
//! launcher rebuilds its index. The server's state is the desktop —
//! every window of every application is a client connection to it. A
//! server that exited has taken all of them with it, and a "restarted"
//! desktop with an empty screen is not a recovery, it is a data loss
//! event dressed as one.
//!
//! So the server's exit ends the session, with the server's exit code, and
//! the decision of what to do next belongs to whatever started the
//! session — `systemd`, which has a `Restart=` line for exactly this
//! judgement, and which on the test box is deliberately set to `no`.
//!
//! # Teardown has one deadline, not one per piece
//!
//! [`Session::teardown`] sends `SIGTERM` to the pieces in reverse order,
//! then waits for **all** of them against a single
//! [`TEARDOWN_TIMEOUT`], and `SIGKILL`s whatever is left. Per-piece
//! timeouts would multiply: four pieces × 5 s is 20 s, which is longer
//! than the unit's own `TimeoutStopSec=5`, so systemd would `SIGKILL` the
//! session mid-teardown and the tty would be left in whatever state the
//! compositor happened to be in. One deadline, shorter than systemd's, is
//! what makes `systemctl stop nitro-dev` give tty1 back.
//!
//! The signalling is still strictly ordered, and the waiting is not: what
//! matters is that the launcher is *asked* to stop before the server is,
//! not that it has finished before the bar is asked.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};

use crate::backoff::Backoff;
use crate::child::{Child, Exit, SpawnError};
use crate::pieces::{Piece, Role};
use crate::power::{self, Command, ParseError};
use crate::socket::{self, Client, ReadOutcome};
use crate::{debug, error, info, warn};

/// How long every piece together gets to answer `SIGTERM` before the
/// session resorts to `SIGKILL`.
///
/// Three seconds, against the unit's `TimeoutStopSec=5`: the session must
/// finish its own teardown *inside* systemd's patience, or systemd kills
/// the session and the pieces are orphaned onto init with the VT still
/// theirs.
pub const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a `logout`/error reply may block the loop on its way out.
const REPLY_TIMEOUT: Duration = Duration::from_millis(200);

/// Everything the session needs to run. Built from the environment by
/// `main.rs`, or by hand in a test.
#[derive(Debug, Clone)]
pub struct Config {
    /// What to run, in start order. Normally [`crate::pieces::PIECES`].
    pub pieces: Vec<Piece>,
    /// Directory searched for the binaries before `$PATH`.
    pub bin_dir: Option<PathBuf>,
    /// Extra arguments per program name, for a test's stub children.
    pub args: Vec<(String, Vec<String>)>,
    /// Wire socket to wait for.
    pub wire_path: PathBuf,
    /// Shell socket to wait for.
    pub shell_path: PathBuf,
    /// Session socket to bind.
    pub session_path: PathBuf,
    /// How long the server has to come up.
    pub ready_timeout: Duration,
    /// Restart policy for the shell pieces.
    pub backoff: Backoff,
    /// How long every piece together gets to answer `SIGTERM` before the
    /// session resorts to `SIGKILL`. Default [`TEARDOWN_TIMEOUT`].
    pub teardown_timeout: Duration,
}

impl Config {
    /// The shipped configuration: the four pieces, binaries next to us,
    /// the runtime dir's sockets.
    #[must_use]
    pub fn new(wire_path: PathBuf, shell_path: PathBuf, session_path: PathBuf) -> Self {
        Self {
            pieces: crate::pieces::PIECES.to_vec(),
            bin_dir: crate::pieces::exe_dir(),
            args: Vec::new(),
            wire_path,
            shell_path,
            session_path,
            ready_timeout: crate::wait::DEFAULT_TIMEOUT,
            backoff: Backoff::default(),
            teardown_timeout: TEARDOWN_TIMEOUT,
        }
    }

    fn args_for(&self, program: &str) -> Vec<String> {
        self.args
            .iter()
            .find(|(p, _)| p == program)
            .map(|(_, a)| a.clone())
            .unwrap_or_default()
    }
}

/// A supervised piece: what it is, whether it is running, and when it may
/// be started again.
#[derive(Debug)]
struct Slot {
    piece: Piece,
    child: Option<Child>,
    backoff: Backoff,
    /// When a crashed piece may be restarted.
    restart_at: Option<Instant>,
}

/// How a *running* session ended.
///
/// There is deliberately no `StartFailed`: a session that never started
/// has no outcome to report, and [`Session::start`] returns `Err` for
/// that case. A variant nothing constructs is a state a reader has to
/// wonder about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A signal, or a `logout` command: exit 0.
    Stopped,
    /// The server exited; its status is ours.
    ServerExited(Exit),
}

impl Outcome {
    /// The process exit code for this outcome.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::ServerExited(e) => e.code(),
        }
    }
}

/// What one `poll` pass found, as indices and flags that borrow nothing.
///
/// The type exists for the borrow checker and is honest about it: the
/// descriptor table borrows the slots and the clients, and handling an
/// event mutates them.
#[derive(Debug, Default)]
struct Ready {
    /// Slot indices whose child's pidfd became readable.
    exited: Vec<usize>,
    /// `(client index, revents)` for every client with something.
    clients: Vec<(usize, PollFlags)>,
    /// The listener has a connection pending.
    accept: bool,
    /// The signal pipe is readable.
    signal: bool,
    /// `poll` itself failed; the session gives up.
    failed: bool,
}

/// The running session.
#[derive(Debug)]
pub struct Session {
    config: Config,
    slots: Vec<Slot>,
    listener: socket::Listener,
    clients: Vec<Client>,
    /// Set by a command or a signal; the loop ends at the next pass.
    stopping: Option<Outcome>,
}

impl Session {
    /// Bind the session socket, start the server, wait for it, start the
    /// shell.
    ///
    /// The socket is bound **first**, and the reason is that binding is
    /// the one step that can fail for a reason nothing else can fix: a
    /// second session already running, or a runtime directory that is
    /// not writable. Discovering that *after* starting a compositor
    /// would mean taking the VT, painting a desktop, and then tearing it
    /// all down again.
    ///
    /// Binding early is safe because binding is not *answering*.
    /// Nothing is accepted until [`Session::run`] polls the listener, so
    /// there is no window in which a half-started session could be told
    /// to `poweroff`: a client that connects during start-up simply
    /// waits, and if start-up fails its connection dies with the
    /// listener. Every failure path here unlinks the socket file — the
    /// `Err` returns drop the local `Listener` (whose `Drop` removes
    /// it), and the not-ready path hands it to a `Session` that tears
    /// down and unlinks. `a_server_that_never_answers_fails_the_start_\
    /// and_leaves_nothing_behind` asserts exactly that.
    ///
    /// # Errors
    /// The session socket failing to bind, or the server failing to
    /// start or to become ready. Anything already started is torn down
    /// first, so a failed `start` leaves no processes behind.
    pub fn start(config: Config) -> Result<Self, String> {
        let listener = socket::Listener::bind(&config.session_path)
            .map_err(|e| format!("session socket {}: {e}", config.session_path.display()))?;
        let mut slots: Vec<Slot> = config
            .pieces
            .iter()
            .map(|p| Slot {
                piece: p.clone(),
                child: None,
                backoff: config.backoff,
                restart_at: None,
            })
            .collect();

        // The server first, on its own, because everything after it
        // depends on it answering. Nothing has been started yet, so a
        // failure here needs no teardown — only the listener, which the
        // `?` drops and whose `Drop` unlinks the socket.
        for slot in &mut slots {
            if slot.piece.role != Role::Server {
                continue;
            }
            spawn_slot(slot, &config).map_err(|e| format!("{}: {e}", slot.piece.program))?;
        }

        // Then the health check, which is a real client handshake.
        let server_alive = |slots: &mut Vec<Slot>| -> bool {
            slots
                .iter_mut()
                .filter(|s| s.piece.role == Role::Server)
                .all(|s| s.child.as_mut().is_some_and(|c| c.reap().is_none()))
        };
        let ready = {
            let mut probe_slots = std::mem::take(&mut slots);
            let r = crate::wait::wait_for_sockets(
                &config.wire_path,
                &config.shell_path,
                config.ready_timeout,
                || server_alive(&mut probe_slots),
            );
            slots = probe_slots;
            r
        };
        match ready {
            Ok(took) => info!("server ready in {:.0} ms", took.as_secs_f32() * 1000.0),
            Err(e) => {
                let mut half = Self {
                    config,
                    slots,
                    listener,
                    clients: Vec::new(),
                    stopping: None,
                };
                half.teardown();
                return Err(e.to_string());
            }
        }

        // Then the shell, in order.
        for slot in &mut slots {
            if slot.piece.role == Role::Server {
                continue;
            }
            if let Err(e) = spawn_slot(slot, &config) {
                // A shell piece that cannot start is a warning, not a
                // failure: a desktop with no bar is still a desktop, and
                // the backoff will keep trying. The failure this turns
                // into is `just deploy` forgetting a binary, which the
                // journal then says in one line.
                warn!("{}: {e}", slot.piece.program);
                slot.restart_at = Some(Instant::now() + slot.backoff.after_exit(Duration::ZERO));
            }
        }

        Ok(Self {
            config,
            slots,
            listener,
            clients: Vec::new(),
            stopping: None,
        })
    }

    /// Path of the bound session socket.
    #[must_use]
    pub fn socket_path(&self) -> &std::path::Path {
        self.listener.path()
    }

    /// Names and pids of the pieces, in start order.
    #[must_use]
    pub fn status(&self) -> Vec<(String, Option<u32>)> {
        self.slots
            .iter()
            .map(|s| (s.piece.program.to_owned(), s.child.as_ref().map(Child::pid)))
            .collect()
    }

    /// Run until a signal, a `logout`, or the server exiting.
    ///
    /// `signals` is the installed signal pipe, or `None` for a test that
    /// drives the session in-process.
    pub fn run(&mut self, signals: Option<&mut crate::signals::Signals>) -> Outcome {
        let mut signals = signals;
        loop {
            if let Some(outcome) = self.stopping {
                self.teardown();
                return outcome;
            }
            self.poll_once(signals.as_deref_mut());
        }
    }

    /// One pass of the loop: poll, then handle whatever woke us.
    ///
    /// Blocks until something happens (or a restart falls due), which is
    /// the whole idle-cost claim. A caller that needs to regain control
    /// on a schedule of its own wants [`Session::poll_once_for`].
    pub fn poll_once(&mut self, signals: Option<&mut crate::signals::Signals>) {
        self.poll_once_for(signals, None);
    }

    /// One pass of the loop, waking after at most `max_wait`.
    ///
    /// Public for the in-process tests, which step the session on their
    /// own thread and must not be parked in `poll` while they wait for a
    /// condition the session itself is not going to signal — a child
    /// writing its mark file, say. The shipped session always passes
    /// `None`: a supervisor that woke on a timer would spend a wakeup
    /// per interval for the whole login, which is exactly the cost this
    /// design refuses to pay.
    pub fn poll_once_for(
        &mut self,
        signals: Option<&mut crate::signals::Signals>,
        max_wait: Option<Duration>,
    ) {
        // The poll's borrows — of the slots, of the clients, of the
        // signal pipe — all end inside this block. What comes out is a
        // plain `Ready`: indices and flags, owning nothing. That is what
        // lets the handling below call `&mut self` methods, which is
        // what handling a wakeup *is* (a client is dropped, a slot is
        // restarted). A loop that tried to handle events while still
        // holding the descriptor table would be fighting the borrow
        // checker over a real aliasing question, not a formality.
        let ready = self.poll_fds(signals.as_deref(), max_wait);

        // Signals first: a SIGTERM that arrives in the same wakeup as a
        // crash should not cost a restart on the way out.
        if let Some(sig) = signals
            && ready.signal
            && sig.drain()
        {
            info!("SIGTERM: shutting down");
            self.stopping = Some(Outcome::Stopped);
            return;
        }
        if ready.failed {
            self.stopping = Some(Outcome::Stopped);
            return;
        }

        for slot_idx in ready.exited {
            self.on_child_exit(slot_idx);
            if self.stopping.is_some() {
                return;
            }
        }

        if ready.accept {
            loop {
                match self.listener.accept() {
                    Ok(Some(c)) => self.clients.push(c),
                    Ok(None) => break,
                    Err(e) => {
                        warn!("session socket accept: {e}");
                        break;
                    }
                }
            }
        }

        // Client traffic, back to front so removal does not shift the
        // indices of the ones not yet visited. New clients accepted just
        // above are past `n_clients` and are simply serviced next pass.
        for (k, revents) in ready.clients.into_iter().rev() {
            if revents.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
                if self.service_client(k) {
                    self.clients.remove(k);
                    continue;
                }
                if self.stopping.is_some() {
                    return;
                }
            }
            if revents.contains(PollFlags::OUT) {
                // A client whose reply could not be written in one go:
                // push the rest. Every path that answers *and then* drops
                // a client flushes it blocking (`flush_blocking`) before
                // returning, so there is nothing to close here — this arm
                // exists for the ordinary client that stays connected.
                let _ = self.clients[k].flush();
            }
        }

        self.start_due_restarts();
    }

    /// Build the descriptor table, poll it, and report what happened as
    /// indices and flags that borrow nothing.
    fn poll_fds(
        &self,
        signals: Option<&crate::signals::Signals>,
        max_wait: Option<Duration>,
    ) -> Ready {
        use std::os::fd::AsFd;

        let child_fds: Vec<_> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.child.as_ref().map(|c| (i, c.as_fd())))
            .collect();
        let client_fds: Vec<_> = self.clients.iter().map(Client::as_fd).collect();
        let listen_fd = self.listener.as_fd();
        let signal_fd = signals.map(AsFd::as_fd);

        let mut fds: Vec<PollFd<'_>> = Vec::with_capacity(child_fds.len() + client_fds.len() + 2);
        for (_, fd) in &child_fds {
            fds.push(PollFd::new(fd, PollFlags::IN));
        }
        for (i, fd) in client_fds.iter().enumerate() {
            let want = if self.clients[i].has_pending_output() {
                PollFlags::IN | PollFlags::OUT
            } else {
                PollFlags::IN
            };
            fds.push(PollFd::new(fd, want));
        }
        fds.push(PollFd::new(&listen_fd, PollFlags::IN));
        if let Some(fd) = &signal_fd {
            fds.push(PollFd::new(fd, PollFlags::IN));
        }

        // The only timer: the next restart that is due. `None` means the
        // loop sleeps until something happens, which is the steady state
        // and the whole idle-cost claim.
        let wait = match (self.next_restart_delay(), max_wait) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let ts = wait.map(|d| Timespec {
            tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(d.subsec_nanos()),
        });
        let mut ready = Ready::default();
        match rustix::event::poll(&mut fds, ts.as_ref()) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                error!("poll: {e}");
                ready.failed = true;
                return ready;
            }
        }

        let n_children = child_fds.len();
        let n_clients = client_fds.len();
        let listen_idx = n_children + n_clients;
        ready.exited = (0..n_children)
            .filter(|&k| {
                fds[k]
                    .revents()
                    .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
            })
            .map(|k| child_fds[k].0)
            .collect();
        ready.clients = (0..n_clients)
            .map(|k| (k, fds[n_children + k].revents()))
            .filter(|(_, r)| !r.is_empty())
            .collect();
        ready.accept = fds[listen_idx].revents().contains(PollFlags::IN);
        ready.signal = signal_fd.is_some() && fds[listen_idx + 1].revents().contains(PollFlags::IN);
        ready
    }

    /// The shortest time until a restart is due, or `None` when nothing
    /// is pending.
    fn next_restart_delay(&self) -> Option<Duration> {
        let now = Instant::now();
        self.slots
            .iter()
            .filter_map(|s| s.restart_at)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    /// A supervised process exited: reap it, and decide what that means.
    fn on_child_exit(&mut self, idx: usize) {
        let Some(child) = self.slots[idx].child.as_mut() else {
            return;
        };
        let Some(exit) = child.reap() else {
            // Spurious wakeup on a pidfd for a process that has not
            // exited: nothing to do, and nothing to log.
            return;
        };
        let uptime = child.uptime();
        let pid = child.pid();
        let program = self.slots[idx].piece.program;
        self.slots[idx].child = None;

        if self.slots[idx].piece.role == Role::Server {
            info!(
                "{program} (pid {pid}) {exit} after {:.0}s — ending the session",
                uptime.as_secs_f32()
            );
            self.stopping = Some(Outcome::ServerExited(exit));
            return;
        }

        let delay = self.slots[idx].backoff.after_exit(uptime);
        warn!(
            "{program} (pid {pid}) {exit} after {:.1}s; restarting in {:.0}s",
            uptime.as_secs_f32(),
            delay.as_secs_f32()
        );
        self.slots[idx].restart_at = Some(Instant::now() + delay);
    }

    /// Start whatever the clock says is due.
    fn start_due_restarts(&mut self) {
        let now = Instant::now();
        for slot in &mut self.slots {
            let Some(at) = slot.restart_at else { continue };
            if at > now || slot.child.is_some() {
                continue;
            }
            slot.restart_at = None;
            if let Err(e) = spawn_slot(slot, &self.config) {
                warn!("{}: {e}", slot.piece.program);
                slot.restart_at = Some(Instant::now() + slot.backoff.after_exit(Duration::ZERO));
            }
        }
    }

    /// Read and answer one client's requests. Returns whether the client
    /// should be dropped.
    fn service_client(&mut self, k: usize) -> bool {
        // A hangup is remembered rather than acted on, because the bytes
        // that arrived *before* it are still requests. `nc -U sock`
        // half-closes its write end the moment stdin ends, so
        // `printf 'status\n' | nc -U session.sock` delivers the line and
        // the EOF in the same wakeup: a loop that returned on `Closed`
        // answered neither, and the socket looked dead from the one
        // client every operator reaches for first. It did, until this
        // comment.
        //
        // This is the same rule `nitro-wire`'s `ClientStream::read`
        // follows for the same reason ("a hangup is remembered rather
        // than raised at once, so the bytes that arrived before it can
        // still be decoded"), and the line protocol has no excuse to be
        // different.
        let hung_up = match self.clients[k].read() {
            ReadOutcome::Closed => true,
            ReadOutcome::Overflow => {
                self.clients[k].send(power::err("request too long"));
                self.clients[k].flush_blocking(REPLY_TIMEOUT);
                return true;
            }
            ReadOutcome::Open => false,
        };
        while let Some(line) = self.clients[k].next_line() {
            match Command::parse(&line) {
                Ok(cmd) => {
                    let reply = self.run_command(cmd);
                    self.clients[k].send(reply);
                    if cmd == Command::Logout {
                        // Answer before going away, then stop.
                        self.clients[k].flush_blocking(REPLY_TIMEOUT);
                        self.stopping = Some(Outcome::Stopped);
                        return true;
                    }
                }
                Err(ParseError::Empty) => {}
                Err(e) => {
                    // A protocol error is fatal for the connection, as it
                    // is everywhere else in this tree: a client that sent
                    // something we do not have has misunderstood its own
                    // situation, and its next line is guesswork.
                    self.clients[k].send(power::err(&e.to_string()));
                    self.clients[k].flush_blocking(REPLY_TIMEOUT);
                    return true;
                }
            }
        }
        if hung_up {
            // Everything that arrived has been answered; now honour the
            // hangup. The flush is blocking because the peer may already
            // be waiting on the reply with nothing left to send.
            self.clients[k].flush_blocking(REPLY_TIMEOUT);
            return true;
        }
        let _ = self.clients[k].flush();
        false
    }

    /// Execute one command and produce its reply.
    fn run_command(&mut self, cmd: Command) -> Vec<u8> {
        debug!("session socket: {cmd:?}");
        match cmd {
            Command::Status => power::status_reply(&self.status()),
            Command::Logout => power::ok(),
            Command::Lock => power::err(
                "lock is not implemented yet (M4: nitro-lock and the logind inhibitor; see crates/nitro-session/README.md)",
            ),
            other => {
                let Some(verb) = other.systemctl_verb() else {
                    return power::err("no action");
                };
                info!("session socket: systemctl {verb}");
                match power::run_systemctl(verb) {
                    Ok(()) => power::ok(),
                    Err(e) => {
                        warn!("{e}");
                        power::err(&e)
                    }
                }
            }
        }
    }

    /// SIGTERM every piece in reverse start order, wait for all of them
    /// against one deadline, SIGKILL the rest.
    pub fn teardown(&mut self) {
        for slot in self.slots.iter_mut().rev() {
            if let Some(child) = slot.child.as_mut() {
                if child.is_reaped() {
                    // Already exited and waited for — the readiness
                    // probe reaps the server to ask whether it is still
                    // alive, so this is the `WaitError::Died` path.
                    // `terminate` would be a no-op (see `Child::exited`),
                    // but logging "stopping" for a process that is
                    // already gone is the kind of line that sends a
                    // reader looking for a bug that is not there.
                    debug!("{} was already gone", slot.piece.program);
                    slot.child = None;
                } else {
                    info!("stopping {} (pid {})", slot.piece.program, child.pid());
                    child.terminate();
                }
            }
            slot.restart_at = None;
        }
        let limit = self.config.teardown_timeout;
        let deadline = Instant::now() + limit;
        loop {
            let live: Vec<usize> = self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.child.as_ref().map(|_| i))
                .collect();
            if live.is_empty() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                // Reverse order here too: `SIGKILL` is the same request
                // as `SIGTERM` with no room to refuse, and a piece must
                // not outlive the thing it talks to on this path either.
                for i in live.into_iter().rev() {
                    let program = self.slots[i].piece.program;
                    if let Some(child) = self.slots[i].child.as_mut() {
                        warn!(
                            "{program} did not stop in {:.1}s; killing it",
                            limit.as_secs_f32()
                        );
                        child.kill();
                        // `SIGKILL` cannot be refused, so a blocking
                        // wait here is bounded by the kernel.
                        let _ = child.wait_blocking();
                        self.slots[i].child = None;
                    }
                }
                break;
            }
            {
                // The borrow of the children's descriptors lives only
                // for the poll; the reaping below needs `&mut`.
                let fds_owned: Vec<_> = live
                    .iter()
                    .filter_map(|&i| self.slots[i].child.as_ref().map(Child::as_fd))
                    .collect();
                let mut fds: Vec<PollFd<'_>> = fds_owned
                    .iter()
                    .map(|fd| PollFd::new(fd, PollFlags::IN))
                    .collect();
                let left = deadline - now;
                let ts = Timespec {
                    tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
                    tv_nsec: i64::from(left.subsec_nanos()),
                };
                match rustix::event::poll(&mut fds, Some(&ts)) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(_) => break,
                }
            }
            for i in live {
                let program = self.slots[i].piece.program;
                let exit = self.slots[i].child.as_mut().and_then(Child::reap);
                if let Some(exit) = exit {
                    debug!("{program} {exit}");
                    self.slots[i].child = None;
                }
            }
        }
        // Answer whatever a client still had queued, then take the
        // socket away: a session that has torn its desktop down must not
        // still be accepting a `suspend` it would never carry out. The
        // listener itself stays alive until `Session` drops, so a client
        // that is mid-write gets an error rather than a hang.
        for client in &mut self.clients {
            client.flush_blocking(REPLY_TIMEOUT);
        }
        self.clients.clear();
        self.listener.unlink();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A `Session` that goes out of scope without `run` returning —
        // a panicking test, a `?` on the way out of `main` — must not
        // leave a compositor holding the VT.
        if self.slots.iter().any(|s| s.child.is_some()) {
            self.teardown();
        }
    }
}

fn spawn_slot(slot: &mut Slot, config: &Config) -> Result<(), SpawnError> {
    let program = crate::pieces::resolve(slot.piece.program, config.bin_dir.as_deref());
    let args = config.args_for(slot.piece.program);
    let child = Child::spawn(
        slot.piece.program,
        slot.piece.role,
        &program,
        &args,
        config.bin_dir.as_deref(),
    )?;
    info!(
        "started {} (pid {}) from {}",
        slot.piece.program,
        child.pid(),
        program.display()
    );
    slot.child = Some(child);
    Ok(())
}

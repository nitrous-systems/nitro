//! The audio thread: decode, equalise, apply volume, write to the output
//! — and publish what it is doing for the window to show.
//!
//! # Why a thread
//!
//! The app loop sleeps in `epoll` and must never block, and playing
//! audio is a blocking write into a pipe that drains at the sound card's
//! rate (see [`crate::sink`]). So the whole signal path lives on one
//! thread of its own, and the window talks to it through two things:
//!
//! * a **command channel** ([`Cmd`]) — load, play, pause, seek, the
//!   volume, the equaliser curve. The thread blocks on it when there is
//!   nothing to play, so a stopped player costs no CPU at all;
//! * a **status snapshot** ([`Status`]) behind a mutex, which the thread
//!   rewrites after every chunk and the window reads on its tick.
//!
//! The window does not need waking by the thread: while something plays,
//! it is already ticking to move the clock and the visualiser, and it
//! reads the end of a track off that same tick. While nothing plays
//! nothing changes, the window has no timer, and the process is idle —
//! the same contract every nitro app keeps.
//!
//! # The heard position
//!
//! What has been written is ahead of what has been heard by the output's
//! buffer ([`crate::sink::Sink::latency_frames`]). The clock and the
//! visualiser both report the *heard* position — the one the listener
//! can check — and pausing rewinds the decoder to it, so the fifth of a
//! second that was in the pipe when the player was killed is played
//! again on resume rather than lost.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crate::dsp::{self, EqSettings, Equalizer, FFT_SIZE};
use crate::sink::{Backend, Sink};
use crate::source::{self, Meta, Source, Tools};

/// Frames decoded and written per step: 23 ms at 44.1 kHz, which is how
/// long a command can wait behind a write.
const CHUNK_FRAMES: usize = 1024;

/// A request to the audio thread.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Open a track. `play` starts it at once; `token` comes back in
    /// [`Status::token`] so the window can tell which load a status —
    /// an error, an end — is about.
    Load {
        /// The file (or URL).
        path: PathBuf,
        /// Start playing immediately.
        play: bool,
        /// Echoed in the status.
        token: u64,
    },
    /// Play, or resume from a pause. A track that has ended restarts.
    Play,
    /// Pause if playing, resume if paused (Winamp's `C`).
    Pause,
    /// Stop and rewind to the start.
    Stop,
    /// Jump to a position, in seconds.
    Seek(f64),
    /// Volume, `0.0..=1.0`.
    Volume(f32),
    /// Balance, `-1.0..=1.0`.
    Balance(f32),
    /// The equaliser curve.
    Eq(EqSettings),
    /// End the thread.
    Quit,
}

/// Transport state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// Nothing is playing.
    #[default]
    Stopped,
    /// Sound is going out.
    Playing,
    /// Held at a position.
    Paused,
}

/// What the audio thread is doing, as of its last chunk.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// Transport state.
    pub state: State,
    /// The `token` of the last [`Cmd::Load`].
    pub token: u64,
    /// The heard position, in seconds.
    pub position: f64,
    /// The track's length, if known.
    pub duration: Option<f64>,
    /// Tags and format of the loaded track.
    pub meta: Meta,
    /// The loaded track played to its end. Cleared by the next load or
    /// play.
    pub ended: bool,
    /// Why the last load or write failed.
    pub error: Option<String>,
    /// The most recent [`FFT_SIZE`] heard samples, mixed to mono, for
    /// the visualiser.
    pub scope: Vec<f32>,
    /// The rate `scope` was sampled at.
    pub rate: u32,
    /// Which output is in use (`pipewire`, `silent`, …).
    pub output: &'static str,
    /// How many commands the thread has taken so far. The window
    /// compares it with how many it sent ([`Player::caught_up`]): a
    /// status read before the thread has seen the last command says
    /// nothing about that command yet.
    pub acked: u64,
}

/// The window's handle on the audio thread.
pub struct Player {
    tx: Sender<Cmd>,
    /// Commands sent, to compare with [`Status::acked`].
    sent: u64,
    status: Arc<Mutex<Status>>,
    join: Option<JoinHandle<()>>,
}

impl Player {
    /// Start the audio thread.
    ///
    /// # Errors
    /// If the thread cannot be spawned.
    pub fn spawn(tools: Tools, backend: Backend) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let status = Arc::new(Mutex::new(Status {
            output: backend.name(),
            ..Status::default()
        }));
        let shared = Arc::clone(&status);
        let join = std::thread::Builder::new()
            .name("nitro-amp-audio".to_owned())
            .spawn(move || Engine::new(tools, backend, shared).run(&rx))?;
        Ok(Self {
            tx,
            sent: 0,
            status,
            join: Some(join),
        })
    }

    /// Send a command. A dead thread (it only ends on `Quit`) is
    /// ignored: the status it left says what it last did.
    pub fn send(&mut self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            self.sent += 1;
        }
    }

    /// Whether the thread has taken every command sent so far, so the
    /// status describes their effect.
    ///
    /// What lets the window stop ticking honestly: "stopped" read a
    /// moment after sending `Load { play: true }` may just mean the
    /// load has not been seen yet.
    #[must_use]
    pub fn caught_up(&self) -> bool {
        lock(&self.status).acked >= self.sent
    }

    /// Read the status without copying it.
    pub fn with_status<R>(&self, f: impl FnOnce(&Status) -> R) -> R {
        f(&lock(&self.status))
    }

    /// A copy of the status.
    #[must_use]
    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Lock, shrugging off poisoning: the status is plain data that is
/// rewritten whole on every chunk, so a panic mid-write cannot leave
/// anything the next write will not replace.
fn lock(m: &Mutex<Status>) -> MutexGuard<'_, Status> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The thread's own state.
struct Engine {
    tools: Tools,
    backend: Backend,
    status: Arc<Mutex<Status>>,
    source: Option<Box<dyn Source>>,
    sink: Option<Sink>,
    eq: Equalizer,
    volume: f32,
    balance: f32,
    state: State,
    ended: bool,
    token: u64,
    duration: Option<f64>,
    meta: Meta,
    error: Option<String>,
    /// Seconds into the track at the last seek (or load, or pause).
    base: f64,
    /// Frames written since `base`.
    written: u64,
    /// A seek asked for and not yet done; see [`Engine::apply_seek`].
    pending_seek: Option<f64>,
    /// Commands taken; published as [`Status::acked`].
    acked: u64,
    /// Recent mono samples: enough to reach back past the output's
    /// latency and still fill an analysis window.
    ring: VecDeque<f32>,
    buf: Vec<f32>,
}

impl Engine {
    fn new(tools: Tools, backend: Backend, status: Arc<Mutex<Status>>) -> Self {
        Self {
            tools,
            backend,
            status,
            source: None,
            sink: None,
            eq: Equalizer::new(44_100, EqSettings::default()),
            volume: 1.0,
            balance: 0.0,
            state: State::Stopped,
            ended: false,
            token: 0,
            duration: None,
            meta: Meta::default(),
            error: None,
            base: 0.0,
            written: 0,
            pending_seek: None,
            acked: 0,
            ring: VecDeque::new(),
            buf: vec![0.0; CHUNK_FRAMES * 2],
        }
    }

    fn run(mut self, rx: &Receiver<Cmd>) {
        loop {
            // Everything already queued, then one step of work.
            loop {
                match rx.try_recv() {
                    Ok(Cmd::Quit) | Err(TryRecvError::Disconnected) => return,
                    Ok(c) => self.handle(c),
                    Err(TryRecvError::Empty) => break,
                }
            }
            self.apply_seek();
            if self.state == State::Playing {
                self.step();
                self.publish();
                continue;
            }
            self.publish();
            // Nothing to play: block. This is the idle contract on the
            // audio side — no wakeups until somebody asks.
            match rx.recv() {
                Ok(Cmd::Quit) | Err(_) => return,
                Ok(c) => self.handle(c),
            }
        }
    }

    /// Carry out the latest requested seek, if any.
    ///
    /// Seeks are **coalesced**: a drag along the seek bar sends one per
    /// pointer move, and for an `ffmpeg` source each one is a process
    /// restart. Only the last of a burst — whatever was queued by the
    /// time the thread looked — is worth doing.
    fn apply_seek(&mut self) {
        if let Some(secs) = self.pending_seek.take() {
            self.ended = false;
            self.reposition(secs);
        }
    }

    fn rate(&self) -> u32 {
        self.source.as_ref().map_or(44_100, |s| s.rate())
    }

    /// The heard position.
    fn position(&self) -> f64 {
        let latency = self.sink.as_ref().map_or(0, Sink::latency_frames);
        let heard = self.written.saturating_sub(latency);
        let p = self.base + heard as f64 / f64::from(self.rate());
        match self.duration {
            Some(d) => p.min(d),
            None => p,
        }
    }

    /// Move the decoder to `secs` and forget everything queued after
    /// the old position.
    fn reposition(&mut self, secs: f64) {
        self.sink = None;
        self.ring.clear();
        self.written = 0;
        self.base = secs;
        if let Some(src) = &mut self.source
            && let Err(e) = src.seek(secs)
        {
            self.error = Some(format!("seek: {e}"));
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        self.acked += 1;
        match cmd {
            // A new track makes a seek in the old one moot.
            Cmd::Load { .. } => self.pending_seek = None,
            Cmd::Seek(_) | Cmd::Volume(_) | Cmd::Balance(_) | Cmd::Eq(_) => {}
            // Anything that depends on the position sees the seek that
            // was asked for before it, not the position before that.
            _ => self.apply_seek(),
        }
        match cmd {
            Cmd::Load { path, play, token } => {
                self.token = token;
                self.ended = false;
                self.error = None;
                self.base = 0.0;
                self.written = 0;
                self.ring.clear();
                self.source = None;
                match source::open(&path, &self.tools) {
                    Ok((src, meta)) => {
                        if self.sink.as_ref().is_some_and(|s| s.rate() != src.rate()) {
                            self.sink = None;
                        }
                        self.eq.set_rate(src.rate());
                        self.duration = meta.duration;
                        self.meta = meta;
                        self.source = Some(src);
                        self.state = if play { State::Playing } else { State::Stopped };
                    }
                    Err(e) => {
                        self.sink = None;
                        self.meta = Meta::default();
                        self.duration = None;
                        self.error = Some(e);
                        self.state = State::Stopped;
                    }
                }
            }
            Cmd::Play => match self.state {
                _ if self.source.is_none() => {}
                State::Playing => {}
                State::Paused => self.state = State::Playing,
                State::Stopped => {
                    if self.ended {
                        self.ended = false;
                        self.reposition(0.0);
                    }
                    self.state = State::Playing;
                }
            },
            Cmd::Pause => match self.state {
                State::Playing => {
                    let heard = self.position();
                    self.reposition(heard);
                    self.state = State::Paused;
                }
                State::Paused => self.state = State::Playing,
                State::Stopped => {}
            },
            Cmd::Stop => {
                if self.source.is_some() {
                    self.reposition(0.0);
                }
                self.state = State::Stopped;
            }
            Cmd::Seek(secs) => {
                if self.source.is_some() {
                    self.pending_seek = Some(match self.duration {
                        Some(d) => secs.clamp(0.0, d),
                        None => secs.max(0.0),
                    });
                }
            }
            Cmd::Volume(v) => self.volume = v.clamp(0.0, 1.0),
            Cmd::Balance(b) => self.balance = b.clamp(-1.0, 1.0),
            Cmd::Eq(s) => self.eq.set(s),
            Cmd::Quit => {}
        }
    }

    /// Decode, process and write one chunk.
    fn step(&mut self) {
        let Some(src) = &mut self.source else {
            self.state = State::Stopped;
            return;
        };
        let rate = src.rate();
        let n = match src.read(&mut self.buf) {
            Ok(n) => n,
            Err(e) => {
                self.error = Some(format!("decode: {e}"));
                0
            }
        };
        if n == 0 {
            // The end. The output is kept: the next track (if the window
            // loads one straight away) queues behind what is still in
            // the pipe, which is as close to gapless as a pipe gets.
            // Everything written will have been heard by the time the
            // listener notices: the clock goes to the end.
            self.written += self.sink.as_ref().map_or(0, Sink::latency_frames);
            if self.duration.is_none() {
                // A stream that did not say how long it was has now.
                self.duration = Some(self.position());
            }
            self.state = State::Stopped;
            self.ended = true;
            return;
        }
        if self.sink.is_none() {
            match self.backend.open(rate) {
                Ok(s) => self.sink = Some(s),
                Err(e) => {
                    // A player that is installed but will not start (no
                    // sound server running) should not stop the music
                    // being *shown*: fall back to silence and say so.
                    self.error = Some(format!("{}: {e}; playing silently", self.backend.name()));
                    self.backend = Backend::Silent;
                    self.sink = Backend::Silent.open(rate).ok();
                }
            }
        }
        let chunk = &mut self.buf[..n];
        self.eq.process(chunk);
        dsp::apply_gain(chunk, self.volume, self.balance);
        let latency = self.sink.as_ref().map_or(0, Sink::latency_frames) as usize;
        let keep = FFT_SIZE + latency;
        for f in chunk.chunks_exact(2) {
            self.ring.push_back((f[0] + f[1]) * 0.5);
        }
        while self.ring.len() > keep {
            self.ring.pop_front();
        }
        if let Some(sink) = &mut self.sink
            && let Err(e) = sink.write(chunk)
        {
            self.error = Some(format!("{}: {e}", self.backend.name()));
            self.sink = None;
            self.state = State::Stopped;
            return;
        }
        self.written += (n / 2) as u64;
    }

    fn publish(&self) {
        let mut s = lock(&self.status);
        s.state = self.state;
        s.token = self.token;
        s.position = if self.source.is_some() {
            self.position()
        } else {
            0.0
        };
        s.duration = self.duration;
        if s.meta != self.meta {
            s.meta = self.meta.clone();
        }
        s.ended = self.ended;
        s.error.clone_from(&self.error);
        s.rate = self.rate();
        s.output = self.backend.name();
        s.acked = self.acked;
        // The window onto the ring that ends where the listener is.
        let latency = self.sink.as_ref().map_or(0, Sink::latency_frames) as usize;
        let end = self.ring.len().saturating_sub(latency);
        let start = end.saturating_sub(FFT_SIZE);
        s.scope.clear();
        s.scope.extend(self.ring.range(start..end));
    }
}

//! Where samples go: a sound server's command-line player, fed raw
//! `f32le` stereo on its stdin.
//!
//! nitro has no audio stack and is not going to grow one (see
//! `nitro-settings`' `audio.rs`, which says why). So output is
//! `pw-cat` (`PipeWire`), else `paplay` (`PulseAudio`, which `PipeWire`
//! also answers), else `aplay` (bare ALSA) — whichever is installed,
//! found once at start-up — and the process's stdin pipe is the whole
//! interface.
//!
//! # A player that will not start
//!
//! An installed player can still die at once: no sound server to talk
//! to, a rate or an argument it refuses. The first write then fails with
//! `EPIPE`, which says nothing, so the player's stderr is kept (its last
//! kilobyte, drained by a thread of its own so a chatty player can never
//! block on it) and appended to the error. A player that died before
//! taking a pipe's worth has played nothing anyone heard, so the engine
//! moves on to the next installed one ([`Backend::detect_all`]) and
//! writes the same samples again ([`Sink::into_unheard`]).
//!
//! # The pipe is the clock
//!
//! Nothing here sleeps or counts time when a real player is attached: a
//! write blocks when the pipe is full, and the pipe drains at exactly
//! the device's rate, so the audio thread runs at the speed the sound
//! card plays. The cost is latency — what has been written is ahead of
//! what has been heard by about one pipe's worth — which
//! [`Sink::latency_frames`] reports so the clock and the visualiser can
//! subtract it.
//!
//! # No player at all
//!
//! [`Backend::Silent`] plays nothing and paces itself to the wall clock,
//! so the player still *works* on a machine without sound (the test
//! box has none): the clock runs, tracks end and advance, the
//! visualiser moves. The window says the output is silent rather than
//! pretending otherwise.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A pipe's default capacity on Linux, in bytes; `F_GETPIPE_SZ` would
/// say exactly, at the cost of an `fcntl` the rest of this module does
/// not need.
const PIPE_BYTES: u64 = 64 * 1024;

/// How much of a player's stderr is kept: its tail, which is where the
/// reason it died is.
const STDERR_KEEP: usize = 1024;

/// How long a failed write waits for the player to exit and its stderr
/// to be read, so the error can say why.
const EXPLAIN_WAIT: Duration = Duration::from_millis(500);

/// Which output to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// `pw-cat --playback --raw`.
    PwCat(PathBuf),
    /// `paplay --raw`.
    Paplay(PathBuf),
    /// `aplay -t raw`.
    Aplay(PathBuf),
    /// Nothing audible, paced to the wall clock.
    Silent,
    /// Nothing audible and **not** paced: runs as fast as the decoder.
    /// For tests, which want a thirty-second track to end now.
    Unpaced,
}

impl Backend {
    /// The first player found on `$PATH`, or [`Backend::Silent`].
    #[must_use]
    pub fn detect() -> Self {
        Self::detect_in(&crate::path_dirs())
    }

    /// The first player found in `dirs`.
    #[must_use]
    pub fn detect_in(dirs: &[PathBuf]) -> Self {
        Self::detect_all_in(dirs)
            .into_iter()
            .next()
            .unwrap_or(Self::Silent)
    }

    /// Every player on `$PATH`, best first.
    #[must_use]
    pub fn detect_all() -> Vec<Self> {
        Self::detect_all_in(&crate::path_dirs())
    }

    /// Every player found in `dirs`, best first: `pw-cat`, `paplay`,
    /// `aplay`. Empty when there is none.
    #[must_use]
    pub fn detect_all_in(dirs: &[PathBuf]) -> Vec<Self> {
        let mut found = Vec::new();
        if let Some(p) = crate::find_program(dirs, "pw-cat") {
            found.push(Self::PwCat(p));
        }
        if let Some(p) = crate::find_program(dirs, "paplay") {
            found.push(Self::Paplay(p));
        }
        if let Some(p) = crate::find_program(dirs, "aplay") {
            found.push(Self::Aplay(p));
        }
        found
    }

    /// A short name for the window's status line.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::PwCat(_) => "pipewire",
            Self::Paplay(_) => "pulseaudio",
            Self::Aplay(_) => "alsa",
            Self::Silent | Self::Unpaced => "silent",
        }
    }

    /// Whether anything will be heard.
    #[must_use]
    pub fn is_audible(&self) -> bool {
        matches!(self, Self::PwCat(_) | Self::Paplay(_) | Self::Aplay(_))
    }

    /// Start an output at `rate` Hz, stereo `f32le`.
    ///
    /// # Errors
    /// If the player process cannot be started.
    pub fn open(&self, rate: u32) -> io::Result<Sink> {
        let (bin, args): (&PathBuf, Vec<String>) = match self {
            Self::PwCat(p) => (
                p,
                // `--raw`: without it a current `pw-cat` hands `-` to
                // libsndfile as a container file, which headerless f32
                // is not ("Format not recognised"), and exits at once.
                vec![
                    "--playback".into(),
                    "--raw".into(),
                    "--format".into(),
                    "f32".into(),
                    "--rate".into(),
                    rate.to_string(),
                    "--channels".into(),
                    "2".into(),
                    "--media-role".into(),
                    "Music".into(),
                    "-".into(),
                ],
            ),
            Self::Paplay(p) => (
                p,
                vec![
                    "--raw".into(),
                    "--format=float32le".into(),
                    format!("--rate={rate}"),
                    "--channels=2".into(),
                    "--client-name=nitro-amp".into(),
                ],
            ),
            Self::Aplay(p) => (
                p,
                vec![
                    "-q".into(),
                    "-t".into(),
                    "raw".into(),
                    "-f".into(),
                    "FLOAT_LE".into(),
                    "-r".into(),
                    rate.to_string(),
                    "-c".into(),
                    "2".into(),
                    "-".into(),
                ],
            ),
            Self::Silent => return Ok(Sink::silent(rate, true)),
            Self::Unpaced => return Ok(Sink::silent(rate, false)),
        };
        let mut child = Command::new(bin)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("no stdin pipe"))?;
        let stderr = Arc::new(Mutex::new(Stderr::default()));
        if let Some(pipe) = child.stderr.take() {
            let shared = Arc::clone(&stderr);
            let spawned = std::thread::Builder::new()
                .name("nitro-amp-stderr".to_owned())
                .spawn(move || drain(pipe, &shared));
            if spawned.is_err() {
                // No reader: say so rather than leave "why" empty; the
                // pipe is dropped with the closure, so the player gets
                // EPIPE on stderr instead of blocking on it.
                lock(&stderr).done = true;
            }
        }
        Ok(Sink {
            rate,
            out: Out::Process {
                child,
                stdin,
                stderr,
                given: 0,
                head: Vec::new(),
            },
            bytes: Vec::new(),
        })
    }
}

/// An open output.
pub struct Sink {
    rate: u32,
    out: Out,
    bytes: Vec<u8>,
}

enum Out {
    Process {
        child: Child,
        stdin: ChildStdin,
        /// The tail of the player's stderr.
        stderr: Arc<Mutex<Stderr>>,
        /// Bytes handed to `write` so far, including a write that failed.
        given: u64,
        /// Every sample handed to `write`, while `given` is within a
        /// pipe's worth: what to replay on another player if this one
        /// turns out never to have started.
        head: Vec<f32>,
    },
    Silent {
        /// When the first frame was "played", or `None` for unpaced.
        epoch: Option<Instant>,
        written: u64,
    },
}

impl Sink {
    fn silent(rate: u32, paced: bool) -> Self {
        Self {
            rate,
            out: Out::Silent {
                epoch: paced.then(Instant::now),
                written: 0,
            },
            bytes: Vec::new(),
        }
    }

    /// The rate this output was opened at.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Roughly how many frames have been written but not yet heard.
    #[must_use]
    pub fn latency_frames(&self) -> u64 {
        match self.out {
            // One pipe's worth; the player's own buffer adds a little
            // more, which is below what a clock with one-second
            // resolution or a 30 Hz visualiser can show.
            Out::Process { .. } => PIPE_BYTES / 8,
            Out::Silent { .. } => 0,
        }
    }

    /// Play interleaved stereo `samples`, blocking until they are taken.
    ///
    /// # Errors
    /// The player died (`EPIPE`) or the write failed.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        match &mut self.out {
            Out::Process {
                child,
                stdin,
                stderr,
                given,
                head,
            } => {
                self.bytes.clear();
                for s in samples {
                    self.bytes.extend_from_slice(&s.to_le_bytes());
                }
                *given += self.bytes.len() as u64;
                if *given <= PIPE_BYTES {
                    head.extend_from_slice(samples);
                } else if !head.is_empty() {
                    *head = Vec::new();
                }
                stdin
                    .write_all(&self.bytes)
                    .map_err(|e| explain(e, child, stderr))
            }
            Out::Silent { epoch, written } => {
                *written += (samples.len() / 2) as u64;
                if let Some(t0) = epoch {
                    let due = Duration::from_secs_f64(*written as f64 / f64::from(self.rate));
                    if let Some(wait) = due.checked_sub(t0.elapsed()) {
                        std::thread::sleep(wait);
                    }
                }
                Ok(())
            }
        }
    }
}

impl Sink {
    /// For an output whose player failed before it can have played
    /// anything — it was given no more than a pipe's worth — every
    /// sample it was given, to write again elsewhere. `None` once it has
    /// played for real, and for silent outputs, which never fail.
    #[must_use]
    pub fn into_unheard(mut self) -> Option<Vec<f32>> {
        match &mut self.out {
            Out::Process { given, head, .. } if *given <= PIPE_BYTES => Some(std::mem::take(head)),
            _ => None,
        }
    }
}

/// What has been read of a player's stderr.
#[derive(Default)]
struct Stderr {
    /// The last [`STDERR_KEEP`] bytes, lossily decoded.
    tail: String,
    /// The pipe reached its end (or could not be read).
    done: bool,
}

fn lock(m: &Mutex<Stderr>) -> std::sync::MutexGuard<'_, Stderr> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Read `pipe` to its end into `into`, keeping only the tail. Runs on a
/// thread of its own, so the player never blocks on a full stderr pipe;
/// it ends when the player (and anything it started) closes stderr.
fn drain(mut pipe: ChildStderr, into: &Mutex<Stderr>) {
    let mut buf = [0u8; 512];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut s = lock(into);
                s.tail.push_str(&String::from_utf8_lossy(&buf[..n]));
                if s.tail.len() > STDERR_KEEP {
                    let mut cut = s.tail.len() - STDERR_KEEP;
                    while !s.tail.is_char_boundary(cut) {
                        cut += 1;
                    }
                    s.tail.drain(..cut);
                }
            }
        }
    }
    lock(into).done = true;
}

/// Add the player's last word to a failed write's error: give it a
/// moment to exit (after `EPIPE` it already has) and its stderr to be
/// read, then append the last non-empty line.
fn explain(e: io::Error, child: &mut Child, stderr: &Mutex<Stderr>) -> io::Error {
    let deadline = Instant::now() + EXPLAIN_WAIT;
    while Instant::now() < deadline {
        let exited = !matches!(child.try_wait(), Ok(None));
        if exited && lock(stderr).done {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let s = lock(stderr);
    match s.tail.lines().map(str::trim).rfind(|l| !l.is_empty()) {
        Some(line) => io::Error::new(e.kind(), format!("{e}: {line}")),
        None => e,
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Out::Process { child, .. } = &mut self.out {
            // Kill rather than close-and-wait: a stop should be silent
            // now, not after the pipe's last fifth of a second drains.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(dir: &std::path::Path, name: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn every_player_is_found_best_first() {
        let d = std::env::temp_dir().join(format!("nitro-amp-sink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(Backend::detect_all_in(std::slice::from_ref(&d)).is_empty());
        assert_eq!(
            Backend::detect_in(std::slice::from_ref(&d)),
            Backend::Silent
        );
        fake(&d, "aplay");
        fake(&d, "pw-cat");
        fake(&d, "paplay");
        let found = Backend::detect_all_in(std::slice::from_ref(&d));
        let names: Vec<_> = found.iter().map(Backend::name).collect();
        assert_eq!(names, ["pipewire", "pulseaudio", "alsa"]);
        assert_eq!(Backend::detect_in(std::slice::from_ref(&d)), found[0]);
        let _ = std::fs::remove_dir_all(&d);
    }
}

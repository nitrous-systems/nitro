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

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

/// A pipe's default capacity on Linux, in bytes; `F_GETPIPE_SZ` would
/// say exactly, at the cost of an `fcntl` the rest of this module does
/// not need.
const PIPE_BYTES: u64 = 64 * 1024;

/// Which output to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// `pw-cat --playback`.
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
        if let Some(p) = crate::find_program(dirs, "pw-cat") {
            return Self::PwCat(p);
        }
        if let Some(p) = crate::find_program(dirs, "paplay") {
            return Self::Paplay(p);
        }
        if let Some(p) = crate::find_program(dirs, "aplay") {
            return Self::Aplay(p);
        }
        Self::Silent
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
                vec![
                    "--playback".into(),
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
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("no stdin pipe"))?;
        Ok(Sink {
            rate,
            out: Out::Process { child, stdin },
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
            Out::Process { stdin, .. } => {
                self.bytes.clear();
                for s in samples {
                    self.bytes.extend_from_slice(&s.to_le_bytes());
                }
                stdin.write_all(&self.bytes)
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

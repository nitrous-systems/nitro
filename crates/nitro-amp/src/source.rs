//! Where samples come from: a file, decoded to interleaved stereo `f32`.
//!
//! Two decoders, one trait. A WAV file is read in-process by
//! [`crate::wav`]; everything else — MP3, FLAC, Ogg, Opus, AAC, tracker
//! modules, an `http://` stream — is handed to **`ffmpeg`** as a child
//! process that writes raw `f32le` to a pipe.
//!
//! # Why a subprocess and not a codec crate
//!
//! The same reason `nitro-settings` shells out to `wpctl`: the codecs are
//! somebody else's decade of work, the machines this runs on already
//! have them, and the alternative is a dozen crates in
//! `DEPENDENCIES.md` whose parsers would run on untrusted files *inside*
//! the player. A child process is the better sandbox and the smaller
//! build. Its cost is that seeking means restarting the child at the
//! new offset (`-ss`), which `ffmpeg` does by index where the format has
//! one, so it is fast on everything people seek in.
//!
//! When `ffmpeg` is not installed, WAV still plays and everything else
//! reports that plainly.
//!
//! # The search path is injected
//!
//! [`Tools::find_in`] takes the directories to look in, so the tests can
//! point it at a fake `ffmpeg` script — setting `PATH` would be
//! process-global and `unsafe`. `nitro-settings`' audio backend makes the
//! same choice for the same reason.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use crate::wav;

/// The rate external decodes are resampled to.
///
/// One fixed rate for everything `ffmpeg` decodes, so a playlist of
/// mixed 44.1 and 48 kHz files does not restart the output process at
/// every track boundary; 44.1 kHz because that is what nearly all music
/// already is, so nearly nothing is actually resampled.
pub const EXTERNAL_RATE: u32 = 44_100;

/// The helper programs, where they were found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tools {
    /// `ffmpeg`, the decoder.
    pub ffmpeg: Option<PathBuf>,
    /// `ffprobe`, for tags and duration. Optional even with `ffmpeg`
    /// present: without it a track simply shows its file name and no
    /// length until it ends.
    pub ffprobe: Option<PathBuf>,
}

impl Tools {
    /// Look for the helpers on `$PATH`.
    #[must_use]
    pub fn find() -> Self {
        Self::find_in(&crate::path_dirs())
    }

    /// Look for the helpers in `dirs`, in order.
    #[must_use]
    pub fn find_in(dirs: &[PathBuf]) -> Self {
        Self {
            ffmpeg: crate::find_program(dirs, "ffmpeg"),
            ffprobe: crate::find_program(dirs, "ffprobe"),
        }
    }
}

/// What is known about a track once it is open.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    /// Title tag.
    pub title: Option<String>,
    /// Artist tag.
    pub artist: Option<String>,
    /// Length in seconds, if the container says.
    pub duration: Option<f64>,
    /// The file's own sample rate, as Winamp's "kHz" shows it.
    pub rate: Option<u32>,
    /// The file's own channel count.
    pub channels: Option<u16>,
    /// Bit rate in kbit/s.
    pub kbps: Option<u32>,
}

impl Meta {
    /// `Artist - Title`, `Title`, or `None` when the tags say nothing.
    #[must_use]
    pub fn display_title(&self) -> Option<String> {
        match (&self.artist, &self.title) {
            (Some(a), Some(t)) => Some(format!("{a} - {t}")),
            (None, Some(t)) => Some(t.clone()),
            _ => None,
        }
    }
}

/// A decoder.
///
/// Every source yields **interleaved stereo** at [`Source::rate`]: the
/// channel mapping happens here, once, so nothing downstream has to know
/// how many channels a file had.
pub trait Source: Send {
    /// Frames per second of what [`Source::read`] produces.
    fn rate(&self) -> u32;

    /// Fill `out` with up to `out.len() / 2` frames; returns the number
    /// of **samples** written (always even). Zero is the end.
    ///
    /// # Errors
    /// A read or decode failure.
    fn read(&mut self, out: &mut [f32]) -> io::Result<usize>;

    /// Continue from `secs` into the track.
    ///
    /// # Errors
    /// A failed seek or restart.
    fn seek(&mut self, secs: f64) -> io::Result<()>;
}

/// Open `path` with whichever decoder it needs.
///
/// A file that parses as WAV is decoded in-process; one that does not —
/// including WAV encodings [`crate::wav`] refuses, like ADPCM — goes to
/// `ffmpeg` if there is one.
///
/// # Errors
/// A message for the user: the file is missing, or nothing can decode it.
pub fn open(path: &Path, tools: &Tools) -> Result<(Box<dyn Source>, Meta), String> {
    let wav_error = match WavSource::open(path) {
        Ok(src) => {
            let meta = src.meta();
            return Ok((Box::new(src), meta));
        }
        Err(e) => e,
    };
    let is_url = path.to_string_lossy().contains("://");
    if !is_url && !path.exists() {
        return Err(format!("{}: no such file", path.display()));
    }
    let Some(ffmpeg) = &tools.ffmpeg else {
        return Err(match wav_error {
            wav::WavError::NotWav => format!(
                "{}: needs ffmpeg to decode, and ffmpeg is not installed",
                name_of(path)
            ),
            e => format!("{}: {e}", name_of(path)),
        });
    };
    let meta = tools
        .ffprobe
        .as_deref()
        .and_then(|p| probe(p, path))
        .unwrap_or_default();
    let src = ExternalSource::start(ffmpeg.clone(), path.to_path_buf(), 0.0)
        .map_err(|e| format!("{}: {e}", ffmpeg.display()))?;
    Ok((Box::new(src), meta))
}

fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Frames read per `read` from a WAV file.
const WAV_CHUNK_FRAMES: usize = 1024;

/// A WAV file, decoded in-process.
pub struct WavSource {
    info: wav::WavInfo,
    file: BufReader<File>,
    /// Frame index of the next frame to read.
    frame: u64,
    buf: Vec<u8>,
    decoded: Vec<f32>,
}

impl WavSource {
    /// Open and parse `path`.
    ///
    /// # Errors
    /// Anything [`wav::parse`] refuses, or an I/O failure.
    pub fn open(path: &Path) -> Result<Self, wav::WavError> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut file = BufReader::new(file);
        let info = wav::parse(&mut file, len)?;
        file.seek(SeekFrom::Start(info.data_offset))?;
        Ok(Self {
            info,
            file,
            frame: 0,
            buf: Vec::new(),
            decoded: Vec::new(),
        })
    }

    /// The header's description of the track.
    #[must_use]
    pub fn meta(&self) -> Meta {
        Meta {
            title: self.info.title.clone(),
            artist: self.info.artist.clone(),
            duration: Some(self.info.duration()),
            rate: Some(self.info.rate),
            channels: Some(self.info.channels),
            kbps: Some(self.info.kbps()),
        }
    }
}

impl Source for WavSource {
    fn rate(&self) -> u32 {
        self.info.rate
    }

    fn read(&mut self, out: &mut [f32]) -> io::Result<usize> {
        let left = self.info.frames().saturating_sub(self.frame);
        let want = (out.len() / 2).min(WAV_CHUNK_FRAMES).min(left as usize);
        if want == 0 {
            return Ok(0);
        }
        let fb = self.info.frame_bytes();
        self.buf.resize(want * fb, 0);
        let mut got = 0;
        while got < self.buf.len() {
            match self.file.read(&mut self.buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.decoded.clear();
        let frames = wav::to_stereo(&self.info, &self.buf[..got], &mut self.decoded);
        self.frame += frames as u64;
        out[..frames * 2].copy_from_slice(&self.decoded);
        Ok(frames * 2)
    }

    fn seek(&mut self, secs: f64) -> io::Result<()> {
        let frame = ((secs.max(0.0) * f64::from(self.info.rate)) as u64).min(self.info.frames());
        let at = self.info.data_offset + frame * self.info.frame_bytes() as u64;
        self.file.seek(SeekFrom::Start(at))?;
        self.frame = frame;
        Ok(())
    }
}

/// A file decoded by an `ffmpeg` child writing raw `f32le` stereo.
pub struct ExternalSource {
    ffmpeg: PathBuf,
    path: PathBuf,
    child: Child,
    out: ChildStdout,
    /// Bytes of a sample split across two reads.
    carry: Vec<u8>,
    bytes: Vec<u8>,
}

impl ExternalSource {
    /// Start decoding `path` from `from` seconds in.
    ///
    /// # Errors
    /// If the process cannot be started.
    pub fn start(ffmpeg: PathBuf, path: PathBuf, from: f64) -> io::Result<Self> {
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
        if from > 0.0 {
            // Before `-i`: an input seek, which uses the index rather
            // than decoding and discarding everything up to the point.
            cmd.args(["-ss", &format!("{from:.3}")]);
        }
        cmd.arg("-i")
            .arg(&path)
            .args(["-vn", "-f", "f32le", "-acodec", "pcm_f32le", "-ac", "2"])
            .args(["-ar", &EXTERNAL_RATE.to_string(), "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn()?;
        let out = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("no stdout pipe"))?;
        Ok(Self {
            ffmpeg,
            path,
            child,
            out,
            carry: Vec::new(),
            bytes: Vec::new(),
        })
    }
}

impl Source for ExternalSource {
    fn rate(&self) -> u32 {
        EXTERNAL_RATE
    }

    fn read(&mut self, out: &mut [f32]) -> io::Result<usize> {
        let frames = (out.len() / 2).min(WAV_CHUNK_FRAMES);
        self.bytes.resize(frames * 8, 0);
        let start = self.carry.len();
        self.bytes[..start].copy_from_slice(&self.carry);
        let n = loop {
            match self.out.read(&mut self.bytes[start..]) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        };
        if n == 0 {
            // End of stream; a torn trailing sample is dropped.
            self.carry.clear();
            return Ok(0);
        }
        let have = start + n;
        // Whole frames only, so left and right never swap sides.
        let whole = have / 8 * 8;
        for (o, b) in out.iter_mut().zip(self.bytes[..whole].chunks_exact(4)) {
            let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            *o = if v.is_finite() { v } else { 0.0 };
        }
        self.carry.clear();
        self.carry.extend_from_slice(&self.bytes[whole..have]);
        if whole == 0 {
            // Less than one frame arrived; ask again rather than report
            // an end that has not happened.
            return self.read(out);
        }
        Ok(whole / 4)
    }

    fn seek(&mut self, secs: f64) -> io::Result<()> {
        let fresh = Self::start(self.ffmpeg.clone(), self.path.clone(), secs)?;
        // Assigning drops the old one, which kills and reaps it.
        *self = fresh;
        Ok(())
    }
}

impl Drop for ExternalSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Ask `ffprobe` for the tags and the length.
fn probe(ffprobe: &Path, path: &Path) -> Option<Meta> {
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-show_entries"])
        .arg("format=duration,bit_rate:format_tags=title,artist:stream=sample_rate,channels")
        .args([
            "-select_streams",
            "a:0",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_probe(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `ffprobe -of default=noprint_wrappers=1` output.
///
/// `key=value` per line, tags as `TAG:title=…`. The keys are matched
/// case-insensitively (containers disagree on `TITLE` vs `title`), the
/// first value of a key wins, and `N/A` is absence — which is what
/// `ffprobe` prints for a stream without a duration.
#[must_use]
pub fn parse_probe(text: &str) -> Meta {
    let mut m = Meta::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        if v.is_empty() || v == "N/A" {
            continue;
        }
        let k = k.trim().to_ascii_lowercase();
        let k = k.strip_prefix("tag:").unwrap_or(&k);
        match k {
            "title" if m.title.is_none() => m.title = Some(v.to_owned()),
            "artist" if m.artist.is_none() => m.artist = Some(v.to_owned()),
            "duration" if m.duration.is_none() => {
                m.duration = v.parse::<f64>().ok().filter(|d| d.is_finite() && *d >= 0.0);
            }
            "bit_rate" if m.kbps.is_none() => {
                m.kbps = v.parse::<u64>().ok().map(|b| (b / 1000) as u32);
            }
            "sample_rate" if m.rate.is_none() => m.rate = v.parse().ok(),
            "channels" if m.channels.is_none() => m.channels = v.parse().ok(),
            _ => {}
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_is_read_leniently() {
        let text = "sample_rate=48000\nchannels=2\nduration=215.380000\nbit_rate=320000\n\
                    TAG:title=Song = Name\nTAG:ARTIST=Band\nTAG:title=second wins not\n\
                    garbage line\n";
        let m = parse_probe(text);
        assert_eq!(m.rate, Some(48_000));
        assert_eq!(m.channels, Some(2));
        assert_eq!(m.kbps, Some(320));
        assert_eq!(m.title.as_deref(), Some("Song = Name"));
        assert_eq!(m.artist.as_deref(), Some("Band"));
        assert!((m.duration.unwrap() - 215.38).abs() < 1e-9);
        assert_eq!(m.display_title().as_deref(), Some("Band - Song = Name"));
        let m = parse_probe("duration=N/A\n");
        assert_eq!(m.duration, None);
        assert_eq!(m.display_title(), None);
    }
}

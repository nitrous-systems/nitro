//! RIFF/WAVE, read in-process.
//!
//! The one format `nitro-amp` decodes itself. Everything else goes to
//! `ffmpeg` (see [`crate::source`]), but WAV is the format a test can
//! write in ten lines and the one a machine with no `ffmpeg` can still
//! play, so it earns a parser of its own: a header walk and a sample
//! converter, both total over hostile input — a truncated or lying file
//! is an error or a short track, never a panic.
//!
//! Supported: integer PCM at 8, 16, 24 and 32 bits, IEEE float at 32 and
//! 64, any channel count, and `WAVE_FORMAT_EXTENSIBLE` wrapping either.
//! Compressed WAV (ADPCM, µ-law, MP3-in-RIFF) is refused by name, and
//! `crate::source` hands such a file to `ffmpeg` instead.

use std::io::{self, Read, Seek, SeekFrom};

/// `WAVE_FORMAT_PCM`.
const FORMAT_PCM: u16 = 1;
/// `WAVE_FORMAT_IEEE_FLOAT`.
const FORMAT_FLOAT: u16 = 3;
/// `WAVE_FORMAT_EXTENSIBLE`: the real format is in the sub-format GUID,
/// whose first two bytes are one of the two above.
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// How the samples in the `data` chunk are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// Unsigned 8-bit, centred on 128.
    U8,
    /// Signed 16-bit little-endian.
    I16,
    /// Signed 24-bit little-endian, packed in three bytes.
    I24,
    /// Signed 32-bit little-endian.
    I32,
    /// IEEE 754 single.
    F32,
    /// IEEE 754 double.
    F64,
}

impl SampleFormat {
    /// Bytes per sample.
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::I16 => 2,
            Self::I24 => 3,
            Self::I32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    /// One sample at the start of `b`, as `-1.0..=1.0`.
    ///
    /// `b` must hold at least [`SampleFormat::bytes`] bytes; the callers
    /// slice by whole frames, so it always does.
    fn read(self, b: &[u8]) -> f32 {
        match self {
            Self::U8 => (f32::from(b[0]) - 128.0) / 128.0,
            Self::I16 => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
            Self::I24 => {
                // Sign-extend by placing the three bytes at the top of an
                // i32 and shifting back down.
                let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                v as f32 / 8_388_608.0
            }
            Self::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            Self::F32 => {
                let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if v.is_finite() { v } else { 0.0 }
            }
            Self::F64 => {
                let v = f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                if v.is_finite() { v as f32 } else { 0.0 }
            }
        }
    }
}

/// What the header says, and where the samples are.
#[derive(Debug, Clone, PartialEq)]
pub struct WavInfo {
    /// Frames per second.
    pub rate: u32,
    /// Interleaved channels per frame.
    pub channels: u16,
    /// Sample encoding.
    pub format: SampleFormat,
    /// Byte offset of the first sample.
    pub data_offset: u64,
    /// Length of the sample data in bytes, clamped to what the file
    /// really holds and rounded down to whole frames.
    pub data_len: u64,
    /// `INAM` from a `LIST/INFO` chunk before the data, if any.
    pub title: Option<String>,
    /// `IART` from the same chunk.
    pub artist: Option<String>,
}

impl WavInfo {
    /// Bytes per interleaved frame.
    #[must_use]
    pub fn frame_bytes(&self) -> usize {
        self.format.bytes() * usize::from(self.channels)
    }

    /// Frames in the file.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.data_len / self.frame_bytes() as u64
    }

    /// Duration in seconds.
    #[must_use]
    pub fn duration(&self) -> f64 {
        self.frames() as f64 / f64::from(self.rate)
    }

    /// Nominal bit rate, in kbit/s, as Winamp shows it for PCM.
    #[must_use]
    pub fn kbps(&self) -> u32 {
        let bits = u64::from(self.rate) * self.frame_bytes() as u64 * 8;
        (bits / 1000) as u32
    }
}

/// Why a file is not a WAV this module can play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WavError {
    /// Not RIFF/WAVE at all.
    NotWav,
    /// RIFF/WAVE with an encoding this module does not decode — the tag
    /// is in the message, and `ffmpeg` may well manage it.
    Unsupported(String),
    /// The header is inconsistent: no `fmt `, no `data`, zero channels.
    Malformed(&'static str),
    /// The read failed.
    Io(String),
}

impl std::fmt::Display for WavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWav => f.write_str("not a WAV file"),
            Self::Unsupported(what) => write!(f, "unsupported WAV encoding: {what}"),
            Self::Malformed(what) => write!(f, "malformed WAV: {what}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for WavError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Read four bytes, or fail as "not a WAV" at a clean end of file.
fn read4(r: &mut impl Read) -> Result<[u8; 4], WavError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            WavError::NotWav
        } else {
            WavError::from(e)
        }
    })?;
    Ok(b)
}

/// Walk the chunks of a RIFF/WAVE stream and describe it.
///
/// `file_len` is the length of the whole stream; a `data` chunk that
/// claims more than is there (a recording cut short, or the `0xFFFFFFFF`
/// streaming writers put there) is clamped to it.
///
/// # Errors
/// See [`WavError`].
pub fn parse<R: Read + Seek>(r: &mut R, file_len: u64) -> Result<WavInfo, WavError> {
    if &read4(r)? != b"RIFF" {
        return Err(WavError::NotWav);
    }
    let _riff_len = read4(r)?;
    if &read4(r)? != b"WAVE" {
        return Err(WavError::NotWav);
    }
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut title = None;
    let mut artist = None;
    let mut pos: u64 = 12;
    loop {
        let id = match read4(r) {
            Ok(id) => id,
            Err(WavError::NotWav) => return Err(WavError::Malformed("no data chunk")),
            Err(e) => return Err(e),
        };
        let len = u64::from(u32::from_le_bytes(read4(r)?));
        pos += 8;
        match &id {
            b"fmt " => {
                if len < 16 {
                    return Err(WavError::Malformed("short fmt chunk"));
                }
                let mut b = vec![0u8; len.min(64) as usize];
                r.read_exact(&mut b)?;
                let mut tag = u16::from_le_bytes([b[0], b[1]]);
                let channels = u16::from_le_bytes([b[2], b[3]]);
                let rate = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                let bits = u16::from_le_bytes([b[14], b[15]]);
                if tag == FORMAT_EXTENSIBLE {
                    // cbSize(2) validBits(2) channelMask(4) then the GUID,
                    // whose first two bytes are the real tag.
                    if b.len() < 26 {
                        return Err(WavError::Malformed("short extensible fmt chunk"));
                    }
                    tag = u16::from_le_bytes([b[24], b[25]]);
                }
                fmt = Some((tag, channels, rate, bits));
                r.seek(SeekFrom::Start(pos + len + (len & 1)))?;
            }
            b"LIST" if len >= 4 => {
                // Bounded: metadata is short, and a hostile length must
                // not become a gigabyte allocation.
                let mut b = vec![0u8; len.min(64 * 1024) as usize];
                r.read_exact(&mut b)?;
                if &b[..4] == b"INFO" {
                    read_info(&b[4..], &mut title, &mut artist);
                }
                r.seek(SeekFrom::Start(pos + len + (len & 1)))?;
            }
            b"data" => {
                let (tag, channels, rate, bits) =
                    fmt.ok_or(WavError::Malformed("data before fmt"))?;
                let format = match (tag, bits) {
                    (FORMAT_PCM, 8) => SampleFormat::U8,
                    (FORMAT_PCM, 16) => SampleFormat::I16,
                    (FORMAT_PCM, 24) => SampleFormat::I24,
                    (FORMAT_PCM, 32) => SampleFormat::I32,
                    (FORMAT_FLOAT, 32) => SampleFormat::F32,
                    (FORMAT_FLOAT, 64) => SampleFormat::F64,
                    (FORMAT_PCM | FORMAT_FLOAT, b) => {
                        return Err(WavError::Unsupported(format!("{b}-bit samples")));
                    }
                    (t, _) => return Err(WavError::Unsupported(format!("format tag {t:#06x}"))),
                };
                if channels == 0 {
                    return Err(WavError::Malformed("zero channels"));
                }
                if rate == 0 {
                    return Err(WavError::Malformed("zero sample rate"));
                }
                let frame = (format.bytes() * usize::from(channels)) as u64;
                let there = file_len.saturating_sub(pos);
                let data_len = len.min(there) / frame * frame;
                return Ok(WavInfo {
                    rate,
                    channels,
                    format,
                    data_offset: pos,
                    data_len,
                    title,
                    artist,
                });
            }
            _ => {
                r.seek(SeekFrom::Start(pos + len + (len & 1)))?;
            }
        }
        pos += len + (len & 1);
        if pos >= file_len {
            return Err(WavError::Malformed("no data chunk"));
        }
    }
}

/// Pull `INAM` and `IART` out of a `LIST/INFO` body.
fn read_info(mut b: &[u8], title: &mut Option<String>, artist: &mut Option<String>) {
    while b.len() >= 8 {
        let id = [b[0], b[1], b[2], b[3]];
        let len = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let body = &b[8..];
        let len = len.min(body.len());
        let text = String::from_utf8_lossy(&body[..len])
            .trim_end_matches('\0')
            .trim()
            .to_owned();
        if !text.is_empty() {
            match &id {
                b"INAM" => *title = Some(text),
                b"IART" => *artist = Some(text),
                _ => {}
            }
        }
        let step = len + (len & 1);
        if step >= body.len() {
            break;
        }
        b = &body[step..];
    }
}

/// Convert whole frames of `bytes` into interleaved **stereo** `f32`.
///
/// Mono is duplicated to both sides; more than two channels keep the
/// first two, which by the WAV channel-mask convention are front left and
/// front right. A trailing partial frame is ignored. Appends to `out`
/// and returns the number of frames converted.
pub fn to_stereo(info: &WavInfo, bytes: &[u8], out: &mut Vec<f32>) -> usize {
    let fb = info.frame_bytes();
    let sb = info.format.bytes();
    let mut n = 0;
    for frame in bytes.chunks_exact(fb) {
        let l = info.format.read(frame);
        let r = if info.channels > 1 {
            info.format.read(&frame[sb..])
        } else {
            l
        };
        out.push(l);
        out.push(r);
        n += 1;
    }
    n
}

/// A 16-bit PCM WAV of `frames` of interleaved `samples`, as bytes.
///
/// The writer the tests and the docs use to make a file to play. It is
/// in the library rather than a test helper because the integration
/// tests in `tests/` are a separate crate and would otherwise need a
/// copy.
#[must_use]
pub fn encode_i16(rate: u32, channels: u16, samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&FORMAT_PCM.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    let block = u32::from(channels) * 2;
    out.extend_from_slice(&(rate * block).to_le_bytes());
    out.extend_from_slice(&(block as u16).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse_bytes(b: &[u8]) -> Result<WavInfo, WavError> {
        parse(&mut Cursor::new(b), b.len() as u64)
    }

    #[test]
    fn a_written_file_reads_back() {
        let samples = [0.0, 0.5, -0.5, 1.0];
        let bytes = encode_i16(8000, 2, &samples);
        let info = parse_bytes(&bytes).unwrap();
        assert_eq!(info.rate, 8000);
        assert_eq!(info.channels, 2);
        assert_eq!(info.format, SampleFormat::I16);
        assert_eq!(info.data_offset, 44);
        assert_eq!(info.frames(), 2);
        assert_eq!(info.kbps(), 256);
        let mut out = Vec::new();
        let body = &bytes[44..];
        assert_eq!(to_stereo(&info, body, &mut out), 2);
        for (got, want) in out.iter().zip(samples) {
            assert!((got - want).abs() < 1e-3, "{got} vs {want}");
        }
    }

    #[test]
    fn mono_is_duplicated_to_both_sides() {
        let bytes = encode_i16(8000, 1, &[0.25, -0.25]);
        let info = parse_bytes(&bytes).unwrap();
        let mut out = Vec::new();
        to_stereo(&info, &bytes[44..], &mut out);
        assert_eq!(out.len(), 4);
        assert!((out[0] - out[1]).abs() < f32::EPSILON);
        assert!(out[2] < 0.0);
    }

    #[test]
    fn a_data_chunk_longer_than_the_file_is_clamped() {
        let mut bytes = encode_i16(8000, 2, &[0.1; 8]);
        bytes[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes.truncate(bytes.len() - 3); // and a torn last frame
        let info = parse_bytes(&bytes).unwrap();
        assert_eq!(info.frames(), 3);
    }

    #[test]
    fn unknown_chunks_are_skipped_and_info_is_read() {
        let base = encode_i16(8000, 1, &[0.0; 4]);
        let mut info = Vec::new();
        info.extend_from_slice(b"INFO");
        for (id, text) in [(b"INAM", "Song\0"), (b"IART", "Band")] {
            info.extend_from_slice(id);
            info.extend_from_slice(&(text.len() as u32).to_le_bytes());
            info.extend_from_slice(text.as_bytes());
            if text.len() % 2 == 1 {
                info.push(0);
            }
        }
        let mut bytes = base[..36].to_vec();
        bytes.extend_from_slice(b"junk");
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 0]); // odd length, padded
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(&(info.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&info);
        bytes.extend_from_slice(&base[36..]);
        let got = parse_bytes(&bytes).unwrap();
        assert_eq!(got.title.as_deref(), Some("Song"));
        assert_eq!(got.artist.as_deref(), Some("Band"));
        assert_eq!(got.frames(), 4);
    }

    #[test]
    fn the_sample_formats_decode_to_the_same_scale() {
        assert!((SampleFormat::U8.read(&[255]) - 0.992).abs() < 0.01);
        assert!((SampleFormat::U8.read(&[0]) + 1.0).abs() < f32::EPSILON);
        let v = SampleFormat::I24.read(&[0x00, 0x00, 0x80]);
        assert!((v + 1.0).abs() < f32::EPSILON, "{v}");
        let v = SampleFormat::I24.read(&[0xff, 0xff, 0x7f]);
        assert!((v - 1.0).abs() < 1e-6, "{v}");
        assert!((SampleFormat::F32.read(&0.5f32.to_le_bytes()) - 0.5).abs() < f32::EPSILON);
        assert!(SampleFormat::F32.read(&f32::NAN.to_le_bytes()).abs() < f32::EPSILON);
        assert!((SampleFormat::F64.read(&(-0.25f64).to_le_bytes()) + 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn hostile_headers_are_errors_not_panics() {
        assert_eq!(parse_bytes(b""), Err(WavError::NotWav));
        assert_eq!(parse_bytes(b"RIFF\0\0\0\0AVI "), Err(WavError::NotWav));
        let mut bytes = encode_i16(8000, 1, &[0.0; 2]);
        bytes[22..24].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            parse_bytes(&bytes),
            Err(WavError::Malformed("zero channels"))
        );
        let mut bytes = encode_i16(8000, 1, &[0.0; 2]);
        bytes[20..22].copy_from_slice(&2u16.to_le_bytes()); // ADPCM
        assert!(matches!(parse_bytes(&bytes), Err(WavError::Unsupported(_))));
        // Truncated inside the fmt chunk.
        let bytes = encode_i16(8000, 1, &[0.0; 2]);
        assert!(parse_bytes(&bytes[..30]).is_err());
        // No data chunk at all.
        assert!(parse_bytes(&bytes[..36]).is_err());
    }
}

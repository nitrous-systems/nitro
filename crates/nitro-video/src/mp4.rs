//! A small, safe ISO-BMFF (MP4) demuxer for the first H.264 video track.
//!
//! [`parse`] walks only the containers it needs (`moov`, `trak`, `mdia`,
//! `minf`, `stbl`, `edts`) and flattens the sample tables into a
//! decode-order list of [`Sample`]s with absolute file offsets, so the
//! caller can slice access units straight out of the file bytes with
//! [`Track::sample_data`]. Every read is bounds-checked, every table count
//! is validated against the bytes actually present before allocating, and
//! all offset/timestamp arithmetic is checked: malformed input yields an
//! [`Mp4Error`], never a panic.
//!
//! Fragmented MP4 (`moof`/`mvex`) is rejected, as is any codec other than
//! H.264 (`avc1`/`avc3`).

use std::fmt;
use std::iter;
use std::path::Path;

/// Upper bound on the number of samples in a track; larger tables are
/// rejected before anything is allocated.
const MAX_SAMPLES: usize = 10_000_000;

/// One access unit of the video track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Absolute byte offset of the sample data in the file.
    pub offset: u64,
    /// Size of the sample data in bytes.
    pub size: u32,
    /// Decode timestamp, in [`Track::timescale`] units.
    pub dts: i64,
    /// Presentation timestamp, in [`Track::timescale`] units, with the
    /// first edit-list entry's `media_time` already subtracted.
    pub pts: i64,
    /// Whether this is a sync sample (a keyframe / IDR).
    pub sync: bool,
}

/// The demuxed H.264 video track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// Coded width from the `avc1`/`avc3` sample entry.
    pub width: u32,
    /// Coded height from the `avc1`/`avc3` sample entry.
    pub height: u32,
    /// Ticks per second of all timestamps (`mdhd` timescale, never 0).
    pub timescale: u32,
    /// Track duration in timescale units: `mdhd` duration, or the end of
    /// the last sample's decode time when `mdhd` reports 0.
    pub duration: u64,
    /// Raw `avcC` box payload (an `AVCDecoderConfigurationRecord`).
    pub avcc: Vec<u8>,
    /// `colr` box of colour type `nclx`: `(matrix_coefficients, full_range)`.
    pub color: Option<(u16, bool)>,
    /// All samples, in decode order.
    pub samples: Vec<Sample>,
}

/// Why an MP4 file could not be demuxed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mp4Error {
    /// The file could not be read.
    Io(String),
    /// A box or table ended before the named structure was complete.
    Truncated(String),
    /// A box header declares an impossible size.
    BadBox(String),
    /// A required box is absent (the fourcc is carried).
    MissingBox(String),
    /// The file is a fragmented MP4 (`moof` or `mvex` present).
    Fragmented,
    /// No track has a `vide` handler.
    NoVideoTrack,
    /// The video track's sample entry is not H.264 (the fourcc is carried).
    UnsupportedCodec(String),
    /// The track has more samples than this demuxer accepts.
    TooManySamples(u64),
    /// Any other structural inconsistency.
    Invalid(String),
}

impl fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "cannot read MP4 file: {msg}"),
            Self::Truncated(what) => write!(f, "MP4 file is truncated (in {what})"),
            Self::BadBox(msg) | Self::Invalid(msg) => write!(f, "malformed MP4: {msg}"),
            Self::MissingBox(fourcc) => write!(f, "malformed MP4: missing required '{fourcc}' box"),
            Self::Fragmented => f.write_str("fragmented MP4 is not supported in v1"),
            Self::NoVideoTrack => f.write_str("MP4 file has no video track"),
            Self::UnsupportedCodec(fourcc) => {
                write!(f, "{fourcc} not supported in v1 (H.264 only)")
            }
            Self::TooManySamples(n) => write!(
                f,
                "video track has {n} samples (at most {MAX_SAMPLES} are supported)"
            ),
        }
    }
}

impl std::error::Error for Mp4Error {}

impl Track {
    /// Index (in decode order) of the last sync sample whose pts is `<= pts`;
    /// 0 when there is none.
    pub fn keyframe_at_or_before(&self, pts: i64) -> usize {
        self.samples
            .iter()
            .rposition(|s| s.sync && s.pts <= pts)
            .unwrap_or(0)
    }

    /// Converts seconds to timescale ticks (rounded to nearest, saturating).
    pub fn secs_to_ticks(&self, secs: f64) -> i64 {
        (secs * f64::from(self.timescale)).round() as i64
    }

    /// Converts timescale ticks to seconds.
    pub fn ticks_to_secs(&self, t: i64) -> f64 {
        t as f64 / f64::from(self.timescale)
    }

    /// The track duration in seconds.
    pub fn duration_secs(&self) -> f64 {
        self.duration as f64 / f64::from(self.timescale)
    }

    /// The pts of `samples[from..]`, sorted ascending (presentation order).
    /// Empty when `from` is out of range.
    pub fn pts_sorted_from(&self, from: usize) -> Vec<i64> {
        let mut pts: Vec<i64> = self
            .samples
            .get(from..)
            .unwrap_or(&[])
            .iter()
            .map(|s| s.pts)
            .collect();
        pts.sort_unstable();
        pts
    }

    /// The bytes of sample `i` within `file` (the whole file's contents),
    /// or `None` when `i` is out of range or the sample lies outside `file`.
    pub fn sample_data<'a>(&self, file: &'a [u8], i: usize) -> Option<&'a [u8]> {
        let s = self.samples.get(i)?;
        let start = usize::try_from(s.offset).ok()?;
        let end = start.checked_add(usize::try_from(s.size).ok()?)?;
        file.get(start..end)
    }
}

/// Demuxes the file at `path`, reading **only its `moov` box**.
///
/// The top-level box headers are walked with positioned reads and the
/// sample data (`mdat`, usually almost all of the file) is never loaded:
/// a player reads each access unit when it needs it
/// ([`std::os::unix::fs::FileExt::read_at`] at [`Sample::offset`]), so
/// opening a 2 GB film costs its index, not 2 GB of memory.
///
/// # Errors
///
/// [`Mp4Error::Io`] when the file cannot be read, otherwise as [`parse`].
pub fn open(path: &Path) -> Result<Track, Mp4Error> {
    use std::os::unix::fs::FileExt as _;
    let io = |e: std::io::Error| Mp4Error::Io(format!("{}: {e}", path.display()));
    let file = std::fs::File::open(path).map_err(io)?;
    let len = file.metadata().map_err(io)?.len();
    let mut pos = 0u64;
    let mut moov: Option<Vec<u8>> = None;
    while len.saturating_sub(pos) >= 8 {
        let mut hdr = [0u8; 16];
        let want = usize::try_from((len - pos).min(16)).unwrap_or(16);
        file.read_exact_at(&mut hdr[..want], pos).map_err(io)?;
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let ty = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let size = match size32 {
            0 => len - pos,
            1 if want >= 16 => u64::from_be_bytes([
                hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
            ]),
            1 => return Err(Mp4Error::Truncated("box header".to_owned())),
            n => u64::from(n),
        };
        if size < 8 {
            return Err(Mp4Error::BadBox(format!(
                "'{}' box declares {size} bytes",
                fourcc_str(ty)
            )));
        }
        match &ty {
            b"moof" => return Err(Mp4Error::Fragmented),
            b"moov" if moov.is_none() => {
                let end = pos.checked_add(size).filter(|&e| e <= len).ok_or_else(|| {
                    Mp4Error::Truncated("moov".to_owned())
                })?;
                let n = usize::try_from(end - pos)
                    .map_err(|_| Mp4Error::Invalid("moov box too large".to_owned()))?;
                let mut buf = vec![0u8; n];
                file.read_exact_at(&mut buf, pos).map_err(io)?;
                moov = Some(buf);
            }
            _ => {}
        }
        match pos.checked_add(size) {
            Some(next) => pos = next,
            None => break,
        }
    }
    let moov = moov.ok_or_else(|| Mp4Error::MissingBox("moov".to_owned()))?;
    // The buffer holds just the `moov` box; sample offsets come from
    // `stco`/`co64` and are file offsets regardless.
    parse(&moov)
}

/// Demuxes the first H.264 video track of a whole MP4 file held in memory.
///
/// # Errors
///
/// Returns an [`Mp4Error`] for fragmented files, files without a video
/// track, non-H.264 video, missing required boxes, and any truncated or
/// inconsistent structure.
pub fn parse(data: &[u8]) -> Result<Track, Mp4Error> {
    let mut moov = None;
    for item in Boxes::new(data) {
        match item {
            Ok((ty, payload)) => match &ty {
                b"moof" => return Err(Mp4Error::Fragmented),
                b"moov" if moov.is_none() => moov = Some(payload),
                _ => {}
            },
            // A damaged tail (e.g. a cut-off `mdat`) after a complete `moov`
            // is tolerated; sample_data bounds-checks against the real file.
            Err(_) if moov.is_some() => break,
            Err(e) => return Err(e),
        }
    }
    let moov = moov.ok_or_else(|| Mp4Error::MissingBox("moov".to_owned()))?;
    let mut video = None;
    for item in Boxes::new(moov) {
        let (ty, payload) = item?;
        match &ty {
            b"mvex" => return Err(Mp4Error::Fragmented),
            b"trak" if video.is_none() => video = parse_trak(payload)?,
            _ => {}
        }
    }
    video.ok_or(Mp4Error::NoVideoTrack)
}

/// Renders a fourcc for messages, replacing non-printable bytes with `?`.
fn fourcc_str(ty: [u8; 4]) -> String {
    ty.iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '?'
            }
        })
        .collect()
}

/// A bounds-checked big-endian cursor over one box's bytes.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], what: &'static str) -> Self {
        Self { data, pos: 0, what }
    }

    fn truncated(&self) -> Mp4Error {
        Mp4Error::Truncated(self.what.to_owned())
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Mp4Error> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.data.len())
            .ok_or_else(|| self.truncated())?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn skip(&mut self, n: usize) -> Result<(), Mp4Error> {
        self.bytes(n).map(|_| ())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Mp4Error> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Mp4Error> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, Mp4Error> {
        self.array().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Result<u32, Mp4Error> {
        self.array().map(u32::from_be_bytes)
    }

    fn i32(&mut self) -> Result<i32, Mp4Error> {
        self.array().map(i32::from_be_bytes)
    }

    fn u64(&mut self) -> Result<u64, Mp4Error> {
        self.array().map(u64::from_be_bytes)
    }

    fn i64(&mut self) -> Result<i64, Mp4Error> {
        self.array().map(i64::from_be_bytes)
    }

    /// Reads a table entry count and checks that `count * entry_size`
    /// bytes are actually present, so the caller may allocate for it.
    fn count(&mut self, entry_size: usize) -> Result<usize, Mp4Error> {
        let count = self.u32()? as usize;
        match count.checked_mul(entry_size) {
            Some(bytes) if bytes <= self.remaining() => Ok(count),
            _ => Err(Mp4Error::Invalid(format!(
                "'{}' declares {count} entries but only {} bytes follow",
                self.what,
                self.remaining()
            ))),
        }
    }
}

/// Opens a full box: returns its version and a reader past the
/// version/flags word.
fn full_box<'a>(data: &'a [u8], what: &'static str) -> Result<(u8, Reader<'a>), Mp4Error> {
    let mut r = Reader::new(data, what);
    let version = r.u8()?;
    r.skip(3)?;
    Ok((version, r))
}

/// Iterates the child boxes of a container payload, yielding
/// `(fourcc, payload)`. Stops after the first error.
struct Boxes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Boxes<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = Result<([u8; 4], &'a [u8]), Mp4Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.data.get(self.pos..)?;
        // Fewer bytes than a box header: trailing padding (some writers emit
        // a 4-byte zero terminator), not a box.
        if rest.len() < 8 {
            self.pos = self.data.len();
            return None;
        }
        match parse_box(rest) {
            Ok((ty, payload, total)) => {
                self.pos += total;
                Some(Ok((ty, payload)))
            }
            Err(e) => {
                self.pos = self.data.len();
                Some(Err(e))
            }
        }
    }
}

/// Parses the box at the start of `data` (the rest of its container):
/// returns its fourcc, payload and total size.
fn parse_box(data: &[u8]) -> Result<([u8; 4], &[u8], usize), Mp4Error> {
    let mut r = Reader::new(data, "box header");
    let size32 = r.u32()?;
    let ty = r.array::<4>()?;
    let size = match size32 {
        0 => data.len() as u64,
        1 => r.u64()?,
        s => u64::from(s),
    };
    let header = r.pos;
    let name = fourcc_str(ty);
    if size < header as u64 {
        return Err(Mp4Error::BadBox(format!(
            "'{name}' box size {size} is smaller than its {header}-byte header"
        )));
    }
    let size = usize::try_from(size)
        .ok()
        .filter(|&s| s <= data.len())
        .ok_or_else(|| {
            Mp4Error::BadBox(format!(
                "'{name}' box size {size} overruns its container ({} bytes left)",
                data.len()
            ))
        })?;
    Ok((ty, &data[header..size], size))
}

/// The first child box of type `ty`, if any.
fn find(data: &[u8], ty: [u8; 4]) -> Result<Option<&[u8]>, Mp4Error> {
    for item in Boxes::new(data) {
        let (t, payload) = item?;
        if t == ty {
            return Ok(Some(payload));
        }
    }
    Ok(None)
}

/// The first child box of type `ty`, or [`Mp4Error::MissingBox`].
fn require(data: &[u8], ty: [u8; 4]) -> Result<&[u8], Mp4Error> {
    find(data, ty)?.ok_or_else(|| Mp4Error::MissingBox(fourcc_str(ty)))
}

/// Parses one `trak`; `Ok(None)` when it is not a video track.
fn parse_trak(trak: &[u8]) -> Result<Option<Track>, Mp4Error> {
    let Some(mdia) = find(trak, *b"mdia")? else {
        return Ok(None);
    };
    let Some(hdlr) = find(mdia, *b"hdlr")? else {
        return Ok(None);
    };
    let (_, mut r) = full_box(hdlr, "hdlr")?;
    r.skip(4)?; // pre_defined
    if &r.array::<4>()? != b"vide" {
        return Ok(None);
    }
    let (timescale, mdhd_duration) = parse_mdhd(require(mdia, *b"mdhd")?)?;
    let media_time = match find(trak, *b"edts")? {
        Some(edts) => match find(edts, *b"elst")? {
            Some(elst) => parse_elst(elst)?,
            None => 0,
        },
        None => 0,
    };
    let stbl = require(require(mdia, *b"minf")?, *b"stbl")?;
    let entry = parse_stsd(require(stbl, *b"stsd")?)?;
    let (samples, end_dts) = build_samples(stbl, media_time)?;
    let duration = if mdhd_duration == 0 {
        u64::try_from(end_dts).unwrap_or(0)
    } else {
        mdhd_duration
    };
    Ok(Some(Track {
        width: entry.width,
        height: entry.height,
        timescale,
        duration,
        avcc: entry.avcc,
        color: entry.color,
        samples,
    }))
}

/// `mdhd` → `(timescale, duration)`; an "unknown" all-ones duration is 0.
fn parse_mdhd(data: &[u8]) -> Result<(u32, u64), Mp4Error> {
    let (version, mut r) = full_box(data, "mdhd")?;
    let (timescale, duration) = if version == 1 {
        r.skip(16)?;
        let ts = r.u32()?;
        (ts, r.u64()?)
    } else {
        r.skip(8)?;
        let ts = r.u32()?;
        let d = r.u32()?;
        (ts, if d == u32::MAX { 0 } else { u64::from(d) })
    };
    if timescale == 0 {
        return Err(Mp4Error::Invalid("mdhd timescale is 0".to_owned()));
    }
    Ok((timescale, if duration == u64::MAX { 0 } else { duration }))
}

/// The first `elst` entry's `media_time`, or 0 for an empty edit (-1),
/// any other negative value, or no entries.
fn parse_elst(data: &[u8]) -> Result<i64, Mp4Error> {
    let (version, mut r) = full_box(data, "elst")?;
    let entry_size = if version == 1 { 20 } else { 12 };
    if r.count(entry_size)? == 0 {
        return Ok(0);
    }
    let media_time = if version == 1 {
        r.skip(8)?;
        r.i64()?
    } else {
        r.skip(4)?;
        i64::from(r.i32()?)
    };
    Ok(media_time.max(0))
}

/// What the `avc1`/`avc3` sample entry contributes to the [`Track`].
struct SampleEntry {
    width: u32,
    height: u32,
    avcc: Vec<u8>,
    color: Option<(u16, bool)>,
}

/// Parses the first `stsd` entry, which must be `avc1` or `avc3`.
fn parse_stsd(data: &[u8]) -> Result<SampleEntry, Mp4Error> {
    let (_, mut r) = full_box(data, "stsd")?;
    let no_entries = || Mp4Error::Invalid("stsd has no sample entries".to_owned());
    if r.u32()? == 0 {
        return Err(no_entries());
    }
    let (fourcc, entry) = Boxes::new(r.rest()).next().ok_or_else(no_entries)??;
    if &fourcc != b"avc1" && &fourcc != b"avc3" {
        return Err(Mp4Error::UnsupportedCodec(fourcc_str(fourcc)));
    }
    // VisualSampleEntry: 6 reserved + data_reference_index + 16 bytes of
    // pre_defined/reserved, then width/height, then 50 more fixed bytes.
    let mut r = Reader::new(entry, "avc1 sample entry");
    r.skip(24)?;
    let width = u32::from(r.u16()?);
    let height = u32::from(r.u16()?);
    r.skip(50)?;
    let mut avcc = None;
    let mut color = None;
    for item in Boxes::new(r.rest()) {
        let (ty, payload) = item?;
        match &ty {
            b"avcC" if avcc.is_none() => avcc = Some(payload.to_vec()),
            b"colr" if color.is_none() => color = parse_colr(payload),
            _ => {}
        }
    }
    Ok(SampleEntry {
        width,
        height,
        avcc: avcc.ok_or_else(|| Mp4Error::MissingBox("avcC".to_owned()))?,
        color,
    })
}

/// `colr` of type `nclx` → `(matrix_coefficients, full_range)`. Other
/// colour types and damaged boxes are ignored (the box is optional).
fn parse_colr(data: &[u8]) -> Option<(u16, bool)> {
    let mut r = Reader::new(data, "colr");
    if &r.array::<4>().ok()? != b"nclx" {
        return None;
    }
    r.skip(4).ok()?; // colour_primaries, transfer_characteristics
    let matrix = r.u16().ok()?;
    let full_range = r.u8().ok()? & 0x80 != 0;
    Some((matrix, full_range))
}

/// The raw sample tables of one `stbl`.
#[derive(Default)]
struct Tables<'a> {
    stts: Option<&'a [u8]>,
    ctts: Option<&'a [u8]>,
    stss: Option<&'a [u8]>,
    stsc: Option<&'a [u8]>,
    stsz: Option<&'a [u8]>,
    stz2: Option<&'a [u8]>,
    stco: Option<&'a [u8]>,
    co64: Option<&'a [u8]>,
}

impl<'a> Tables<'a> {
    fn collect(stbl: &'a [u8]) -> Result<Self, Mp4Error> {
        let mut t = Self::default();
        for item in Boxes::new(stbl) {
            let (ty, payload) = item?;
            let slot = match &ty {
                b"stts" => &mut t.stts,
                b"ctts" => &mut t.ctts,
                b"stss" => &mut t.stss,
                b"stsc" => &mut t.stsc,
                b"stsz" => &mut t.stsz,
                b"stz2" => &mut t.stz2,
                b"stco" => &mut t.stco,
                b"co64" => &mut t.co64,
                _ => continue,
            };
            slot.get_or_insert(payload);
        }
        Ok(t)
    }
}

/// Flattens the sample tables into decode-order samples; also returns the
/// decode time just past the last sample.
fn build_samples(stbl: &[u8], media_time: i64) -> Result<(Vec<Sample>, i64), Mp4Error> {
    let t = Tables::collect(stbl)?;
    let sizes = match (t.stsz, t.stz2) {
        (Some(stsz), _) => parse_stsz(stsz)?,
        (None, Some(stz2)) => parse_stz2(stz2)?,
        (None, None) => return Err(Mp4Error::MissingBox("stsz".to_owned())),
    };
    if sizes.is_empty() {
        return Err(Mp4Error::Invalid("video track has no samples".to_owned()));
    }
    let chunks = match (t.stco, t.co64) {
        (Some(stco), _) => parse_chunk_offsets(stco, false)?,
        (None, Some(co64)) => parse_chunk_offsets(co64, true)?,
        (None, None) => return Err(Mp4Error::MissingBox("stco".to_owned())),
    };
    let stsc = parse_stsc(
        t.stsc
            .ok_or_else(|| Mp4Error::MissingBox("stsc".to_owned()))?,
    )?;
    let offsets = sample_offsets(&stsc, &chunks, &sizes)?;
    let stts = parse_stts(
        t.stts
            .ok_or_else(|| Mp4Error::MissingBox("stts".to_owned()))?,
    )?;
    let ctts_runs = t.ctts.map(parse_ctts).transpose()?.unwrap_or_default();
    let mut sync = vec![t.stss.is_none(); sizes.len()];
    if let Some(stss) = t.stss {
        for number in parse_stss(stss)? {
            if let Some(flag) = (number as usize)
                .checked_sub(1)
                .and_then(|i| sync.get_mut(i))
            {
                *flag = true;
            }
        }
    }

    // Samples beyond the stts/ctts coverage repeat the last delta / get
    // no composition offset.
    let last_delta = stts.last().map_or(0, |&(_, d)| d);
    let deltas = stts
        .iter()
        .flat_map(|&(n, d)| iter::repeat_n(d, n as usize))
        .chain(iter::repeat(last_delta));
    let cts = ctts_runs
        .iter()
        .flat_map(|&(n, o)| iter::repeat_n(o, n as usize))
        .chain(iter::repeat(0));
    let overflow = || Mp4Error::Invalid("timestamp overflow".to_owned());
    let mut samples = Vec::with_capacity(sizes.len());
    let mut dts = 0i64;
    for ((((&size, &offset), &sync), delta), cto) in
        sizes.iter().zip(&offsets).zip(&sync).zip(deltas).zip(cts)
    {
        let pts = dts
            .checked_add(cto)
            .and_then(|p| p.checked_sub(media_time))
            .ok_or_else(overflow)?;
        samples.push(Sample {
            offset,
            size,
            dts,
            pts,
            sync,
        });
        dts = dts.checked_add(i64::from(delta)).ok_or_else(overflow)?;
    }
    Ok((samples, dts))
}

/// Rejects sample counts above [`MAX_SAMPLES`].
fn check_sample_count(count: u32) -> Result<usize, Mp4Error> {
    let n = count as usize;
    if n > MAX_SAMPLES {
        return Err(Mp4Error::TooManySamples(u64::from(count)));
    }
    Ok(n)
}

fn parse_stsz(data: &[u8]) -> Result<Vec<u32>, Mp4Error> {
    let (_, mut r) = full_box(data, "stsz")?;
    let constant = r.u32()?;
    if constant != 0 {
        let n = check_sample_count(r.u32()?)?;
        return Ok(vec![constant; n]);
    }
    let n = r.count(4)?;
    check_sample_count(n as u32)?;
    (0..n).map(|_| r.u32()).collect()
}

fn parse_stz2(data: &[u8]) -> Result<Vec<u32>, Mp4Error> {
    let (_, mut r) = full_box(data, "stz2")?;
    r.skip(3)?; // reserved
    let field_size = r.u8()?;
    let count = r.u32()?;
    let n = check_sample_count(count)?;
    let bits = match field_size {
        4 | 8 | 16 => usize::from(field_size),
        other => {
            return Err(Mp4Error::Invalid(format!(
                "stz2 field size {other} (must be 4, 8 or 16)"
            )));
        }
    };
    let bytes = r.bytes((n * bits).div_ceil(8))?;
    Ok((0..n)
        .map(|i| match bits {
            4 => {
                let b = bytes[i / 2];
                u32::from(if i % 2 == 0 { b >> 4 } else { b & 0x0f })
            }
            8 => u32::from(bytes[i]),
            _ => u32::from(u16::from_be_bytes([bytes[2 * i], bytes[2 * i + 1]])),
        })
        .collect())
}

fn parse_chunk_offsets(data: &[u8], wide: bool) -> Result<Vec<u64>, Mp4Error> {
    let (_, mut r) = full_box(data, if wide { "co64" } else { "stco" })?;
    let n = r.count(if wide { 8 } else { 4 })?;
    (0..n)
        .map(|_| {
            if wide {
                r.u64()
            } else {
                r.u32().map(u64::from)
            }
        })
        .collect()
}

/// `stsc` → `(first_chunk, samples_per_chunk)` runs.
fn parse_stsc(data: &[u8]) -> Result<Vec<(u32, u32)>, Mp4Error> {
    let (_, mut r) = full_box(data, "stsc")?;
    let n = r.count(12)?;
    (0..n)
        .map(|_| {
            let first = r.u32()?;
            let per_chunk = r.u32()?;
            r.skip(4)?; // sample_description_index
            Ok((first, per_chunk))
        })
        .collect()
}

/// `stts` → `(sample_count, sample_delta)` runs.
fn parse_stts(data: &[u8]) -> Result<Vec<(u32, u32)>, Mp4Error> {
    let (_, mut r) = full_box(data, "stts")?;
    let n = r.count(8)?;
    (0..n).map(|_| Ok((r.u32()?, r.u32()?))).collect()
}

/// `ctts` → `(sample_count, composition_offset)` runs; version 0 offsets
/// are unsigned, version 1 signed.
fn parse_ctts(data: &[u8]) -> Result<Vec<(u32, i64)>, Mp4Error> {
    let (version, mut r) = full_box(data, "ctts")?;
    let n = r.count(8)?;
    (0..n)
        .map(|_| {
            let count = r.u32()?;
            let offset = if version == 0 {
                i64::from(r.u32()?)
            } else {
                i64::from(r.i32()?)
            };
            Ok((count, offset))
        })
        .collect()
}

/// `stss` → 1-based sync sample numbers.
fn parse_stss(data: &[u8]) -> Result<Vec<u32>, Mp4Error> {
    let (_, mut r) = full_box(data, "stss")?;
    let n = r.count(4)?;
    (0..n).map(|_| r.u32()).collect()
}

/// Absolute file offset of every sample, from the `stsc` chunk runs, the
/// chunk offsets and the sample sizes.
fn sample_offsets(
    stsc: &[(u32, u32)],
    chunks: &[u64],
    sizes: &[u32],
) -> Result<Vec<u64>, Mp4Error> {
    let mut offsets = Vec::with_capacity(sizes.len());
    for (i, &(first, per_chunk)) in stsc.iter().enumerate() {
        let start = (first as usize)
            .checked_sub(1)
            .ok_or_else(|| Mp4Error::Invalid("stsc first_chunk is 0".to_owned()))?;
        let end = match stsc.get(i + 1) {
            Some(&(next, _)) if next <= first => {
                return Err(Mp4Error::Invalid(
                    "stsc first_chunk values are not increasing".to_owned(),
                ));
            }
            Some(&(next, _)) => next as usize - 1,
            None => chunks.len(),
        };
        for &chunk_offset in chunks.get(start..end.min(chunks.len())).unwrap_or(&[]) {
            let mut offset = chunk_offset;
            for _ in 0..per_chunk {
                let Some(&size) = sizes.get(offsets.len()) else {
                    return Ok(offsets);
                };
                offsets.push(offset);
                offset = offset
                    .checked_add(u64::from(size))
                    .ok_or_else(|| Mp4Error::Invalid("sample offset overflow".to_owned()))?;
            }
        }
    }
    if offsets.len() < sizes.len() {
        return Err(Mp4Error::Invalid(format!(
            "chunk tables place only {} of {} samples",
            offsets.len(),
            sizes.len()
        )));
    }
    Ok(offsets)
}

#[cfg(test)]
#[allow(
    clippy::trivially_copy_pass_by_ref,
    clippy::struct_excessive_bools,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;

    const DELTA: u32 = 512;
    const TIMESCALE: u32 = 12_800;
    const AVCC: [u8; 15] = [
        1, 100, 0, 31, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0x00, 0x1f, 1, 0, 0,
    ];

    fn bx(ty: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(payload);
        v
    }

    fn full(ty: &[u8; 4], version: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![version, 0, 0, 0];
        p.extend_from_slice(payload);
        bx(ty, &p)
    }

    fn be32(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    #[derive(Clone)]
    struct Opts {
        sizes: Vec<u32>,
        /// Samples per chunk; chunks are separated by 3 padding bytes.
        chunks: Vec<u32>,
        ctts: Option<(u8, Vec<i32>)>,
        stss: Option<Vec<u32>>,
        co64: bool,
        stz2: Option<u8>,
        elst: Option<(u8, i64)>,
        codec: [u8; 4],
        colr: Option<(u16, bool)>,
        handler: [u8; 4],
        audio_first: bool,
        moof: bool,
        mvex: bool,
        mdhd_v1: bool,
        duration: u32,
        large_moov: bool,
    }

    impl Default for Opts {
        fn default() -> Self {
            Self {
                sizes: vec![10, 20, 30, 40, 50],
                chunks: vec![5],
                ctts: None,
                stss: None,
                co64: false,
                stz2: None,
                elst: None,
                codec: *b"avc1",
                colr: None,
                handler: *b"vide",
                audio_first: false,
                moof: false,
                mvex: false,
                mdhd_v1: false,
                duration: 5 * DELTA,
                large_moov: false,
            }
        }
    }

    fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
        let mut p = vec![0; 4];
        p.extend_from_slice(handler);
        p.extend_from_slice(&[0; 12]);
        p.extend_from_slice(b"Handler\0");
        full(b"hdlr", 0, &p)
    }

    fn stbl(o: &Opts, chunk_offsets: &[u64]) -> Vec<u8> {
        let n = o.sizes.len() as u32;
        let mut entry = vec![0u8; 6];
        entry.extend_from_slice(&1u16.to_be_bytes());
        entry.extend_from_slice(&[0; 16]);
        entry.extend_from_slice(&641u16.to_be_bytes());
        entry.extend_from_slice(&361u16.to_be_bytes());
        entry.extend_from_slice(&[0; 50]);
        entry.extend(bx(b"avcC", &AVCC));
        if let Some((matrix, full_range)) = o.colr {
            let mut c = b"nclx".to_vec();
            c.extend_from_slice(&[0, 1, 0, 1]);
            c.extend_from_slice(&matrix.to_be_bytes());
            c.push(if full_range { 0x80 } else { 0 });
            entry.extend(bx(b"colr", &c));
        }
        let mut out = full(b"stsd", 0, &[be32(&[1]), bx(&o.codec, &entry)].concat());
        out.extend(full(b"stts", 0, &be32(&[1, n, DELTA])));
        if let Some((version, offsets)) = &o.ctts {
            let mut p = be32(&[offsets.len() as u32]);
            for &off in offsets {
                p.extend(be32(&[1]));
                p.extend_from_slice(&off.to_be_bytes());
            }
            out.extend(full(b"ctts", *version, &p));
        }
        if let Some(stss) = &o.stss {
            out.extend(full(
                b"stss",
                0,
                &[be32(&[stss.len() as u32]), be32(stss)].concat(),
            ));
        }
        // stsc: compress consecutive equal samples-per-chunk into runs.
        let mut runs = Vec::new();
        for (i, &spc) in o.chunks.iter().enumerate() {
            if runs.last().is_none_or(|&(_, last)| last != spc) {
                runs.push((i as u32 + 1, spc));
            }
        }
        let mut p = be32(&[runs.len() as u32]);
        for (first, spc) in runs {
            p.extend(be32(&[first, spc, 1]));
        }
        out.extend(full(b"stsc", 0, &p));
        match o.stz2 {
            None => out.extend(full(b"stsz", 0, &[be32(&[0, n]), be32(&o.sizes)].concat())),
            Some(bits) => {
                let mut p = vec![0, 0, 0, bits];
                p.extend(be32(&[n]));
                match bits {
                    4 => {
                        for pair in o.sizes.chunks(2) {
                            let lo = pair.get(1).copied().unwrap_or(0);
                            p.push(((pair[0] << 4) | lo) as u8);
                        }
                    }
                    8 => p.extend(o.sizes.iter().map(|&s| s as u8)),
                    _ => p.extend(o.sizes.iter().flat_map(|&s| (s as u16).to_be_bytes())),
                }
                out.extend(full(b"stz2", 0, &p));
            }
        }
        let count = be32(&[chunk_offsets.len() as u32]);
        if o.co64 {
            let offs: Vec<u8> = chunk_offsets.iter().flat_map(|v| v.to_be_bytes()).collect();
            out.extend(full(b"co64", 0, &[count, offs].concat()));
        } else {
            let offs: Vec<u32> = chunk_offsets.iter().map(|&v| v as u32).collect();
            out.extend(full(b"stco", 0, &[count, be32(&offs)].concat()));
        }
        bx(b"stbl", &out)
    }

    fn moov(o: &Opts, chunk_offsets: &[u64]) -> Vec<u8> {
        let mdhd = if o.mdhd_v1 {
            let mut p = vec![0; 16];
            p.extend(be32(&[TIMESCALE]));
            p.extend_from_slice(&u64::from(o.duration).to_be_bytes());
            p.extend_from_slice(&[0; 4]);
            full(b"mdhd", 1, &p)
        } else {
            full(b"mdhd", 0, &be32(&[0, 0, TIMESCALE, o.duration, 0]))
        };
        let minf = bx(
            b"minf",
            &[bx(b"dinf", &[]), stbl(o, chunk_offsets)].concat(),
        );
        let mdia = bx(b"mdia", &[mdhd, hdlr(&o.handler), minf].concat());
        let mut trak = full(b"tkhd", 0, &[0; 80]);
        if let Some((version, media_time)) = o.elst {
            let p = if version == 1 {
                let mut p = be32(&[1, 0, 0]);
                p.extend_from_slice(&media_time.to_be_bytes());
                p.extend(be32(&[0x1_0000]));
                p
            } else {
                be32(&[1, 0, media_time as u32, 0x1_0000])
            };
            trak.extend(bx(b"edts", &full(b"elst", version, &p)));
        }
        trak.extend(mdia);
        let mut body = full(b"mvhd", 0, &[0; 96]);
        if o.audio_first {
            let audio = bx(b"mdia", &hdlr(b"soun"));
            body.extend(bx(b"trak", &audio));
        }
        body.extend(bx(b"trak", &trak));
        if o.mvex {
            body.extend(bx(b"mvex", &[]));
        }
        if o.large_moov {
            let mut v = be32(&[1]);
            v.extend_from_slice(b"moov");
            v.extend_from_slice(&(body.len() as u64 + 16).to_be_bytes());
            v.extend(body);
            v
        } else {
            bx(b"moov", &body)
        }
    }

    /// Sample `i`'s bytes are all `i + 1`.
    fn build(o: &Opts) -> Vec<u8> {
        let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomavc1");
        let mut mdat = Vec::new();
        let mut rel_offsets = Vec::new();
        let mut sample = 0usize;
        for &spc in &o.chunks {
            mdat.extend_from_slice(&[0xee; 3]);
            rel_offsets.push(mdat.len() as u64);
            for _ in 0..spc {
                let size = o.sizes[sample] as usize;
                mdat.extend(iter::repeat_n(sample as u8 + 1, size));
                sample += 1;
            }
        }
        let placeholder = vec![0u64; rel_offsets.len()];
        let base = (ftyp.len() + moov(o, &placeholder).len() + 8) as u64;
        let abs: Vec<u64> = rel_offsets.iter().map(|r| r + base).collect();
        let mut file = ftyp;
        file.extend(moov(o, &abs));
        file.extend(bx(b"mdat", &mdat));
        if o.moof {
            file.extend(bx(b"moof", &[]));
        }
        file
    }

    fn check_data(o: &Opts, file: &[u8], track: &Track) {
        assert_eq!(track.samples.len(), o.sizes.len());
        for (i, s) in track.samples.iter().enumerate() {
            assert_eq!(s.size, o.sizes[i]);
            let data = track.sample_data(file, i).unwrap();
            assert_eq!(data.len(), o.sizes[i] as usize);
            assert!(data.iter().all(|&b| b == i as u8 + 1), "sample {i}");
        }
        assert!(track.sample_data(file, o.sizes.len()).is_none());
    }

    #[test]
    fn basic_track() {
        let o = Opts::default();
        let file = build(&o);
        let t = parse(&file).unwrap();
        assert_eq!((t.width, t.height), (641, 361));
        assert_eq!(t.timescale, TIMESCALE);
        assert_eq!(t.duration, 5 * u64::from(DELTA));
        assert!((t.duration_secs() - 0.2).abs() < 1e-9);
        assert_eq!(t.avcc, AVCC);
        assert_eq!(t.color, None);
        check_data(&o, &file, &t);
        for (i, s) in t.samples.iter().enumerate() {
            assert_eq!(s.dts, i as i64 * 512);
            assert_eq!(s.pts, s.dts);
            assert!(s.sync);
        }
        // contiguous within the single chunk
        assert_eq!(t.samples[1].offset, t.samples[0].offset + 10);
        assert_eq!(t.keyframe_at_or_before(1100), 2);
    }

    #[test]
    fn ctts_v0_and_v1() {
        let o = Opts {
            ctts: Some((0, vec![1024, 2560, 512, 1024, 1024])),
            ..Opts::default()
        };
        let t = parse(&build(&o)).unwrap();
        let pts: Vec<i64> = t.samples.iter().map(|s| s.pts).collect();
        assert_eq!(pts, vec![1024, 3072, 1536, 2560, 3072]);
        assert_eq!(t.pts_sorted_from(1), vec![1536, 2560, 3072, 3072]);
        assert!(t.pts_sorted_from(99).is_empty());

        let o = Opts {
            ctts: Some((1, vec![0, 1024, -512, -512, 0])),
            ..Opts::default()
        };
        let t = parse(&build(&o)).unwrap();
        let pts: Vec<i64> = t.samples.iter().map(|s| s.pts).collect();
        assert_eq!(pts, vec![0, 1536, 512, 1024, 2048]);
    }

    #[test]
    fn stss_and_keyframes() {
        let o = Opts {
            stss: Some(vec![1, 4, 99]),
            ..Opts::default()
        };
        let t = parse(&build(&o)).unwrap();
        let sync: Vec<bool> = t.samples.iter().map(|s| s.sync).collect();
        assert_eq!(sync, vec![true, false, false, true, false]);
        assert_eq!(t.keyframe_at_or_before(-5), 0);
        assert_eq!(t.keyframe_at_or_before(0), 0);
        assert_eq!(t.keyframe_at_or_before(1535), 0);
        assert_eq!(t.keyframe_at_or_before(1536), 3);
        assert_eq!(t.keyframe_at_or_before(1_000_000), 3);
    }

    #[test]
    fn co64_and_multiple_chunks() {
        for co64 in [false, true] {
            let o = Opts {
                sizes: vec![10, 20, 30, 40, 50, 60, 70],
                chunks: vec![2, 2, 1, 1, 1],
                co64,
                duration: 0,
                ..Opts::default()
            };
            let file = build(&o);
            let t = parse(&file).unwrap();
            check_data(&o, &file, &t);
            // padding between chunks: 2nd chunk starts 3 bytes after the 1st ends
            assert_eq!(t.samples[2].offset, t.samples[1].offset + 20 + 3);
            assert_eq!(t.samples[1].offset, t.samples[0].offset + 10);
            // duration derived from samples when mdhd says 0
            assert_eq!(t.duration, 7 * u64::from(DELTA));
        }
    }

    #[test]
    fn stz2_field_sizes() {
        for (bits, sizes) in [
            (4u8, vec![1, 15, 7, 3, 9]),
            (8, vec![200, 1, 255, 17, 3]),
            (16, vec![1000, 65_535, 3, 400, 12]),
        ] {
            let o = Opts {
                sizes,
                stz2: Some(bits),
                ..Opts::default()
            };
            let file = build(&o);
            let t = parse(&file).unwrap();
            check_data(&o, &file, &t);
        }
    }

    #[test]
    fn edit_list_offset() {
        for version in [0u8, 1] {
            let o = Opts {
                ctts: Some((0, vec![1024, 2560, 512, 1024, 1024])),
                elst: Some((version, 1024)),
                ..Opts::default()
            };
            let t = parse(&build(&o)).unwrap();
            let pts: Vec<i64> = t.samples.iter().map(|s| s.pts).collect();
            assert_eq!(pts, vec![0, 2048, 512, 1536, 2048]);
            assert_eq!(t.samples[1].dts, 512);
        }
        let o = Opts {
            elst: Some((0, -1)),
            ..Opts::default()
        };
        let t = parse(&build(&o)).unwrap();
        assert_eq!(t.samples[0].pts, 0);
    }

    #[test]
    fn colr_nclx() {
        let o = Opts {
            colr: Some((1, true)),
            ..Opts::default()
        };
        assert_eq!(parse(&build(&o)).unwrap().color, Some((1, true)));
        let o = Opts {
            colr: Some((6, false)),
            ..Opts::default()
        };
        assert_eq!(parse(&build(&o)).unwrap().color, Some((6, false)));
    }

    #[test]
    fn mdhd_v1_large_moov_and_audio_track() {
        let o = Opts {
            mdhd_v1: true,
            large_moov: true,
            audio_first: true,
            ..Opts::default()
        };
        let file = build(&o);
        let t = parse(&file).unwrap();
        assert_eq!(t.duration, 5 * u64::from(DELTA));
        check_data(&o, &file, &t);
    }

    #[test]
    fn zero_size_box_extends_to_end() {
        let o = Opts::default();
        let mut file = build(&o);
        // mdat is the last box: 3 bytes of padding + 150 bytes of samples.
        let at = file.len() - 153 - 8;
        assert_eq!(&file[at + 4..at + 8], b"mdat");
        file[at..at + 4].copy_from_slice(&[0; 4]);
        let t = parse(&file).unwrap();
        check_data(&o, &file, &t);
    }

    #[test]
    fn rejects_hvc1() {
        let o = Opts {
            codec: *b"hvc1",
            ..Opts::default()
        };
        let err = parse(&build(&o)).unwrap_err();
        assert_eq!(err.to_string(), "hvc1 not supported in v1 (H.264 only)");
    }

    #[test]
    fn rejects_fragmented() {
        for o in [
            Opts {
                moof: true,
                ..Opts::default()
            },
            Opts {
                mvex: true,
                ..Opts::default()
            },
        ] {
            let err = parse(&build(&o)).unwrap_err();
            assert_eq!(err, Mp4Error::Fragmented);
            assert_eq!(err.to_string(), "fragmented MP4 is not supported in v1");
        }
    }

    #[test]
    fn no_video_track() {
        let o = Opts {
            handler: *b"soun",
            ..Opts::default()
        };
        assert_eq!(parse(&build(&o)).unwrap_err(), Mp4Error::NoVideoTrack);
        assert_eq!(
            parse(&bx(b"ftyp", b"isom")).unwrap_err(),
            Mp4Error::MissingBox("moov".to_owned())
        );
    }

    #[test]
    fn bad_box_sizes() {
        let mut file = bx(b"moov", &[]);
        file[3] = 4; // size 4 < 8-byte header
        assert!(matches!(parse(&file), Err(Mp4Error::BadBox(_))));
        let mut file = bx(b"moov", &[0; 8]);
        file[3] = 200; // overruns the file
        assert!(matches!(parse(&file), Err(Mp4Error::BadBox(_))));
    }

    #[test]
    fn huge_counts_rejected_before_allocating() {
        let stsz = full(b"stsz", 0, &be32(&[0, 0xffff_ffff]));
        assert!(parse_stsz(&stsz[8..]).is_err());
        let stsz = full(b"stsz", 0, &be32(&[7, 0xffff_ffff]));
        assert!(matches!(
            parse_stsz(&stsz[8..]),
            Err(Mp4Error::TooManySamples(_))
        ));
        let stco = full(b"stco", 0, &be32(&[3, 1, 2]));
        assert!(parse_chunk_offsets(&stco[8..], false).is_err());
    }

    #[test]
    fn time_conversions() {
        let t = parse(&build(&Opts::default())).unwrap();
        assert_eq!(t.secs_to_ticks(1.5), 19_200);
        assert!((t.ticks_to_secs(6400) - 0.5).abs() < 1e-12);
        assert_eq!(t.secs_to_ticks(t.ticks_to_secs(12_345)), 12_345);
    }

    #[test]
    fn truncation_and_mutation_never_panic() {
        let o = Opts {
            sizes: vec![10, 20, 30, 40, 50, 60, 70],
            chunks: vec![2, 2, 1, 1, 1],
            ctts: Some((1, vec![0, 1024, -512, -512, 0, 0, 0])),
            stss: Some(vec![1, 4]),
            elst: Some((0, 512)),
            colr: Some((1, false)),
            ..Opts::default()
        };
        let file = build(&o);
        assert!(parse(&file).is_ok());
        for n in 0..file.len() {
            let _ = parse(&file[..n]);
        }
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut data = file.clone();
            for _ in 0..=(next() % 4) {
                let i = (next() % data.len() as u64) as usize;
                data[i] = next() as u8;
            }
            if let Ok(t) = parse(&data) {
                for i in 0..t.samples.len() {
                    let _ = t.sample_data(&data, i);
                }
                let _ = t.keyframe_at_or_before(1000);
                let _ = t.pts_sorted_from(2);
            }
        }
    }
}

//! The ledger: one benchmark run per line, as JSON.
//!
//! # Why a file of lines and not a database
//!
//! A benchmark run is worth nothing on its own. The question is always
//! "is this build slower than the one from Tuesday", and answering it
//! needs the runs kept, not printed. The cheapest store that survives
//! `scp`, `git diff` and a human squinting at it is a `.jsonl` file:
//! append one self-describing object per run, never rewrite, never lock.
//! Two runs on two machines concatenate with `cat`.
//!
//! # Why the JSON is hand-written
//!
//! This workspace has a fixed budget of external crates and a rule
//! against growing it (see `DEPENDENCIES.md`). A serializer for a flat
//! object of scalars plus two string→number maps is about eighty lines,
//! and the parser that reads it back is about a hundred and fifty. That
//! is a smaller, more auditable cost than a derive macro and its two
//! transitive dependencies, and it is the same trade the rest of the tree
//! makes for the control socket's line protocol.
//!
//! # Forward compatibility
//!
//! [`Record::from_json`] **skips keys it does not know** and defaults the
//! ones it does not find. This is `nitro-demo`'s `parse_stats` rule —
//! "the key set is documented but the server is free to add to it, and a
//! demo that refuses to print anything because it met an unknown line
//! would be the most annoying possible failure mode" — applied to the
//! ledger, and it is what lets today's `nitro-bench report` read a file
//! that tomorrow's `nitro-bench run` wrote. A *malformed* line is still
//! an error, because that is a bug and not a schema change.
//!
//! # The headline ratio
//!
//! Every derived figure on [`Record`] is per *presented frame* or per
//! *second*, never a raw total, because totals are a function of how long
//! the run happened to last. And the one to quote is
//! [`Record::server_cpu_us_per_frame`]: a 60 Hz cap makes frames-per-second
//! stop discriminating the moment the client is fast enough, while CPU
//! burned per frame put on the glass keeps discriminating for three more
//! orders of magnitude.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;

/// One benchmark run: the raw counters, the environment, and nothing
/// derived.
///
/// Everything here is measured; every ratio a report prints is computed
/// from these on the fly. Storing the ratios instead would freeze the
/// definition of "per frame" into the file, and the whole point of the
/// ledger is that a *later* report tool gets to reinterpret an *earlier*
/// run.
///
/// `Default` is meaningful and load-bearing: a key absent from a line
/// takes the default rather than failing the parse, so a zero in any
/// count reads as "not applicable or not measured", which is why no
/// derived ratio is allowed to divide by one without checking.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Record {
    /// Scenario name, e.g. `rects`, `boing-node`, `plasma`.
    pub scenario: String,
    /// The `--n` sweep point (node count, star count); 0 = not applicable.
    pub n: u64,
    /// The `--size` sweep point (buffer edge in px); 0 = not applicable.
    pub size: u32,
    /// Window logical width, px.
    pub width: u32,
    /// Window logical height, px.
    pub height: u32,
    /// Output refresh in millihertz, as the server's `Frame` message
    /// reports it (60000, 120000). Millihertz because 59.94 Hz is a real
    /// mode and rounding it to 60 would quietly move the frame budget.
    pub refresh_mhz: u32,
    /// Measured wall duration of the run, seconds.
    pub seconds: f64,
    /// `Commit` messages sent.
    pub commits: u64,
    /// `Presented` messages received — the only count that means pixels
    /// actually reached the glass.
    pub presented: u64,
    /// `Frame` callbacks received.
    pub frames_received: u64,
    /// Wire mutation messages sent, excluding `Commit`: the measure of
    /// how chatty the scenario is per frame.
    pub mutations: u64,
    /// Bytes written to the socket.
    pub tx_bytes: u64,
    /// Bytes read from the socket.
    pub rx_bytes: u64,
    /// Total microseconds spent in the effect itself (pixel scenarios).
    pub compute_us: u64,
    /// Total microseconds spent writing pixels into the memfd, kept apart
    /// from `compute_us` because "the effect is slow" and "the upload is
    /// slow" have completely different fixes.
    pub upload_us: u64,
    /// Client `utime+stime` delta over the run, microseconds.
    pub client_cpu_us: u64,
    /// Server `utime+stime` delta over the run, microseconds.
    pub server_cpu_us: u64,
    /// The server's `stats` control-socket reply from before the run.
    pub stats_before: BTreeMap<String, u64>,
    /// ... and from after it, so every server counter can be differenced.
    pub stats_after: BTreeMap<String, u64>,
    /// Git sha of the build under test.
    pub sha: String,
    /// Where it ran; a number without a machine is not a measurement.
    pub host: String,
    /// Free text: `bar+launcher up`, `control: shell killed`.
    pub note: String,
}

/// The fallback frame budget, µs: one 60 Hz period.
///
/// Used when a run never learned its refresh rate. 60 Hz is the
/// assumption the whole latency dossier is written against, so falling
/// back to it keeps a refresh-less run comparable with the rest instead
/// of dividing by zero and printing infinities.
const DEFAULT_FRAME_BUDGET_US: f64 = 16_666.67;

/// How far past the frame budget a flip interval has to stretch before it
/// counts as a dropped frame.
///
/// A missed vblank shows up as an interval of about *two* periods, and an
/// on-time frame as about one. 1.5 is the midpoint: high enough that
/// ordinary scheduling jitter on a frame that still made its deadline does
/// not trip it, low enough that a genuine skip always does.
const DROP_FACTOR: f64 = 1.5;

/// How far below the refresh rate the presentation rate may sit and still
/// count as keeping up: 5 %, i.e. three frames a second at 60 Hz.
///
/// A client that is exactly at the cap still misses the odd frame to a
/// scheduler hiccup that has nothing to do with it; demanding equality
/// would make every row red and the column useless.
const KEPT_UP_TOLERANCE: f64 = 0.05;

/// `numerator / denominator`, or 0.0 when there is nothing to divide by.
///
/// Every derived figure funnels through here so that an empty run reports
/// an honest zero instead of a `NaN` that then poisons a mean, a sort and
/// a markdown cell.
fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator <= 0.0 {
        0.0
    } else {
        numerator / denominator
    }
}

impl Record {
    /// Frames put on the glass per second.
    ///
    /// `Presented` and not `Frame`: a frame callback says the server is
    /// willing to take another commit, which a client can receive while
    /// presenting nothing at all.
    #[must_use]
    pub fn presented_per_s(&self) -> f64 {
        ratio(self.presented as f64, self.seconds)
    }

    /// Commits sent per second.
    ///
    /// Compared against [`Record::presented_per_s`] it says whether the
    /// client is over-committing: more commits than presents means work
    /// thrown away before it was ever seen.
    #[must_use]
    pub fn commits_per_s(&self) -> f64 {
        ratio(self.commits as f64, self.seconds)
    }

    /// Server CPU microseconds per presented frame — **the headline
    /// number**.
    ///
    /// Divided by `presented` rather than by `commits` or by seconds:
    /// seconds flatter an idle run, commits flatter a client that drops
    /// its own work, and only "CPU burned per frame the user saw" stays
    /// comparable across a 60 Hz box, a 120 Hz box and a scenario that
    /// deliberately runs open-loop. With nothing presented the ratio is
    /// undefined, and 0.0 is the answer that keeps a table readable.
    #[must_use]
    pub fn server_cpu_us_per_frame(&self) -> f64 {
        ratio(self.server_cpu_us as f64, self.presented as f64)
    }

    /// Client CPU microseconds per presented frame.
    ///
    /// The same denominator as the server's on purpose: the two numbers
    /// are only worth anything side by side, because a cheap server that
    /// made the client expensive has not made the system faster.
    #[must_use]
    pub fn client_cpu_us_per_frame(&self) -> f64 {
        ratio(self.client_cpu_us as f64, self.presented as f64)
    }

    /// Wire bytes per committed frame.
    ///
    /// Per *commit*, not per present: the bytes were spent when they were
    /// written, whether or not the frame they described ever reached the
    /// glass. This is the number that says whether a scenario is sending
    /// a scene graph or a framebuffer.
    #[must_use]
    pub fn bytes_per_frame(&self) -> f64 {
        ratio(self.tx_bytes as f64, self.commits as f64)
    }

    /// Mutation messages per committed frame.
    ///
    /// `DESIGN.md`'s first goal is work proportional to what changed;
    /// this is the client's half of that claim, measured rather than
    /// asserted.
    #[must_use]
    pub fn mutations_per_frame(&self) -> f64 {
        ratio(self.mutations as f64, self.commits as f64)
    }

    /// Microseconds of effect computation per committed frame.
    ///
    /// Per commit because the effect ran once per commit regardless of
    /// what the compositor later did with it.
    #[must_use]
    pub fn compute_us_per_frame(&self) -> f64 {
        ratio(self.compute_us as f64, self.commits as f64)
    }

    /// Microseconds spent writing pixels into the memfd per committed
    /// frame.
    ///
    /// Kept separate from [`Record::compute_us_per_frame`] because the
    /// fix for a slow upload (smaller buffer, fewer bytes touched) has
    /// nothing to do with the fix for a slow effect.
    #[must_use]
    pub fn upload_us_per_frame(&self) -> f64 {
        ratio(self.upload_us as f64, self.commits as f64)
    }

    /// A server counter's change across the run, **signed**.
    ///
    /// Signed and not clamped: the server's counters reset when it
    /// restarts, so a negative delta is the ledger telling you the server
    /// under test was not the same process at both ends of the run. That
    /// is exactly the kind of invalid measurement a clamp to zero would
    /// hide. A key missing from either side counts as 0.
    #[must_use]
    pub fn stat_delta(&self, key: &str) -> i64 {
        let before = self.stats_before.get(key).copied().unwrap_or(0);
        let after = self.stats_after.get(key).copied().unwrap_or(0);
        let before = i64::try_from(before).unwrap_or(i64::MAX);
        let after = i64::try_from(after).unwrap_or(i64::MAX);
        after.saturating_sub(before)
    }

    /// A server counter as it stood after the run, or `None`.
    ///
    /// `Option` rather than a zero default because the report is required
    /// to print `?` for a key the server did not report, and a caller
    /// that got `0` could not tell the two apart.
    #[must_use]
    pub fn stat_after(&self, key: &str) -> Option<u64> {
        self.stats_after.get(key).copied()
    }

    /// Microseconds available per frame at this run's refresh rate.
    ///
    /// `1e9 / refresh_mhz`: millihertz to a period in microseconds in one
    /// division, exact for 60000 (16666.67 µs) and 120000 (8333.33 µs)
    /// and honest for 59940. A run that never learned its refresh gets
    /// [`DEFAULT_FRAME_BUDGET_US`] instead of an infinity.
    #[must_use]
    pub fn frame_budget_us(&self) -> f64 {
        if self.refresh_mhz == 0 {
            return DEFAULT_FRAME_BUDGET_US;
        }
        1e9 / f64::from(self.refresh_mhz)
    }

    /// The refresh rate this run should be judged against, Hz.
    ///
    /// Derived from the frame budget rather than from `refresh_mhz`
    /// directly so that the 60 Hz fallback applies in exactly one place.
    #[must_use]
    pub fn refresh_hz(&self) -> f64 {
        ratio(1e6, self.frame_budget_us())
    }

    /// Whether the server reported a flip gap long enough to be a missed
    /// vblank.
    ///
    /// The test is `flip_interval_max_us > 1.5 × frame_budget_us`: a
    /// skipped frame leaves an interval of about two periods and a
    /// healthy frame about one, so [`DROP_FACTOR`] sits at the midpoint,
    /// far from both. A server that does not report the counter is not
    /// evidence of a drop, so the answer there is `false` — the report
    /// prints `?` for the missing cell and lets the human see the gap.
    #[must_use]
    pub fn dropped(&self) -> bool {
        self.stat_after("flip_interval_max_us")
            .is_some_and(|max_us| max_us as f64 > DROP_FACTOR * self.frame_budget_us())
    }

    /// Whether the run held the output's rate: presenting within 5 % of
    /// the refresh **and** no flip gap.
    ///
    /// Both halves are needed. Rate alone passes a run that presented 60
    /// frames a second with one 33 ms hitch in the middle — the exact
    /// stutter a user complains about — and the flip gap alone passes a
    /// run that was uniformly, smoothly half as fast as the display.
    #[must_use]
    pub fn kept_up(&self) -> bool {
        let expected = self.refresh_hz();
        let within = (self.presented_per_s() - expected).abs() <= KEPT_UP_TOLERANCE * expected;
        within && !self.dropped()
    }

    /// The row label a table uses for this run.
    ///
    /// The sweep point belongs in the label because a table of `rects`,
    /// `rects`, `rects` is not a table. `n=` and `size=` appear only when
    /// they apply; a scenario swept by neither (the full-screen effects)
    /// is identified by its resolution instead, which is the thing that
    /// actually varies between its rows.
    #[must_use]
    pub fn label(&self) -> String {
        let mut out = self.scenario.clone();
        if self.n != 0 {
            let _ = write!(out, " n={}", self.n);
        }
        if self.size != 0 {
            let _ = write!(out, " size={}", self.size);
        }
        if self.n == 0 && self.size == 0 && self.width != 0 && self.height != 0 {
            let _ = write!(out, " {}x{}", self.width, self.height);
        }
        out
    }

    /// The record as one line of JSON, ready to append to a `.jsonl`
    /// ledger.
    ///
    /// Keys are emitted in a fixed order — `scenario`, `n`, `size`,
    /// `width`, `height`, `refresh_mhz`, `seconds`, `commits`,
    /// `presented`, `frames_received`, `mutations`, `tx_bytes`,
    /// `rx_bytes`, `compute_us`, `upload_us`, `client_cpu_us`,
    /// `server_cpu_us`, `stats_before`, `stats_after`, `sha`, `host`,
    /// `note` — and the two maps are `BTreeMap`s, so the whole line is a
    /// deterministic function of the record. That is what makes two
    /// ledgers diffable and a re-run's line comparable byte for byte with
    /// the line it replaces.
    ///
    /// No trailing newline: the caller owns the line separator, because
    /// the caller is the one appending to a file that may or may not end
    /// in one.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push('{');
        push_string(&mut out, "scenario", &self.scenario);
        push_u64(&mut out, "n", self.n);
        push_u64(&mut out, "size", u64::from(self.size));
        push_u64(&mut out, "width", u64::from(self.width));
        push_u64(&mut out, "height", u64::from(self.height));
        push_u64(&mut out, "refresh_mhz", u64::from(self.refresh_mhz));
        push_f64(&mut out, "seconds", self.seconds);
        push_u64(&mut out, "commits", self.commits);
        push_u64(&mut out, "presented", self.presented);
        push_u64(&mut out, "frames_received", self.frames_received);
        push_u64(&mut out, "mutations", self.mutations);
        push_u64(&mut out, "tx_bytes", self.tx_bytes);
        push_u64(&mut out, "rx_bytes", self.rx_bytes);
        push_u64(&mut out, "compute_us", self.compute_us);
        push_u64(&mut out, "upload_us", self.upload_us);
        push_u64(&mut out, "client_cpu_us", self.client_cpu_us);
        push_u64(&mut out, "server_cpu_us", self.server_cpu_us);
        push_map(&mut out, "stats_before", &self.stats_before);
        push_map(&mut out, "stats_after", &self.stats_after);
        push_string(&mut out, "sha", &self.sha);
        push_string(&mut out, "host", &self.host);
        push_string(&mut out, "note", &self.note);
        // Every `push_*` leaves a trailing comma; dropping it here beats
        // threading a "first field" flag through twenty-two calls.
        if out.ends_with(',') {
            out.pop();
        }
        out.push('}');
        out
    }

    /// Parse one line written by [`Record::to_json`].
    ///
    /// Unknown keys are skipped and missing keys take their `Default`, so
    /// a report tool built before a field existed still reads a ledger
    /// written after it did, and one built after still reads the old
    /// lines. Syntax errors are *not* forgiven: a truncated line is a
    /// lost run, and silently returning a half-populated record would put
    /// a fabricated row in a table.
    ///
    /// # Errors
    /// Any deviation from the emitted subset of JSON: a missing brace or
    /// colon, an unterminated string, a bad escape, a non-numeric value
    /// for a numeric key, or trailing text after the object.
    pub fn from_json(line: &str) -> Result<Self, ParseError> {
        let mut parser = Parser::new(line);
        let record = parser.record()?;
        parser.skip_ws();
        if parser.at < line.len() {
            return Err(parser.error("trailing text after the object"));
        }
        Ok(record)
    }
}

/// Read a whole `.jsonl` ledger, keeping the good lines and the bad news.
///
/// Errors are *collected*, not propagated: a report over the 39 runs that
/// parsed is worth far more than an error message about the 40th, which
/// is typically a line truncated by a machine that lost power mid-append.
/// The caller prints the errors under the tables and the human decides.
///
/// Blank lines are skipped so a file can be spaced out by hand, and lines
/// whose first non-space character is `#` are skipped so a ledger can
/// carry a comment about the machine it came from.
#[must_use]
pub fn read_jsonl(text: &str) -> (Vec<Record>, Vec<ParseError>) {
    let mut records = Vec::new();
    let mut errors = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match Record::from_json(line) {
            Ok(record) => records.push(record),
            Err(mut error) => {
                // The offset alone is useless in a 400-line file, so the
                // line number goes in front of the message.
                error.message = format!("line {}: {}", index + 1, error.message);
                errors.push(error);
            }
        }
    }
    (records, errors)
}

/// A line that was not the JSON this module emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// What was wrong, in the terms of the grammar.
    pub message: String,
    /// Byte offset within the line where it went wrong.
    ///
    /// A byte offset and not a column: the ledger is machine-written and
    /// the reader is likely a machine too, and `head -c` beats counting
    /// characters in a 900-byte line.
    pub at: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.at)
    }
}

impl std::error::Error for ParseError {}

/// Append `"key":"value",` with the string escaped.
fn push_string(out: &mut String, key: &str, value: &str) {
    push_key(out, key);
    push_quoted(out, value);
    out.push(',');
}

/// Append `"key":value,` for an unsigned count.
fn push_u64(out: &mut String, key: &str, value: u64) {
    push_key(out, key);
    out.push_str(&value.to_string());
    out.push(',');
}

/// Append `"key":value,` for a float.
///
/// `{:?}` and not `{}`: `Display` prints `12` for `12.0_f64`, which reads
/// back as an integer and loses the type, and it renders `1e300` as three
/// hundred digits. `Debug` on `f64` is the shortest representation that
/// parses back to the identical bit pattern — round-tripping is the whole
/// job here. Non-finite values cannot appear in a measurement and would
/// not be JSON if they did, so they are written as `0.0`.
fn push_f64(out: &mut String, key: &str, value: f64) {
    push_key(out, key);
    let value = if value.is_finite() { value } else { 0.0 };
    let _ = write!(out, "{value:?}");
    out.push(',');
}

/// Append `"key":{...},` for a map of counters.
fn push_map(out: &mut String, key: &str, map: &BTreeMap<String, u64>) {
    push_key(out, key);
    out.push('{');
    for (name, value) in map {
        push_quoted(out, name);
        out.push(':');
        out.push_str(&value.to_string());
        out.push(',');
    }
    if out.ends_with(',') {
        out.pop();
    }
    out.push_str("},");
}

/// Append `"key":`.
fn push_key(out: &mut String, key: &str) {
    push_quoted(out, key);
    out.push(':');
}

/// Append a JSON string literal.
///
/// The short escapes for the three whitespace characters a `note` will
/// actually contain, `\u00XX` for the rest of C0, and nothing else: JSON
/// requires no escaping above `0x1f` except `"` and `\`, and escaping
/// non-ASCII would make the ledger unreadable in exactly the cases
/// (hostnames, notes) where a human most wants to read it.
fn push_quoted(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Recursive-descent reader over the subset of JSON this module emits.
///
/// A cursor over bytes rather than `chars`: every structural character in
/// JSON is ASCII, so byte positions are exact and a multi-byte character
/// inside a string is copied through untouched. The cursor never lands in
/// the middle of one, which is what keeps the final `String::from_utf8`
/// infallible in practice.
struct Parser<'a> {
    src: &'a str,
    at: usize,
    depth: u32,
}

/// How deep a skipped value may nest before the parser gives up.
///
/// Unknown keys are skipped with a recursive walk, and a hostile line of
/// ten thousand `[` would otherwise overflow the stack of a tool whose
/// input is "a file someone sent me". Nothing this module emits nests
/// more than twice.
const MAX_DEPTH: u32 = 32;

impl<'a> Parser<'a> {
    /// A parser positioned at the start of `src`.
    fn new(src: &'a str) -> Self {
        Self {
            src,
            at: 0,
            depth: 0,
        }
    }

    /// An error at the cursor.
    fn error(&self, message: &str) -> ParseError {
        ParseError {
            message: message.to_owned(),
            at: self.at,
        }
    }

    /// The byte under the cursor, if any.
    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.at).copied()
    }

    /// Advance past spaces, tabs and line endings.
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// Consume one expected structural byte.
    fn eat(&mut self, byte: u8) -> Result<(), ParseError> {
        if self.peek() == Some(byte) {
            self.at += 1;
            return Ok(());
        }
        Err(self.error(&format!("expected `{}`", byte as char)))
    }

    /// The top-level object, field by field.
    fn record(&mut self) -> Result<Record, ParseError> {
        let mut record = Record::default();
        self.skip_ws();
        self.eat(b'{')?;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(record);
        }
        loop {
            self.skip_ws();
            let key = self.text()?;
            self.skip_ws();
            self.eat(b':')?;
            self.skip_ws();
            self.assign(&key, &mut record)?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(record);
                }
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }

    /// Read one value into the field `key` names, or skip it.
    ///
    /// The `_` arm is the forward-compatibility rule: a key this build
    /// has never heard of is consumed and discarded, not refused.
    fn assign(&mut self, key: &str, record: &mut Record) -> Result<(), ParseError> {
        match key {
            "scenario" => record.scenario = self.text()?,
            "n" => record.n = self.unsigned()?,
            "size" => record.size = self.unsigned32()?,
            "width" => record.width = self.unsigned32()?,
            "height" => record.height = self.unsigned32()?,
            "refresh_mhz" => record.refresh_mhz = self.unsigned32()?,
            "seconds" => record.seconds = self.float()?,
            "commits" => record.commits = self.unsigned()?,
            "presented" => record.presented = self.unsigned()?,
            "frames_received" => record.frames_received = self.unsigned()?,
            "mutations" => record.mutations = self.unsigned()?,
            "tx_bytes" => record.tx_bytes = self.unsigned()?,
            "rx_bytes" => record.rx_bytes = self.unsigned()?,
            "compute_us" => record.compute_us = self.unsigned()?,
            "upload_us" => record.upload_us = self.unsigned()?,
            "client_cpu_us" => record.client_cpu_us = self.unsigned()?,
            "server_cpu_us" => record.server_cpu_us = self.unsigned()?,
            "stats_before" => record.stats_before = self.number_object()?,
            "stats_after" => record.stats_after = self.number_object()?,
            "sha" => record.sha = self.text()?,
            "host" => record.host = self.text()?,
            "note" => record.note = self.text()?,
            _ => self.skip_value()?,
        }
        Ok(())
    }

    /// A string literal, escapes decoded.
    fn text(&mut self) -> Result<String, ParseError> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let byte = self
                .peek()
                .ok_or_else(|| self.error("unterminated string"))?;
            self.at += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escape = self
                        .peek()
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.error("unknown escape")),
                    }
                }
                _ => {
                    // Copy the byte through: inside a `&str` every
                    // non-ASCII byte belongs to a well-formed sequence
                    // and no escape can split one.
                    let start = self.at - 1;
                    let end = self.src[start..]
                        .char_indices()
                        .nth(1)
                        .map_or(self.src.len(), |(offset, _)| start + offset);
                    out.push_str(&self.src[start..end]);
                    self.at = end;
                }
            }
        }
    }

    /// The four hex digits after `\u`, surrogate pairs included.
    ///
    /// This module never *writes* a surrogate — it escapes only C0 — but
    /// another tool appending to the same ledger might, and a report that
    /// died on someone else's perfectly legal line would be the wrong
    /// kind of strict. An unpaired surrogate becomes U+FFFD, which is
    /// what it means.
    fn unicode_escape(&mut self) -> Result<char, ParseError> {
        let first = self.hex4()?;
        if (0xd800..0xdc00).contains(&first) {
            let save = self.at;
            if self.peek() == Some(b'\\') {
                self.at += 1;
                if self.peek() == Some(b'u') {
                    self.at += 1;
                    let second = self.hex4()?;
                    if (0xdc00..0xe000).contains(&second) {
                        let combined = 0x1_0000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                        return char::from_u32(combined)
                            .ok_or_else(|| self.error("bad surrogate pair"));
                    }
                }
            }
            self.at = save;
            return Ok(char::REPLACEMENT_CHARACTER);
        }
        Ok(char::from_u32(first).unwrap_or(char::REPLACEMENT_CHARACTER))
    }

    /// Four hex digits as a number.
    fn hex4(&mut self) -> Result<u32, ParseError> {
        let end = self.at + 4;
        let digits = self
            .src
            .get(self.at..end)
            .ok_or_else(|| self.error("short \\u escape"))?;
        let value =
            u32::from_str_radix(digits, 16).map_err(|_| self.error("bad hex in \\u escape"))?;
        self.at = end;
        Ok(value)
    }

    /// The raw text of a number token.
    fn number_token(&mut self) -> Result<&'a str, ParseError> {
        let start = self.at;
        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
        ) {
            self.at += 1;
        }
        if self.at == start {
            return Err(self.error("expected a number"));
        }
        Ok(&self.src[start..self.at])
    }

    /// An unsigned count.
    ///
    /// Accepts the float spelling too (`60.0`, `6e1`): a hand-edited
    /// ledger or a different writer may produce it, and refusing a value
    /// that names an exact integer would be pedantry, not safety.
    fn unsigned(&mut self) -> Result<u64, ParseError> {
        let start = self.at;
        let token = self.number_token()?;
        if let Ok(value) = token.parse::<u64>() {
            return Ok(value);
        }
        match token.parse::<f64>() {
            Ok(value) if value.is_finite() && value >= 0.0 => Ok(value as u64),
            _ => Err(ParseError {
                message: format!("expected an unsigned number, found `{token}`"),
                at: start,
            }),
        }
    }

    /// An unsigned count that has to fit a `u32` field; larger values
    /// saturate rather than fail, because a nonsense width is still a
    /// readable run.
    fn unsigned32(&mut self) -> Result<u32, ParseError> {
        Ok(u32::try_from(self.unsigned()?).unwrap_or(u32::MAX))
    }

    /// A float.
    fn float(&mut self) -> Result<f64, ParseError> {
        let start = self.at;
        let token = self.number_token()?;
        token.parse::<f64>().map_err(|_| ParseError {
            message: format!("expected a number, found `{token}`"),
            at: start,
        })
    }

    /// An object whose values are all numbers: a `stats` snapshot.
    fn number_object(&mut self) -> Result<BTreeMap<String, u64>, ParseError> {
        let mut map = BTreeMap::new();
        self.eat(b'{')?;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(map);
        }
        loop {
            self.skip_ws();
            let key = self.text()?;
            self.skip_ws();
            self.eat(b':')?;
            self.skip_ws();
            let value = self.unsigned()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(map);
                }
                _ => return Err(self.error("expected `,` or `}` in a stats object")),
            }
        }
    }

    /// Consume and discard any value, whatever shape a future schema
    /// gives it.
    fn skip_value(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("value nested too deeply"));
        }
        let result = self.skip_value_inner();
        self.depth -= 1;
        result
    }

    /// The body of [`Parser::skip_value`], minus the depth bookkeeping.
    fn skip_value_inner(&mut self) -> Result<(), ParseError> {
        match self.peek() {
            Some(b'"') => {
                self.text()?;
                Ok(())
            }
            Some(b'{' | b'[') => {
                let close = if self.peek() == Some(b'{') {
                    b'}'
                } else {
                    b']'
                };
                self.at += 1;
                self.skip_ws();
                if self.peek() == Some(close) {
                    self.at += 1;
                    return Ok(());
                }
                loop {
                    self.skip_ws();
                    if close == b'}' {
                        self.text()?;
                        self.skip_ws();
                        self.eat(b':')?;
                        self.skip_ws();
                    }
                    self.skip_value()?;
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b) if b == close => {
                            self.at += 1;
                            return Ok(());
                        }
                        _ => return Err(self.error("expected `,` or a closing bracket")),
                    }
                }
            }
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(_) => {
                self.number_token()?;
                Ok(())
            }
            None => Err(self.error("expected a value")),
        }
    }

    /// Consume one of the three JSON keywords.
    fn literal(&mut self, word: &str) -> Result<(), ParseError> {
        if self.src[self.at..].starts_with(word) {
            self.at += word.len();
            return Ok(());
        }
        Err(self.error("expected a value"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record with every field set to something distinguishable.
    fn full() -> Record {
        Record {
            scenario: "rects".to_owned(),
            n: 1_000,
            size: 256,
            width: 1_920,
            height: 1_080,
            refresh_mhz: 60_000,
            seconds: 5.25,
            commits: 300,
            presented: 299,
            frames_received: 301,
            mutations: 3_000,
            tx_bytes: 120_000,
            rx_bytes: 9_000,
            compute_us: 60_000,
            upload_us: 30_000,
            client_cpu_us: 450_000,
            server_cpu_us: 900_000,
            stats_before: [("frames".to_owned(), 10_u64)].into_iter().collect(),
            stats_after: [
                ("frames".to_owned(), 310_u64),
                ("paint_us_mean".to_owned(), 420),
            ]
            .into_iter()
            .collect(),
            sha: "deadbeef".to_owned(),
            host: "bench-box".to_owned(),
            note: "bar+launcher up".to_owned(),
        }
    }

    #[test]
    fn a_record_round_trips_through_json() {
        let record = full();
        let line = record.to_json();
        assert!(!line.contains('\n'), "the ledger is one line per run");
        assert_eq!(Record::from_json(&line).unwrap(), record);
    }

    /// The `note` field is free text a human typed, so it is the one that
    /// will contain a quote, a path and an accident.
    #[test]
    fn awkward_text_survives_the_round_trip() {
        let record = Record {
            note: "a\"b\\c\nd\te — ✓ \u{1}".to_owned(),
            host: "boîte-à-café".to_owned(),
            ..Record::default()
        };
        let line = record.to_json();
        assert!(line.contains("\\u0001"), "{line}");
        assert!(line.contains("\\n") && line.contains("\\\""), "{line}");
        assert_eq!(Record::from_json(&line).unwrap(), record);
    }

    #[test]
    fn the_key_order_is_fixed() {
        let line = full().to_json();
        let scenario = line.find("\"scenario\"").unwrap();
        let seconds = line.find("\"seconds\"").unwrap();
        let stats = line.find("\"stats_after\"").unwrap();
        let note = line.find("\"note\"").unwrap();
        assert!(
            scenario < seconds && seconds < stats && stats < note,
            "{line}"
        );
    }

    #[test]
    fn an_unknown_key_is_skipped_not_an_error() {
        let line =
            r#"{"scenario":"plasma","future":[1,{"deep":true},null],"presented":7,"also":"x"}"#;
        let record = Record::from_json(line).unwrap();
        assert_eq!(record.scenario, "plasma");
        assert_eq!(record.presented, 7);
    }

    #[test]
    fn a_missing_key_takes_the_default() {
        let record = Record::from_json(r#"{"scenario":"fire"}"#).unwrap();
        assert_eq!(record.scenario, "fire");
        assert_eq!(record.commits, 0);
        assert!(record.seconds.abs() < f64::EPSILON);
        assert!(record.stats_after.is_empty());
    }

    #[test]
    fn an_empty_object_is_a_default_record() {
        assert_eq!(Record::from_json("{}").unwrap(), Record::default());
    }

    #[test]
    fn a_malformed_line_errors_with_a_position() {
        let error = Record::from_json(r#"{"scenario":"rects", "n":}"#).unwrap_err();
        assert_eq!(error.at, 25, "{error}");
        assert!(error.to_string().contains("number"), "{error}");

        let truncated = Record::from_json(r#"{"scenario":"rec"#).unwrap_err();
        assert!(truncated.message.contains("unterminated"), "{truncated}");

        let trailing = Record::from_json(r"{} junk").unwrap_err();
        assert!(trailing.message.contains("trailing"), "{trailing}");
    }

    #[test]
    fn a_stats_object_of_non_numbers_is_an_error() {
        let error = Record::from_json(r#"{"stats_after":{"frames":"many"}}"#).unwrap_err();
        assert!(error.message.contains("number"), "{error}");
    }

    #[test]
    fn a_mixed_quality_file_yields_records_and_errors() {
        let text = concat!(
            "# bench-box, 2026-09-16\n",
            "{\"scenario\":\"rects\",\"presented\":10}\n",
            "\n",
            "   \n",
            "{\"scenario\":\"oops\",\n",
            "{\"scenario\":\"plasma\",\"presented\":20}\n",
        );
        let (records, errors) = read_jsonl(text);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].scenario, "plasma");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.starts_with("line 5:"), "{}", errors[0]);
    }

    #[test]
    fn the_derived_ratios_divide_by_the_right_thing() {
        let record = full();
        assert!((record.presented_per_s() - 299.0 / 5.25).abs() < 1e-9);
        assert!((record.commits_per_s() - 300.0 / 5.25).abs() < 1e-9);
        assert!((record.server_cpu_us_per_frame() - 900_000.0 / 299.0).abs() < 1e-9);
        assert!((record.client_cpu_us_per_frame() - 450_000.0 / 299.0).abs() < 1e-9);
        assert!((record.bytes_per_frame() - 400.0).abs() < 1e-9);
        assert!((record.mutations_per_frame() - 10.0).abs() < 1e-9);
        assert!((record.compute_us_per_frame() - 200.0).abs() < 1e-9);
        assert!((record.upload_us_per_frame() - 100.0).abs() < 1e-9);
    }

    /// Nothing measured must produce 0.0 and never a `NaN`: a `NaN` sorts
    /// wrong, formats as `NaN` and infects every mean it touches.
    #[test]
    fn a_zero_denominator_is_zero_and_not_a_nan() {
        let empty = Record::default();
        for value in [
            empty.presented_per_s(),
            empty.commits_per_s(),
            empty.server_cpu_us_per_frame(),
            empty.client_cpu_us_per_frame(),
            empty.bytes_per_frame(),
            empty.mutations_per_frame(),
            empty.compute_us_per_frame(),
            empty.upload_us_per_frame(),
        ] {
            assert!(value.abs() < f64::EPSILON, "{value}");
        }
    }

    #[test]
    fn stat_deltas_are_signed_so_a_server_restart_shows() {
        let mut record = full();
        assert_eq!(record.stat_delta("frames"), 300);
        assert_eq!(record.stat_after("paint_us_mean"), Some(420));
        assert_eq!(record.stat_after("nope"), None);
        record.stats_after.insert("frames".to_owned(), 1);
        assert_eq!(record.stat_delta("frames"), -9);
    }

    #[test]
    fn the_frame_budget_follows_the_refresh() {
        let mut record = Record {
            refresh_mhz: 60_000,
            ..Record::default()
        };
        assert!((record.frame_budget_us() - 16_666.666_666).abs() < 0.01);
        record.refresh_mhz = 120_000;
        assert!((record.frame_budget_us() - 8_333.333_333).abs() < 0.01);
        record.refresh_mhz = 0;
        assert!((record.frame_budget_us() - 16_666.67).abs() < 0.01);
        assert!((record.refresh_hz() - 60.0).abs() < 0.01);
    }

    #[test]
    fn dropped_fires_on_both_sides_of_the_threshold() {
        let mut record = Record {
            refresh_mhz: 60_000,
            ..Record::default()
        };
        // One and a half budgets exactly is not yet a drop.
        record
            .stats_after
            .insert("flip_interval_max_us".to_owned(), 25_000);
        assert!(!record.dropped());
        record
            .stats_after
            .insert("flip_interval_max_us".to_owned(), 25_001);
        assert!(record.dropped());
        // No counter is not evidence of a drop.
        record.stats_after.clear();
        assert!(!record.dropped());
    }

    #[test]
    fn keeping_up_needs_the_rate_and_the_absence_of_a_gap() {
        let mut record = Record {
            refresh_mhz: 60_000,
            seconds: 10.0,
            presented: 598,
            ..Record::default()
        };
        assert!(record.kept_up());
        record
            .stats_after
            .insert("flip_interval_max_us".to_owned(), 33_000);
        assert!(!record.kept_up(), "a hitch is not keeping up");
        record.stats_after.clear();
        record.presented = 300;
        assert!(!record.kept_up(), "half rate is not keeping up");
    }

    #[test]
    fn a_label_names_the_sweep_point() {
        let mut record = Record {
            scenario: "rects".to_owned(),
            n: 1_000,
            ..Record::default()
        };
        assert_eq!(record.label(), "rects n=1000");
        record.n = 0;
        record.size = 1_080;
        record.scenario = "putimage".to_owned();
        assert_eq!(record.label(), "putimage size=1080");
        record.size = 0;
        record.scenario = "plasma".to_owned();
        record.width = 1_920;
        record.height = 1_080;
        assert_eq!(record.label(), "plasma 1920x1080");
    }
}

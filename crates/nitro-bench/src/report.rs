//! The report: a ledger of runs in, the markdown of `docs/bench.md` out.
//!
//! # Why markdown and why generated
//!
//! The output of a benchmark is an argument, and an argument lives in a
//! document a human reads, not in a terminal that scrolls away. So the
//! tool's job ends at a markdown table that is pasted into
//! `docs/bench.md` — reviewable in a diff, comparable against the last
//! run's paste, and readable on a machine that has nothing installed.
//!
//! Generated rather than hand-written because a hand-written table is a
//! table someone stopped updating in March. The header line names the
//! generator and the record count so a stale paste is visible at a
//! glance.
//!
//! # Why a missing number prints `?`
//!
//! Several columns come from the server's `stats` reply, and the server
//! is free to add and rename counters. `nitro-demo::control::i2p_line`
//! already settled the rule for that: report "the server's i2p figures as
//! a printable line, with the keys it is missing **named** rather than
//! silently zeroed". A `0` in a `paint µs` column is a claim that
//! painting was free; a `?` is the truth, which is that this build of the
//! server did not say. The distinction matters most in exactly the case
//! where it is tempting to skip — comparing two builds, one of which has
//! the counter.
//!
//! # Why the refresh pivot is its own table
//!
//! The question the pixel scenarios exist to answer is "where does the
//! path stop keeping up at 120 Hz that it kept up at 60". That is a
//! comparison between two rows which, in a per-scenario table sorted by
//! sweep point, are nowhere near each other. Putting them side by side
//! in a table of their own is the difference between the data being
//! present and the answer being visible.
//!
//! # Why the rate tables are per scenario, and the pivot is not
//!
//! Two shapes, two questions. [`refresh_pivot`] is the index: every
//! sweep point measured at more than one rate, three cells each, in one
//! table a reader skims for "what moved". [`rate_tables`] is the
//! evidence: one table per scenario family, a column per rate, and the
//! five figures §9 argues from — presented/s, server µs/frame, client
//! µs/frame, `paint µs mean`, verdict.
//!
//! The per-scenario shape exists because the §9 questions are asked of a
//! *scenario*, not of a matrix: "does the retained path keep per-frame
//! cost flat across rates" is answered by reading `boing-node`'s three
//! server-µs cells and finding them equal, and no arrangement that
//! scatters those three cells across a forty-row table answers it. Each
//! table also states its own frame budget per column — 16 667 / 8 335 /
//! 4 167 µs — because every verdict in it is a comparison against that
//! number and a reader should not have to hold three of them in their
//! head.

use crate::record::Record;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The column header, and with it the column order.
///
/// `run` first because it is what a reader scans down; the two rate
/// columns next because they say whether the run was valid at all; then
/// the cost columns in the order the work happens (client mutations →
/// server paint → copy → wire bytes); `verdict` last because it is the
/// conclusion drawn from everything to its left.
///
/// `flip rise` sits next to the verdict because it is the evidence for
/// it. The server's `flip_interval_max_us` is a cumulative all-time
/// maximum, so its *value* says nothing about a given run and its **rise**
/// says everything — see [`Record::dropped`], where reading the value
/// alone once marked 25 of 45 clean runs as dropped. Printing the rise
/// lets a reader check the verdict rather than take it.
const HEADER: &str = "| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |";

/// The markdown separator row matching [`HEADER`].
const SEPARATOR: &str = "|---|---|---|---|---|---|---|---|---|---|---|---|";

/// What to print for a server counter this run does not have.
///
/// See the module docs: a named gap, never a fabricated zero.
const MISSING: &str = "?";

/// How far below the refresh a run has to present before it is called
/// **slow** rather than merely imperfect: 15 %.
///
/// Well outside [`Record::kept_up`]'s 5 % tolerance, so a run is never
/// both "not keeping up" and "not slow enough to mention" by accident,
/// and far enough down that the cause has to be the client's own loop
/// rather than a couple of unlucky vblanks.
const SLOW_FRACTION: f64 = 0.85;

/// The whole report: a provenance line, one table per scenario family,
/// then the refresh pivot when there is anything to pivot.
///
/// Scenario tables come first because they are the data and the pivot is
/// a reading of it; a reader who disagrees with the pivot needs the rows
/// it was drawn from to already be above it.
#[must_use]
pub fn markdown(records: &[Record]) -> String {
    if records.is_empty() {
        // An empty report says so in one line. A header and an empty
        // table would look like a result — "we measured, and there was
        // nothing" — when the truth is that nothing was measured.
        return "<!-- generated by `nitro-bench report` -->\n\nNo runs in this ledger.\n"
            .to_owned();
    }
    let mut out = format!(
        "<!-- generated by `nitro-bench report`: {} run{} -->\n\n",
        records.len(),
        if records.len() == 1 { "" } else { "s" }
    );
    for (scenario, group) in group_by_scenario(records) {
        out.push_str(&table(&scenario, &group));
    }
    out.push_str(&rate_tables(records));
    let pivot = refresh_pivot(records);
    if !pivot.is_empty() {
        out.push_str(&pivot);
    }
    out
}

/// What identifies "the same measurement at a different refresh rate".
///
/// A named type rather than a bare tuple because it is the thing two
/// tables and three bugs are about: scenario, sweep point, buffer edge
/// (windowed runs only), fullscreen, control. See [`rate_key`], which
/// builds one and documents why each field is or is not in it.
type RateKey = (String, u64, u32, bool, bool);

/// A rate key's runs, by rounded hertz.
type ByRate<'a> = BTreeMap<u64, &'a Record>;

/// The key a run is compared *across rates* by.
///
/// `(scenario, n, size, fullscreen, control)` — and deliberately **not**
/// the geometry, with `size` dropped for a fullscreen run. Three
/// decisions, each one a wrong table in the first version of this
/// function, and each visible only against a real three-rate ledger:
///
/// * **Not the geometry.** The whole point of the 720p@240 arm is that
///   its screen is a different size, so keying on width and height
///   files `boing 1920x1080` and `boing 1280x720` as two unrelated rows
///   and prints the comparison this table exists for as two columns of
///   `?`.
/// * **`size` only when the run is windowed.** At a fullscreen run
///   `size` is not a sweep point at all, it is an *output*: the pixel
///   scenarios record the screen's width (1920, 1920, **1280**) and
///   `boing-node` records the ball's on-screen diameter (389 at 1080p,
///   **260** at 720p). Keying on it therefore re-introduced the
///   geometry through the back door and scattered every fullscreen row
///   across three keys — the same empty table as above, arrived at by a
///   field that does not have `width` in its name. A fullscreen arm is
///   identified by *being fullscreen*; its size is what the rate sweep
///   varies.
/// * **The control arm is not the same run as the row it shadows.**
///   `deploy/bench.sh` takes one `rects n=500` per rate with the shell
///   clients killed, and it is otherwise identical to the measured
///   `rects n=500`. Without this, "last run wins" silently replaced
///   every rate's real row with its control — a table reporting the
///   shell's absence as if it were the baseline, which is a *plausible*
///   number in the right units and the worst kind of wrong.
fn rate_key(record: &Record) -> RateKey {
    let fullscreen = record.is_fullscreen();
    (
        record.scenario.clone(),
        record.n,
        if fullscreen { 0 } else { record.size },
        fullscreen,
        record.is_control(),
    )
}

/// The rate at which a run is filed, in whole hertz.
///
/// Rounded, so 119 982 mHz and a true 120 000 share a column — the
/// grouping a human means when they say "the 120 Hz arm", and the
/// spelling `outputs` does not use.
fn rate_of(record: &Record) -> u64 {
    hz(u64::from(record.refresh_mhz))
}

/// One rate-comparison table per scenario family, or an empty string
/// when nothing was measured at more than one rate.
///
/// The five columns per rate are the ones §9's questions are asked in:
/// **presented/s** (did it hold the rate), **server** and **client
/// µs/frame** (the headline pair — and the pair whose *flatness* across
/// rates is the retained path's claim), **paint µs mean** (the server's
/// own counter, which is where a bandwidth wall shows up as a rise that
/// the CPU column blurs), and the **verdict**.
///
/// A scenario with runs at only one rate is skipped entirely rather than
/// printed with two empty columns: a table of `?` is not evidence of
/// anything, and the per-scenario table above already has the row.
#[must_use]
pub fn rate_tables(records: &[Record]) -> String {
    let mut by_key: BTreeMap<RateKey, ByRate> = BTreeMap::new();
    // First-appearance order for both the families and the rows inside
    // them, for the reason `group_by_scenario` gives: the ledger's order
    // is the order the sweep ran, and re-sorting by a measured column
    // would let a noisy run reorder the table.
    let mut order: Vec<RateKey> = Vec::new();
    let mut families: Vec<String> = Vec::new();
    for record in records {
        let key = rate_key(record);
        if !by_key.contains_key(&key) {
            order.push(key.clone());
        }
        if !families.contains(&record.scenario) {
            families.push(record.scenario.clone());
        }
        // Last run wins for a repeated (key, rate): a re-run is a
        // correction, not a second opinion. The same rule the pivot uses.
        by_key
            .entry(key)
            .or_default()
            .insert(rate_of(record), record);
    }

    let mut out = String::new();
    for family in families {
        let rows: Vec<_> = order
            .iter()
            .filter(|key| key.0 == family)
            .filter_map(|key| {
                let rates = by_key.get(key)?;
                (rates.len() >= 2).then_some(rates)
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        let mut rates: Vec<u64> = rows
            .iter()
            .flat_map(|rates| rates.keys().copied())
            .collect();
        rates.sort_unstable();
        rates.dedup();

        let _ = write!(out, "#### {family} across rates\n\n");
        // The budget line, once per table, **over the same `rates`
        // vector the header is built from**.
        //
        // It used to be built from `rows.first()`, which is only the
        // same list when the family's first row happens to have been run
        // at every rate in the table. When it was not — `rects`, whose
        // first row is the VGA-only n=10 pair — the table printed 240 Hz
        // columns full of data under a budget line naming 60 and 120,
        // i.e. a caption that was wrong about the one column §9.3 argues
        // from. A per-table caption has to be derived from the table,
        // not from a row of it; the row is a sample and the header is
        // the population.
        //
        // Any record filed under a rate will do for the budget, since
        // `frame_budget_us` is a function of `refresh_mhz` alone and
        // every record under one column shares a rounded rate — so the
        // lookup takes the first row that actually has that rate rather
        // than assuming a particular one does.
        let budgets: Vec<String> = rates
            .iter()
            .map(|rate| {
                rows.iter()
                    .find_map(|by_rate| by_rate.get(rate))
                    .map_or_else(
                        || format!("{rate} Hz = {MISSING} µs"),
                        |r| format!("{:.0} Hz = {:.0} µs", r.refresh_hz(), r.frame_budget_us()),
                    )
            })
            .collect();
        if !budgets.is_empty() {
            let _ = write!(out, "Frame budget: {}.\n\n", budgets.join(", "));
        }
        out.push_str("| run |");
        for rate in &rates {
            let _ = write!(
                out,
                " {rate} presented/s | {rate} server µs | {rate} client µs | {rate} paint µs | {rate} verdict |"
            );
        }
        out.push('\n');
        out.push_str("|---|");
        for _ in &rates {
            out.push_str("---|---|---|---|---|");
        }
        out.push('\n');
        for by_rate in rows {
            // The label comes from the run itself, and at the 240 Hz arm
            // a fullscreen run's label says `1280x720` while its 60 Hz
            // twin says `1920x1080`. Taking the *first* rate's label
            // would print one size over a row of three; the label is
            // therefore stripped of geometry and the geometry stated per
            // cell would be four more columns. Instead the row is named
            // by scenario and sweep point, and §9 states the sizes once.
            let label = by_rate
                .values()
                .next()
                .map_or_else(String::new, |r| rate_row_label(r));
            let _ = write!(out, "| {label} |");
            for rate in &rates {
                match by_rate.get(rate) {
                    Some(record) => {
                        let _ = write!(
                            out,
                            " {:.1} | {:.1} | {:.1} | {} | {} |",
                            record.presented_per_s(),
                            record.server_cpu_us_per_frame(),
                            record.client_cpu_us_per_frame(),
                            stat_cell(record, "paint_us_mean", 1),
                            verdict(record)
                        );
                    }
                    // A cell with no run is a gap in the sweep, and
                    // saying so beats leaving the reader to infer it.
                    None => {
                        let _ = write!(
                            out,
                            " {MISSING} | {MISSING} | {MISSING} | {MISSING} | {MISSING} |"
                        );
                    }
                }
            }
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// The row label inside a rate table: scenario, sweep point, whether
/// the window covered the screen, and whether it is the control — but
/// never a size.
///
/// [`Record::label`] puts the geometry in, which is right everywhere
/// else and wrong here: a rate row spans arms whose screens are
/// *different sizes* (1920x1080 at 60 and 120, 1280x720 at 240), so one
/// arm's geometry printed over all three would be a false label on two
/// thirds of the row. `fullscreen` is the honest name for what is held
/// constant.
///
/// The `control` suffix matters for the same reason the key carries it:
/// two `rects n=500 640x480` rows in one table, identical but for the
/// shell being up, are indistinguishable without it — and a reader who
/// cannot tell which is which will read the control as the baseline.
fn rate_row_label(record: &Record) -> String {
    let mut out = record.scenario.clone();
    if record.n != 0 {
        let _ = write!(out, " n={}", record.n);
    }
    if record.is_fullscreen() {
        out.push_str(" fullscreen");
    } else {
        if record.size != 0 {
            let _ = write!(out, " size={}", record.size);
        }
        out.push_str(" 640x480");
    }
    if record.is_control() {
        out.push_str(" (control: shell down)");
    }
    out
}

/// One table: a `### title` heading, the header, and a row per record, in
/// the order the records arrived.
///
/// Arrival order and not a sort by any column: the ledger's order is the
/// order the sweep ran, which is monotonic in `n` or `size` by
/// construction, and re-sorting it by a *measured* column would let a
/// noisy run reorder the table and make two reports of the same data look
/// like different data.
#[must_use]
pub fn table(title: &str, records: &[Record]) -> String {
    let mut out = format!("### {title}\n\n{HEADER}\n{SEPARATOR}\n");
    for record in records {
        let _ = writeln!(
            out,
            "| {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {} | {} | {:.0} | {} | {} |",
            record.label(),
            record.presented_per_s(),
            record.commits_per_s(),
            record.mutations_per_frame(),
            record.server_cpu_us_per_frame(),
            record.client_cpu_us_per_frame(),
            paint_cell(record),
            stat_cell(record, "copy_us_mean", 1),
            stat_cell(record, "damage_px_mean", 0),
            record.bytes_per_frame(),
            record.flip_max_rise_us(),
            verdict(record),
        );
    }
    out.push('\n');
    out
}

/// The rate comparison as one index table, or an empty string when no
/// sweep point was run at two refresh rates.
///
/// Only the three columns that answer the question are carried over:
/// server cost per frame, the rate achieved, and the verdict. Everything
/// else is already in the scenario tables above and in [`rate_tables`],
/// and a wide pivot would hide the one comparison it exists to make.
///
/// Rates are named by their rounded hertz rather than by millihertz, so
/// the columns read `60 Hz`, `120 Hz` and `240 Hz`; a mode that is really
/// 119.982 therefore shares a column with a true 120, which is the
/// grouping a human means.
#[must_use]
pub fn refresh_pivot(records: &[Record]) -> String {
    // `BTreeMap` keyed by the sweep point, then by rate: deterministic
    // order without sorting floats, and the rate order falls out
    // ascending, which is the direction the question is asked in.
    let mut by_key: BTreeMap<RateKey, ByRate> = BTreeMap::new();
    let mut seen = Vec::new();
    for record in records {
        let key = rate_key(record);
        if !by_key.contains_key(&key) {
            seen.push(key.clone());
        }
        // Last run wins for a repeated (key, rate): a re-run is a
        // correction, not a second opinion.
        by_key
            .entry(key)
            .or_default()
            .insert(rate_of(record), record);
    }
    let pivots: Vec<_> = seen
        .into_iter()
        .filter_map(|key| {
            let rates = by_key.get(&key)?;
            (rates.len() >= 2).then_some((key, rates))
        })
        .collect();
    if pivots.is_empty() {
        return String::new();
    }
    let mut rates: Vec<u64> = pivots
        .iter()
        .flat_map(|(_, rates)| rates.keys().copied())
        .collect();
    rates.sort_unstable();
    rates.dedup();

    let mut out = String::from("### refresh pivot\n\n");
    out.push_str("| run |");
    for rate in &rates {
        let _ = write!(
            out,
            " {rate} Hz server µs/frame | {rate} Hz presented/s | {rate} Hz verdict |"
        );
    }
    out.push('\n');
    out.push_str("|---|");
    for _ in &rates {
        out.push_str("---|---|---|");
    }
    out.push('\n');
    for ((scenario, n, size, _, _), by_rate) in pivots {
        let label = by_rate.values().next().map_or_else(
            || format!("{scenario} n={n} size={size}"),
            |r| rate_row_label(r),
        );
        let _ = write!(out, "| {label} |");
        for rate in &rates {
            match by_rate.get(rate) {
                Some(record) => {
                    let _ = write!(
                        out,
                        " {:.1} | {:.1} | {} |",
                        record.server_cpu_us_per_frame(),
                        record.presented_per_s(),
                        verdict(record)
                    );
                }
                // A cell with no run is a gap in the sweep, and saying so
                // beats leaving the reader to infer it from a blank.
                None => {
                    let _ = write!(out, " {MISSING} | {MISSING} | {MISSING} |");
                }
            }
        }
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Records grouped by scenario, families in order of first appearance.
///
/// First appearance rather than alphabetical: the ledger's order is the
/// order the suite ran, which groups the micro-benchmarks before the
/// effects because that is how the suite is written, and alphabetising
/// would interleave `boing-node` with `bitblt` for no reader's benefit.
fn group_by_scenario(records: &[Record]) -> Vec<(String, Vec<Record>)> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<Record>> = BTreeMap::new();
    for record in records {
        if !groups.contains_key(&record.scenario) {
            order.push(record.scenario.clone());
        }
        groups
            .entry(record.scenario.clone())
            .or_default()
            .push(record.clone());
    }
    order
        .into_iter()
        .filter_map(|name| groups.remove(&name).map(|group| (name, group)))
        .collect()
}

/// Millihertz as whole hertz, for a column heading.
fn hz(mhz: u64) -> u64 {
    // Round rather than truncate so 59 940 mHz reads as 60 Hz, which is
    // the mode a human would name it by.
    (mhz + 500) / 1_000
}

/// One `stats_after` counter, formatted, or [`MISSING`].
fn stat_cell(record: &Record, key: &str, decimals: usize) -> String {
    record.stat_after(key).map_or_else(
        || MISSING.to_owned(),
        |value| format!("{:.*}", decimals, value as f64),
    )
}

/// The `paint µs mean/max` cell: two counters in one column, each
/// independently present or `?`.
///
/// Both in one cell because the pair is the interesting thing — a mean of
/// 400 µs with a max of 20 000 is a completely different machine from a
/// mean of 400 with a max of 600 — and independently missing because a
/// server that reports one and not the other should still contribute the
/// one.
fn paint_cell(record: &Record) -> String {
    format!(
        "{}/{}",
        stat_cell(record, "paint_us_mean", 1),
        stat_cell(record, "paint_us_max", 1)
    )
}

/// The verdict cell: `ok`, `**dropped**` or `**slow**`.
///
/// The order of the tests is the order of blame. A flip gap is the
/// server's problem and is checked first, because a run that both dropped
/// frames and ran slow is a dropped-frame story. Only then does a rate
/// well under the refresh *without* a gap mean the client was the limit:
/// the server flipped on time every time, there was simply nothing new to
/// flip. Anything else that is not keeping up gets no bold — it is
/// within noise of the cap, and shouting about it would train the reader
/// to ignore the column.
fn verdict(record: &Record) -> &'static str {
    if record.dropped() {
        return "**dropped**";
    }
    if record.kept_up() {
        return "ok";
    }
    if record.presented_per_s() < SLOW_FRACTION * record.refresh_hz() {
        return "**slow**";
    }
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run that keeps up at 60 Hz with every server counter present.
    fn healthy(scenario: &str, n: u64) -> Record {
        Record {
            scenario: scenario.to_owned(),
            n,
            width: 1_920,
            height: 1_080,
            refresh_mhz: 60_000,
            seconds: 10.0,
            commits: 600,
            presented: 600,
            frames_received: 600,
            mutations: 1_200,
            tx_bytes: 60_000,
            client_cpu_us: 600_000,
            server_cpu_us: 1_200_000,
            stats_after: [
                ("paint_us_mean".to_owned(), 420_u64),
                ("paint_us_max".to_owned(), 900),
                ("copy_us_mean".to_owned(), 130),
                ("damage_px_mean".to_owned(), 250_000),
                ("flip_interval_max_us".to_owned(), 16_700),
            ]
            .into_iter()
            .collect(),
            ..Record::default()
        }
    }

    #[test]
    fn a_table_has_a_header_and_a_row_per_record() {
        let records = [
            healthy("rects", 10),
            healthy("rects", 100),
            healthy("rects", 1_000),
        ];
        let out = table("rects", &records);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "### rects");
        assert_eq!(lines[2], HEADER);
        assert_eq!(lines[3], SEPARATOR);
        let rows = lines.iter().filter(|l| l.starts_with("| rects")).count();
        assert_eq!(rows, 3);
        assert!(out.ends_with("\n\n"), "every table ends with a blank line");
        // The fixture is a 1920x1080 run, so the label carries its
        // geometry rather than a bare `size=` that would read as square.
        assert!(out.contains("| rects n=1000 1920x1080 |"), "{out}");
    }

    #[test]
    fn a_missing_stats_key_prints_a_question_mark_and_not_a_zero() {
        let mut record = healthy("plasma", 0);
        record.stats_after.remove("copy_us_mean");
        record.stats_after.remove("paint_us_max");
        let out = table("plasma", &[record]);
        let row = out.lines().find(|l| l.starts_with("| plasma")).unwrap();
        assert!(row.contains("420.0/?"), "{row}");
        assert!(row.contains("| ? |"), "{row}");
        assert!(
            !row.contains("| 0.0 | 250000 |"),
            "a gap must not read as zero: {row}"
        );
    }

    #[test]
    fn the_three_verdicts_each_fire() {
        let ok = healthy("rects", 10);
        assert_eq!(verdict(&ok), "ok");

        let mut dropped = healthy("rects", 10);
        dropped
            .stats_after
            .insert("flip_interval_max_us".to_owned(), 34_000);
        assert_eq!(verdict(&dropped), "**dropped**");

        // Half the rate with every flip on time: the client is the limit.
        let mut slow = healthy("rects", 10);
        slow.presented = 300;
        slow.commits = 300;
        assert_eq!(verdict(&slow), "**slow**");

        let out = table("rects", &[ok, dropped, slow]);
        assert!(out.contains("**dropped**") && out.contains("**slow**") && out.contains(" ok |"));
    }

    #[test]
    fn the_pivot_appears_only_with_two_refresh_rates_and_names_both() {
        let sixty = healthy("plasma", 0);
        assert!(refresh_pivot(std::slice::from_ref(&sixty)).is_empty());

        let mut fast = sixty.clone();
        fast.refresh_mhz = 120_000;
        fast.presented = 900;
        fast.commits = 900;
        let pivot = refresh_pivot(&[sixty.clone(), fast.clone()]);
        assert!(
            pivot.contains("60 Hz") && pivot.contains("120 Hz"),
            "{pivot}"
        );
        assert!(pivot.contains("### refresh pivot"), "{pivot}");
        let rows = pivot.lines().filter(|l| l.starts_with("| plasma")).count();
        assert_eq!(rows, 1, "{pivot}");
        assert!(pivot.ends_with("\n\n"), "{pivot}");

        // A different sweep point is a different key and does not pivot
        // against the first one.
        let mut other = fast.clone();
        other.n = 7;
        assert!(refresh_pivot(&[sixty, other]).is_empty());
    }

    #[test]
    fn a_pivot_gap_is_named_not_blank() {
        let a = healthy("rects", 1);
        let mut b = healthy("rects", 1);
        b.refresh_mhz = 120_000;
        let mut c = healthy("fire", 0);
        c.refresh_mhz = 144_000;
        let mut d = healthy("fire", 0);
        d.refresh_mhz = 60_000;
        let pivot = refresh_pivot(&[a, b, c, d]);
        assert!(pivot.contains("144 Hz"), "{pivot}");
        let rects = pivot.lines().find(|l| l.starts_with("| rects")).unwrap();
        assert!(rects.contains("| ? | ? | ? |"), "{rects}");
    }

    /// The 240 Hz arm runs at 1280×720 while the 60 and 120 Hz arms run
    /// at 1920×1080, so a comparison keyed on the geometry would file
    /// the same scenario as three unrelated rows and print the answer as
    /// gaps.
    ///
    /// This is the property the whole rate table turns on, and it is the
    /// one a size-keyed version got confidently wrong: three full
    /// columns of data, no row with more than one of them, and a table
    /// that reads as "nothing was measured twice".
    #[test]
    fn a_fullscreen_row_pairs_across_rates_despite_a_different_screen() {
        let sixty = healthy("boing", 0);
        let mut onetwenty = healthy("boing", 0);
        onetwenty.refresh_mhz = 120_000;
        let mut twoforty = healthy("boing", 0);
        twoforty.refresh_mhz = 240_000;
        twoforty.width = 1_280;
        twoforty.height = 720;
        let records = [sixty, onetwenty, twoforty];

        let tables = rate_tables(&records);
        assert!(tables.contains("#### boing across rates"), "{tables}");
        let row = tables
            .lines()
            .find(|l| l.starts_with("| boing"))
            .unwrap_or_default();
        assert!(
            !row.contains(MISSING),
            "all three rates land in one row: {row}"
        );
        assert_eq!(
            tables.lines().filter(|l| l.starts_with("| boing")).count(),
            1,
            "{tables}"
        );
        // The label names what is held constant, never one arm's size:
        // `1920x1080` over a row two thirds of which is 1280x720 would
        // be a false label rather than a missing one.
        assert!(row.contains("| boing fullscreen |"), "{row}");
        assert!(
            !row.contains("1920x1080") && !row.contains("1280x720"),
            "{row}"
        );

        // And the pivot pairs them on the same key.
        let pivot = refresh_pivot(&records);
        let pivot_row = pivot.lines().find(|l| l.starts_with("| boing")).unwrap();
        assert!(!pivot_row.contains(MISSING), "{pivot_row}");
    }

    /// A fullscreen arm and a 640×480 arm of the same scenario are two
    /// different measurements and must not collapse into one row — the
    /// other half of the key, and the failure a purely
    /// `(scenario, n)` key would introduce while fixing the first one.
    #[test]
    fn the_vga_arm_and_the_fullscreen_arm_stay_separate_rows() {
        let mut vga = healthy("starfield", 500);
        vga.width = 640;
        vga.height = 480;
        let mut vga_fast = vga.clone();
        vga_fast.refresh_mhz = 120_000;
        let full = healthy("starfield", 500);
        let mut full_fast = full.clone();
        full_fast.refresh_mhz = 120_000;

        let tables = rate_tables(&[vga, vga_fast, full, full_fast]);
        let rows: Vec<&str> = tables
            .lines()
            .filter(|l| l.starts_with("| starfield"))
            .collect();
        assert_eq!(rows.len(), 2, "{tables}");
        assert!(rows.iter().any(|r| r.contains("640x480")), "{tables}");
        assert!(rows.iter().any(|r| r.contains("fullscreen")), "{tables}");
    }

    /// Each rate table states the budget its verdicts were drawn
    /// against, and a scenario measured at one rate only is skipped
    /// rather than printed as a row of `?`.
    #[test]
    fn a_rate_table_names_its_budgets_and_skips_the_unpaired() {
        let sixty = healthy("rects", 10);
        let mut fast = healthy("rects", 10);
        fast.refresh_mhz = 120_000;
        let lonely = healthy("plasma", 0);

        let tables = rate_tables(&[sixty, fast, lonely]);
        assert!(tables.contains("60 Hz = 16667 µs"), "{tables}");
        assert!(tables.contains("120 Hz = 8333 µs"), "{tables}");
        assert!(
            !tables.contains("#### plasma"),
            "one rate is not a comparison: {tables}"
        );

        // Nothing paired at all is an empty string, not an empty table.
        assert!(rate_tables(&[healthy("rects", 10)]).is_empty());
    }

    /// The budget line must cover every column the **header** has, not
    /// every rate the family's **first row** happens to carry.
    ///
    /// This is the shipped `rects` table's shape: its first row (n=10)
    /// ran only at 60 and 120, while later rows add a 240 Hz cell. The
    /// first version built the caption from `rows.first()` and so
    /// printed a full 240 Hz column of data under a budget line naming
    /// only 60 and 120 — a caption wrong about the one column §9.3
    /// draws its "how many mutations fit in 4.2 ms" answer from.
    #[test]
    fn the_budget_line_covers_every_column_not_just_the_first_rows() {
        // n=10: 60 and 120 only — the unpaired-at-240 first row.
        let small = healthy("rects", 10);
        let mut small_fast = healthy("rects", 10);
        small_fast.refresh_mhz = 120_000;
        // n=100: all three, so the table grows a 240 Hz column.
        let big = healthy("rects", 100);
        let mut big_fast = healthy("rects", 100);
        big_fast.refresh_mhz = 120_000;
        let mut big_fastest = healthy("rects", 100);
        big_fastest.refresh_mhz = 239_840;

        let tables = rate_tables(&[small, small_fast, big, big_fast, big_fastest]);
        assert!(tables.contains("240 presented/s"), "{tables}");
        assert!(
            tables.contains("240 Hz = 4169 µs"),
            "a column with data needs its budget in the caption: {tables}"
        );
        // And the caption is still complete for the other two.
        assert!(tables.contains("60 Hz = 16667 µs"), "{tables}");
        assert!(tables.contains("120 Hz = 8333 µs"), "{tables}");
    }

    /// The 240 Hz column's budget is 4 167 µs, which is the number §9
    /// judges every 720p row against.
    #[test]
    fn the_240_hz_budget_is_the_one_the_document_quotes() {
        let sixty = healthy("fire", 0);
        let mut fast = healthy("fire", 0);
        fast.refresh_mhz = 239_840;
        let tables = rate_tables(&[sixty, fast]);
        assert!(tables.contains("240 Hz = 4169 µs"), "{tables}");
        // 239.840 Hz is filed under the 240 Hz column, because that is
        // the mode a human names — and the rate no connector offers.
        assert!(tables.contains("240 presented/s"), "{tables}");
    }

    /// A fullscreen run's `size` is an **output**, not a sweep point,
    /// and it differs per rate arm — so the key must ignore it.
    ///
    /// Measured values from the real ledger: the pixel scenarios record
    /// the screen width (1920 at 60 and 120, 1280 at 240) and
    /// `boing-node` records the ball's on-screen diameter (389 against
    /// 260). Keying on `size` re-introduced the geometry through a field
    /// that does not have `width` in its name, and scattered every
    /// fullscreen row across three keys — an all-`?` table that looks
    /// exactly like "the 240 Hz arm never ran".
    #[test]
    fn a_fullscreen_runs_size_is_an_output_and_does_not_split_the_row() {
        let mut sixty = healthy("boing-node", 0);
        sixty.fullscreen = true;
        sixty.size = 389;
        let mut fast = sixty.clone();
        fast.refresh_mhz = 239_840;
        fast.width = 1_280;
        fast.height = 720;
        fast.size = 260;

        let tables = rate_tables(&[sixty, fast]);
        let rows: Vec<&str> = tables
            .lines()
            .filter(|l| l.starts_with("| boing-node"))
            .collect();
        assert_eq!(rows.len(), 1, "{tables}");
        assert!(!rows[0].contains(MISSING), "{}", rows[0]);
        // And neither arm's `size` is printed, because neither is true
        // of the other two thirds of the row.
        assert!(
            !rows[0].contains("389") && !rows[0].contains("260"),
            "{}",
            rows[0]
        );
    }

    /// The control arm — one `rects n=500` per rate with the shell
    /// clients killed — is otherwise identical to the measured row, so
    /// "last run wins" silently replaced every rate's real row with its
    /// control.
    ///
    /// That is the worst shape of wrong: a plausible number, in the
    /// right units, in the right cell, reporting the shell's *absence*
    /// as the baseline that every other row was taken with it present.
    #[test]
    fn the_control_arm_does_not_overwrite_the_row_it_controls() {
        let sixty = healthy("rects", 500);
        let mut fast = healthy("rects", 500);
        fast.refresh_mhz = 120_000;
        let mut control = healthy("rects", 500);
        control.note = "control: shell clients killed; 4 procs".to_owned();
        control.server_cpu_us = sixty.server_cpu_us / 2;
        let mut control_fast = control.clone();
        control_fast.refresh_mhz = 120_000;

        let tables = rate_tables(&[sixty, fast, control, control_fast]);
        let rows: Vec<&str> = tables
            .lines()
            .filter(|l| l.starts_with("| rects"))
            .collect();
        assert_eq!(rows.len(), 2, "the control is its own row: {tables}");
        assert!(
            rows.iter().filter(|r| r.contains("(control:")).count() == 1,
            "{tables}"
        );
        // The measured row keeps its own cost, rather than inheriting
        // the control's cheaper one.
        let measured = rows.iter().find(|r| !r.contains("(control:")).unwrap();
        assert!(measured.contains("2000.0"), "{measured}");
    }

    #[test]
    fn the_report_groups_by_scenario_in_first_appearance_order() {
        let records = [
            healthy("rects", 10),
            healthy("plasma", 0),
            healthy("rects", 100),
        ];
        let out = markdown(&records);
        assert!(out.starts_with("<!-- generated by `nitro-bench report`: 3 runs -->"));
        let rects = out.find("### rects").unwrap();
        let plasma = out.find("### plasma").unwrap();
        assert!(rects < plasma, "{out}");
        assert_eq!(out.matches("### rects").count(), 1);
    }

    #[test]
    fn an_empty_ledger_is_an_honest_sentence() {
        let out = markdown(&[]);
        assert!(out.contains("No runs"), "{out}");
        assert!(!out.contains(HEADER), "{out}");
    }

    #[test]
    fn a_single_run_is_not_pluralised() {
        let out = markdown(&[healthy("rects", 1)]);
        assert!(
            out.starts_with("<!-- generated by `nitro-bench report`: 1 run -->"),
            "{out}"
        );
    }

    #[test]
    fn millihertz_rounds_to_the_mode_a_human_names() {
        assert_eq!(hz(60_000), 60);
        assert_eq!(hz(59_940), 60);
        assert_eq!(hz(120_000), 120);
        assert_eq!(hz(0), 0);
    }
}

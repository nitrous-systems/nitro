//! `nitro-bench`: run one scenario, or turn a ledger into a table.
//!
//! See the crate docs in `lib.rs` for what is measured and why, and
//! `docs/bench.md` for the results and the verdicts.
//!
//! ```text
//! ssh box 'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-bench rects --n 1000 --json'
//! ```

use std::fmt::Write as _;
use std::io::Write as _;
use std::process::ExitCode;

use nitro_bench::args::{self, Args, Command, USAGE};
use nitro_bench::harness::{RunConfig, run};
use nitro_bench::record::{Record, read_jsonl};
use nitro_bench::{SCENARIOS, bandwidth, report, scenario};

fn main() -> ExitCode {
    let cmd = match args::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match dispatch(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nitro-bench: {e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cmd: Command) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Command::Help => emit(USAGE),
        Command::List => list(),
        Command::Bandwidth { json } => bandwidth_probe(json),
        Command::Report { path } => report_file(&path),
        Command::Run(a) => run_scenario(&a),
    }
}

/// The scenario table, with the x11perf operation each ports.
fn list() -> Result<(), Box<dyn std::error::Error>> {
    for (name, why) in SCENARIOS {
        emit(&format!("{name:<16} {why}"))?;
    }
    Ok(())
}

/// Measure this machine's memory bandwidth.
///
/// A separate subcommand rather than a column on every run: it takes a
/// second, it does not change between runs on the same machine, and the
/// pixel-path verdicts need it once. `deploy/bench.sh` calls it first and
/// puts the line at the top of the ledger.
fn bandwidth_probe(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let probe = bandwidth::probe();
    if json {
        let mut s = String::from("{\"kind\":\"bandwidth\"");
        for (name, b) in &probe {
            write!(
                s,
                ",\"{name}_bytes_per_s\":{:?},\"{name}_gb_per_s\":{:?},\"{name}_1080p_fps\":{:?}",
                b.bytes_per_second(),
                b.gb_per_second(),
                b.frames_1080p()
            )?;
        }
        // The checksums are printed so the reader can see the loops were
        // not optimised away — the #3711 rule: a measurement of a
        // deleted loop reports a spectacular number for nothing.
        for (name, b) in &probe {
            write!(s, ",\"{name}_checksum\":{}", b.checksum)?;
        }
        s.push('}');
        return emit(&s);
    }
    emit("memory bandwidth (64 MB buffers, 8 rounds; a 1080p BGRA frame is 8 294 400 B)")?;
    for (name, b) in &probe {
        emit(&format!(
            "  {name:<6} {:>7.2} GB/s  = {:>6.0} 1080p frames/s  (checksum {:#018x})",
            b.gb_per_second(),
            b.frames_1080p(),
            b.checksum
        ))?;
    }
    Ok(())
}

/// Read a `.jsonl` ledger and print the markdown for `docs/bench.md`.
///
/// Bad lines are named on stderr and the good ones are still reported: a
/// table of thirty-nine runs is worth more than an error about the
/// fortieth, and a silently dropped row is the one failure mode a
/// benchmark must not have.
fn report_file(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let (records, errors) = read_jsonl(&text);
    for e in &errors {
        eprintln!("{path}: {e}");
    }
    emit(&report::markdown(&records))
}

/// Run one scenario and print its record.
fn run_scenario(a: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = RunConfig {
        seconds: a.seconds,
        size: a.window,
        fullscreen: a.fullscreen,
        note: a.note.clone(),
        // Taken from the environment rather than from `git` at runtime:
        // the binary runs on a box that has a checkout at a different
        // commit from the one it was built from, and `deploy/bench.sh`
        // knows which sha it shipped.
        sha: std::env::var("NITRO_BENCH_SHA").unwrap_or_default(),
        host: std::env::var("NITRO_BENCH_HOST").unwrap_or_else(|_| hostname()),
    };
    // The scenario is built for the size the *command line* asked for; a
    // fullscreen run rebuilds it inside `build` once the server has
    // configured the window, which is the only moment the real size is
    // known.
    let want = cfg.size.unwrap_or(nitro_bench::harness::PERIOD_SIZE);
    let mut s = scenario(&a.scenario, a.n, a.size, want.w as u32, want.h as u32)?;
    let rec = run(s.as_mut(), &cfg, a.n, a.size)?;
    if a.json {
        emit(&rec.to_json())
    } else {
        emit(&human(&rec))
    }
}

/// The one-line human rendering of a record.
///
/// Deliberately the same numbers the table's columns carry, in the same
/// units, so a reader watching a run go past and a reader reading
/// `docs/bench.md` are looking at the same measurement rather than two
/// renderings that could disagree.
fn human(r: &Record) -> String {
    format!(
        "{}: {:.1}s  {:.1} presented/s ({:.0} Hz budget {:.1} ms)  {:.0} mutations/frame  \
         server {:.0} us/frame  client {:.0} us/frame  {:.0} B/frame  \
         effect {:.0} us  upload {:.0} us  paint {} us  damage {} px  verdict {}",
        r.label(),
        r.seconds,
        r.presented_per_s(),
        r.refresh_hz(),
        r.frame_budget_us() / 1000.0,
        r.mutations_per_frame(),
        r.server_cpu_us_per_frame(),
        r.client_cpu_us_per_frame(),
        r.bytes_per_frame(),
        r.compute_us_per_frame(),
        r.upload_us_per_frame(),
        r.stat_after("paint_us_mean")
            .map_or_else(|| "?".to_owned(), |v| v.to_string()),
        r.stat_after("damage_px_mean")
            .map_or_else(|| "?".to_owned(), |v| v.to_string()),
        if r.dropped() {
            "DROPPED"
        } else if r.kept_up() {
            "ok"
        } else {
            "slow"
        }
    )
}

/// This machine's hostname, or `unknown`.
///
/// Read from `/etc/hostname` rather than through `gethostname`: the
/// syscall wrapper wants a fixed buffer and an `unsafe` block this tree
/// denies, and the file is what every distribution writes. A benchmark
/// whose host column is missing is a benchmark whose rows cannot be
/// compared across machines, which is most of what this ledger is for.
fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map_or_else(|_| "unknown".to_owned(), |s| s.trim().to_owned())
}

/// Print one line and flush, so output survives a pipe (`ssh`, `tee`).
fn emit(line: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

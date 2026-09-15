//! The command line.
//!
//! Parsing is a free function over an iterator so it is testable without a
//! process — the same shape `nitro-demo` and `nitro-shot` use, and the
//! reason every flag below has a test rather than a manual check.

use std::fmt;

use nitro_core::Size;

/// What the binary was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run one scenario and print (or append) its record.
    Run(Box<Args>),
    /// Turn a `.jsonl` ledger into the markdown `docs/bench.md` wants.
    Report {
        /// The ledger file.
        path: String,
    },
    /// Measure this machine's memory bandwidth — the denominator the
    /// pixel-path verdicts need.
    Bandwidth {
        /// Emit a JSON object rather than a table.
        json: bool,
    },
    /// List the scenarios, with the x11perf name each ports.
    List,
    /// Print the usage text and exit successfully.
    Help,
}

/// Parsed arguments of a `run`.
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    /// Scenario name.
    pub scenario: String,
    /// Length of the measured window in seconds.
    pub seconds: f64,
    /// The sweep point: node count, star count, ball count, text count.
    pub n: u64,
    /// The other sweep point: buffer edge in pixels, for `putimage`.
    pub size: u32,
    /// Window size in logical pixels, or `None` for the period-correct
    /// 640×480 default.
    pub window: Option<Size>,
    /// Open the window fullscreen and take whatever the server gives.
    pub fullscreen: bool,
    /// Emit one JSON object rather than a human line.
    pub json: bool,
    /// Free text recorded with the run: which shell was up, whether this
    /// is the control arm.
    pub note: String,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            scenario: String::new(),
            seconds: 5.0,
            n: 0,
            size: 0,
            window: None,
            fullscreen: false,
            json: false,
            note: String::new(),
        }
    }
}

impl fmt::Display for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} seconds={} n={} size={} fullscreen={}",
            self.scenario, self.seconds, self.n, self.size, self.fullscreen
        )
    }
}

/// Usage text, printed for `--help` and for a bad argument.
pub const USAGE: &str = "\
usage: nitro-bench <scenario> [options]
       nitro-bench report <file.jsonl>
       nitro-bench bandwidth [--json]
       nitro-bench list

Scenarios (x11perf ports):
  rects           N Rect nodes recoloured every frame     (x11perf -rect*)
  rects-move      N Rect nodes moved every frame          (x11perf -rect*)
  text            N Text nodes relabelled every frame     (x11perf -ftext)
  text-static     N Text nodes moved, never relabelled    (the retained win)
  putimage        an S x S client buffer re-uploaded      (x11perf -putimage*)
  scroll          a tall clipped column scrolled a row    (x11perf -scroll500)
  create          N nodes created and destroyed per frame (x11perf -create)

Scenarios (demo effects):
  plasma          sine-sum palette animation, fullscreen buffer
  fire            Doom-style cellular fire, fullscreen buffer
  rotozoom        texture rotate+zoom, fullscreen buffer
  boing           the Amiga Boing ball, redrawn into a buffer
  boing-node      the same ball as one moved Image node    (the comparison)
  starfield       N stars in a fullscreen buffer
  starfield-nodes the same N stars as N moved Rect nodes   (the comparison)
  balls           N bouncing circles in a buffer
  balls-nodes     the same N as rounded Rect nodes

Options:
  --seconds S     length of the measured window (default 5)
  --n N           sweep point: node/star/ball/label count
  --size S        buffer edge in pixels, for putimage
  --window WxH    window size in logical pixels (default 640x480)
  --fullscreen    open fullscreen and use the size the server gives
  --json          emit one JSON object instead of a human line
  --note TEXT     free text recorded with the run
  -h, --help      this text

Environment: NITRO_SOCKET, NITRO_CONTROL, NITRO_BENCH_SHA, NITRO_BENCH_HOST.";

/// Parse `args` (without the program name).
///
/// # Errors
/// A message ready to print: unknown flag, missing value, or `--help`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut it = args.into_iter().peekable();
    let Some(first) = it.next() else {
        return Err(USAGE.to_owned());
    };
    match first.as_str() {
        "-h" | "--help" => return Ok(Command::Help),
        "list" => return Ok(Command::List),
        "report" => {
            let path = it.next().ok_or("report needs a FILE\n")?;
            return Ok(Command::Report { path });
        }
        "bandwidth" => {
            let json = it.next().is_some_and(|a| a == "--json");
            return Ok(Command::Bandwidth { json });
        }
        _ => {}
    }
    if first.starts_with('-') {
        return Err(format!("expected a scenario name, got {first:?}\n{USAGE}"));
    }

    let mut out = Args {
        scenario: first,
        ..Args::default()
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seconds" => out.seconds = float(&mut it, "--seconds")?,
            "--n" => out.n = number(&mut it, "--n")?,
            "--size" => out.size = number(&mut it, "--size")? as u32,
            "--window" => out.window = Some(size(&mut it)?),
            "--fullscreen" => out.fullscreen = true,
            "--json" => out.json = true,
            "--note" => out.note = it.next().ok_or("--note needs TEXT")?,
            "-h" | "--help" => return Ok(Command::Help),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if out.seconds <= 0.0 {
        return Err("--seconds must be positive".to_owned());
    }
    Ok(Command::Run(Box::new(out)))
}

/// Read the next argument as a `u64`.
fn number(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    let raw = it.next().ok_or_else(|| format!("{flag} needs a number"))?;
    raw.parse()
        .map_err(|_| format!("{flag}: {raw:?} is not a number"))
}

/// Read the next argument as an `f64`, so `--seconds 2.5` works.
fn float(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<f64, String> {
    let raw = it.next().ok_or_else(|| format!("{flag} needs a number"))?;
    raw.parse()
        .map_err(|_| format!("{flag}: {raw:?} is not a number"))
}

/// Read the next argument as `WxH`.
fn size(it: &mut impl Iterator<Item = String>) -> Result<Size, String> {
    let raw = it.next().ok_or("--window needs WxH")?;
    let (w, h) = raw
        .split_once('x')
        .ok_or_else(|| format!("--window: {raw:?} is not WxH"))?;
    let w: f32 = w
        .parse()
        .map_err(|_| format!("--window: {raw:?} is not WxH"))?;
    let h: f32 = h
        .parse()
        .map_err(|_| format!("--window: {raw:?} is not WxH"))?;
    if w <= 0.0 || h <= 0.0 {
        return Err(format!("--window: {raw:?} has a zero side"));
    }
    Ok(Size::new(w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Result<Command, String> {
        parse(s.split_whitespace().map(str::to_owned))
    }

    /// Exact equality for a parsed number is the honest check — `--seconds
    /// 12.5` produced that value by `str::parse`, not by arithmetic that
    /// could round — but clippy's `float_cmp` cannot tell the two apart.
    /// The same shape `nitro-ui`'s layout tests use, for the same reason.
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn run(s: &str) -> Args {
        match cmd(s).unwrap() {
            Command::Run(a) => *a,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_scenario_is_a_five_second_vga_run() {
        let a = run("plasma");
        assert_eq!(a.scenario, "plasma");
        assert!(close(a.seconds, 5.0));
        assert_eq!((a.n, a.size), (0, 0));
        assert_eq!(a.window, None);
        assert!(!a.fullscreen && !a.json);
    }

    #[test]
    fn every_flag_is_parsed() {
        let a =
            run("rects --seconds 12.5 --n 1000 --size 500 --window 1920x1080 --fullscreen --json");
        assert_eq!(a.scenario, "rects");
        assert!(close(a.seconds, 12.5));
        assert_eq!((a.n, a.size), (1000, 500));
        assert_eq!(a.window, Some(Size::new(1920.0, 1080.0)));
        assert!(a.fullscreen && a.json);
    }

    /// The note carries a whole sentence, which is what the box protocol
    /// wants recorded next to a number ("bar+launcher up", "control arm").
    #[test]
    fn the_note_takes_one_argument_verbatim() {
        let a = parse(
            ["rects", "--note", "bar + launcher up"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        let Command::Run(a) = a else { panic!() };
        assert_eq!(a.note, "bar + launcher up");
    }

    #[test]
    fn the_subcommands_are_recognised() {
        assert_eq!(
            cmd("report tmp/bench/abc.jsonl").unwrap(),
            Command::Report {
                path: "tmp/bench/abc.jsonl".to_owned()
            }
        );
        assert_eq!(
            cmd("bandwidth").unwrap(),
            Command::Bandwidth { json: false }
        );
        assert_eq!(
            cmd("bandwidth --json").unwrap(),
            Command::Bandwidth { json: true }
        );
        assert_eq!(cmd("list").unwrap(), Command::List);
        assert_eq!(cmd("--help").unwrap(), Command::Help);
    }

    #[test]
    fn bad_arguments_explain_themselves() {
        assert!(cmd("rects --n").unwrap_err().contains("needs a number"));
        assert!(cmd("rects --n x").unwrap_err().contains("not a number"));
        assert!(cmd("rects --window 640").unwrap_err().contains("WxH"));
        assert!(cmd("rects --window 0x480").unwrap_err().contains("zero"));
        assert!(cmd("rects --wat").unwrap_err().contains("unknown argument"));
        assert!(cmd("report").unwrap_err().contains("needs a FILE"));
        assert!(cmd("--wat").unwrap_err().contains("expected a scenario"));
        assert!(parse(std::iter::empty()).unwrap_err().contains("usage:"));
    }

    /// A zero-second run would divide by zero in every derived ratio; a
    /// negative one is a typo. Both are refused at the boundary rather
    /// than producing a record full of infinities.
    #[test]
    fn a_non_positive_run_length_is_refused() {
        assert!(cmd("rects --seconds 0").unwrap_err().contains("positive"));
        assert!(cmd("rects --seconds -3").unwrap_err().contains("positive"));
    }

    /// Every scenario the binary dispatches on must be named in the usage
    /// text, or `--help` is a lie the moment one is added.
    #[test]
    fn the_usage_names_every_scenario() {
        for s in crate::SCENARIOS {
            assert!(USAGE.contains(s.0), "{} is missing from the usage", s.0);
        }
    }
}

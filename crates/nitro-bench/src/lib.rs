//! `nitro-bench`: old-school graphics benchmarks on the nitro wire.
//!
//! # What this crate is for
//!
//! The tree had one measurement client — `nitro-demo`, which measures
//! **latency**: how long a pointer move takes to become a photon. That is
//! the number `docs/latency.md` is about and it is the right headline for
//! a desktop. It says nothing at all about **throughput**: how much
//! drawing per frame the server sustains, and what a frame costs.
//!
//! This crate is that second instrument, and it is built out of the
//! oldest benchmarks in the field on purpose:
//!
//! * **`x11perf`** (Joel `McCormack` and Keith Packard, 1988) — the X11
//!   operation micro-benchmarks. `-rect100`, `-ftext`, `-putimage500`,
//!   `-scroll500`, `-move`, `-create` are still the vocabulary people use
//!   for 2D performance. [`x11perf`] explains why a literal port is
//!   impossible against a retained scene graph and what the honest
//!   transposition is.
//! * **The demo effects** — the Amiga **Boing ball** (1984), sine-sum
//!   **plasma**, the **fire** of the `PlayStation` Doom (1997),
//!   **rotozoom**, **starfield**, bouncing **balls**. Every one of them
//!   is a fullscreen pixel pusher, which is exactly the workload a
//!   compositor built around "work proportional to change" is worst at —
//!   and that is why they are here. Three of them are *also* implemented
//!   as scene-graph mutations ([`nodes`]), driving the same simulation,
//!   so the two costs sit in one table.
//!
//! # The shape of a run
//!
//! One binary, one scenario per invocation, one JSON object out:
//!
//! ```text
//! nitro-bench rects --n 1000 --seconds 10 --json
//! nitro-bench plasma --fullscreen --seconds 10 --json
//! nitro-bench report tmp/bench/<sha>.jsonl
//! ```
//!
//! `deploy/bench.sh` runs the matrix on the box and appends to
//! `tmp/bench/<sha>.jsonl`; `nitro-bench report` turns that into the
//! markdown in `docs/bench.md`.
//!
//! # Module map
//!
//! * [`effects`] — the pure-compute demo effects. No sockets, no clock,
//!   deterministic, so a unit test can assert a frame checksum.
//! * [`harness`] — connect, open a window, run frame-paced for N seconds,
//!   account for the cost. Every scenario shares it, which is what makes
//!   them comparable.
//! * [`x11perf`] — the operation micro-benchmarks.
//! * [`pixels`] — the client-buffer path: an effect rendered straight
//!   into a mapped client buffer every frame.
//! * [`nodes`] — the retained path: the same effects as scene mutations.
//! * [`cpu`] — µs of CPU per presented frame, the number that does not
//!   saturate at 60 Hz.
//! * [`bandwidth`] — this machine's memcpy rate, the denominator every
//!   pixel-path verdict needs.
//! * [`control`] — the server's own `stats`, before and after.
//! * [`record`] / [`report`] — the JSON ledger and the table generator.

#![forbid(unsafe_code)]

pub mod args;
pub mod bandwidth;
pub mod control;
pub mod cpu;
pub mod effects;
pub mod harness;
pub mod nodes;
pub mod pixels;
pub mod record;
pub mod report;
pub mod x11perf;

use harness::{Error, Scenario};

/// Every scenario, with the x11perf operation it ports (or the demo it
/// is).
///
/// A table rather than a `match` arm and a separate list, because the two
/// would drift: `nitro-bench list`, the usage text's completeness test and
/// the dispatcher all read this, so a scenario that exists is a scenario
/// that is documented and dispatchable by construction.
pub const SCENARIOS: &[(&str, &str)] = &[
    ("rects", "x11perf -rect10/-rect100/-rect500, recoloured"),
    ("rects-move", "x11perf -rect*, moved (damage union)"),
    ("text", "x11perf -ftext/-f24text: relabelled every frame"),
    (
        "text-static",
        "no x11perf equivalent: moved, never reshaped",
    ),
    ("putimage", "x11perf -putimage100/-putimage500"),
    ("scroll", "x11perf -scroll500, as a retained clip offset"),
    ("create", "x11perf -create/-map"),
    ("plasma", "sine-sum plasma, fullscreen buffer"),
    ("fire", "Doom PSX 1997 cellular fire, fullscreen buffer"),
    ("rotozoom", "texture rotate+zoom, fullscreen buffer"),
    ("boing", "Amiga Boing ball (1984), redrawn per frame"),
    ("boing-node", "the same ball as one moved Image node"),
    ("starfield", "N stars, fullscreen buffer"),
    ("starfield-nodes", "the same N stars as N Rect nodes"),
    ("balls", "bouncing circles, fullscreen buffer"),
    ("balls-nodes", "the same circles as rounded Rect nodes"),
];

/// Build the scenario `name` asks for.
///
/// `n` and `size` are the sweep points; each scenario documents what its
/// `n` means and picks a defensible default when it is zero, so a bare
/// `nitro-bench rects` runs something rather than nothing.
///
/// # Errors
/// [`Error::UnknownScenario`] for a name not in [`SCENARIOS`].
pub fn scenario(
    name: &str,
    n: u64,
    size: u32,
    width: u32,
    height: u32,
) -> Result<Box<dyn Scenario>, Error> {
    let n_or = |d: u64| if n == 0 { d } else { n } as usize;
    Ok(match name {
        // x11perf's own sweep points were 10, 100 and 500 pixels on a
        // side; the default here is 100, the middle one and the only one
        // anybody quotes.
        "rects" => Box::new(x11perf::Rects::recolour(n_or(100), edge_or(size, 100.0))),
        "rects-move" => Box::new(x11perf::Rects::moving(n_or(100), edge_or(size, 100.0))),
        "text" => Box::new(x11perf::Text::relabelled(n_or(100), edge_or(size, 12.0))),
        "text-static" => Box::new(x11perf::Text::moved(n_or(100), edge_or(size, 12.0))),
        // x11perf's `-putimage100` and `-putimage500`; 500 is the one the
        // name is famous for, so it is the default.
        "putimage" => Box::new(pixels::PixelScenario::new(
            "putimage",
            Box::new(effects::Plasma::new()),
            if size == 0 { 500 } else { size },
        )),
        "scroll" => Box::new(nodes::Scroll::new(n_or(500), 16.0)),
        "create" => Box::new(nodes::CreateDestroy::new(n_or(50))),
        "plasma" => Box::new(pixels::PixelScenario::new(
            "plasma",
            Box::new(effects::Plasma::new()),
            size,
        )),
        "fire" => Box::new(pixels::PixelScenario::new(
            "fire",
            Box::new(effects::Fire::new(width, height)),
            size,
        )),
        "rotozoom" => Box::new(pixels::PixelScenario::new(
            "rotozoom",
            Box::new(effects::Rotozoom::new()),
            size,
        )),
        "boing" => Box::new(pixels::PixelScenario::new(
            "boing",
            Box::new(effects::Boing::new(width, height)),
            size,
        )),
        "boing-node" => Box::new(nodes::BoingNode::new(
            width,
            height,
            if size == 0 { 128 } else { size },
        )),
        "starfield" => Box::new(pixels::PixelScenario::new(
            "starfield",
            Box::new(effects::Starfield::new(n_or(500), width, height)),
            size,
        )),
        "starfield-nodes" => Box::new(nodes::StarNodes::new(n_or(500), width, height)),
        "balls" => Box::new(pixels::PixelScenario::new(
            "balls",
            Box::new(effects::Balls::new(n_or(32), width, height)),
            size,
        )),
        "balls-nodes" => Box::new(nodes::BallNodes::new(n_or(32), width, height)),
        other => return Err(Error::UnknownScenario(other.to_owned())),
    })
}

/// A sweep `--size` reinterpreted as an edge length, with a default.
///
/// The rect and text scenarios take their `--size` as "how big is one of
/// them", which is x11perf's convention (`-rect100` is a hundred-pixel
/// rect, `-f24text` is 24-point text). The pixel scenarios take it as a
/// buffer edge. One flag with two documented meanings beats two flags of
/// which every invocation uses one.
#[must_use]
fn edge_or(size: u32, default: f32) -> f32 {
    if size == 0 { default } else { size as f32 }
}

/// `CLOCK_MONOTONIC` nanoseconds, the clock every timestamp on the wire
/// uses.
#[must_use]
pub fn monotonic_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(t.tv_sec).unwrap_or(0) * 1_000_000_000 + u64::try_from(t.tv_nsec).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dispatcher and the table must agree in both directions, or
    /// `list` advertises something that cannot run — or, worse, something
    /// runs that nothing documents.
    #[test]
    fn every_listed_scenario_builds() {
        for (name, _) in SCENARIOS {
            let Ok(s) = scenario(name, 4, 0, 320, 240) else {
                panic!("{name} is listed but does not build")
            };
            assert!(!s.name().is_empty());
        }
    }

    #[test]
    fn an_unknown_scenario_is_an_error_naming_itself() {
        let Err(e) = scenario("rectangles", 0, 0, 640, 480) else {
            panic!("a name that is not in SCENARIOS must not build");
        };
        assert!(format!("{e}").contains("rectangles"), "{e}");
    }

    /// A scenario's own `name()` is what the report groups rows on, so it
    /// must be the name it was asked for — a mismatch would split one
    /// sweep across two tables.
    #[test]
    fn a_scenarios_name_is_the_one_it_was_asked_for() {
        for (name, _) in SCENARIOS {
            let Ok(s) = scenario(name, 4, 0, 320, 240) else {
                panic!("{name} does not build")
            };
            assert_eq!(&s.name(), name, "{name} reports a different name");
        }
    }

    #[test]
    fn a_zero_sweep_point_takes_a_documented_default() {
        assert!((edge_or(0, 100.0) - 100.0).abs() < f32::EPSILON);
        assert!((edge_or(500, 100.0) - 500.0).abs() < f32::EPSILON);
    }

    #[test]
    fn every_scenario_has_a_provenance_line() {
        for (name, why) in SCENARIOS {
            assert!(why.len() > 10, "{name} has no provenance");
        }
    }
}

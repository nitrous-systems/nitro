//! `just bench "<mode> <mode>"` — the command `docs/bench.md` §10 prints,
//! executed rather than written down.
//!
//! The three-rate sweep was documented from §10's first version and
//! **never worked** (#618, #3843). `just` passed the modes string to `ssh`
//! as one argv element, correctly; but ssh does not preserve argv — it
//! joins its remaining arguments with spaces into a single command string
//! and hands that to the remote login shell, which splits it again. So
//! every arm after the first reached `deploy/bench.sh` as a stray
//! positional and its parser exited 2 with `unknown argument 720p240`.
//! The fix is to quote the string twice in the recipe
//! (`quote(quote(modes))`), one layer for each shell.
//!
//! What makes this testable without a twenty-minute box booking is a pair
//! of cheap pieces:
//!
//! * `deploy/bench.sh --dry-run` parses its arguments, prints one `arm …`
//!   line per arm, and exits above every side effect. Arm *count* and arm
//!   *names* together are exactly the evidence that the whole string
//!   arrived as one argument.
//! * a stub `ssh` on `PATH` that reproduces OpenSSH's argv-join
//!   (`joined="$*"`, then a shell) and records the joined command string.
//!   Without the join the stub would pass argv through faithfully and the
//!   test would pass against the *broken* recipe too — the join is the
//!   bug, so the stub has to have it.
//!
//! The real `justfile` (which imports the `bench` recipe from
//! `deploy/dev.just`) and the real `deploy/bench.sh` are driven, found
//! from `CARGO_MANIFEST_DIR` (the `crates/nitro-launcher/tests/deployed.rs`
//! precedent): a copy of the recipe pasted in here would pass for ever
//! after somebody edited the real one, which is the failure genre this
//! whole task is filed under.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository root, from this crate's manifest rather than the current
/// directory, which `cargo test` does not promise.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate>/")
        .to_path_buf()
}

/// `just`, if it is installed. `cargo test` must not start requiring it,
/// so a missing binary skips rather than fails — the `nitro-text` /
/// `nitro-server` `eprintln!("skipping: …")` precedent.
fn have_just() -> bool {
    Command::new("just")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// What one `just bench` invocation produced.
struct Run {
    /// `deploy/bench.sh --dry-run`'s stdout, as the remote shell saw it.
    stdout: String,
    /// The command strings ssh was asked to run, in order — post-join, so
    /// this is the remote shell's view and the thing the bug lived in.
    commands: Vec<String>,
    /// Did the whole recipe succeed?
    ok: bool,
}

impl Run {
    /// The arm names the script reported, in order.
    fn arms(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .filter_map(|l| l.strip_prefix("arm "))
            .map(|l| l.split_once(" -> ").map_or(l, |(name, _)| name))
            .collect()
    }

    /// The first `bash -s` command string — the one the bench recipe emits.
    fn bench_command(&self) -> &str {
        let Some(cmd) = self.commands.iter().find(|c| c.contains("bash -s")) else {
            panic!("no `bash -s` hop in {:?}", self.commands)
        };
        cmd
    }
}

/// Run `just bench <modes> <seconds>` against a stub `ssh`.
///
/// The scratch dir lives under `target/` so it is inside the repository:
/// the recipe runs `git rev-parse --short HEAD`, and a working directory
/// outside the repo makes that print `fatal: not a git repository` and
/// yield an empty sha. Harmless for these assertions, but noise in a
/// passing test is how a real warning gets missed later.
fn run_bench(tag: &str, modes: &str, seconds: &str) -> Run {
    let root = repo_root();
    let scratch = root.join("target/bench-recipe").join(tag);
    let bin = scratch.join("bin");
    std::fs::remove_dir_all(&scratch).ok();
    std::fs::create_dir_all(&bin).expect("create scratch bin/");

    // The recipe's redirect is the relative `< deploy/bench.sh`, so the
    // real script has to be reachable from the working directory. A
    // symlink keeps it the *real* one.
    let deploy = scratch.join("deploy");
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("deploy"), &deploy).expect("symlink deploy/");

    let log = scratch.join("ssh.log");
    // OpenSSH's behaviour, in six lines: drop the host, join what is left
    // with spaces, hand the result to a shell. `--dry-run` is appended so
    // the script parses and reports without touching anything. The later
    // hops (`cat ~/tmp/bench/*.jsonl`) succeed silently, so the test
    // exercises the whole recipe rather than only its first line.
    let stub = format!(
        "#!/usr/bin/env bash\n\
         shift\n\
         joined=\"$*\"\n\
         printf '%s\\n' \"$joined\" >> {log}\n\
         if [[ $joined == *\"bash -s\"* ]]; then\n\
         \x20   exec bash -c \"$joined --dry-run\"\n\
         fi\n\
         exit 0\n",
        log = shell_quote(&log.display().to_string()),
    );
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, stub).expect("write stub ssh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub ssh");
    }

    let path = match std::env::var("PATH") {
        Ok(p) => format!("{}:{p}", bin.display()),
        Err(_) => bin.display().to_string(),
    };
    let out = Command::new("just")
        .arg("--justfile")
        .arg(root.join("justfile"))
        .arg("--working-directory")
        .arg(&scratch)
        .arg("bench")
        .arg(modes)
        .arg(seconds)
        .env("PATH", path)
        .output()
        .expect("spawn just");

    let commands = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        commands,
        ok: out.status.success(),
    }
}

/// A POSIX single-quoted string, for embedding a path in the stub script.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The command §10 prints for the three-rate sweep behind §9. This is the
/// one that exited 2 with `unknown argument 720p240`.
#[test]
fn three_rate_sweep_parses() {
    if !have_just() {
        eprintln!("skipping: `just` is not on PATH");
        return;
    }
    let run = run_bench("three", "1920x1080@60 1920x1080@120 720p240", "6");
    assert_eq!(
        run.arms(),
        ["1920x1080@60", "1920x1080@120", "720p240"],
        "the modes string did not arrive as one argument; stdout:\n{}\ncommand: {}",
        run.stdout,
        run.bench_command(),
    );
    assert!(run.ok, "the recipe failed; stdout:\n{}", run.stdout);
    // The remote shell's view really is one quoted argument, not three
    // bare words — the property the double quoting buys.
    assert!(
        run.bench_command()
            .contains("--modes '1920x1080@60 1920x1080@120 720p240'"),
        "unexpected command string: {}",
        run.bench_command(),
    );
}

/// The two-arm sweep behind §7.10b, also printed in §10.
#[test]
fn two_arm_sweep_parses() {
    if !have_just() {
        eprintln!("skipping: `just` is not on PATH");
        return;
    }
    let run = run_bench("two", "1920x1080@60 720p240", "6");
    assert_eq!(
        run.arms(),
        ["1920x1080@60", "720p240"],
        "stdout:\n{}",
        run.stdout
    );
    assert!(run.ok, "the recipe failed; stdout:\n{}", run.stdout);
}

/// The bare-number spelling `deploy/bench.sh`'s header promises still
/// works (`--modes "60 120"` → 1080p at each rate). Multi-word, so it was
/// broken by the same mechanism.
#[test]
fn legacy_bare_rate_spelling_parses() {
    if !have_just() {
        eprintln!("skipping: `just` is not on PATH");
        return;
    }
    let run = run_bench("legacy", "60 120", "6");
    assert_eq!(run.arms(), ["60", "120"], "stdout:\n{}", run.stdout);
    assert!(
        run.stdout
            .contains("arm 60 -> 1920 1080 60000 mode 1920x1080@60"),
        "a bare rate should mean 1080p at that rate; stdout:\n{}",
        run.stdout,
    );
    assert!(run.ok, "the recipe failed; stdout:\n{}", run.stdout);
}

/// A single arm — the form that always worked. Pinned so a fix for the
/// multi-word case cannot break it.
#[test]
fn single_arm_parses() {
    if !have_just() {
        eprintln!("skipping: `just` is not on PATH");
        return;
    }
    let run = run_bench("single", "720p240", "6");
    assert_eq!(run.arms(), ["720p240"], "stdout:\n{}", run.stdout);
    assert!(run.ok, "the recipe failed; stdout:\n{}", run.stdout);
}

/// `just bench` with no modes: no `--modes` reaches the script at all, so
/// it runs one matrix at whatever the box is already set to. Pins the
/// recipe's `if modes == ""` branch, which quoting is easy to break —
/// `--modes ''` would look fine and silently mean "one arm named empty".
#[test]
fn no_modes_passes_no_flag() {
    if !have_just() {
        eprintln!("skipping: `just` is not on PATH");
        return;
    }
    let run = run_bench("none", "", "6");
    assert!(run.arms().is_empty(), "stdout:\n{}", run.stdout);
    assert!(
        run.stdout.contains("arms: none"),
        "expected the no-arms line; stdout:\n{}",
        run.stdout,
    );
    assert!(
        !run.bench_command().contains("--modes"),
        "an empty modes string must not emit the flag: {}",
        run.bench_command(),
    );
    assert!(run.ok, "the recipe failed; stdout:\n{}", run.stdout);
}

/// Every `just bench` line this repository prints, round-tripped.
///
/// This is the assertion that actually closes the issue's stated failure:
/// §10's command was *written down rather than executed*, and stayed wrong
/// for as long as nothing ran it. Extracting the commands from the files
/// themselves means a future edit to any of them is checked too — adding a
/// sweep to the docs with a spelling that cannot work now fails here
/// rather than at the next person's twenty-minute box booking.
#[test]
fn every_documented_bench_command_runs() {
    if !have_just() {
        eprintln!("skipping: `just` is not on PATH");
        return;
    }
    let root = repo_root();
    let mut found = 0;
    for rel in ["docs/bench.md", "docs/testbox.md", "deploy/dev.just"] {
        let path = root.join(rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is missing: {e}", path.display()));
        for (n, modes) in documented_modes(&text) {
            found += 1;
            let run = run_bench(&format!("doc{found}"), &modes, "6");
            let arms = run.arms();
            assert!(
                run.ok && !arms.is_empty(),
                "{}:{n} documents `just bench \"{modes}\"`, which does not parse.\n\
                 arms: {arms:?}\ncommand: {}\nstdout:\n{}",
                path.display(),
                run.bench_command(),
                run.stdout,
            );
            assert_eq!(
                arms.len(),
                modes.split_whitespace().count(),
                "{}:{n} documents `just bench \"{modes}\"`: {} arm(s) reached the script, \
                 so the string was re-split.\ncommand: {}",
                path.display(),
                arms.len(),
                run.bench_command(),
            );
        }
    }
    // A zero here would make every assertion above vacuous: the docs would
    // have stopped printing the command, or the extraction would have
    // stopped seeing it, and the test would pass by testing nothing.
    assert!(
        found >= 3,
        "expected the documented sweeps to be found; got {found}",
    );
}

/// The quoted modes string of every `just bench "<modes>" …` in a file,
/// with its 1-based line number.
///
/// Deliberately narrow: only a **quoted, multi-word** argument, which is
/// the shape the bug lived in. A bare single-word `just bench 720p240`
/// cannot be re-split and `single_arm_parses` covers it; prose mentioning
/// `just bench` without arguments is not a command. Lines carrying a shell
/// escape (`\ `) are the stale spelling this task removed — they are
/// reported, not skipped, so the spelling cannot creep back in.
fn documented_modes(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let Some(rest) = line.split_once("just bench ").map(|(_, r)| r) else {
            continue;
        };
        let Some(quote @ ('"' | '\'')) = rest.chars().next() else {
            continue;
        };
        let Some(end) = rest[1..].find(quote) else {
            continue;
        };
        let modes = &rest[1..=end];
        if modes.split_whitespace().count() < 2 {
            continue;
        }
        out.push((i + 1, modes.to_owned()));
    }
    out
}

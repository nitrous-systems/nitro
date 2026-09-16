//! The `.desktop` files this repository actually ships, read by the
//! parser that will read them on the box.
//!
//! `just deploy-bins` installs `deploy/*.desktop` into
//! `~/.local/share/applications`, so from #3723 these four files are part
//! of the deployed set rather than something only a packager would use.
//! That makes three claims testable here that used to be true only of
//! fixtures, and every one of them had a live counter-example before:
//!
//! 1. **The files parse into launchable entries**, with the `Icon=` the
//!    server's `app_id` → `.desktop` hop needs (#3715) and the
//!    `StartupWMClass`/basename alignment the bar's rule needs (#3714).
//! 2. **A bare `Exec=` resolves through `PATH`** — which is `execvp`'s
//!    business rather than the launcher's, and the reason `nitro-session`
//!    now puts its own directory on the `PATH` its children inherit.
//!    Before that, installing `nitro-term.desktop` on the box produced
//!    `spawn: No such file or directory`.
//! 3. **An installed file replaces the launcher's built-in** for the same
//!    program rather than appearing beside it, so the box shows *one*
//!    Terminal entry and not two.
//!
//! The fixtures here are the repository's own files, read from
//! `deploy/`: a copy pasted into this file would pass for ever after
//! somebody edited the real one.

use std::path::{Path, PathBuf};

use nitro_launcher::desktop::{self, Entry, Source};
use nitro_launcher::{Launcher, spawn};

/// `deploy/`, found from this crate's manifest rather than from the
/// current directory, which `cargo test` does not promise.
fn deploy_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate>/")
        .join("deploy")
}

/// The shipped entries, parsed, keyed by file basename.
fn shipped() -> Vec<(String, Entry)> {
    let dir = deploy_dir();
    let mut out = Vec::new();
    for name in [
        "nitro-calc.desktop",
        "nitro-files.desktop",
        "nitro-settings.desktop",
        "nitro-term.desktop",
    ] {
        let path = dir.join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is missing: {e}", path.display()));
        let entry = desktop::parse(&text, &path)
            .unwrap_or_else(|| panic!("{} did not parse as a launchable entry", path.display()));
        out.push((name.to_owned(), entry));
    }
    out
}

/// Distinguishes the scratch directories of tests running in parallel.
static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A private directory, cleaned up by the caller.
fn scratch(tag: &str) -> PathBuf {
    // No `ThreadId(N)` in the name: the parentheses would have to be
    // quoted in the stub shell script below, and an unquoted one is a
    // syntax error that arrives as a bare `exit 2`.
    let dir = std::env::temp_dir().join(format!(
        "nitro-launcher-deployed-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Each shipped file names the icon, the app id and the program that the
/// rest of the desktop assumes it does.
///
/// The three strings are deliberately one string: the file is
/// `nitro-calc.desktop`, the app id `nitro-calc` and `StartupWMClass`
/// `nitro-calc`, because the bar asks for an icon by app id and the
/// server's third lookup step turns that into `<app_id>.desktop`'s
/// `Icon=`. Renaming any one of them silently costs the bar its icon, so
/// the alignment is asserted rather than left to a comment.
#[test]
fn the_shipped_entries_name_the_icons_the_desktop_expects() {
    let expected = [
        (
            "nitro-calc.desktop",
            "Calculator",
            "nitro-calc",
            "calculator",
        ),
        ("nitro-files.desktop", "Files", "nitro-files", "folder-fill"),
        (
            "nitro-settings.desktop",
            "Settings",
            "nitro-settings",
            "gear",
        ),
        ("nitro-term.desktop", "Terminal", "nitro-term", "terminal"),
    ];
    for (file, entry) in shipped() {
        let (_, name, program, icon) = expected
            .iter()
            .find(|(f, _, _, _)| *f == file)
            .unwrap_or_else(|| panic!("unexpected file {file}"));
        assert_eq!(&entry.name, name, "{file}: Name=");
        assert_eq!(
            entry.icon.as_deref(),
            Some(*icon),
            "{file}: Icon= is what the server's `.desktop` hop resolves \
             the app id to"
        );
        assert_eq!(entry.program(), *program, "{file}: Exec=");
        assert!(entry.runnable(), "{file}: not runnable");

        // The file's own name is the app id, which is what makes the
        // server's `app_id` → `<app_id>.desktop` step find it at all.
        assert_eq!(
            file,
            format!("{program}.desktop"),
            "the file is not named after the app id it launches"
        );
        // And `StartupWMClass` is the same string again. The parser has
        // no opinion on the key, so this is read from the text.
        let text = std::fs::read_to_string(deploy_dir().join(&file)).expect("re-read");
        assert!(
            text.lines()
                .any(|l| l.trim() == format!("StartupWMClass={program}")),
            "{file}: StartupWMClass is not `{program}`"
        );
    }
}

/// Every shipped `Exec=` is a **bare name**, not a path — and that is the
/// property the session's `PATH` change exists to support.
///
/// A packager installs the binary into `/usr/bin` and the spec says to
/// write the bare name; writing `/home/kaspar/nitro-bin/nitro-term` here
/// instead would make the files box-specific and useless to anybody else.
#[test]
fn every_shipped_exec_is_a_bare_name() {
    for (file, entry) in shipped() {
        assert!(
            !entry.program().contains('/'),
            "{file}: `Exec={}` is a path; these files are for a packager \
             and the box alike, and the box is covered by the session \
             putting its own directory on the children's PATH",
            entry.program()
        );
    }
}

/// A bare `Exec=` resolves against `PATH`, and does not without it.
///
/// This is the regression the justfile's old "deliberately NOT installed"
/// comment guarded, reproduced in both directions. The launcher does
/// nothing special for it — `Command::new("nitro-term")` is `execvp`, so
/// the kernel searches `PATH` — which is exactly why the fix belongs in
/// `nitro-session`, the process that knows where the binaries are.
///
/// The `PATH` is set **on the command** rather than on this process:
/// `std::env::set_var` in a test binary that runs its tests on threads is
/// the race `spawn.rs` already refuses to take for `NITRO_SHELL_SOCKET`.
#[test]
fn a_bare_exec_resolves_against_a_path_containing_the_binary() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = scratch("path");
    let marker = dir.join("ran");
    // A stand-in for the deployed binary, under the real entry's own
    // program name, in a directory that is on no machine's `PATH`.
    let (_, term) = shipped()
        .into_iter()
        .find(|(f, _)| f == "nitro-term.desktop")
        .expect("the terminal entry");
    let program = term.program().to_owned();
    let stub = dir.join(&program);
    std::fs::write(
        &stub,
        format!("#!/bin/sh\necho launched > {}\n", marker.display()),
    )
    .expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // The arm: the deploy directory on `PATH`, as `nitro-session` now
    // arranges for everything it starts.
    let status = spawn::command(&program, &term.argv[1..])
        .env("PATH", &dir)
        .status()
        .expect("spawn with the directory on PATH");
    assert!(status.success(), "the bare name did not resolve: {status}");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap_or_default().trim(),
        "launched"
    );

    // The control, which is what the box actually had: the same command
    // with a `PATH` that does not contain the binary is ENOENT — the
    // "spawn: No such file or directory" the old justfile comment
    // recorded. Without this arm the test above is equally consistent
    // with a machine that happens to have a `nitro-term` installed.
    let _ = std::fs::remove_file(&marker);
    let empty = dir.join("nothing-here");
    let err = spawn::command(&program, &term.argv[1..])
        .env("PATH", &empty)
        .status();
    assert!(
        err.is_err(),
        "a bare name resolved with the binary off PATH: {err:?}"
    );
    assert!(!marker.exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// With the four files installed, the launcher shows **one** entry each —
/// the file's, not the file's plus the built-in's.
///
/// This is the second half of the reason the files were not installed:
/// a `.desktop` file shadows the built-in for the same program, so a
/// half-working file replaced a working entry. Now that the bare `Exec=`
/// resolves, the shadowing is the behaviour we want — and it has to be
/// pinned against the *real* files, because the dedup compares the
/// program's **file name**, which is `nitro-term` for a bare `Exec=` and
/// `nitro-term` for the built-in's absolute path only by construction.
#[test]
fn an_installed_file_replaces_the_builtin_rather_than_doubling_it() {
    let dir = scratch("shadow");
    let applications = dir.join("applications");
    std::fs::create_dir_all(&applications).expect("applications dir");
    for (file, _) in shipped() {
        std::fs::copy(deploy_dir().join(&file), applications.join(&file))
            .unwrap_or_else(|e| panic!("install {file}: {e}"));
    }

    // Built-ins in the shape `nitro-launcher::builtins()` produces: an
    // **absolute** path into the binaries' directory, which is what the
    // box has and what makes the file-name comparison load-bearing.
    let bin = dir.join("nitro-bin");
    let builtins: Vec<Entry> = [
        ("nitro-calc", "Calculator", "calculator"),
        ("nitro-term", "Terminal", "terminal"),
        ("nitro-settings", "Settings", "gear"),
        ("nitro-files", "Files", "folder-fill"),
        ("nitro-demo", "Nitro Demo", "palette"),
    ]
    .iter()
    .map(|(prog, name, icon)| Entry {
        name: (*name).to_owned(),
        argv: vec![bin.join(prog).to_string_lossy().into_owned()],
        terminal: false,
        icon: Some((*icon).to_owned()),
        source: Source::Builtin,
    })
    .collect();

    let mut l = Launcher::new()
        .with_dirs(vec![applications.clone()])
        .with_builtins(builtins);
    l.rescan();

    let names: Vec<String> = l.entries().iter().map(|e| e.name.clone()).collect();
    for want in ["Calculator", "Files", "Settings", "Terminal"] {
        assert_eq!(
            names.iter().filter(|n| *n == want).count(),
            1,
            "{want} appears {names:?} times, not once"
        );
    }
    // Each of the four is the *file's* entry, so the launcher runs the
    // packaged command — and `nitro-demo`, which ships no `.desktop` on
    // purpose (it is a tool, not an application), keeps its built-in.
    for e in l.entries() {
        match e.name.as_str() {
            "Calculator" | "Files" | "Settings" | "Terminal" => {
                assert!(
                    matches!(e.source, Source::Desktop(_)),
                    "{} came from the built-in, so the installed file did \
                     not shadow it",
                    e.name
                );
                assert!(
                    !e.program().contains('/'),
                    "{}: the packaged bare `Exec=` is what will run",
                    e.name
                );
            }
            "Nitro Demo" => assert_eq!(e.source, Source::Builtin),
            other => panic!("unexpected entry {other}"),
        }
    }

    // A second `just deploy` re-copies the same files: the scan must
    // still show one of each rather than growing. Entries are keyed by
    // file basename, so this is the idempotence the box run checks for
    // real.
    for (file, _) in shipped() {
        std::fs::copy(deploy_dir().join(&file), applications.join(&file)).expect("re-install");
    }
    l.rescan();
    let again: Vec<String> = l.entries().iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, again, "a re-deploy changed the entry list");

    let _ = std::fs::remove_dir_all(&dir);
}

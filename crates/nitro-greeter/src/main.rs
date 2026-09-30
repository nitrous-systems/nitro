//! `nitro-greeter`: greetd's greeter (what `nitro-session --greeter`
//! runs), or with `--lock` the lock screen. Exit 0 is "done" (a session
//! was started, or the session unlocked); any error is exit 1, and the
//! supervising `nitro-session` restarts the greeter with backoff.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let run = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--lock"] => nitro_greeter::run_lock,
        [] => nitro_greeter::run_greeter,
        _ => {
            eprintln!("usage: nitro-greeter [--lock]");
            return ExitCode::from(2);
        }
    };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nitro-greeter: {e}");
            ExitCode::FAILURE
        }
    }
}

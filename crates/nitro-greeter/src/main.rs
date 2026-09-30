//! `nitro-greeter --lock`: the lock screen. With no flag it would be
//! greetd's greeter, which is not built yet (docs/greeter.md, step 5).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--lock"] => match nitro_greeter::run_lock() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("nitro-greeter: {e}");
                ExitCode::FAILURE
            }
        },
        [] => {
            eprintln!("nitro-greeter: greeter mode needs greetd; not implemented yet (use --lock)");
            ExitCode::from(2)
        }
        _ => {
            eprintln!("usage: nitro-greeter --lock");
            ExitCode::from(2)
        }
    }
}

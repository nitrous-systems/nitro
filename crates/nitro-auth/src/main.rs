//! `nitro-auth [--service NAME]`: greetd's protocol on stdin/stdout,
//! PAM underneath. Exit 0 at end of input, 1 on a broken stream, 2 on
//! bad arguments. stderr carries diagnostics only.

use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!("usage: nitro-auth [--service NAME]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut service = nitro_auth::pam::DEFAULT_SERVICE.to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--service" => match args.next() {
                Some(s) if !s.is_empty() => service = s,
                _ => return usage(),
            },
            _ => return usage(),
        }
    }
    let Some(owner) = nitro_login::owner() else {
        eprintln!("nitro-auth: cannot tell whose session this is");
        return ExitCode::from(1);
    };
    let mut pam = nitro_auth::pam::Pam::new(service);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match nitro_auth::serve::serve(&mut stdin.lock(), &mut stdout.lock(), &owner, &mut pam) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nitro-auth: {e}");
            ExitCode::from(1)
        }
    }
}

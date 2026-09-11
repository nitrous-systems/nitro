//! `nitro-shot`: ask the running server for a screenshot and write a PNG.
//!
//! ```text
//! nitro-shot [-o FILE] [--raw] [--output NAME]   screenshot (PNG, or raw XRGB8888 with --raw)
//! nitro-shot --outputs                            list outputs
//! nitro-shot --stats                              frame counters
//! nitro-shot --quit                               stop the server
//! ```
//!
//! The control socket is `$NITRO_CONTROL`, else
//! `$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/control.sock`.

mod png;

use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Shot { raw: bool, output: Option<String> },
    Outputs,
    Stats,
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    mode: Mode,
    file: Option<PathBuf>,
}

const USAGE: &str =
    "usage: nitro-shot [-o FILE] [--raw] [--output NAME] | --outputs | --stats | --quit";

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut file = None;
    let mut raw = false;
    let mut output = None;
    let mut cmd: Option<Mode> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "--out" => file = Some(PathBuf::from(it.next().ok_or("-o needs a FILE")?)),
            "--raw" => raw = true,
            "--output" => output = Some(it.next().ok_or("--output needs a NAME")?),
            "--outputs" => cmd = Some(Mode::Outputs),
            "--stats" => cmd = Some(Mode::Stats),
            "--quit" => cmd = Some(Mode::Quit),
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    let mode = cmd.unwrap_or(Mode::Shot { raw, output });
    Ok(Args { mode, file })
}

fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("NITRO_CONTROL") {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join("nitro").join("control.sock");
        }
    }
    let uid = std::fs::metadata("/proc/self").map_or(0, |m| m.uid());
    PathBuf::from(format!("/tmp/nitro-{uid}/control.sock"))
}

fn connect() -> io::Result<BufReader<UnixStream>> {
    let path = socket_path();
    let s = UnixStream::connect(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    Ok(BufReader::new(s))
}

/// Send one line, read the status line. `Ok(rest)` is what followed `ok`.
fn request(conn: &mut BufReader<UnixStream>, line: &str) -> io::Result<String> {
    conn.get_mut().write_all(line.as_bytes())?;
    let mut status = String::new();
    conn.read_line(&mut status)?;
    let status = status.trim_end_matches('\n');
    if let Some(rest) = status.strip_prefix("ok") {
        Ok(rest.trim_start().to_owned())
    } else if let Some(msg) = status.strip_prefix("err ") {
        Err(io::Error::other(format!("server: {msg}")))
    } else if status.is_empty() {
        Err(io::Error::other("server closed the connection"))
    } else {
        Err(io::Error::other(format!("malformed reply {status:?}")))
    }
}

/// Read the text body after `ok\n`: lines up to a blank one.
fn read_text_body(conn: &mut BufReader<UnixStream>) -> io::Result<String> {
    let mut body = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if conn.read_line(&mut line)? == 0 || line == "\n" {
            return Ok(body);
        }
        body.push_str(&line);
    }
}

struct Shot {
    width: u32,
    height: u32,
    stride: u32,
    data: Vec<u8>,
}

fn read_shot(conn: &mut BufReader<UnixStream>, header: &str) -> io::Result<Shot> {
    let fields: Vec<u32> = header
        .split_whitespace()
        .map(|f| f.parse::<u32>().ok())
        .collect::<Option<_>>()
        .ok_or_else(|| io::Error::other(format!("bad shot header {header:?}")))?;
    let [width, height, stride] = fields[..] else {
        return Err(io::Error::other(format!("bad shot header {header:?}")));
    };
    let mut data = vec![0u8; (stride as usize) * (height as usize)];
    conn.read_exact(&mut data)?;
    Ok(Shot {
        width,
        height,
        stride,
        data,
    })
}

fn write_out(file: Option<&PathBuf>, bytes: &[u8]) -> io::Result<()> {
    if let Some(p) = file {
        std::fs::write(p, bytes)
    } else {
        let mut out = io::stdout().lock();
        out.write_all(bytes)?;
        out.flush()
    }
}

fn run(args: Args) -> io::Result<()> {
    let mut conn = connect()?;
    match args.mode {
        Mode::Shot { raw, output } => {
            let line = match output {
                Some(name) => format!("shot {name}\n"),
                None => "shot\n".to_owned(),
            };
            let header = request(&mut conn, &line)?;
            let shot = read_shot(&mut conn, &header)?;
            if raw {
                write_out(args.file.as_ref(), &shot.data)
            } else {
                let png = png::encode_xrgb(shot.width, shot.height, shot.stride, &shot.data);
                write_out(args.file.as_ref(), &png)
            }
        }
        Mode::Outputs | Mode::Stats => {
            let line = if args.mode == Mode::Outputs {
                "outputs\n"
            } else {
                "stats\n"
            };
            request(&mut conn, line)?;
            let body = read_text_body(&mut conn)?;
            write_out(args.file.as_ref(), body.as_bytes())
        }
        Mode::Quit => {
            request(&mut conn, "quit\n")?;
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nitro-shot: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Args, String> {
        parse_args(s.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn parses_arguments() {
        assert_eq!(
            parse(""),
            Ok(Args {
                mode: Mode::Shot {
                    raw: false,
                    output: None
                },
                file: None
            })
        );
        assert_eq!(
            parse("-o x.png --raw --output HDMI-A-1"),
            Ok(Args {
                mode: Mode::Shot {
                    raw: true,
                    output: Some("HDMI-A-1".into())
                },
                file: Some("x.png".into())
            })
        );
        assert_eq!(parse("--stats").unwrap().mode, Mode::Stats);
        assert_eq!(parse("--outputs -o o.txt").unwrap().mode, Mode::Outputs);
        assert_eq!(parse("--quit").unwrap().mode, Mode::Quit);
        assert!(parse("-o").is_err());
        assert!(parse("--frob").is_err());
    }
}

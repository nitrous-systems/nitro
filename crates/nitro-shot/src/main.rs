//! `nitro-shot`: ask the running server for a screenshot and write a PNG.
//!
//! ```text
//! nitro-shot [-o FILE] [--raw] [--output NAME] [--no-cursor] [-v]
//!                                                 screenshot (PNG, or raw ARGB8888, alpha 255, with --raw)
//! nitro-shot --outputs                            list outputs
//! nitro-shot --modes                              list every mode each connector offers
//! nitro-shot --stats                              frame counters
//! nitro-shot --quit                               stop the server
//! nitro-shot --input "ARGS"                       inject input: `input ARGS` (see nitro-server's protocol.rs)
//! nitro-shot --samples i2p|flip|paint|damage      raw recent samples, oldest first (µs; pixels for damage)
//! nitro-shot --record N [--output NAME] [--fps F] [-o FILE]
//!                                                 record N frames over the wire (#676): fps, latency,
//!                                                 damage; the last frame to FILE as PNG
//! ```
//!
//! The control socket is `$NITRO_CONTROL`, else
//! `$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/control.sock`.

mod png;
mod record;

use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Shot {
        raw: bool,
        output: Option<String>,
        /// Leave the cursor out (`cursor=0`).
        no_cursor: bool,
        /// Print the server's shot metadata to stderr.
        verbose: bool,
    },
    Outputs,
    /// Every mode each connected connector offers — what
    /// `output.<connector>.mode` may be set to. See `docs/settings.md`.
    Modes,
    Stats,
    Quit,
    /// `input <args>`: synthetic input through the server's real input
    /// path. Prints the status line's count.
    Input(String),
    /// `samples i2p|flip|paint|damage`.
    Samples(String),
    /// Record `frames` frames of `output` at ≤`fps` (0: the output's
    /// rate) over the wire's capture ops (#676).
    Record {
        frames: u32,
        output: Option<String>,
        fps: u32,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    mode: Mode,
    file: Option<PathBuf>,
}

const USAGE: &str = "usage: nitro-shot [-o FILE] [--raw] [--output NAME] [--no-cursor] [-v] | --outputs | --modes | --stats | --quit | --input ARGS | --samples i2p|flip|paint|damage | --record N [--output NAME] [--fps F] [-o FILE]";

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut file = None;
    let mut raw = false;
    let mut output = None;
    let mut no_cursor = false;
    let mut verbose = false;
    let mut cmd: Option<Mode> = None;
    let mut record: Option<u32> = None;
    let mut fps = 0;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "--out" => file = Some(PathBuf::from(it.next().ok_or("-o needs a FILE")?)),
            "--raw" => raw = true,
            "--no-cursor" => no_cursor = true,
            "-v" | "--verbose" => verbose = true,
            "--output" => output = Some(it.next().ok_or("--output needs a NAME")?),
            "--outputs" => cmd = Some(Mode::Outputs),
            "--modes" => cmd = Some(Mode::Modes),
            "--stats" => cmd = Some(Mode::Stats),
            "--quit" => cmd = Some(Mode::Quit),
            "--input" => cmd = Some(Mode::Input(it.next().ok_or("--input needs ARGS")?)),
            "--samples" => match it.next().as_deref() {
                Some(k @ ("i2p" | "flip" | "paint" | "damage")) => {
                    cmd = Some(Mode::Samples(k.to_owned()));
                }
                _ => return Err("--samples needs i2p, flip, paint or damage".to_owned()),
            },
            "--record" => {
                record = Some(
                    it.next()
                        .and_then(|n| n.parse().ok())
                        .filter(|n| *n > 0)
                        .ok_or("--record needs a frame count")?,
                );
            }
            "--fps" => {
                fps = it
                    .next()
                    .and_then(|n| n.parse().ok())
                    .ok_or("--fps needs a number")?;
            }
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if let Some(frames) = record {
        cmd = Some(Mode::Record {
            frames,
            output: output.clone(),
            fps,
        });
    }
    let mode = cmd.unwrap_or(Mode::Shot {
        raw,
        output,
        no_cursor,
        verbose,
    });
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

/// `k=v` pairs after a shot's `w h stride`.
type Meta = Vec<(String, String)>;

/// `w h stride` and the `k=v` metadata after them (`shot meta=1`).
fn parse_header(header: &str) -> Option<(u32, u32, u32, Meta)> {
    let mut words = header.split_whitespace();
    let mut num = || words.next()?.parse::<u32>().ok();
    let (w, h, stride) = (num()?, num()?, num()?);
    let meta = words
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .collect::<Option<_>>()?;
    Some((w, h, stride, meta))
}

fn read_shot(conn: &mut BufReader<UnixStream>, header: &str) -> io::Result<(Shot, Meta)> {
    let (width, height, stride, meta) = parse_header(header)
        .ok_or_else(|| io::Error::other(format!("bad shot header {header:?}")))?;
    let mut data = vec![0u8; (stride as usize) * (height as usize)];
    conn.read_exact(&mut data)?;
    Ok((
        Shot {
            width,
            height,
            stride,
            data,
        },
        meta,
    ))
}

/// Say on stderr when Surfaces could not be shown (#3962).
fn report(meta: &[(String, String)], verbose: bool) {
    let get = |k: &str| meta.iter().find(|(m, _)| m == k).map(|(_, v)| v.as_str());
    if verbose {
        let line: Vec<String> = meta.iter().map(|(k, v)| format!("{k}={v}")).collect();
        eprintln!("nitro-shot: {}", line.join(" "));
    }
    if let Some(n) = get("placeholder").filter(|n| *n != "0") {
        eprintln!(
            "nitro-shot: {n} surface(s) shown as placeholder ({})",
            get("reason").unwrap_or("?")
        );
    }
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
    if let Mode::Record {
        frames,
        ref output,
        fps,
    } = args.mode
    {
        return record::run(frames, output.as_deref(), fps, args.file.as_deref());
    }
    let mut conn = connect()?;
    match args.mode {
        Mode::Shot {
            raw,
            output,
            no_cursor,
            verbose,
        } => {
            let mut line = "shot".to_owned();
            if let Some(name) = output {
                line.push(' ');
                line.push_str(&name);
            }
            line.push_str(" meta=1");
            if no_cursor {
                line.push_str(" cursor=0");
            }
            line.push('\n');
            let header = request(&mut conn, &line)?;
            let (shot, meta) = read_shot(&mut conn, &header)?;
            report(&meta, verbose);
            if raw {
                write_out(args.file.as_ref(), &shot.data)
            } else {
                let png = png::encode_xrgb(shot.width, shot.height, shot.stride, &shot.data);
                write_out(args.file.as_ref(), &png)
            }
        }
        Mode::Outputs | Mode::Modes | Mode::Stats => {
            let line = match args.mode {
                Mode::Outputs => "outputs\n",
                Mode::Modes => "modes\n",
                _ => "stats\n",
            };
            request(&mut conn, line)?;
            let body = read_text_body(&mut conn)?;
            write_out(args.file.as_ref(), body.as_bytes())
        }
        Mode::Quit => {
            request(&mut conn, "quit\n")?;
            Ok(())
        }
        Mode::Input(ref a) => {
            let n = request(&mut conn, &format!("input {a}\n"))?;
            write_out(args.file.as_ref(), format!("{n}\n").as_bytes())
        }
        Mode::Record { .. } => unreachable!("handled above"),
        Mode::Samples(ref k) => {
            let total = request(&mut conn, &format!("samples {k}\n"))?;
            let body = read_text_body(&mut conn)?;
            eprintln!("total {total}");
            write_out(args.file.as_ref(), body.as_bytes())
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
                    output: None,
                    no_cursor: false,
                    verbose: false,
                },
                file: None
            })
        );
        assert_eq!(
            parse("-o x.png --raw --output HDMI-A-1 --no-cursor -v"),
            Ok(Args {
                mode: Mode::Shot {
                    raw: true,
                    output: Some("HDMI-A-1".into()),
                    no_cursor: true,
                    verbose: true,
                },
                file: Some("x.png".into())
            })
        );
        assert_eq!(parse("--stats").unwrap().mode, Mode::Stats);
        assert_eq!(parse("--outputs -o o.txt").unwrap().mode, Mode::Outputs);
        assert_eq!(parse("--modes").unwrap().mode, Mode::Modes);
        assert_eq!(parse("--quit").unwrap().mode, Mode::Quit);
        assert_eq!(
            parse_args(["--input".to_owned(), "wheel 0 15 count=3".to_owned()])
                .unwrap()
                .mode,
            Mode::Input("wheel 0 15 count=3".into())
        );
        assert_eq!(
            parse("--samples i2p").unwrap().mode,
            Mode::Samples("i2p".into())
        );
        assert_eq!(
            parse("--samples paint").unwrap().mode,
            Mode::Samples("paint".into())
        );
        assert_eq!(
            parse("--samples damage").unwrap().mode,
            Mode::Samples("damage".into())
        );
        assert_eq!(
            parse("--record 30 --output DP-1 --fps 15").unwrap().mode,
            Mode::Record {
                frames: 30,
                output: Some("DP-1".into()),
                fps: 15
            }
        );
        assert!(parse("--record 0").is_err());
        assert!(parse("--samples frob").is_err());
        assert!(parse("--input").is_err());
        assert!(parse("-o").is_err());
        assert!(parse("--frob").is_err());
    }

    #[test]
    fn parses_shot_headers() {
        assert_eq!(parse_header("4 2 16"), Some((4, 2, 16, vec![])));
        let (w, h, s, m) =
            parse_header("640 480 2560 surfaces=2 cpu=1 helper=0 placeholder=1 reason=helper-off")
                .unwrap();
        assert_eq!((w, h, s, m.len()), (640, 480, 2560, 5));
        assert_eq!(m[4], ("reason".to_owned(), "helper-off".to_owned()));
        assert_eq!(parse_header("4 2"), None);
        assert_eq!(parse_header("4 2 x"), None);
        assert_eq!(parse_header("4 2 16 junk"), None);
    }
}

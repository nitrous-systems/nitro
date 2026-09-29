//! `nitro-video FILE [--fullscreen] [--frames N] [--stats] [--synthetic]`.

use std::os::fd::AsFd as _;
use std::path::PathBuf;
use std::process::ExitCode;

use nitro_ui::{App, Size};
use nitro_video::decode::{Decoder, SyntheticDecoder};
use nitro_video::ffmpeg::LibavDecoder;
use nitro_video::player::{self, Opts, Player};

const USAGE: &str = "usage: nitro-video FILE [--fullscreen] [--frames N] [--stats]
       nitro-video --synthetic [--fullscreen] [--frames N] [--stats]

  --fullscreen   start fullscreen (F / F11 / double-click toggle it)
  --frames N     quit after N presented frames
  --stats        print presented/dropped/late/skipped counts on exit
  --synthetic    a generated 720p30 test stream instead of a file

keys: Space play/pause, Left/Right seek 5 s, F fullscreen, Esc leave
fullscreen, Q quit";

struct Args {
    file: Option<PathBuf>,
    synthetic: bool,
    stats: bool,
    opts: Opts,
}

fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        file: None,
        synthetic: false,
        stats: false,
        opts: Opts::default(),
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--fullscreen" => a.opts.fullscreen = true,
            "--stats" => a.stats = true,
            "--synthetic" => a.synthetic = true,
            "--frames" => {
                let n = it.next().ok_or("--frames needs a number")?;
                a.opts.frames = n
                    .parse()
                    .map_err(|_| format!("--frames: bad number {n:?}"))?;
            }
            "-h" | "--help" => return Err(USAGE.to_owned()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n{USAGE}")),
            _ if a.file.is_some() => return Err(format!("one file at a time\n{USAGE}")),
            _ => a.file = Some(PathBuf::from(arg)),
        }
    }
    if a.file.is_none() && !a.synthetic {
        return Err(USAGE.to_owned());
    }
    Ok(a)
}

/// Decoder threads: one per core up to four. libavcodec's frame threading
/// holds a frame per thread, so its own default (one per core) costs a
/// many-core machine tens of MB for no gain at 1080p.
fn decode_threads() -> u32 {
    std::thread::available_parallelism().map_or(2, |n| n.get().min(4) as u32)
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let dec: Box<dyn Decoder> = match &args.file {
        Some(f) if !args.synthetic => match LibavDecoder::open(f, decode_threads()) {
            Ok(d) => Box::new(d),
            Err(e) => {
                eprintln!("nitro-video: {e}");
                return ExitCode::FAILURE;
            }
        },
        _ => Box::new(SyntheticDecoder::new(1280, 720, 30, 30 * 60)),
    };
    let info = dec.info().clone();
    let title = args.file.as_ref().and_then(|f| f.file_name()).map_or_else(
        || "nitro-video".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let player = match Player::new(dec, args.opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("nitro-video: {e}");
            return ExitCode::FAILURE;
        }
    };
    let wake = match rustix::io::dup(player.wake_fd()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("nitro-video: {e}");
            return ExitCode::FAILURE;
        }
    };
    let app = match App::new("nitro-video") {
        Ok(a) => a,
        Err(e) => {
            eprintln!("nitro-video: {e}");
            return ExitCode::FAILURE;
        }
    };
    let size = player::initial_size(&info);
    let mut ui = match app
        .title(title)
        .size(Size::new(size.0, size.1))
        .build(|ui| player::install(ui, &info, wake.as_fd()))
    {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("nitro-video: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut player = player;
    let socket = nitro_ui::introspect::Socket::bind("nitro-video").ok();
    let r = nitro_ui::app::event_loop_with(&mut ui, &mut player, socket);
    if args.stats {
        eprintln!("{}", player.summary_line());
    }
    if let Some(e) = &player.error {
        eprintln!("nitro-video: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = r {
        eprintln!("nitro-video: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

//! `nitro-video FILE [--fullscreen] [--frames N] [--stats] [--hwdec MODE]
//! [--vaapi-device PATH] [--no-scale] [--synthetic]`.

use std::os::fd::AsFd as _;
use std::path::PathBuf;
use std::process::ExitCode;

use nitro_ui::{App, Size};
use nitro_video::decode::{HwDec, SyntheticDecoder, VideoSource};
use nitro_video::ffmpeg;
use nitro_video::player::{self, Opts, Player};

const USAGE: &str = "usage: nitro-video FILE [--fullscreen] [--frames N] [--stats]
                   [--hwdec auto|dmabuf|download|off] [--vaapi-device PATH]
       nitro-video --synthetic [--fullscreen] [--frames N] [--stats]

  --fullscreen   start fullscreen (F / F11 / double-click toggle it)
  --frames N     quit after N presented frames
  --stats        print presented/dropped/late/skipped counts on exit
  --synthetic    a generated 720p30 test stream instead of a file
  --hwdec MODE   VA-API decode: auto (default: VA-API when the hardware
                 takes the stream, dma-bufs when the server shows them
                 as they are, else software), dmabuf (always present VA surfaces, a
                 tiled one off a plane is a placeholder),
                 download (copy frames into shm), off (software)
  --vaapi-device PATH  the render node (default /dev/dri/renderD128)
  --no-scale     never scale VA frames to the server's plane hint (VPP)

keys: Space play/pause, Left/Right seek 5 s, R repeat on/off,
F fullscreen, Esc leave fullscreen, Q quit";

struct Args {
    file: Option<PathBuf>,
    synthetic: bool,
    stats: bool,
    device: String,
    opts: Opts,
}

fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        file: None,
        synthetic: false,
        stats: false,
        device: ffmpeg::DEFAULT_VAAPI_DEVICE.to_owned(),
        opts: Opts::default(),
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--fullscreen" => a.opts.fullscreen = true,
            "--stats" => a.stats = true,
            "--no-scale" => a.opts.no_scale = true,
            "--synthetic" => a.synthetic = true,
            "--hwdec" => {
                let m = it.next().ok_or("--hwdec needs a mode")?;
                a.opts.hwdec = HwDec::parse(&m)?;
            }
            "--vaapi-device" => a.device = it.next().ok_or("--vaapi-device needs a path")?,
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

/// Decoder threads for streams above 1080p: one per core up to four.
/// libavcodec's frame threading holds a context and a frame per thread
/// (+10.5 MB each at 1080p), so its own default (one per core) costs a
/// many-core machine tens of MB. Up to 1080p the shim decodes on one
/// thread whatever this says (#3924).
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
    let mut fallback = None;
    let dec: Box<dyn VideoSource> = match &args.file {
        Some(f) if !args.synthetic => {
            match ffmpeg::open(f, args.opts.hwdec, &args.device, decode_threads()) {
                Ok(o) => {
                    if let Some(why) = &o.fallback {
                        eprintln!("nitro-video: software decode: {why}");
                    }
                    fallback = o.fallback;
                    Box::new(o.decoder)
                }
                Err(e) => {
                    eprintln!("nitro-video: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        _ => Box::new(SyntheticDecoder::new(1280, 720, 30, 30 * 60)),
    };
    let info = dec.info().clone();
    let title = args.file.as_ref().and_then(|f| f.file_name()).map_or_else(
        || "nitro-video".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let player = match Player::new(dec, args.opts) {
        Ok(mut p) => {
            p.set_fallback(fallback);
            if let Some(f) = args.file.clone().filter(|_| !args.synthetic) {
                p.set_software(Box::new(move || {
                    ffmpeg::LibavDecoder::open(&f, decode_threads())
                        .map(|d| Box::new(d) as Box<dyn VideoSource>)
                }));
            }
            p
        }
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
    if let Some(why) = player.fallback()
        && player.decode_mode() != "vaapi-dmabuf"
        && args.stats
    {
        eprintln!("nitro-video: {} decode: {why}", player.decode_mode());
    }
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

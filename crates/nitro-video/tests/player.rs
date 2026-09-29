//! nitro-video end to end: the libav backend on a real (tiny) clip, and
//! the player in the in-process harness on the synthetic decoder.

use std::os::fd::AsFd as _;
use std::path::Path;

use nitro_ui::Size;
use nitro_ui::test::Harness;
use nitro_video::decode::{Decoder, HwDec, Matrix, Nv12Layout, Output, SyntheticDecoder};
use nitro_video::ffmpeg::LibavDecoder;
use nitro_video::player::{self, HIDE_MS, Opts, Player, State};
use nitro_wire::types::{WindowState, modifier};

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/tiny.mp4");

#[test]
fn libav_decodes_a_b_frame_clip_in_pts_order() {
    let mut d = LibavDecoder::open(Path::new(TINY), 1).expect("open");
    let info = d.info().clone();
    assert_eq!((info.width, info.height), (96, 54));
    assert!((info.duration - 3.0).abs() < 0.2, "{}", info.duration);
    assert_eq!(info.codec, "h264");
    assert_eq!(info.matrix, Matrix::Bt601);
    let l = Nv12Layout::for_video(info.width, info.height);
    let mut buf = vec![0; l.frame_len()];
    let mut pts = Vec::new();
    while let Some(p) = d.next_frame(&mut buf, l).expect("decode") {
        pts.push(p);
    }
    assert_eq!(pts.len(), 30);
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "{pts:?}");
    assert_eq!(pts[0], 0);
    assert_eq!(pts[10], 1_000_000);
    // testsrc2 is colourful: chroma is not flat grey.
    assert!(buf[l.luma_len()..].iter().any(|&c| c.abs_diff(128) > 20));

    // A seek lands on the keyframe at or before the target.
    d.seek(1.5).expect("seek");
    let first = d.next_frame(&mut buf, l).expect("decode").expect("a frame");
    assert!((0..=1_500_000).contains(&first), "{first}");
}

struct Built {
    h: Harness<Player>,
}

fn harness(frames: u32, opts: Opts) -> Built {
    harness_on(SyntheticDecoder::new(64, 36, 30, frames), opts)
}

fn harness_on(dec: SyntheticDecoder, opts: Opts) -> Built {
    let info = dec.info().clone();
    let p = Player::new(Box::new(dec), opts).expect("player");
    let wake = rustix::io::dup(p.wake_fd()).expect("dup");
    let mut h = Harness::sized("nitro-video", p, Size::new(256.0, 144.0), move |ui| {
        player::install(ui, &info, wake.as_fd())
    });
    h.auto_fds(true);
    Built { h }
}

fn wait_presented(h: &mut Harness<Player>, n: u64) {
    h.wait_for("frames presented", |h| {
        h.settle();
        h.state().stats.presented >= n
    });
}

#[test]
fn frames_are_presented_in_pts_order() {
    let mut b = harness(300, Opts::default());
    let h = &mut b.h;
    wait_presented(h, 10);
    let shown = h.state().stats.shown.clone();
    assert!(shown.windows(2).all(|w| w[0] < w[1]), "{shown:?}");
    assert_eq!(h.state().state(), State::Playing);
    assert!(h.state().error.is_none());
}

#[test]
fn pause_stops_presents_and_play_resumes() {
    let mut b = harness(600, Opts::default());
    let h = &mut b.h;
    wait_presented(h, 3);
    h.key(nitro_ui::event::key::SPACE);
    assert_eq!(h.state().state(), State::Paused);
    h.settle();
    let sent = h.state().stats.sent;
    std::thread::sleep(std::time::Duration::from_millis(200));
    h.settle();
    assert_eq!(h.state().stats.sent, sent, "no presents while paused");
    h.key(nitro_ui::event::key::SPACE);
    let n = h.state().stats.presented;
    wait_presented(h, n + 3);
}

#[test]
fn a_seek_reanchors_at_the_target() {
    let mut b = harness(3000, Opts::default());
    let h = &mut b.h;
    wait_presented(h, 2);
    h.key(nitro_ui::event::key::SPACE); // pause, so the landing frame stays
    let (ui, p) = h.parts();
    p.request_seek(ui, 40.0);
    h.wait_for("the seek to land", |h| {
        h.settle();
        (h.state().position() - 40.0).abs() < 0.05
            && h.state().stats.shown.last() == Some(&40_000_000)
    });
    // Right steps 5 s on from there.
    h.key(nitro_ui::event::key::RIGHT);
    h.wait_for("the step to land", |h| {
        h.settle();
        h.state().stats.shown.last() == Some(&45_000_000)
    });
}

#[test]
fn the_controls_hide_after_the_timeout_and_come_back_on_motion() {
    let mut b = harness(3000, Opts::default());
    let h = &mut b.h;
    wait_presented(h, 1);
    let ids = h.state().ids().expect("ids");
    assert!(h.state().controls_visible());
    h.advance_timers(HIDE_MS + 10);
    h.settle();
    assert!(!h.state().controls_visible());
    assert!(!h.ui().is_visible(ids.bar));
    h.move_pointer(nitro_ui::Point::new(50.0, 40.0));
    assert!(h.state().controls_visible());
    assert!(h.ui().is_visible(ids.bar));
}

#[test]
fn f_toggles_fullscreen() {
    let mut b = harness(3000, Opts::default());
    let h = &mut b.h;
    wait_presented(h, 1);
    h.key(33); // KEY_F
    h.wait_for("fullscreen", |h| {
        h.settle();
        h.ui().window_state() == WindowState::Fullscreen
    });
    let (w, hh) = nitro_ui::test::OUTPUT;
    assert_eq!(h.ui().window_size(), Size::new(w as f32, hh as f32));
    h.key(nitro_ui::event::key::ESC);
    h.wait_for("windowed", |h| {
        h.settle();
        h.ui().window_state() == WindowState::Normal
    });
}

#[test]
fn a_short_stream_ends() {
    let mut b = harness(
        6,
        Opts {
            frames: 0,
            ..Opts::default()
        },
    );
    let h = &mut b.h;
    h.wait_for("the end", |h| {
        h.settle();
        h.state().state() == State::Ended
    });
    assert!(h.state().stats.presented >= 1);
}

#[test]
fn frames_limit_quits() {
    let mut b = harness(
        300,
        Opts {
            frames: 5,
            ..Opts::default()
        },
    );
    let h = &mut b.h;
    h.wait_for("quit after 5 frames", |h| {
        h.settle();
        h.ui().should_quit()
    });
}

fn hw(pool: usize, m: u64, hwdec: HwDec) -> Built {
    let dec = SyntheticDecoder::with_dmabuf(64, 36, 30, 3000, pool, m).expect("synthetic hw");
    harness_on(
        dec,
        Opts {
            hwdec,
            ..Opts::default()
        },
    )
}

fn wait_output(h: &mut Harness<Player>) -> Output {
    h.wait_for("the output choice", |h| {
        h.settle();
        h.state().output().is_some()
    });
    h.state().output().expect("chosen")
}

#[test]
fn a_linear_hw_decoder_presents_its_surfaces_as_dmabufs() {
    let mut b = hw(6, modifier::LINEAR, HwDec::Auto);
    let h = &mut b.h;
    assert_eq!(wait_output(h), Output::DmaBuf);
    wait_presented(h, 20);
    let p = h.state();
    assert!(p.error.is_none(), "{:?}", p.error);
    assert_eq!(p.decode_mode(), "vaapi-dmabuf");
    assert!(p.fallback().is_none(), "{:?}", p.fallback());
    // Each surface registered once, whatever the frame count.
    assert!(p.buffer_count() <= 6, "{}", p.buffer_count());
    let shown = p.stats.shown.clone();
    assert!(shown.windows(2).all(|w| w[0] < w[1]), "{shown:?}");
    // Seeks keep working and never over-hold the pool (the synthetic
    // decoder errors out if the player keeps every surface).
    h.key(nitro_ui::event::key::SPACE);
    let (ui, p) = h.parts();
    p.request_seek(ui, 40.0);
    h.wait_for("the seek to land", |h| {
        h.settle();
        h.state().stats.shown.last() == Some(&40_000_000)
    });
    h.key(nitro_ui::event::key::SPACE);
    let n = h.state().stats.presented;
    wait_presented(h, n + 10);
    assert!(h.state().error.is_none(), "{:?}", h.state().error);
    assert!(h.state().buffer_count() <= 6);
}

#[test]
fn a_big_pool_is_capped_at_the_registration_limit() {
    let mut b = hw(30, modifier::LINEAR, HwDec::Auto);
    let h = &mut b.h;
    assert_eq!(wait_output(h), Output::DmaBuf);
    wait_presented(h, 60);
    let p = h.state();
    assert!(p.error.is_none(), "{:?}", p.error);
    assert_eq!(
        p.buffer_count(),
        nitro_video::decode::MAX_DMABUF_BUFFERS,
        "30 surfaces round-robin fill the cap and evict"
    );
}

#[test]
fn a_tiled_hw_decoder_downloads_while_the_server_cannot_show_it() {
    // The fake server imports only linear NV12 (no planes): auto (with
    // no software decoder to swap to) and a forced dmabuf both fall back
    // to download with a reason, and never send a CreateDmabufBuffer the
    // server would refuse.
    for pref in [HwDec::Auto, HwDec::DmaBuf] {
        let mut b = hw(6, modifier::I915_Y_TILED, pref);
        let h = &mut b.h;
        assert_eq!(wait_output(h), Output::Shm, "{pref:?}");
        wait_presented(h, 5);
        let p = h.state();
        assert!(p.error.is_none(), "{:?}", p.error);
        assert_eq!(p.decode_mode(), "vaapi-download");
        let why = p.fallback().expect("a reason");
        assert!(why.contains("does not import"), "{why}");
        assert!(p.summary_line().contains("decode=vaapi-download"));
    }
}

#[test]
fn auto_swaps_a_tiled_hw_decoder_for_software() {
    let dec = SyntheticDecoder::with_dmabuf(64, 36, 30, 3000, 6, modifier::I915_Y_TILED)
        .expect("synthetic hw");
    let info = dec.info().clone();
    let mut p = Player::new(Box::new(dec), Opts::default()).expect("player");
    p.set_software(Box::new(|| {
        Ok(Box::new(SyntheticDecoder::new(64, 36, 30, 3000)) as Box<dyn Decoder>)
    }));
    let wake = rustix::io::dup(p.wake_fd()).expect("dup");
    let mut h = Harness::sized("nitro-video", p, Size::new(256.0, 144.0), move |ui| {
        player::install(ui, &info, wake.as_fd())
    });
    h.auto_fds(true);
    assert_eq!(wait_output(&mut h), Output::Shm);
    wait_presented(&mut h, 5);
    let p = h.state();
    assert!(p.error.is_none(), "{:?}", p.error);
    assert_eq!(p.decode_mode(), "software");
    assert!(p.fallback().expect("why").contains("does not import"));
}

#[test]
fn download_is_honoured() {
    let mut b = hw(6, modifier::LINEAR, HwDec::Download);
    let h = &mut b.h;
    assert_eq!(wait_output(h), Output::Shm);
    wait_presented(h, 3);
    assert!(h.state().fallback().is_none());
}

#[test]
fn software_says_so() {
    let mut b = harness(300, Opts::default());
    let h = &mut b.h;
    wait_presented(h, 3);
    assert_eq!(h.state().decode_mode(), "software");
    assert!(h.state().summary_line().contains("decode=software"));
}

#[test]
fn open_falls_back_to_software_without_a_vaapi_device() {
    let o = nitro_video::ffmpeg::open(Path::new(TINY), HwDec::Auto, "/nonexistent/renderD128", 1)
        .expect("software fallback");
    assert!(o.decoder.hw().is_none());
    let why = o.fallback.expect("a reason");
    assert!(why.contains("/nonexistent/renderD128"), "{why}");
    let o = nitro_video::ffmpeg::open(Path::new(TINY), HwDec::Off, "/nonexistent/renderD128", 1)
        .expect("software");
    assert!(o.fallback.is_none());
}

/// Runs only where a VA-API device takes H.264 (skips, saying why,
/// elsewhere — CI has no /dev/dri).
#[test]
fn vaapi_decodes_the_clip_like_software_when_present() {
    let dev = nitro_video::ffmpeg::DEFAULT_VAAPI_DEVICE;
    let mut hw = match LibavDecoder::open_hw(Path::new(TINY), dev) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipped: no VA-API here: {e:?}");
            return;
        }
    };
    let info = hw.info().clone();
    assert!(hw.hw().is_some());
    let l = Nv12Layout::for_video(info.width, info.height);
    let mut buf = vec![0; l.frame_len()];
    let mut pts = Vec::new();
    // Download half, dma-buf the rest: both come out in pts order.
    while pts.len() < 15 {
        let Some(p) = hw.next_frame(&mut buf, l).expect("download") else { break };
        pts.push(p);
    }
    while let Some(f) = hw.next_dmabuf().expect("dmabuf") {
        assert_eq!(f.desc.planes.len(), 2);
        pts.push(f.pts_us);
        hw.release(f.key);
    }
    let mut sw = LibavDecoder::open(Path::new(TINY), 1).expect("sw");
    let mut sw_pts = Vec::new();
    while let Some(p) = sw.next_frame(&mut buf, l).expect("decode") {
        sw_pts.push(p);
    }
    assert_eq!(pts, sw_pts);
}

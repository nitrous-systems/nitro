//! nitro-video end to end: the libav backend on a real (tiny) clip, and
//! the player in the in-process harness on the synthetic decoder.

use std::os::fd::AsFd as _;
use std::path::Path;

use nitro_ui::Size;
use nitro_ui::test::Harness;
use nitro_video::decode::{Decoder, Matrix, Nv12Layout, SyntheticDecoder};
use nitro_video::ffmpeg::LibavDecoder;
use nitro_video::player::{self, HIDE_MS, Opts, Player, State};
use nitro_wire::types::WindowState;

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
    let dec = SyntheticDecoder::new(64, 36, 30, frames);
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
            fullscreen: false,
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
            fullscreen: false,
        },
    );
    let h = &mut b.h;
    h.wait_for("quit after 5 frames", |h| {
        h.settle();
        h.ui().should_quit()
    });
}

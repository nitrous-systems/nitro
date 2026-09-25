//! The audio path without a window: the engine, its decoders and its
//! outputs, driven through [`Player`] the way the window drives it.
//!
//! The helper programs are **fakes**: shell scripts in a temporary
//! directory, found by the same injected-search-path code the app uses
//! on `$PATH`. A fake `ffmpeg` writes a known amount of silence, a fake
//! `ffprobe` prints known tags, and a fake `pw-cat` copies its stdin to
//! a file — so what the engine sends to a sound server is checked to
//! the byte on a machine that has none.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nitro_amp::engine::{Cmd, Player, State, Status};
use nitro_amp::sink::Backend;
use nitro_amp::source::{EXTERNAL_RATE, Tools};
use nitro_amp::wav;

/// A fresh temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("nitro-amp-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An executable shell script at `dir/name`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// A stereo WAV of `secs` of a 440 Hz tone at `rate`.
fn tone(dir: &Path, name: &str, rate: u32, secs: f32) -> PathBuf {
    let frames = (rate as f32 * secs) as usize;
    let mut samples = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin();
        samples.extend([v, v]);
    }
    let p = dir.join(name);
    std::fs::write(&p, wav::encode_i16(rate, 2, &samples)).unwrap();
    p
}

/// Poll the status until `f` holds.
fn until(p: &Player, what: &str, f: impl Fn(&Status) -> bool) -> Status {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = p.status();
        if f(&st) {
            return st;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {st:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_wav_plays_to_its_end() {
    let d = TempDir::new("end");
    let f = tone(d.path(), "t.wav", 8_000, 0.5);
    let mut p = Player::spawn(Tools::default(), Backend::Unpaced).unwrap();
    p.send(Cmd::Load {
        path: f,
        play: true,
        token: 1,
    });
    let st = until(&p, "the end", |s| s.ended);
    assert_eq!(st.token, 1);
    assert_eq!(st.state, State::Stopped);
    assert_eq!(st.meta.rate, Some(8_000));
    assert_eq!(st.meta.channels, Some(2));
    assert!((st.duration.unwrap() - 0.5).abs() < 1e-6);
    assert!(
        (st.position - 0.5).abs() < 1e-6,
        "clock at the end: {}",
        st.position
    );
    assert!(st.error.is_none());

    // Play after the end starts again from the top.
    p.send(Cmd::Play);
    until(&p, "a second run to the end", |s| s.ended);
}

#[test]
fn pause_holds_the_position_and_stop_rewinds() {
    let d = TempDir::new("pause");
    let f = tone(d.path(), "t.wav", 8_000, 5.0);
    // Paced to the wall clock, so there is time to pause mid-track.
    let mut p = Player::spawn(Tools::default(), Backend::Silent).unwrap();
    p.send(Cmd::Load {
        path: f,
        play: true,
        token: 1,
    });
    until(&p, "some progress", |s| s.position > 0.2);
    p.send(Cmd::Pause);
    let held = until(&p, "the pause", |s| s.state == State::Paused).position;
    std::thread::sleep(Duration::from_millis(150));
    let st = p.status();
    assert!((st.position - held).abs() < 1e-9, "a paused clock moved");
    assert!(
        held < 1.0,
        "silent output is paced, not free-running: {held}"
    );

    // Pause again resumes.
    p.send(Cmd::Pause);
    until(&p, "resumed progress", |s| {
        s.state == State::Playing && s.position > held + 0.05
    });

    p.send(Cmd::Stop);
    let st = until(&p, "the stop", |s| s.state == State::Stopped);
    assert!(st.position.abs() < 1e-9);
    assert!(!st.ended, "a stop is not an end");
}

#[test]
fn seeks_land_where_they_were_asked_and_coalesce() {
    let d = TempDir::new("seek");
    let f = tone(d.path(), "t.wav", 8_000, 10.0);
    let mut p = Player::spawn(Tools::default(), Backend::Silent).unwrap();
    p.send(Cmd::Load {
        path: f,
        play: false,
        token: 1,
    });
    // A drag's worth of seeks, then a pause-free read of where it went.
    for s in [1.0, 2.0, 3.0, 7.5] {
        p.send(Cmd::Seek(s));
    }
    let st = until(&p, "the last seek", |s| (s.position - 7.5).abs() < 1e-9);
    assert_eq!(st.state, State::Stopped, "seeking does not start playback");
    // Past the end is clamped to it.
    p.send(Cmd::Seek(99.0));
    until(&p, "a clamped seek", |s| (s.position - 10.0).abs() < 1e-9);
}

#[test]
fn a_missing_file_is_an_error_not_a_hang() {
    let mut p = Player::spawn(Tools::default(), Backend::Unpaced).unwrap();
    p.send(Cmd::Load {
        path: "/nonexistent/x.wav".into(),
        play: true,
        token: 4,
    });
    let st = until(&p, "an error", |s| s.token == 4 && s.error.is_some());
    assert_eq!(st.state, State::Stopped);
    assert!(st.error.unwrap().contains("no such file"));
}

#[test]
fn without_ffmpeg_other_formats_say_why() {
    let d = TempDir::new("noff");
    let f = d.path().join("song.mp3");
    std::fs::write(&f, b"ID3 not really").unwrap();
    let mut p = Player::spawn(Tools::default(), Backend::Unpaced).unwrap();
    p.send(Cmd::Load {
        path: f,
        play: true,
        token: 1,
    });
    let st = until(&p, "an error", |s| s.error.is_some());
    let e = st.error.unwrap();
    assert!(e.contains("ffmpeg is not installed"), "{e}");
}

#[test]
fn ffmpeg_decodes_what_wav_does_not_and_ffprobe_names_it() {
    let d = TempDir::new("ff");
    let bin = d.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A tenth of a second of silence at the external rate, whatever it
    // is asked to decode; and it records its arguments.
    let args_file = d.path().join("args_file");
    let bytes = EXTERNAL_RATE as usize / 10 * 8;
    script(
        &bin,
        "ffmpeg",
        &format!(
            "echo \"$@\" > '{}'\nhead -c {bytes} /dev/zero",
            args_file.display()
        ),
    );
    script(
        &bin,
        "ffprobe",
        "printf 'sample_rate=48000\\nchannels=2\\nduration=0.100000\\nbit_rate=192000\\nTAG:artist=Band\\nTAG:title=Song\\n'",
    );
    let tools = Tools::find_in(std::slice::from_ref(&bin));
    assert!(tools.ffmpeg.is_some() && tools.ffprobe.is_some());
    let f = d.path().join("song.flac");
    std::fs::write(&f, b"fLaC").unwrap();
    let mut p = Player::spawn(tools, Backend::Unpaced).unwrap();
    p.send(Cmd::Load {
        path: f.clone(),
        play: true,
        token: 1,
    });
    let st = until(&p, "the end", |s| s.ended);
    assert_eq!(st.meta.display_title().as_deref(), Some("Band - Song"));
    assert_eq!(st.meta.kbps, Some(192));
    assert_eq!(st.rate, EXTERNAL_RATE);
    assert!((st.position - 0.1).abs() < 1e-3, "{}", st.position);
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(recorded.contains("-f f32le"), "{recorded}");
    assert!(recorded.contains(&f.display().to_string()), "{recorded}");

    // A seek restarts the decoder at the new offset.
    p.send(Cmd::Seek(0.05));
    p.send(Cmd::Play);
    until(&p, "the restarted decoder to finish", |s| {
        s.ended && s.position > 0.0
    });
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(recorded.starts_with("-nostdin"), "{recorded}");
}

#[test]
fn the_output_process_gets_exactly_the_samples_scaled_by_the_volume() {
    let d = TempDir::new("out");
    let bin = d.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let out = d.path().join("pcm");
    let args_file = d.path().join("args_file");
    script(
        &bin,
        "pw-cat",
        &format!(
            "echo \"$@\" > '{}'\ncat > '{}'",
            args_file.display(),
            out.display()
        ),
    );
    let backend = Backend::detect_in(&[bin]);
    assert!(matches!(backend, Backend::PwCat(_)), "{backend:?}");
    assert!(backend.is_audible());

    let f = tone(d.path(), "t.wav", 8_000, 0.25);
    let mut p = Player::spawn(Tools::default(), backend).unwrap();
    p.send(Cmd::Volume(0.5));
    p.send(Cmd::Load {
        path: f,
        play: true,
        token: 1,
    });
    until(&p, "the end", |s| s.ended);
    // The output is kept alive for a following track; dropping the
    // player ends it, and the fake's `cat` finishes writing.
    drop(p);
    let deadline = Instant::now() + Duration::from_secs(5);
    let pcm = loop {
        let pcm = std::fs::read(&out).unwrap_or_default();
        if pcm.len() >= 2_000 * 8 || Instant::now() > deadline {
            break pcm;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(pcm.len(), 2_000 * 8, "every frame, as f32le stereo");
    let samples: Vec<f32> = pcm
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let peak = samples.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    // A 0.5 tone at half volume: gain 0.25, so a peak of 0.125.
    assert!((peak - 0.125).abs() < 0.005, "peak {peak}");
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(recorded.contains("--rate 8000"), "{recorded}");
    assert!(recorded.contains("--format f32"), "{recorded}");
}

//! Drive the whole server loop in-process on the fake backend: connect to
//! the control socket, check `outputs`, `shot` pixels, `stats`, `quit`.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_kms::Image;
use nitro_server::render::{BAR_COLOR, BAR_WIDTH, expected_color};
use nitro_server::{Config, run};

struct Harness {
    dir: PathBuf,
    path: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, width: u32, height: u32, bar_stop: Option<Duration>) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(width, height, &path);
        config.bar_stop = bar_stop;
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            thread: Some(thread),
        };
        // Wait for the socket to accept.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if UnixStream::connect(&h.path).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "server never bound its socket");
            std::thread::sleep(Duration::from_millis(10));
        }
        h
    }

    fn connect(&self) -> BufReader<UnixStream> {
        let s = UnixStream::connect(&self.path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        BufReader::new(s)
    }

    fn request_text(&self, req: &str) -> Vec<String> {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut lines = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = c.read_line(&mut line).unwrap();
            assert!(n > 0, "connection closed mid-reply");
            let l = line.trim_end_matches('\n').to_owned();
            if l.is_empty() {
                break;
            }
            lines.push(l);
        }
        lines
    }

    fn shot(&self, name: Option<&str>) -> Result<Image, String> {
        let mut c = self.connect();
        let req = match name {
            Some(n) => format!("shot {n}\n"),
            None => "shot\n".to_owned(),
        };
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut header = String::new();
        c.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if let Some(msg) = header.strip_prefix("err ") {
            return Err(msg.to_owned());
        }
        let fields: Vec<u32> = header
            .strip_prefix("ok ")
            .expect("ok header")
            .split(' ')
            .map(|f| f.parse().unwrap())
            .collect();
        let (width, height, stride) = (fields[0], fields[1], fields[2]);
        let mut data = vec![0u8; (stride * height) as usize];
        c.read_exact(&mut data).unwrap();
        Ok(Image {
            width,
            height,
            stride,
            data,
        })
    }

    fn quit(mut self) {
        let mut c = self.connect();
        c.get_mut().write_all(b"quit\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        assert_eq!(line, "ok\n");
        let t = self.thread.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !t.is_finished() {
            assert!(Instant::now() < deadline, "server did not stop after quit");
            std::thread::sleep(Duration::from_millis(10));
        }
        t.join().unwrap().expect("server returned an error");
        assert!(!self.path.exists(), "socket file removed on shutdown");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stat(lines: &[String], key: &str) -> u64 {
    lines
        .iter()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
        .unwrap_or_else(|| panic!("no `{key}` in {lines:?}"))
        .parse()
        .unwrap()
}

#[test]
fn outputs_shot_stats_quit_on_fake_backend() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("static", w, h, Some(Duration::ZERO));

    assert_eq!(
        h_.request_text("outputs\n"),
        ["ok", &format!("Virtual-1 {w}x{h}@60000")]
    );

    // The first frame is committed at startup; wait for it to flip so
    // `read_front` has a painted buffer either way.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let s = h_.request_text("stats\n");
        assert_eq!(s[0], "ok");
        if stat(&s, "frames") >= 1 {
            assert_eq!(stat(&s, "active"), 1);
            break;
        }
        assert!(Instant::now() < deadline, "no flip: {s:?}");
        std::thread::sleep(Duration::from_millis(10));
    }

    let img = h_.shot(None).unwrap();
    assert_eq!((img.width, img.height, img.stride), (w, h, w * 4));
    // Bar frozen at x = 0: frame, bar, gradient at a few exact spots.
    for (x, y) in [
        (0, 0),
        (2, 100),
        (w - 1, h - 1),
        (5, 5),
        (39, 100),
        (40, 100),
        (100, 4),
        (160, 100),
        (200, h - 5),
        (w - 5, 50),
    ] {
        assert_eq!(
            img.pixel(x, y),
            expected_color(x, y, w, h, 0),
            "pixel ({x},{y})"
        );
    }
    // And the whole image, for good measure.
    for y in 0..h {
        for x in 0..w {
            assert_eq!(img.pixel(x, y), expected_color(x, y, w, h, 0), "({x},{y})");
        }
    }

    assert_eq!(h_.shot(Some("Virtual-1")).unwrap(), img);
    assert_eq!(
        h_.shot(Some("HDMI-A-9")),
        Err("no output named HDMI-A-9".to_owned())
    );
    let mut c = h_.connect();
    c.get_mut().write_all(b"bogus\n").unwrap();
    let mut line = String::new();
    c.read_line(&mut line).unwrap();
    assert_eq!(line, "err unknown request `bogus`\n");

    // Static mode: frames stop after the two full repaints.
    std::thread::sleep(Duration::from_millis(100));
    let s = h_.request_text("stats\n");
    assert_eq!(stat(&s, "frames"), 2, "{s:?}");
    assert_eq!(stat(&s, "flips_pending"), 0);

    h_.quit();
}

#[test]
fn moving_bar_advances_and_keeps_flipping() {
    let (w, h) = (128, 32);
    let h_ = Harness::start("moving", w, h, None);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut positions = Vec::new();
    while positions.len() < 3 {
        let img = h_.shot(None).unwrap();
        let row: Vec<bool> = (0..w).map(|x| img.pixel(x, h / 2) == BAR_COLOR).collect();
        if let Some(first) = row.iter().position(|&b| b) {
            let last = row.iter().rposition(|&b| b).unwrap() as u32;
            let first = first as u32;
            // The 4-px frame hides the bar's left edge when it sits at 0.
            let bar_x = if first > 4 {
                first
            } else {
                last + 1 - BAR_WIDTH
            };
            assert_eq!(bar_x % 8, 0, "bar at {bar_x}");
            if positions.last() != Some(&bar_x) {
                positions.push(bar_x);
            }
        }
        assert!(Instant::now() < deadline, "bar never moved: {positions:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let s = h_.request_text("stats\n");
    assert!(stat(&s, "frames") >= 3, "{s:?}");
    assert!(stat(&s, "flip_interval_mean_us") > 0, "{s:?}");
    h_.quit();
}

#[test]
fn many_clients_and_partial_lines() {
    let h_ = Harness::start("clients", 64, 64, Some(Duration::ZERO));
    let mut conns: Vec<_> = (0..8).map(|_| h_.connect()).collect();
    for c in &mut conns {
        c.get_mut().write_all(b"out").unwrap();
    }
    for c in &mut conns {
        c.get_mut().write_all(b"puts\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        assert_eq!(line, "ok\n");
    }
    // Two requests on one connection.
    let mut c = h_.connect();
    c.get_mut().write_all(b"stats\nstats\n").unwrap();
    let mut blank = 0;
    let mut line = String::new();
    while blank < 2 {
        line.clear();
        assert!(c.read_line(&mut line).unwrap() > 0);
        if line == "\n" {
            blank += 1;
        }
    }
    drop(conns);
    h_.quit();
}

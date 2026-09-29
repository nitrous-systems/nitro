//! Cross-client Surface sharing (#3904) end to end on the fake backend: an
//! owner exports a Surface node, another connection imports the token and
//! presents into it, and every way the sharing can end or be misused —
//! destroy, disconnect, re-export, displacement, a bogus token, the wrong
//! client, a remote link, a missing cap — behaves as `docs/wire.md` §
//! Surface sharing says.
//!
//! The fake output runs at **2 Hz** where a test needs a flip to be
//! pending, so "queued but not latched" is a half-second window.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{IRect, Rect, Size};
use nitro_server::{BackendKind, Config, run};
use nitro_shm::MappingMut;
use nitro_wire::Endpoint;
use nitro_wire::client::Connection;
use nitro_wire::msg::{ClientMsg, CreateSurfaceBuffer, PresentSurface, ServerMsg, SetBounds};
use nitro_wire::types::{
    BufferId, ColorMatrix, ColorRange, ErrorCode, Layer, NodeId, ShareToken, caps, format,
};

const OUT: (u32, u32) = (320, 240);
const SIDE: u32 = 32;

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Harness {
    dir: PathBuf,
    path: PathBuf,
    wire_path: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    /// A fake output at `mhz` millihertz; `remote` also opens the TCP
    /// listener on a kernel-chosen port.
    fn start(name: &str, mhz: u32, remote: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-share-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake {
            width: OUT.0,
            height: OUT.1,
        };
        config.fake_modes = vec![(OUT.0, OUT.1, mhz)];
        if remote {
            let conf = dir.join("config").join("server.conf");
            std::fs::create_dir_all(conf.parent().unwrap()).unwrap();
            std::fs::write(&conf, "remote.listen = 127.0.0.1:0\n").unwrap();
            config.config_path = Some(conf);
        }
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
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

    fn stat_text(&self, key: &str) -> String {
        let lines = self.request_text("stats\n");
        lines
            .iter()
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
            .unwrap_or_else(|| panic!("no `{key}` in {lines:?}"))
            .to_owned()
    }

    fn stat(&self, key: &str) -> u64 {
        self.stat_text(key).parse().unwrap()
    }

    /// The front buffer as `(stride, bytes)`.
    fn shot(&self) -> (u32, Vec<u8>) {
        let mut c = self.connect();
        c.get_mut().write_all(b"shot\n").unwrap();
        let mut header = String::new();
        c.read_line(&mut header).unwrap();
        let f: Vec<u32> = header
            .trim_end()
            .strip_prefix("ok ")
            .expect("ok header")
            .split(' ')
            .map(|f| f.parse().unwrap())
            .collect();
        let mut data = vec![0u8; (f[2] * f[1]) as usize];
        c.read_exact(&mut data).unwrap();
        (f[2], data)
    }

    fn settle(&self) {
        let mut stable = 0;
        let mut last = u64::MAX;
        wait_for("the server to go quiet", || {
            let frames = self.stat("frames");
            if self.stat("flips_pending") == 0 && frames == last {
                stable += 1;
            } else {
                stable = 0;
            }
            last = frames;
            std::thread::sleep(Duration::from_millis(8));
            stable >= 3
        });
    }

    fn client(&self, name: &str) -> Connection {
        Connection::connect(&self.wire_path, name).expect("wire connect")
    }

    /// A local client that listed `SHARE`, `SURFACE` and `RELEASE`.
    fn sharer(&self, name: &str) -> (Connection, Vec<ServerMsg>) {
        let mut conn = self.client(name);
        assert!(conn.has_caps(caps::SHARE), "caps = {:#x}", conn.caps());
        conn.client_caps(caps::SHARE | caps::SURFACE | caps::RELEASE)
            .unwrap();
        (conn, Vec::new())
    }

    fn remote_client(&self, name: &str) -> Connection {
        let mut addr = String::new();
        wait_for("the remote listener", || {
            addr = self.stat_text("remote_listen");
            addr != "off"
        });
        let endpoint = Endpoint::parse(&format!("tcp://{addr}")).unwrap();
        Connection::connect_endpoint(&endpoint, name).expect("tcp connect")
    }

    fn quit(mut self) {
        let mut c = self.connect();
        c.get_mut().write_all(b"quit\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        let t = self.thread.take().unwrap();
        wait_for("the server thread to stop", || t.is_finished());
        t.join().unwrap().expect("server returned an error");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Poll until `f` matches something in `seen`.
fn expect<T>(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    what: &str,
    f: impl Fn(&ServerMsg) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = seen.iter().find_map(&f) {
            return found;
        }
        assert!(Instant::now() < deadline, "no {what}; got {seen:?}");
        let _ = conn.flush();
        if let Err(e) = conn.poll(seen) {
            if let Some(found) = seen.iter().find_map(&f) {
                return found;
            }
            panic!("{what}: {e}; got {seen:?}");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Drain whatever is readable now into `seen` (after a settle).
fn drain(conn: &mut Connection, seen: &mut Vec<ServerMsg>) {
    let _ = conn.flush();
    let _ = conn.poll(seen);
}

fn presented(conn: &mut Connection, seen: &mut Vec<ServerMsg>, serial: u32) {
    expect(conn, seen, &format!("Presented {serial}"), |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == serial).then_some(())
    });
}

fn released(conn: &mut Connection, seen: &mut Vec<ServerMsg>, id: BufferId) {
    expect(conn, seen, &format!("release of {}", id.raw()), |m| {
        matches!(m, ServerMsg::BufferReleased(r) if r.id == id).then_some(())
    });
}

fn revoked(conn: &mut Connection, seen: &mut Vec<ServerMsg>, id: NodeId) {
    expect(conn, seen, &format!("SurfaceRevoked {}", id.raw()), |m| {
        matches!(m, ServerMsg::SurfaceRevoked(r) if r.id == id).then_some(())
    });
}

fn was_presented(seen: &[ServerMsg], serial: u32) -> bool {
    seen.iter()
        .any(|m| matches!(m, ServerMsg::Presented(p) if p.serial == serial))
}

fn error(conn: &mut Connection, seen: &mut Vec<ServerMsg>) -> (ErrorCode, String) {
    expect(conn, seen, "Error", |m| match m {
        ServerMsg::Error(e) => Some((e.code, e.msg.clone())),
        _ => None,
    })
}

/// A solid XR24 buffer.
struct Buf {
    id: BufferId,
    fd: OwnedFd,
    _map: MappingMut,
}

impl Buf {
    fn new(id: u32, bgr: [u8; 3]) -> Self {
        let len = SIDE * SIDE * 4;
        let fd = nitro_shm::create_sealed("nitro-share-test", u64::from(len)).unwrap();
        let mut map = MappingMut::map_mut(fd.as_fd(), len as usize).unwrap();
        for px in map.as_bytes_mut().chunks_exact_mut(4) {
            px.copy_from_slice(&[bgr[0], bgr[1], bgr[2], 0xff]);
        }
        Self {
            id: BufferId(id),
            fd,
            _map: map,
        }
    }

    fn create(&self) -> CreateSurfaceBuffer {
        CreateSurfaceBuffer {
            id: self.id,
            width: SIDE,
            height: SIDE,
            format: format::XR24,
            size: SIDE * SIDE * 4,
            offset0: 0,
            stride0: SIDE * 4,
            offset1: 0,
            stride1: 0,
            fd: self.fd.try_clone().unwrap(),
        }
    }
}

const ROOT: NodeId = NodeId(1);
const SURF: NodeId = NodeId(2);
/// The importer's id for the shared node — deliberately the owner's
/// `SURF` number, since ids are per connection.
const IMP: NodeId = NodeId(2);

/// The owner's window: a 128×96 window with Surface `SURF` at (8, 8),
/// SIDE×SIDE, plus `bufs`. Returns the window's position.
fn owner_window(conn: &mut Connection, seen: &mut Vec<ServerMsg>, bufs: &[&Buf]) -> (u32, u32) {
    let mut tx = conn
        .tx()
        .create_window(ROOT, "owner", Size::new(128.0, 96.0), Layer::Normal)
        .create_surface(SURF, ROOT, Rect::new(8.0, 8.0, SIDE as f32, SIDE as f32));
    for b in bufs {
        tx = tx.create_surface_buffer(b.create());
    }
    tx.commit(1).unwrap();
    conn.flush().unwrap();
    let c = expect(conn, seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == ROOT => Some(*c),
        _ => None,
    });
    presented(conn, seen, 1);
    (c.position.x as u32 + 8, c.position.y as u32 + 8)
}

fn export(conn: &mut Connection, seen: &mut Vec<ServerMsg>, id: NodeId) -> ShareToken {
    conn.export_surface(id).unwrap();
    conn.flush().unwrap();
    let t = expect(conn, seen, "SurfaceExported", |m| match m {
        ServerMsg::SurfaceExported(e) if e.id == id => Some(e.token),
        _ => None,
    });
    seen.retain(|m| !matches!(m, ServerMsg::SurfaceExported(_)));
    t
}

/// Register `bufs` on an importer (a commit of their own).
fn buffers(conn: &mut Connection, seen: &mut Vec<ServerMsg>, bufs: &[&Buf], serial: u32) {
    let mut tx = conn.tx();
    for b in bufs {
        tx = tx.create_surface_buffer(b.create());
    }
    tx.commit(serial).unwrap();
    conn.flush().unwrap();
    presented(conn, seen, serial);
}

fn import(conn: &mut Connection, token: ShareToken, id: NodeId) {
    conn.import_surface(token, id).unwrap();
    conn.flush().unwrap();
}

fn present(conn: &mut Connection, id: NodeId, buf: &Buf, serial: u32) {
    conn.present_surface(PresentSurface {
        id,
        buffer: buf.id,
        serial,
        src: IRect::new(0, 0, SIDE.cast_signed(), SIDE.cast_signed()),
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        damage: vec![],
    })
    .unwrap();
    conn.flush().unwrap();
}

/// The shot's pixel at the centre of the surface, `[b, g, r]`.
fn centre(h: &Harness, at: (u32, u32)) -> [u8; 3] {
    let (stride, data) = h.shot();
    let (x, y) = (at.0 + SIDE / 2, at.1 + SIDE / 2);
    let o = (y * stride + x * 4) as usize;
    [data[o], data[o + 1], data[o + 2]]
}

/// Put a flip in flight from the owner: change something visible.
fn hold_a_flip(h: &Harness, conn: &mut Connection, serial: u32, x: f32) {
    conn.tx()
        .bounds(ROOT, Rect::new(0.0, 0.0, 128.0 + x, 96.0))
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    wait_for("a flip in flight", || h.stat("flips_pending") == 1);
}

const RED: [u8; 3] = [0, 0, 0xff];
const GREEN: [u8; 3] = [0, 0xff, 0];
const BLUE: [u8; 3] = [0xff, 0, 0];

#[test]
fn an_importer_presents_into_the_owners_window() {
    let h = Harness::start("happy", 60_000, false);
    let (mut a, mut sa) = h.sharer("browser");
    let at = owner_window(&mut a, &mut sa, &[]);
    let token = export(&mut a, &mut sa, SURF);

    let (mut b, mut sb) = h.sharer("gpu");
    let (red, green) = (Buf::new(1, RED), Buf::new(2, GREEN));
    buffers(&mut b, &mut sb, &[&red, &green], 1);
    import(&mut b, token, IMP);
    present(&mut b, IMP, &red, 10);
    presented(&mut b, &mut sb, 10);
    h.settle();
    assert_eq!(centre(&h, at), RED, "the importer's pixels, in A's window");

    present(&mut b, IMP, &green, 11);
    presented(&mut b, &mut sb, 11);
    released(&mut b, &mut sb, red.id);
    h.settle();
    assert_eq!(centre(&h, at), GREEN);
    drain(&mut a, &mut sa);
    assert!(
        !sa.iter().any(|m| matches!(
            m,
            ServerMsg::Presented(p) if p.serial == 10 || p.serial == 11
        ) || matches!(m, ServerMsg::BufferReleased(_))),
        "the owner hears nothing of B's frames: {sa:?}"
    );

    // B also gets a SurfaceHint for its import id.
    let hint = expect(&mut b, &mut sb, "SurfaceHint", |m| match m {
        ServerMsg::SurfaceHint(s) => Some(*s),
        _ => None,
    });
    assert_eq!((hint.id, hint.width, hint.height), (IMP, SIDE, SIDE));
    sb.clear();
    a.send(&ClientMsg::SetBounds(SetBounds {
        id: SURF,
        rect: Rect::new(8.0, 8.0, 48.0, 40.0),
    }))
    .unwrap();
    a.commit(2).unwrap();
    a.flush().unwrap();
    let hint = expect(&mut b, &mut sb, "a resize hint", |m| match m {
        ServerMsg::SurfaceHint(s) => Some(*s),
        _ => None,
    });
    assert_eq!((hint.id, hint.width, hint.height), (IMP, 48, 40));
    h.quit();
}

#[test]
fn the_owners_set_surface_cancels_the_importers_queued_frame() {
    let h = Harness::start("cancel", 2_000, false);
    let (mut a, mut sa) = h.sharer("browser");
    let blue = Buf::new(1, BLUE);
    owner_window(&mut a, &mut sa, &[&blue]);
    let token = export(&mut a, &mut sa, SURF);
    let (mut b, mut sb) = h.sharer("gpu");
    let red = Buf::new(1, RED);
    buffers(&mut b, &mut sb, &[&red], 1);
    import(&mut b, token, IMP);
    h.settle();

    hold_a_flip(&h, &mut a, 2, 1.0);
    present(&mut b, IMP, &red, 5);
    a.tx()
        .set_surface(
            SURF,
            blue.id,
            IRect::new(0, 0, SIDE.cast_signed(), SIDE.cast_signed()),
            ColorMatrix::Bt709,
            ColorRange::Limited,
        )
        .commit(3)
        .unwrap();
    a.flush().unwrap();
    released(&mut b, &mut sb, red.id);
    presented(&mut a, &mut sa, 3);
    h.settle();
    drain(&mut b, &mut sb);
    assert!(!was_presented(&sb, 5), "cancelled: {sb:?}");
    h.quit();
}

#[test]
fn destroying_the_node_revokes_and_later_presents_are_released() {
    let h = Harness::start("destroy", 60_000, false);
    let (mut a, mut sa) = h.sharer("browser");
    owner_window(&mut a, &mut sa, &[]);
    let token = export(&mut a, &mut sa, SURF);
    let (mut b, mut sb) = h.sharer("gpu");
    let red = Buf::new(1, RED);
    buffers(&mut b, &mut sb, &[&red], 1);
    import(&mut b, token, IMP);

    a.tx().destroy_node(SURF).commit(2).unwrap();
    a.flush().unwrap();
    revoked(&mut b, &mut sb, IMP);
    present(&mut b, IMP, &red, 7);
    released(&mut b, &mut sb, red.id);
    h.settle();
    drain(&mut b, &mut sb);
    assert!(!was_presented(&sb, 7), "{sb:?}");
    // Still connected, and the dead id can be destroyed and reused.
    b.tx().destroy_node(IMP).commit(3).unwrap();
    b.flush().unwrap();
    presented(&mut b, &mut sb, 3);
    h.quit();
}

#[test]
fn the_owner_disconnecting_revokes() {
    let h = Harness::start("ownergone", 60_000, false);
    let (mut a, mut sa) = h.sharer("browser");
    owner_window(&mut a, &mut sa, &[]);
    let token = export(&mut a, &mut sa, SURF);
    let (mut b, mut sb) = h.sharer("gpu");
    import(&mut b, token, IMP);
    h.settle();
    drop(a);
    revoked(&mut b, &mut sb, IMP);
    h.quit();
}

#[test]
fn an_importer_leaving_blanks_the_node_and_a_new_one_reimports() {
    let h = Harness::start("restart", 60_000, false);
    let (mut a, mut sa) = h.sharer("browser");
    let at = owner_window(&mut a, &mut sa, &[]);
    let token = export(&mut a, &mut sa, SURF);
    let background = centre(&h, at);

    let (mut b, mut sb) = h.sharer("gpu");
    let red = Buf::new(1, RED);
    buffers(&mut b, &mut sb, &[&red], 1);
    import(&mut b, token, IMP);
    present(&mut b, IMP, &red, 4);
    presented(&mut b, &mut sb, 4);
    h.settle();
    assert_eq!(centre(&h, at), RED);
    drop(b);
    h.settle();
    assert_eq!(centre(&h, at), background, "its buffers went with it");

    let (mut c, mut sc) = h.sharer("gpu2");
    let green = Buf::new(1, GREEN);
    buffers(&mut c, &mut sc, &[&green], 1);
    import(&mut c, token, IMP);
    present(&mut c, IMP, &green, 2);
    presented(&mut c, &mut sc, 2);
    h.settle();
    assert_eq!(centre(&h, at), GREEN);
    drain(&mut a, &mut sa);
    assert!(
        !sa.iter().any(|m| matches!(m, ServerMsg::Error(_))),
        "{sa:?}"
    );
    h.quit();
}

#[test]
fn a_second_importer_displaces_the_first() {
    let h = Harness::start("displace", 60_000, false);
    let (mut a, mut sa) = h.sharer("browser");
    owner_window(&mut a, &mut sa, &[]);
    let token = export(&mut a, &mut sa, SURF);
    let (mut b, mut sb) = h.sharer("gpu");
    import(&mut b, token, IMP);
    let (mut c, mut sc) = h.sharer("gpu2");
    import(&mut c, token, NodeId(9));
    revoked(&mut b, &mut sb, IMP);
    h.settle();
    drain(&mut c, &mut sc);
    assert!(
        !sc.iter().any(|m| matches!(m, ServerMsg::SurfaceRevoked(_))),
        "{sc:?}"
    );
    h.quit();
}

#[test]
fn re_exporting_rotates_the_token() {
    let h = Harness::start("rotate", 60_000, false);
    let (mut a, mut sa) = h.sharer("browser");
    owner_window(&mut a, &mut sa, &[]);
    let old = export(&mut a, &mut sa, SURF);
    let (mut b, mut sb) = h.sharer("gpu");
    import(&mut b, old, IMP);
    let new = export(&mut a, &mut sa, SURF);
    assert_ne!(old, new);
    revoked(&mut b, &mut sb, IMP);
    sb.clear();
    import(&mut b, old, NodeId(5));
    revoked(&mut b, &mut sb, NodeId(5));
    import(&mut b, new, NodeId(6));
    h.settle();
    drain(&mut b, &mut sb);
    assert!(
        !sb.iter()
            .any(|m| matches!(m, ServerMsg::SurfaceRevoked(r) if r.id == NodeId(6))),
        "{sb:?}"
    );
    h.quit();
}

#[test]
fn a_bogus_token_is_a_dead_import_not_a_disconnect() {
    let h = Harness::start("bogus", 60_000, false);
    let (mut b, mut sb) = h.sharer("gpu");
    let red = Buf::new(1, RED);
    buffers(&mut b, &mut sb, &[&red], 1);
    import(&mut b, ShareToken([0x42; 16]), IMP);
    revoked(&mut b, &mut sb, IMP);
    present(&mut b, IMP, &red, 3);
    released(&mut b, &mut sb, red.id);
    // Still alive: a commit is answered.
    b.commit(4).unwrap();
    b.flush().unwrap();
    presented(&mut b, &mut sb, 4);
    assert!(!was_presented(&sb, 3));
    h.quit();
}

/// Each misuse is fatal with `code`; `setup` gets an owner-exported token
/// and an importer to misbehave with.
fn misuse(
    name: &str,
    code: ErrorCode,
    act: impl FnOnce(&mut Connection, &mut Connection, ShareToken),
    victim_is_owner: bool,
) {
    let h = Harness::start(name, 60_000, false);
    let (mut a, mut sa) = h.sharer("browser");
    owner_window(&mut a, &mut sa, &[]);
    let token = export(&mut a, &mut sa, SURF);
    let (mut b, mut sb) = h.sharer("gpu");
    let red = Buf::new(1, RED);
    buffers(&mut b, &mut sb, &[&red], 1);
    act(&mut a, &mut b, token);
    let (got, msg) = if victim_is_owner {
        error(&mut a, &mut sa)
    } else {
        error(&mut b, &mut sb)
    };
    assert_eq!(got, code, "{name}: {msg}");
    h.quit();
}

#[test]
fn presenting_on_the_owners_raw_id_is_unknown_node() {
    misuse(
        "rawid",
        ErrorCode::UnknownNode,
        |_, b, _| {
            // B has no node 2 of its own and imported nothing.
            present(b, SURF, &Buf::new(1, RED), 3);
        },
        false,
    );
}

#[test]
fn only_present_and_destroy_apply_to_an_import() {
    misuse(
        "setbounds",
        ErrorCode::WrongKind,
        |_, b, t| {
            import(b, t, IMP);
            b.tx()
                .bounds(IMP, Rect::new(0.0, 0.0, 1.0, 1.0))
                .commit(2)
                .unwrap();
            b.flush().unwrap();
        },
        false,
    );
    misuse(
        "setsurface",
        ErrorCode::WrongKind,
        |_, b, t| {
            import(b, t, IMP);
            b.tx()
                .set_surface(
                    IMP,
                    BufferId(1),
                    IRect::new(0, 0, 4, 4),
                    ColorMatrix::Bt709,
                    ColorRange::Limited,
                )
                .commit(2)
                .unwrap();
            b.flush().unwrap();
        },
        false,
    );
}

#[test]
fn importing_ones_own_token_or_a_taken_id_is_a_protocol_error() {
    misuse(
        "own",
        ErrorCode::Protocol,
        |a, _, t| import(a, t, NodeId(50)),
        true,
    );
    misuse(
        "taken",
        ErrorCode::Protocol,
        |_, b, t| {
            b.tx()
                .create_window(NodeId(7), "b", Size::new(10.0, 10.0), Layer::Normal)
                .commit(2)
                .unwrap();
            b.flush().unwrap();
            import(b, t, NodeId(7));
        },
        false,
    );
}

#[test]
fn exporting_a_non_surface_or_an_unknown_node_fails() {
    misuse(
        "rect",
        ErrorCode::WrongKind,
        |a, _, _| {
            a.export_surface(ROOT).unwrap();
            a.flush().unwrap();
        },
        true,
    );
    misuse(
        "unknown",
        ErrorCode::UnknownNode,
        |a, _, _| {
            a.export_surface(NodeId(99)).unwrap();
            a.flush().unwrap();
        },
        true,
    );
}

#[test]
fn the_share_ops_need_the_cap_listed() {
    let h = Harness::start("nocap", 60_000, false);
    let mut conn = h.client("old");
    conn.client_caps(caps::SURFACE).unwrap();
    let mut seen = Vec::new();
    conn.export_surface(SURF).unwrap();
    conn.flush().unwrap();
    let (code, msg) = error(&mut conn, &mut seen);
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("SHARE"), "{msg}");
    h.quit();
}

#[test]
fn a_remote_link_has_no_share() {
    let h = Harness::start("remote", 60_000, true);
    let mut conn = h.remote_client("remote");
    assert_eq!(conn.caps() & caps::SHARE, 0, "never on a remote link");
    let mut seen = Vec::new();
    // Raw, since `client_caps` masks to what was advertised: listing it
    // anyway is refused, and so is the op itself.
    conn.send(&ClientMsg::ExportSurface(nitro_wire::msg::ExportSurface {
        id: SURF,
    }))
    .unwrap();
    conn.flush().unwrap();
    let (code, msg) = error(&mut conn, &mut seen);
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("remote"), "{msg}");
    h.quit();
}

//! Drag and drop into a `nitro-ui` app: the drop-target hooks
//! (`drag_over`, `drag_leave`, `dropped`) end to end through the
//! in-process server, with a raw-wire peer as the drag source.

use nitro_core::{Color, Point, Rect, Size};
use nitro_ui::clipboard::{READ_TIMEOUT_MS, TEXT_MIME};
use nitro_ui::dnd::{Accept, DragAction, DragOffer};
use nitro_ui::event::button;
use nitro_ui::test::{ClipboardPeer as Peer, Harness};
use nitro_ui::widgets::row;
use nitro_ui::{Built, Constraints, EventCx, MeasureCx, PaintCx, Ui, Widget, WidgetId};
use nitro_wire::types::drag_actions;

/// A box that paints itself (so the server hit-tests it) and, when
/// `accepts`, takes `text/plain;charset=utf-8` as a copy.
#[derive(Default)]
struct DropBox {
    accepts: bool,
    overs: usize,
    leaves: usize,
    drops: Vec<(String, Option<Vec<u8>>)>,
}

impl Widget<()> for DropBox {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, ()>, c: Constraints) -> Size {
        c.constrain(Size::new(100.0, 80.0))
    }
    fn paint(&mut self, cx: &mut PaintCx<'_, ()>) {
        let b = cx.bounds;
        // lint-colors: allow — a test widget, painted only to be hit.
        cx.fill_rect(
            0,
            Rect::new(0.0, 0.0, b.w, b.h),
            Color::rgb(0x30, 0x30, 0x30),
        );
    }
    fn drag_over(
        &mut self,
        _cx: &mut EventCx<'_, ()>,
        _pos: Point,
        offer: &DragOffer,
    ) -> Option<Accept> {
        self.overs += 1;
        if !self.accepts {
            return None;
        }
        offer
            .first_of(&[TEXT_MIME])
            .map(|m| Accept::new(DragAction::Copy, m))
    }
    fn drag_leave(&mut self, _cx: &mut EventCx<'_, ()>) {
        self.leaves += 1;
    }
    fn dropped(&mut self, _cx: &mut EventCx<'_, ()>, mime: &str, data: Option<&[u8]>) {
        self.drops.push((mime.to_owned(), data.map(<[u8]>::to_vec)));
    }
}

struct Scene {
    h: Harness<()>,
    peer: Peer,
    /// The accepting box and the one that declines.
    target: WidgetId,
    plain: WidgetId,
    /// The peer's window, in output coordinates.
    src: Rect,
    src_node: nitro_wire::types::NodeId,
}

fn scene(name: &str) -> Scene {
    let mut h = Harness::sized(name, (), Size::new(200.0, 80.0), |ui: &mut Ui<()>| {
        let t = ui.build(Built::new(DropBox {
            accepts: true,
            ..DropBox::default()
        }));
        let p = ui.build(Built::new(DropBox::default()));
        let root = ui.build(row());
        ui.attach(root, t).unwrap();
        ui.attach(root, p).unwrap();
        root
    });
    h.settle();
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);
    let mut peer = Peer::new(&h, "source");
    let (src_node, src) = peer.drag_window(&mut h);
    Scene {
        h,
        peer,
        target: kids[0],
        plain: kids[1],
        src,
        src_node,
    }
}

impl Scene {
    /// A point inside widget `id`, in output coordinates, clear of the
    /// peer's window (which is on top).
    fn over(&mut self, id: WidgetId) -> Point {
        let origin = self.h.ui().window_position_of(nitro_ui::WindowId::MAIN);
        let b = self.h.ui().bounds(id);
        let p = Point::new(origin.x + b.x + 10.0, origin.y + b.y + 10.0);
        assert!(
            !self.src.contains(p),
            "{p:?} is under the peer's window {:?}",
            self.src
        );
        p
    }

    /// Press on the peer's window and start a drag of `mimes` from it.
    fn start(&mut self, mimes: &[&str]) {
        let c = Point::new(self.src.x + self.src.w / 2.0, self.src.y + self.src.h / 2.0);
        self.h.move_pointer_abs(c);
        self.h.press(button::LEFT);
        self.peer.start_drag(
            &mut self.h,
            self.src_node,
            mimes,
            drag_actions::COPY | drag_actions::MOVE,
        );
    }

    /// Move over `p` in two steps, so a `DragMotion` follows the enter.
    fn hover(&mut self, p: Point) {
        self.h.move_pointer_abs(p);
        self.h.move_pointer_abs(Point::new(p.x + 2.0, p.y + 2.0));
    }

    fn boxw(&self, id: WidgetId) -> &DropBox {
        self.h.widget::<DropBox>(id)
    }
}

#[test]
fn a_drop_on_an_accepting_widget_delivers_the_bytes_and_finishes() {
    let mut s = scene("dnd-drop");
    s.start(&[TEXT_MIME, "text/plain"]);
    let p = s.over(s.target);
    s.hover(p);
    assert!(s.h.ui().drag_active());
    assert_eq!(s.h.ui().drop_target(), Some(s.target));
    assert!(s.boxw(s.target).overs >= 1);
    s.h.release(button::LEFT);
    let (id, mime) = s.peer.asked(&mut s.h);
    assert_eq!(mime, TEXT_MIME);
    s.peer.answer_bytes(id, b"hello, drop");
    s.h.settle();
    assert_eq!(
        s.peer.drag_finished(&mut s.h),
        (true, DragAction::Copy),
        "our FinishDrag completed the drop"
    );
    let b = s.boxw(s.target);
    assert_eq!(
        b.drops,
        [(TEXT_MIME.to_owned(), Some(b"hello, drop".to_vec()))]
    );
    assert_eq!(b.leaves, 1, "told it is no longer the target");
    assert!(!s.h.ui().drag_active());
    assert!(s.boxw(s.plain).drops.is_empty());
    s.h.quit();
}

#[test]
fn a_drop_where_nothing_accepts_is_rejected() {
    let mut s = scene("dnd-reject");
    s.start(&[TEXT_MIME]);
    let p = s.over(s.plain);
    s.hover(p);
    assert!(s.boxw(s.plain).overs >= 1, "asked");
    assert_eq!(s.h.ui().drop_target(), None);
    s.h.release(button::LEFT);
    assert_eq!(s.peer.drag_finished(&mut s.h), (false, DragAction::None));
    assert!(s.boxw(s.plain).drops.is_empty());
    assert!(s.boxw(s.target).drops.is_empty());
    s.h.settle();
    assert!(!s.h.ui().drag_active(), "the DragLeave cleared the state");
    s.h.quit();
}

#[test]
fn an_unoffered_type_is_not_accepted() {
    let mut s = scene("dnd-unoffered");
    s.start(&["image/png"]);
    let p = s.over(s.target);
    s.hover(p);
    assert_eq!(s.h.ui().drop_target(), None);
    s.h.release(button::LEFT);
    assert_eq!(s.peer.drag_finished(&mut s.h), (false, DragAction::None));
    s.h.quit();
}

#[test]
fn moving_off_the_target_withdraws_the_acceptance() {
    let mut s = scene("dnd-withdraw");
    s.start(&[TEXT_MIME]);
    let (t, p) = (s.over(s.target), s.over(s.plain));
    s.hover(t);
    assert_eq!(s.h.ui().drop_target(), Some(s.target));
    s.hover(p);
    assert_eq!(s.h.ui().drop_target(), None);
    assert_eq!(s.boxw(s.target).leaves, 1, "drag_leave on the move away");
    // Back on, then out of the window entirely.
    s.hover(t);
    assert_eq!(s.h.ui().drop_target(), Some(s.target));
    s.hover(Point::new(2.0, 2.0));
    assert!(!s.h.ui().drag_active(), "DragLeave cleared the state");
    assert_eq!(s.boxw(s.target).leaves, 2);
    // Back over the plain box and dropped: the reject was re-sent.
    s.hover(p);
    s.h.release(button::LEFT);
    assert_eq!(s.peer.drag_finished(&mut s.h), (false, DragAction::None));
    assert!(s.boxw(s.target).drops.is_empty());
    s.h.quit();
}

#[test]
fn a_source_that_never_reaches_eof_still_gets_its_finish() {
    let mut s = scene("dnd-hostile");
    s.start(&[TEXT_MIME]);
    let p = s.over(s.target);
    s.hover(p);
    s.h.release(button::LEFT);
    let (id, _) = s.peer.asked(&mut s.h);
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    rustix::io::write(&ours, b"partial").unwrap();
    s.peer.answer(id, theirs.into());
    s.h.settle();
    assert!(s.boxw(s.target).drops.is_empty(), "still reading");
    assert!(s.h.ui().drag_active());
    assert!(!s.peer.saw_drag_finished());
    s.h.advance_timers(READ_TIMEOUT_MS + 1);
    s.h.settle();
    assert_eq!(s.boxw(s.target).drops, [(TEXT_MIME.to_owned(), None)]);
    assert_eq!(s.peer.drag_finished(&mut s.h), (true, DragAction::Copy));
    assert!(!s.h.ui().drag_active());
    drop(ours);
    s.h.quit();
}

// -- as a drag source --------------------------------------------------

use nitro_ui::build::StyleBuilder as _;
use nitro_ui::clipboard::{PLAIN_MIME, URI_LIST_MIME};
use nitro_ui::dnd::{DragIcon, DragOutcome, DragSource};
use nitro_ui::{List, Row, WindowId, list};

/// What the source app saw.
#[derive(Default)]
struct Src {
    outcomes: Vec<DragOutcome>,
    /// Whether `start_drag` answered true, per attempt.
    started: Vec<bool>,
    /// Draw an icon under the pointer.
    icon: bool,
}

const URIS: &[u8] = b"file:///tmp/a.txt\r\n";

/// A draggable list whose `on_drag` offers a uri-list and text.
struct SourceScene {
    h: Harness<Src>,
    peer: Peer,
    list: WidgetId,
    target: Rect,
}

fn source_scene(name: &str, icon: bool, remote: bool) -> SourceScene {
    let build = |ui: &mut Ui<Src>| {
        ui.build(
            list()
                .rows((0..5).map(|i| Row::new(format!("row {i}"))).collect())
                .on_drag(|s: &mut Src, ui: &mut Ui<Src>, sel: &[usize]| {
                    assert!(!sel.is_empty());
                    let icon = s.icon.then(|| DragIcon {
                        root: ui.build(nitro_ui::widgets::label("dragging")),
                        size: None,
                        offset: Point::new(-8.0, -8.0),
                    });
                    let offer = DragSource {
                        items: vec![
                            (URI_LIST_MIME.to_owned(), URIS.to_vec()),
                            (TEXT_MIME.to_owned(), b"/tmp/a.txt".to_vec()),
                            (PLAIN_MIME.to_owned(), b"/tmp/a.txt".to_vec()),
                        ],
                        actions: drag_actions::COPY | drag_actions::LINK,
                        icon,
                    };
                    let ok = ui
                        .start_drag(WindowId::MAIN, offer, |s: &mut Src, _ui, o| {
                            s.outcomes.push(o);
                        })
                        .unwrap();
                    s.started.push(ok);
                })
                .width_percent(1.0)
                .height_percent(1.0),
        )
    };
    let state = Src {
        icon,
        ..Src::default()
    };
    let mut h = if remote {
        Harness::remote(name, state, build)
    } else {
        Harness::sized(name, state, Size::new(40.0, 100.0), build)
    };
    h.settle();
    let list = h.ui().root().unwrap();
    let mut peer = Peer::new(&h, "target");
    let (_, target) = peer.target_window(&mut h);
    SourceScene {
        h,
        peer,
        list,
        target,
    }
}

impl SourceScene {
    /// Row `i`'s middle, in output coordinates, clear of the target.
    fn row(&mut self, i: usize) -> Point {
        let origin = self.h.ui().window_position_of(WindowId::MAIN);
        let row_h = self.h.widget::<List<Src>>(self.list).row_height();
        let p = Point::new(origin.x + 10.0, origin.y + (i as f32 + 0.5) * row_h);
        assert!(!self.target.contains(p), "{p:?} under {:?}", self.target);
        p
    }

    /// A point on the target, outside the source window (and its frame):
    /// both are centred on the output, and a press raised ours.
    fn target_mid(&mut self) -> Point {
        let origin = self.h.ui().window_position_of(WindowId::MAIN);
        let p = Point::new(
            self.target.x + self.target.w - 4.0,
            self.target.y + self.target.h / 2.0,
        );
        let main = Rect::new(origin.x - 12.0, origin.y - 40.0, 64.0, 152.0);
        assert!(!main.contains(p), "{p:?} is under the source window at {origin:?}");
        p
    }

    /// Press on row 1 and drag past the threshold (still inside the list).
    fn begin(&mut self) {
        let p = self.row(1);
        self.h.move_pointer_abs(p);
        self.h.press(button::LEFT);
        self.h.move_pointer_abs(Point::new(p.x + 12.0, p.y));
        self.h.settle();
    }
}

#[test]
fn a_dragged_row_is_read_by_the_target_and_the_source_hears_the_outcome() {
    let mut s = source_scene("dnd-src", false, false);
    s.begin();
    assert_eq!(s.h.state().started, [true]);
    assert!(s.h.ui().drag_in_flight());
    assert!(!s.h.ui().can_drag(), "one at a time");
    let t = s.target_mid();
    s.h.move_pointer_abs(t);
    let (mimes, actions) = s.peer.drag_entered(&mut s.h);
    assert_eq!(mimes, [URI_LIST_MIME, TEXT_MIME, PLAIN_MIME]);
    assert_eq!(actions, drag_actions::COPY | drag_actions::LINK);
    s.peer.accept_drop(&mut s.h, DragAction::Copy, URI_LIST_MIME);
    s.h.release(button::LEFT);
    s.peer.dropped(&mut s.h);
    assert_eq!(s.peer.read_drag(&mut s.h, 1, URI_LIST_MIME), URIS);
    assert_eq!(s.peer.read_drag(&mut s.h, 2, TEXT_MIME), b"/tmp/a.txt");
    assert!(s.peer.read_drag(&mut s.h, 3, "image/png").is_empty());
    s.peer.finish_drop(&mut s.h);
    s.h.settle();
    assert_eq!(
        s.h.state().outcomes,
        [DragOutcome {
            accepted: true,
            action: DragAction::Copy
        }]
    );
    assert!(!s.h.ui().drag_in_flight());
    assert_eq!(s.h.server().stat("dnd_active"), 0, "our FinishDrag released it");
    // A second drag starts: nothing was left behind.
    s.begin();
    assert_eq!(s.h.state().started, [true, true]);
    s.h.key(nitro_ui::event::key::ESC);
    s.h.release(button::LEFT);
    s.h.settle();
    assert_eq!(s.h.state().outcomes.len(), 2);
    s.h.quit();
}

#[test]
fn a_drop_on_nothing_is_rejected_and_the_icon_goes() {
    let mut s = source_scene("dnd-src-nothing", true, false);
    s.begin();
    assert_eq!(s.h.state().started, [true]);
    assert_eq!(s.h.ui().windows().len(), 2, "the icon window");
    // Off every window: the drag is over nothing.
    s.h.move_pointer_abs(Point::new(300.0, 230.0));
    s.h.release(button::LEFT);
    s.h.settle();
    assert_eq!(
        s.h.state().outcomes,
        [DragOutcome {
            accepted: false,
            action: DragAction::None
        }]
    );
    assert_eq!(s.h.ui().windows().len(), 1, "the icon window was destroyed");
    assert_eq!(s.h.server().stat("dnd_active"), 0);
    // The drag's bytes are gone: a late read gets nothing.
    assert!(!s.h.ui().drag_in_flight());
    s.h.quit();
}

#[test]
fn escape_cancels_a_drag_over_a_target() {
    let mut s = source_scene("dnd-src-esc", true, false);
    s.begin();
    let t = s.target_mid();
    s.h.move_pointer_abs(t);
    s.peer.drag_entered(&mut s.h);
    s.peer.accept_drop(&mut s.h, DragAction::Copy, TEXT_MIME);
    s.h.key(nitro_ui::event::key::ESC);
    s.peer.drag_left(&mut s.h);
    s.h.settle();
    assert_eq!(
        s.h.state().outcomes,
        [DragOutcome {
            accepted: false,
            action: DragAction::None
        }]
    );
    s.h.release(button::LEFT);
    assert_eq!(s.h.ui().windows().len(), 1);
    assert_eq!(s.h.server().stat("dnd_active"), 0);
    s.h.quit();
}

#[test]
fn a_move_under_the_threshold_is_a_click() {
    let mut s = source_scene("dnd-src-click", false, false);
    let p = s.row(2);
    s.h.move_pointer_abs(p);
    s.h.press(button::LEFT);
    s.h.move_pointer_abs(Point::new(p.x + 3.0, p.y + 2.0));
    s.h.release(button::LEFT);
    assert!(s.h.state().started.is_empty());
    assert_eq!(s.h.widget::<List<Src>>(s.list).selection(), [2]);
    s.h.quit();
}

#[test]
fn a_start_the_server_ignores_still_finishes() {
    // Released before the StartDrag reaches the server: it is refused
    // silently, and the release that does arrive ends the drag here.
    let mut s = source_scene("dnd-src-refused", true, false);
    let p = s.row(1);
    s.h.move_pointer_abs(p);
    s.h.press(button::LEFT);
    {
        let pos = Point::new(p.x + 12.0, p.y);
        let origin = s.h.ui().window_position_of(WindowId::MAIN);
        let (ui, st) = s.h.parts();
        // The move and the release are dispatched in one turn, before the
        // flush, as a fast flick would be.
        ui.dispatch(
            st,
            &nitro_wire::msg::ServerMsg::PointerMotion(nitro_wire::msg::PointerMotion {
                window: WindowId::MAIN.raw(),
                node: nitro_wire::types::NodeId::NONE,
                pos: Point::new(pos.x - origin.x, pos.y - origin.y),
                time_ns: 0,
            }),
        );
    }
    assert_eq!(s.h.state().started, [true]);
    s.h.server().push_input(nitro_server::input::InputEvent::PointerButton {
        button: button::LEFT,
        state: nitro_wire::types::ButtonState::Released,
        time_ns: 1,
    });
    // The server has the release before our (still unflushed) StartDrag.
    s.h.server().settle();
    s.h.settle();
    s.h.settle();
    let outcomes = &s.h.state().outcomes;
    assert_eq!(outcomes.len(), 1, "finished exactly once: {outcomes:?}");
    assert!(!outcomes[0].accepted);
    assert!(!s.h.ui().drag_in_flight());
    assert_eq!(s.h.ui().windows().len(), 1);
    s.h.quit();
}

#[test]
fn without_data_no_drag_starts() {
    let mut s = source_scene("dnd-src-remote", false, true);
    assert!(!s.h.ui().can_drag());
    s.begin();
    assert_eq!(s.h.state().started, [false]);
    s.h.release(button::LEFT);
    assert!(s.h.state().outcomes.is_empty());
    s.h.quit();
}

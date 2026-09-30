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

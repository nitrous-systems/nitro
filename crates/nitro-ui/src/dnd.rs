//! Drag and drop: as a **drop target**, and as a **drag source**.
//!
//! The server (M5-I, `caps::DATA`) routes a drag to the window under the
//! pointer: `DragEnter`, `DragMotion`s, then `DragLeave` or `DragDrop`.
//! This module turns that into three [`Widget`](crate::Widget) hooks on
//! the widget under the pointer, so an app writes
//!
//! ```ignore
//! fn drag_over(&mut self, cx: &mut EventCx<'_, S>, pos: Point, offer: &DragOffer) -> Option<Accept> {
//!     offer.first_of(&[TEXT_MIME]).map(|m| Accept::new(DragAction::Copy, m))
//! }
//! fn dropped(&mut self, cx: &mut EventCx<'_, S>, mime: &str, data: Option<&[u8]>) { … }
//! ```
//!
//! and never sees an `AcceptDrop`, a descriptor or a `FinishDrag`.
//!
//! - **`drag_over`** is asked on `DragEnter` and every `DragMotion`, the
//!   deepest widget under the pointer first, then each ancestor until
//!   one answers `Some`. An answer naming an action the source did not
//!   offer, or a type not in the offer, counts as `None`. The toolkit
//!   sends `AcceptDrop` only when the answer changes.
//! - **`drag_leave`** tells the accepting widget it is no longer the
//!   target: the pointer moved to another widget or left the window, or
//!   the drop was delivered. A widget that painted a highlight in
//!   `drag_over` clears it here.
//! - **`dropped`** delivers the bytes after the drop, read with
//!   `RequestSelection { source: Drag }` exactly as the clipboard reads:
//!   non-blocking, capped at
//!   [`MAX_CLIPBOARD_BYTES`](crate::clipboard::MAX_CLIPBOARD_BYTES), and
//!   `None` after [`READ_TIMEOUT_MS`](crate::clipboard::READ_TIMEOUT_MS).
//!
//! `FinishDrag` is sent exactly once per drop, whatever happened — data,
//! no data, a timeout, a target widget destroyed meanwhile — because the
//! source waits on it.
//!
//! # As a source
//!
//! [`Ui::start_drag`] offers `(mime, bytes)` pairs, like
//! [`Ui::set_clipboard`](crate::Ui::set_clipboard). Call it from a
//! widget's `PointerMove` while it holds the pointer capture, once the
//! pointer has travelled [`DRAG_THRESHOLD`] from the press (a [`List`]
//! with [`on_drag`](crate::list::ListBuilder::on_drag) does all of that).
//! The toolkit then:
//!
//! - queues `SetDragIconOffset` and `StartDrag` behind the icon window's
//!   `CreateWindow` and first paint, so all of it lands in one commit;
//! - serves `SelectionRequest { source: Drag }` from the offered bytes in
//!   sealed memfds, exactly as the clipboard serves;
//! - on `DragFinished`, answers `FinishDrag`, destroys the icon window,
//!   forgets the bytes and runs the `on_finished` callback (deferred)
//!   with the [`DragOutcome`].
//!
//! When the server takes the drag the source window gets a
//! `PointerLeave` and **never the release**: the capture is dropped and
//! the pressed widget sees `PointerLeave` with
//! [`EventCx::is_captured`](crate::EventCx::is_captured) false. The server
//! ignores a `StartDrag` silently (no button down any more, no pointer
//! focus, another drag running); the toolkit notices from the release
//! that does reach the window and finishes the drag as rejected, so an
//! app is never left waiting.
//!
//! [`List`]: crate::List

use nitro_core::{Point, Size};
use nitro_wire::msg::{AcceptDrop, ClientMsg, FinishDrag, ServerMsg, StartDrag};
use nitro_wire::types::{DataSource, Layer, NodeId, window_flags};

pub use nitro_wire::types::{DragAction, drag_actions};

use crate::arena::WidgetId;
use crate::error::Error;
use crate::ui::{Ui, WindowId};

/// How far, in logical pixels, the pointer has to travel from a press
/// before the press becomes a drag. Below it, it is a click.
pub const DRAG_THRESHOLD: f32 = 6.0;

/// What a drag started with [`Ui::start_drag`] offers.
#[derive(Debug, Clone, PartialEq)]
pub struct DragSource {
    /// `(mime, bytes)` pairs, most preferred first. Cleaned as the
    /// clipboard's are: empty, non-ASCII and repeated types are dropped.
    pub items: Vec<(String, Vec<u8>)>,
    /// Offered actions, a [`drag_actions`] bitmask.
    pub actions: u32,
    /// What to draw under the pointer, if anything.
    pub icon: Option<DragIcon>,
}

/// A drag icon: a widget tree shown in its own undecorated, unfocusable
/// window under the pointer for the length of the drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DragIcon {
    /// The tree's root; built and not attached anywhere. The icon window
    /// owns it and destroys it with itself when the drag ends.
    pub root: WidgetId,
    /// The window's size, or `None` for the root's measured size.
    pub size: Option<Size>,
    /// The icon's top-left relative to the pointer, usually negative.
    pub offset: Point,
}

/// How a drag this app started ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragOutcome {
    /// Whether a target took the offer.
    pub accepted: bool,
    /// What the target did with it; [`DragAction::None`] when rejected,
    /// cancelled or refused.
    pub action: DragAction,
}

type FinishedFn<S> = Box<dyn FnOnce(&mut S, &mut Ui<S>, DragOutcome)>;

/// A drag this app is the source of, from `start_drag` to `DragFinished`.
pub(crate) struct Source<S> {
    /// The window the drag starts from.
    from: WindowId,
    /// The server took it: `from` got its `PointerLeave`.
    active: bool,
    /// What `SelectionRequest { source: Drag }` is answered from.
    served: Vec<(String, Vec<u8>)>,
    icon: Option<WindowId>,
    on_finished: Option<FinishedFn<S>>,
}

/// What a drag over one of our windows carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DragOffer {
    /// MIME types offered, most preferred first.
    pub mimes: Vec<String>,
    /// Offered actions, a [`drag_actions`] bitmask.
    pub actions: u32,
}

impl DragOffer {
    /// Whether `mime` is offered.
    #[must_use]
    pub fn has(&self, mime: &str) -> bool {
        self.mimes.iter().any(|m| m == mime)
    }

    /// Whether the source allows `action`. Never for [`DragAction::None`].
    #[must_use]
    pub fn allows(&self, action: DragAction) -> bool {
        let bit = match action {
            DragAction::None => 0,
            DragAction::Copy => drag_actions::COPY,
            DragAction::Move => drag_actions::MOVE,
            DragAction::Link => drag_actions::LINK,
        };
        bit != 0 && self.actions & bit != 0
    }

    /// The first of `wanted` (in the caller's order of preference) that
    /// is offered.
    #[must_use]
    pub fn first_of<'a>(&self, wanted: &[&'a str]) -> Option<&'a str> {
        wanted.iter().copied().find(|w| self.has(w))
    }
}

/// A drop target's answer: what it would do, and which type it would
/// read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accept {
    /// The action it would take.
    pub action: DragAction,
    /// The MIME type it would read; one of the offer's.
    pub mime: String,
}

impl Accept {
    /// An answer taking `action`, reading `mime`.
    #[must_use]
    pub fn new(action: DragAction, mime: &str) -> Self {
        Self {
            action,
            mime: mime.to_owned(),
        }
    }
}

/// One drag over one of our windows, `DragEnter` to its end.
struct Current {
    /// Which drag this is: a drop read that finishes after a newer
    /// `DragEnter` must not touch the newer drag's state.
    generation: u64,
    window: NodeId,
    win: WindowId,
    offer: DragOffer,
    /// The widget whose `drag_over` accepted, if any.
    target: Option<WidgetId>,
    /// The last `AcceptDrop` sent; `None` for "rejected, or nothing yet".
    sent: Option<Accept>,
    /// `DragDrop` arrived and the read is in flight.
    dropping: bool,
}

/// The drop-target state a [`Ui`] carries.
#[derive(Default)]
pub(crate) struct Dnd {
    generation: u64,
    current: Option<Current>,
}

impl<S: 'static> Ui<S> {
    /// Whether a drag is over one of this app's windows, or a drop on
    /// one is still being read.
    #[must_use]
    pub fn drag_active(&self) -> bool {
        self.dnd.current.is_some()
    }

    /// The widget currently accepting the drag, if any.
    #[must_use]
    pub fn drop_target(&self) -> Option<WidgetId> {
        self.dnd.current.as_ref().and_then(|c| c.target)
    }

    /// The `Drag*` arms of [`Ui::dispatch`]: the drop-target side, and
    /// `DragFinished` for a drag this app is the source of.
    pub(crate) fn dnd_msg(&mut self, state: &mut S, msg: &ServerMsg) {
        match msg {
            ServerMsg::DragEnter(e) => {
                let Some(win) = self.window_by_node(e.window) else {
                    return;
                };
                // A stale drag (a leave we never saw, or a drop still
                // reading) is superseded. A drop still reading is
                // finished now: the server has moved on, and its late
                // `FinishDrag` could otherwise land on this new drag.
                let was_dropping = self.dnd.current.as_ref().is_some_and(|c| c.dropping);
                self.dnd_reset(state);
                if was_dropping {
                    self.dnd_finish();
                }
                self.dnd.generation += 1;
                self.dnd.current = Some(Current {
                    generation: self.dnd.generation,
                    window: e.window,
                    win,
                    offer: DragOffer {
                        mimes: e.mimes.clone(),
                        actions: e.actions,
                    },
                    target: None,
                    sent: None,
                    dropping: false,
                });
                self.dnd_motion(state, e.pos);
            }
            ServerMsg::DragMotion(m) if self.dnd_is(m.window) => self.dnd_motion(state, m.pos),
            ServerMsg::DragLeave(l) if self.dnd_is(l.window) => self.dnd_reset(state),
            ServerMsg::DragDrop(d) if self.dnd_is(d.window) => self.dnd_drop(state),
            ServerMsg::DragFinished(f) => self.drag_source_finished(state, f.accepted, f.action),
            _ => {}
        }
    }

    /// Whether `window` is the one the current drag is over, and the drop
    /// has not happened yet.
    fn dnd_is(&self, window: NodeId) -> bool {
        self.dnd
            .current
            .as_ref()
            .is_some_and(|c| c.window == window && !c.dropping)
    }

    /// Tell the target it lost and forget the drag. Sends nothing.
    fn dnd_reset(&mut self, state: &mut S) {
        if let Some(c) = self.dnd.current.take()
            && let Some(t) = c.target
        {
            self.dnd_leave_widget(state, t);
        }
    }

    fn dnd_leave_widget(&mut self, state: &mut S, id: WidgetId) {
        if self.is_live(id) {
            self.with_widget(state, id, crate::widget::Widget::drag_leave);
        }
    }

    /// Ask the widgets under `pos`, deepest first, and tell the server
    /// if the answer changed.
    fn dnd_motion(&mut self, state: &mut S, pos: Point) {
        let Some(c) = self.dnd.current.as_mut() else {
            return;
        };
        let win = c.win;
        let offer = std::mem::take(&mut c.offer);
        let mut chain = Vec::new();
        self.hit_chain(win, pos, &mut chain);
        let mut found: Option<(WidgetId, Accept)> = None;
        for (id, local) in chain.iter().rev() {
            let answer = self
                .with_widget(state, *id, |w, cx| w.drag_over(cx, *local, &offer))
                .flatten()
                .filter(|a| offer.allows(a.action) && offer.has(&a.mime));
            if let Some(a) = answer {
                found = Some((*id, a));
                break;
            }
        }
        let Some(c) = self.dnd.current.as_mut() else {
            return;
        };
        c.offer = offer;
        let new_target = found.as_ref().map(|(id, _)| *id);
        let old_target = std::mem::replace(&mut c.target, new_target);
        let answer = found.map(|(_, a)| a);
        let changed = c.sent != answer;
        if changed {
            c.sent.clone_from(&answer);
        }
        if let Some(old) = old_target
            && Some(old) != new_target
        {
            self.dnd_leave_widget(state, old);
        }
        if changed {
            let (action, mime) =
                answer.map_or((DragAction::None, String::new()), |a| (a.action, a.mime));
            if let Err(e) = self
                .wire_mut()
                .send_now(&ClientMsg::AcceptDrop(AcceptDrop { action, mime }))
            {
                eprintln!("nitro-ui: accept drop: {e}");
            }
        }
    }

    /// `DragDrop`: read the accepted type, deliver it, finish.
    fn dnd_drop(&mut self, state: &mut S) {
        let Some(c) = self.dnd.current.as_ref() else {
            return;
        };
        let target = c.target.filter(|t| self.is_live(*t));
        let mime = c.sent.as_ref().map(|a| a.mime.clone());
        let generation = c.generation;
        let (Some(target), Some(mime)) = (target, mime) else {
            // Nothing (living) accepted: finish at once, so the source
            // is not left waiting.
            self.dnd_reset(state);
            self.dnd_finish();
            return;
        };
        if let Some(c) = self.dnd.current.as_mut() {
            c.dropping = true;
        }
        let m = mime.clone();
        self.read_selection(DataSource::Drag, mime, move |s, ui, got| {
            let data = got.map(|(_, b)| b);
            if ui.is_live(target) {
                ui.with_widget(s, target, |w, cx| w.dropped(cx, &m, data.as_deref()));
            }
            // A newer drag replaced this one while it read: that one
            // already sent our `FinishDrag`, and its state is not ours.
            if ui
                .dnd
                .current
                .as_ref()
                .is_some_and(|c| c.generation == generation)
            {
                ui.dnd_reset(s);
                ui.dnd_finish();
            }
        });
    }

    fn dnd_finish(&mut self) {
        if let Err(e) = self.wire_mut().send_now(&ClientMsg::FinishDrag(FinishDrag)) {
            eprintln!("nitro-ui: finish drag: {e}");
        }
    }

    // -- the source side ---------------------------------------------

    /// Whether [`Ui::start_drag`] can start one now: the server has
    /// `DATA` (not a remote link, not an old server) and no drag of this
    /// app is in flight.
    #[must_use]
    pub fn can_drag(&self) -> bool {
        self.has_clipboard() && self.drag_source.is_none()
    }

    /// Whether a drag this app started has yet to finish.
    #[must_use]
    pub fn drag_in_flight(&self) -> bool {
        self.drag_source.is_some()
    }

    /// Start dragging `offer` out of window `from`; see the module docs.
    ///
    /// Answers `Ok(false)`, sending nothing, without `DATA`, while a drag
    /// is in flight, with nothing offered, or when `from` holds no
    /// pointer capture (no button is down on it, so the server would
    /// ignore the request). Otherwise `on_finished` runs exactly once,
    /// later, with how it ended — rejected too, when the server turned
    /// the request down.
    ///
    /// # Errors
    /// A wire failure, or [`Ui::add_window`]'s errors for the icon root.
    pub fn start_drag(
        &mut self,
        from: WindowId,
        offer: DragSource,
        on_finished: impl FnOnce(&mut S, &mut Ui<S>, DragOutcome) + 'static,
    ) -> Result<bool, Error> {
        if !self.can_drag() || !self.window_captured(from) {
            return Ok(false);
        }
        let items = crate::clipboard::clean_items(offer.items);
        let actions = offer.actions & drag_actions::ALL;
        if items.is_empty() || actions == 0 {
            return Ok(false);
        }
        let mimes: Vec<String> = items.iter().map(|(m, _)| m.clone()).collect();
        let icon = match offer.icon {
            Some(i) => {
                let surface = crate::shell::Surface {
                    layer: Layer::Normal,
                    flags: window_flags::UNDECORATED | window_flags::NO_FOCUS,
                    anchor: None,
                    zone: None,
                };
                let win = self.add_window_with("drag", i.size, i.root, Some(surface))?;
                self.wire_mut().set_drag_icon_offset(win.raw(), i.offset)?;
                Some(win)
            }
            None => None,
        };
        self.wire_mut().start_drag(StartDrag {
            window: from.raw(),
            icon: icon.map_or(NodeId::NONE, WindowId::raw),
            actions,
            mimes,
        })?;
        self.drag_source = Some(Source {
            from,
            active: false,
            served: items,
            icon,
            on_finished: Some(Box::new(on_finished)),
        });
        Ok(true)
    }

    /// The bytes a drag of ours serves in `mime`; empty for none.
    pub(crate) fn drag_bytes(&self, mime: &str) -> &[u8] {
        self.drag_source
            .as_ref()
            .and_then(|d| d.served.iter().find(|(m, _)| m == mime))
            .map_or(&[], |(_, b)| b.as_slice())
    }

    /// `win` lost the pointer: if our drag starts there, the server took it.
    pub(crate) fn drag_source_left(&mut self, win: WindowId) {
        if let Some(d) = self.drag_source.as_mut()
            && d.from == win
        {
            d.active = true;
        }
    }

    /// A release reached `win`. If our drag starts there and the server
    /// never took it, it ignored the `StartDrag`: end it as rejected.
    pub(crate) fn drag_source_released(&mut self, state: &mut S, win: WindowId) {
        if self
            .drag_source
            .as_ref()
            .is_some_and(|d| d.from == win && !d.active)
        {
            self.drag_source_end(
                state,
                DragOutcome {
                    accepted: false,
                    action: DragAction::None,
                },
            );
        }
    }

    /// `DragFinished`: release the offer with `FinishDrag`, then end.
    pub(crate) fn drag_source_finished(&mut self, state: &mut S, accepted: bool, action: DragAction) {
        // Always: the server keeps the drag (and its transfers) until the
        // source finishes, whatever this side remembers.
        if let Err(e) = self.wire_mut().send_now(&ClientMsg::FinishDrag(FinishDrag)) {
            eprintln!("nitro-ui: finish drag: {e}");
        }
        self.drag_source_end(state, DragOutcome { accepted, action });
    }

    fn drag_source_end(&mut self, state: &mut S, outcome: DragOutcome) {
        let Some(mut d) = self.drag_source.take() else {
            return;
        };
        if let Some(icon) = d.icon
            && self.has_window(icon)
            && let Err(e) = self.remove_window(state, icon)
        {
            eprintln!("nitro-ui: drag icon: {e}");
        }
        if let Some(cb) = d.on_finished.take() {
            self.defer(move |s, ui| cb(s, ui, outcome));
        }
    }
}

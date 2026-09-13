# nitro-ui — the toolkit

`nitro-ui` is the client-side half of nitro: a **retained widget tree in
an arena** that maps widgets to scene nodes and sends only mutations for
what changed. It is the layer an app is written against, and the reason
an app binary is 444 KB with no font library in it.

This document is the architecture and the reasoning. The API reference is
the rustdoc; the crate README is the two-minute version.

## The shape of it

```
     app state S ──┐
                   ├── callbacks: Fn(&mut S, &mut Ui<S>)
  ┌────────────────▼─────────────────────────────────┐
  │ Ui<S>                                            │
  │   arena: Vec<Slot<S>>       ← widgets by id      │
  │   passes: TREE → LAYOUT → PAINT → one Commit     │
  │   routing: hit test, focus, Tab                  │
  └────────────────┬─────────────────────────────────┘
                   │ nitro-wire mutations
  ┌────────────────▼─────────────────────────────────┐
  │ nitro-server: scene graph → damage → raster      │
  └──────────────────────────────────────────────────┘
```

Five ideas, argued in `DESIGN.md` and implemented here:

1. Retained widgets with dirty flags — no per-frame rebuild, no virtual
   tree.
2. `WidgetMut<'_, W>` is the only way to mutate a widget.
3. Callbacks take `&mut S` and `&mut Ui<S>`; no `Rc<RefCell>` anywhere in
   the public API.
4. Builders that read like gpui but build once.
5. Passes: `event`, `update`, `layout`, `paint`, `introspect`.

## Arena and slots

Widgets live in a flat `Vec<Slot<S>>` and name each other by `WidgetId`,
a generational index (`u32` index + `u32` generation). There is no `Rc`,
no parent pointer and no interior mutability.

```rust,ignore
struct Slot<S> {
    widget: Option<Box<dyn AnyWidget<S>>>,
    state: WidgetState,
    generation: u32,
    alive: bool,
}
```

`WidgetState` is the framework's half: parent, children, `LayoutStyle`,
`bounds`, the memoized measurement, dirty `flags`, the widget's scene
`node`, its `content` group, its paint `slots`, plus `name`, `focusable`,
`hovered` and `focused`. It is framework-owned rather than
widget-supplied because every pass reads it for every widget, and a
widget that forgot to expose one of these would simply not work.

**A stale id is an error, never a panic.** A freed slot bumps its
generation, so an id kept past its widget's death names nothing:
`Error::StaleWidget`. A slot whose generation would overflow is retired
instead of recycled, so an ancient id can never come back to life — the
same rule `nitro-scene` uses for `NodeKey`.

## Take-out dispatch

Calling a widget's method needs `&mut` to the widget *and* `&mut` to the
tree, which Rust will not give you at once. The usual answers are
`RefCell` (runtime panics, and a borrow you cannot see in the signature)
or passing a disjoint subset of the tree (which forecloses exactly the
thing app code wants to do).

Instead the widget is **moved out of its slot** for the duration of the
call and put back afterwards:

```rust,ignore
let widget = self.take(id)?;             // slot.widget = None
let r = widget.event(&mut cx, ev);       // cx.ui is a full &mut Ui<S>
self.untake(id, widget);                 // slot.widget = Some(..)
```

That is what makes `Fn(&mut S, &mut Ui<S>)` possible. Inside a button's
`on_click` the whole tree is mutable: its siblings, its parent, widgets
created on the spot. The one thing it cannot reach is *itself*, and
asking for it returns `Error::Busy` — a value, not a panic and not a
second mutable borrow. To change yourself, use the `&mut self` you
already have.

A widget destroyed while it was out has nowhere to go back to and is
simply dropped, so a callback may legally remove the widget that is
running it.

## WidgetMut

`WidgetMut<'_, W, S>` is the only door to a widget's properties. Taking
one moves the widget out of its slot (so the `&mut Ui` it also carries
cannot alias it); dropping it puts the widget back. Every setter marks
the widget layout- or paint-dirty:

```rust,ignore
impl<S: 'static> WidgetMut<'_, Label, S> {
    pub fn set_text(&mut self, text: impl Into<String>) {
        if self.text == text { return; }     // no-op changes cost nothing
        self.text = text;
        self.request_layout();               // a new string is a new size
    }
    pub fn set_color(&mut self, color: Color) {
        self.color = Some(color);
        self.request_paint();                // a colour cannot move anything
    }
}
```

Invalidation is therefore not something an app author can forget, because
there is no other way in. The distinction between `request_layout` and
`request_paint` is the widget author's one real responsibility, and it is
the one that decides whether a colour change repaints a subtree.

## Dirty flags

Six bits. Three say *this widget* needs a pass; three say a *descendant*
does:

| flag | meaning |
|---|---|
| `LAYOUT` | measure and place this widget again |
| `PAINT` | emit this widget's scene mutations again |
| `TREE` | this widget's children changed |
| `SUB_LAYOUT`, `SUB_PAINT`, `SUB_TREE` | a descendant has the matching flag |

Marking a widget lights its own flag and then the matching `SUB_` flag on
every ancestor, **stopping as soon as one is already lit** — so marking
the thousandth widget in a settled tree is O(depth) the first time and
O(1) afterwards. A pass that sees neither `X` nor `SUB_X` on a node skips
the entire subtree without walking it. That is the whole mechanism behind
"work is proportional to what changed".

## The passes

`Ui::flush` runs three and commits once:

**TREE** creates, reparents and destroys scene groups for structurally
dirty subtrees. Children are walked backwards so `before` always names a
sibling already in place; reproducing the order needs no second pass.
A child whose `(parent, before)` has not changed sends no `Reparent` at
all.

**LAYOUT** measures and places the dirty subtrees. `measure` is memoized
on `(constraints, size)` and invalidated by `LAYOUT`, so a container that
re-lays out does not re-measure clean children. A widget whose *size* did
not change stops propagation upward; a widget that merely *moved* gets
one `SetBounds` on its group and nothing inside it repaints.

**PAINT** visits only widgets with `PAINT` and lets them emit their own
nodes.

Then exactly one `Commit{serial}`. **Nothing dirty means no commit**, and
no commit means no bytes: idle is free, which is the property the test
`a_settled_tree_sends_nothing_while_idle` checks from the outside over
200 ms.

Scratch vectors are pooled (`id_pool`, `item_pool`, `rect_pool`) because
layout recurses and one buffer is not enough; after the tree has settled
a flush allocates nothing beyond the wire buffer.

`introspect` is the fifth pass and is a plain walk over the same arena —
`Ui::introspect` fills a `Vec<Node>` with id, role, `Access`, window
bounds, focusable and focused. M2 stops there; the socket that serves it
and the `hey`-style CLI are the next task, and this is the shape they
read. The point of goal 5 is that there is *one* tree, not a shadow one
maintained alongside it.

## Mapping widgets to scene nodes

Every widget owns one scene `Group`, positioned by its bounds. Once it
has children it also gets an inner **content group** holding their
groups, and its own painted nodes are created *before* that group — so a
panel's background is under its children whatever order the passes ran
in.

A widget paints into numbered **slots**: slot 0 its background, slot 1
its label, whatever it decides, as long as it is stable between paints,
because the slot number is what the framework diffs against.

```rust,ignore
fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
    cx.rect(0, cx.bounds, Fill::Solid(face), radius, border);
    cx.text(1, text_box, &self.text, &style, color, Align::Center);
}
```

The first paint of a slot sends `CreateNode` under the widget's group;
later ones **diff against a per-slot cache of the last values sent** and
emit only the properties that changed. A repaint that produces the same
rect and the same fill costs zero bytes, which is what makes "just
repaint the dirty widget" cheap enough to be the only strategy. A slot
the paint did not emit this time has its node destroyed; a slot that
changed node kind is a different node and is replaced.

The consequence worth stating: **a moved widget costs one mutation and no
repaint of its content**, because the move is a `SetBounds` on the group
and the content hangs underneath it.

## Layout

A flex subset, implemented as pure functions in `layout.rs` with no
knowledge of widgets, the arena or the connection — which is why the
model has unit tests that need no server (`solve`, `intrinsic_main`,
`intrinsic_cross` over `FlexItem`s).

`LayoutStyle` carries `direction` (Row/Column), `main_align`
(Start/Center/End/SpaceBetween), `cross_align`
(Start/Center/End/Stretch), `gap`, `padding`, `margin`, `width`/`height`
as `Length` (Auto/Px/Percent), `min_`/`max_` on both axes, `flex_grow`
and `flex_shrink`.

Two passes, as CSS does it: measure every child at its intrinsic size,
divide positive free space by `flex_grow`, take negative free space back
weighted by `flex_shrink × basis`, then `MainAlign` places whatever is
still left over and `CrossAlign` sizes and positions on the other axis.

What it does **not** do: wrapping, `order`, baseline alignment, and the
iteration CSS performs when a min/max clamp puts free space back on the
table (we clamp once and accept the second-order error). `Adaptive` and
breakpoints are M3.

## Text measurement is synchronous, and that is an M2 choice

A label cannot say how big it is until the server — which owns the
fonts — has shaped its string. `Ui::measure_text` therefore sends
`MeasureText` and **blocks on the reply**.

This is a deliberate M2 decision, not an oversight, and it is bounded on
three sides:

* `MeasureText` is the protocol's one request/response pair and is
  answered **on receipt**, not at a commit, so the wait is one socket
  turnaround rather than one frame.
* Results are cached by `(text, style, max_width)` in a
  `TextMeasureCache`, so a settled UI does no round trips at all — only a
  *new* string costs one.
* Anything else that arrives while we wait (a keystroke, a `Configure`)
  is kept in a stray queue and delivered on the next drain, so nothing is
  dropped on the floor.

The async path — measure optimistically, re-lay-out when `TextMeasured`
arrives — is the M3 answer, and it becomes worth its complexity when
there is a widget whose text changes on every keystroke. A text field is
exactly that widget, and it is M3.

With no `TEXT` capability there is nothing to ask, and the measurement
falls back to an estimate from the font size, so a fontless server still
produces a plausible layout rather than a tree of zero-sized labels.

## Events

`ServerMsg` → `Event`, with window bookkeeping stripped and pointer
positions already **widget-local**:

* **Pointer** events hit-test the widget bounds, deepest first, and
  bubble up until one answers `Handled::Yes`. Enter/leave is computed
  from the difference between the old and new hover chains, so a widget
  gets exactly one `PointerEnter` per crossing.
* **Keys** go to the focused widget and bubble to the root. A `KeyDown`
  nobody consumed that produced text is re-offered as `Event::Text`,
  which is how an app gets a global shortcut with no filter list — see
  `QuitOnQ` in the example.
* **Tab** is the framework's and is never offered to a widget:
  `focus_next` walks the focusable widgets in pre-order and wraps.
  Shift-Tab walks back.
* **Configure** resizes and re-lays out; **Closed** quits.

After every batch, `flush()`.

### One rule that surprises people

**The server hit-tests painted scene content.** A window whose widgets
draw nothing is not under the pointer at all, and receives no pointer
events — `nitro-scene`'s `hit_test` skips nodes that do not paint, so a
group with no content never swallows a click. A custom widget that wants
pointer events must therefore paint something, even a transparent rect.
This is the right server behaviour (an empty group should not eat
clicks), and it is worth knowing before you debug it.

## The app loop

`App::new(name)` connects (`NITRO_SOCKET`); `App::run(state, build)`
builds the tree, opens one window sized to the root's measured size (or
`App::size`), and runs a level-triggered `epoll` over the connection fd
plus whatever the app registered with `ui.add_fd`. `ui.set_timer(ms, cb)`
sets the `epoll` timeout. With nothing happening it blocks in
`epoll_wait` and no bytes move — the same property the server has, for
the same reason.

`add_fd` **dups** the descriptor and returns an `FdToken`, so the app may
close its own copy and the loop never has to reconstruct a `BorrowedFd`
from a raw number (which would need `unsafe`, which this tree does not
use).

## Writing a widget

A widget is a plain struct with a `Widget<S>` impl. Every method has a
default that does the sensible thing for a leaf, so implement only what
you need.

```rust,ignore
struct Dot { color: Color }

impl<S: 'static> Widget<S> for Dot {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, c: Constraints) -> Size {
        c.constrain(Size::new(16.0, 16.0))
    }
    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        cx.rect(0, cx.bounds, Fill::Solid(self.color), 8.0, (0.0, Color::TRANSPARENT));
    }
    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        match ev {
            Event::PointerDown { .. } => { cx.request_paint(); Handled::Yes }
            _ => Handled::No,
        }
    }
    fn role(&self) -> Role { Role::Other }
}

// And the setters, which are the only way its properties change:
impl<S: 'static> WidgetMut<'_, Dot, S> {
    pub fn set_color(&mut self, c: Color) { self.color = c; self.request_paint(); }
}
```

Then a builder: hold a `Built<S>` (the style setters come free from
`StyleBuilder`), accumulate widget settings, and swap the widget in at
`into_widget` time with `Built::replace_widget`. Containers add
`ContainerBuilder` for `.child()`, `.gap()` and the alignments.

Checklist for a new widget:

* `measure` must be a pure function of its content and the constraints;
  the framework memoizes it.
* `paint` uses **stable slot numbers**, and paints in its own coordinate
  space (`cx.bounds` is `(0, 0, w, h)`).
* Setters pick `request_paint` over `request_layout` when the change
  cannot move anything.
* `role` and `accessible` are not optional in spirit: they are what the
  introspection socket serves.
* If it wants pointer events, it must paint something.

## The test harness

`nitro_ui::test::Harness` (feature `test-support`) starts a real
`nitro-server` on the fake backend in a thread of the test process,
connects a real client, and runs the `Ui` **on the test thread** — no
`App::run` loop, nothing to race with. The test pumps.

```rust,ignore
let mut h = Harness::sized("demo", state, Size::new(240.0, 120.0), |ui| { .. });
h.click(button_id);                       // evdev → server hit test → wire → widget
assert_eq!(h.widget::<Label>(id).text(), "after");
assert!(h.has_ink(h.bounds(id), 0x00ff_ffff));   // real pixels
h.assert_idle(200);                       // and nothing while idle
```

It injects synthetic input through the server's fake `InputSource`,
offers `shot()` for pixel assertions, `widget::<W>(id)` for state, and a
**mutation tap** (`h.tap()`, `h.mutations()`) so a test can assert that a
label's text change cost exactly one commit and exactly one `SetText`.
Counting messages is the only honest way to check the "work is
proportional to what changed" claim from outside.

Two things it needs from `nitro-server`, both behind its `test-support`
feature:

* `test_support::TestServer` — the in-process server, factored out of
  `tests/fake_loop.rs`'s `Harness` rather than copied.
* A **`focus` control request**. Focus normally follows the click, which
  is right for a desktop and wrong for a toolkit test: synthesising a
  click to obtain focus would move the focus to whatever widget was under
  the pointer, which is exactly the state a focus test is about to assert
  on. `focus` gives the topmost window keyboard focus and nothing else.

An absolute pointer device reports in its own unit square, which the
server scales onto the first output — so the harness divides window
coordinates by the output size on the way in. Getting that wrong is why
an early version of every pointer test saw no hover at all.

## Measured

`examples/hello_dialog.rs`, release, stripped, against a fake server:

| what | value |
|---|---|
| binary | **444 200 bytes** (444 KB) |
| RSS | **2 440 kB** |
| threads | 1 |
| context switches over 5 s idle | **0** (client and server both) |
| `ldd` | `libc`, `libgcc_s`, vdso — nothing else |

The binary contains no font library, no rasterizer and no compositor: a
label is a string on the wire. That asymmetry is the whole reason the
toolkit is this small, and it is what makes the remote case cost what the
local one does.

App code in `main`: 31 lines as rustfmt wraps it.

## Deviations and limitations

Recorded here because the spec asks for them, not because they are
regrets:

* **Text measurement is synchronous.** Argued above. The async path is
  M3, and the cache makes the M2 cost a per-string one-off.
* **A widget must paint to be hit.** Inherited from the server's hit
  test, and correct there; documented above because it is surprising from
  inside a widget.
* **The flex solver clamps min/max once** rather than iterating as CSS
  does, so a child whose clamp releases free space does not give it back
  to its siblings.
* **No wrapping, no `order`, no baselines** in layout; no `Adaptive`
  widget yet (M3, and it is a widget, not a new mechanism).
* **`Ui` owns exactly one window.** Multi-window is a shell concern and
  M3; nothing in the arena assumes one window, only `open_window` does.
* **`introspect` produces the tree but serves nothing.** The socket is
  the next task.
* **`Button::on_click` is `Fn`, not `FnMut`** — it is taken out of the
  button for the call (same trick as the arena), and a `FnMut` would need
  either a second take-out or interior mutability. `&mut S` is where the
  mutation belongs anyway.

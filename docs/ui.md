# nitro-ui — the toolkit

`nitro-ui` is the client-side half of nitro: a **retained widget tree in
an arena** that maps widgets to scene nodes and sends only mutations for
what changed. It is the layer an app is written against, and the reason
an app binary is 522 KB with no font library in it.

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
bounds, focusable and focused. The point of goal 5 is that there is *one*
tree, not a shadow one maintained alongside it.

**Where the socket hooks in.** `crate::introspect::Socket` is owned by
the app loop, its listener sits in the same `epoll` set as the
connection, and `Socket::serve` runs *after* the event batch and *before*
`flush` — so a request sees a settled tree and its effects are carried by
the same commit the next real event would have used. A `do … click`
routes through `Ui::action`, which is the same take-out dispatch
`Ui::dispatch` uses for a real click: the widget leaves its slot, gets a
full `&mut Ui<S>` and the app's `&mut S`, and its callback cannot tell
the two apart. `set` goes through the widget's `set_<prop>` action, which
is the `WidgetMut` setter, so invalidation is identical too.

That placement is the whole reason the design works without a lock: IPC
and the application share one message loop, which is what BeOS did and
what `DESIGN.md` goal 5 asks for. The protocol, the path grammar and the
roles/actions table are in `docs/introspection.md`.

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
    cx.text(1, text_box, &self.text, TextRun::new(&style, color).align(Align::Center));
}
```

`cx.text` takes a [`TextRun`] — style, colour, alignment and **wrap
width**. The wrap width must be the one the run was measured at:
`SetText` is the only place the server learns a wrap width, so measuring
wrapped and painting unwrapped would reserve two lines of height and draw
one overflowing line, and measure/paint disagreeing is the one thing a
retained model cannot tolerate. `Label` remembers the width its `measure`
used and hands it back (`.wrap_at(self.wrap_width)`); `Button` measures
unwrapped, because it is sized around its label.

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

## The widget set

| widget | role | value | actions | notes |
|---|---|---|---|---|
| `Flex` (`column()`, `row()`) | `container` | — | — | draws nothing; everything is its `LayoutStyle` |
| `Panel` | `container` | — | — | background, radius, border, each `None` = the theme's |
| `Label` | `label` | its text | `set_value` | remembers the width it was measured at |
| `Button` | `button` | — | `click`, `activate`, `focus` | hover/pressed/focused faces |
| `TextField` | `textfield` | its contents | `set_value`, `submit`, `clear`, `focus` | caret, selection, click-to-place, h-scroll |
| `Checkbox` | `checkbox` | `true`/`false` | `toggle`, `set_value`, `focus` | Space toggles |
| `Slider` | `slider` | the number | `set_value`, `focus` | drag, arrows, Home/End, optional step |
| `Scroll` | `scroll` | the offset | `scroll_to`, `scroll_by`, `focus` | wheel, arrows, PgUp/PgDn, Home/End |
| `Separator` | `separator` | — | — | spans its container on the other axis |
| `Image` | `image` | `WxH` | — | an `ARGB` buffer, uploaded once in a memfd |
| `Spacer` | `spacer` | — | — | `.grow(1.0)` and nothing else |

Three of them are worth a paragraph, because each makes a claim about
cost that the rest of the design has to hold up.

**`TextField` does not re-measure the tree on a keystroke.** Its
`measure` is a function of the *font* and its width style, never of its
contents, so typing repaints one widget and lays out nothing. The one
measurement it genuinely needs is where the caret goes, and that is
`Ui::cursor_positions` — the `cursor_x` table `TextMeasured` already
carries, cached by the same `(text, style, width)` key as the extent, so
a caret move inside a string the field has already measured is free. This
is also why the synchronous measurement of `docs/ui.md`'s M2 decision
does not bite here: a text field was the widget that argument was worried
about, and it turns out not to need a round trip per keystroke after all,
only per *new string* — which is one, on the way in.

Its overflow scrolls **by composition**: the text node hangs under a
clipping group whose transform is the scroll offset, so a caret past the
right edge sends one `SetTransform` and no `SetText`.

**`Scroll` is the same trick one level up, and it uses no extra node at
all.** A widget's children already hang under its *content group*; the
scroller clips that group (`Ui::set_content_clip`) and translates it
(`Ui::set_content_transform`). Scrolling is therefore exactly one
`SetTransform` — no relayout, no repaint of anything inside — and the
harness test `scrolling_is_one_set_transform_and_nothing_else` asserts
that from the outside by counting mutations, because a cost claim nothing
checks stops being true. A scroll that would change nothing (already at
the end) sends nothing at all.

**`Checkbox` draws its tick as an inset rect, not a glyph.** The server
draws rectangles and text, and a checkmark string would need a font that
has one — which is exactly the assumption a toolkit that ships no fonts
must not make.

Every one of them answers `role()`, `accessible()` and `action()`, which
is what the introspection socket serves and what an AT-SPI bridge will
read. `action()` is not optional in spirit: it is the difference between
a widget a script can drive and one it can only look at.

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
* **Focus changes are queued, not delivered inline.** `Ui::focus` is
  reachable from `EventCx::request_focus`, i.e. from inside a widget's
  own `event` — where that widget is out of its slot and the app state is
  already borrowed, so it could not receive the notification anyway.
  `pump` drains the queue through `Ui::deliver_focus_events` once the
  batch is done and every widget is back in place; a notification the
  tree has already overtaken is dropped rather than delivered stale. An
  app driving `Ui` by hand should call it too.
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
* `role`, `accessible` and `action` are not optional in spirit: they are
  what the introspection socket serves, and `action` is what makes the
  widget drivable from outside. A widget whose `action` answers
  `Handled::No` to everything can be read but not used.
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

| what | value | at the end of #3682 |
|---|---|---|
| binary | **522 032 bytes** (522 KB) | 444 608 (444 KB) |
| RSS / HWM | **2 580 kB** | 2 500 kB |
| threads | 1 | 1 |
| context switches over 5 s idle | **0** (client and server both) | 0 |
| `ldd` | `libc`, `libgcc_s`, vdso — nothing else | same |

The binary contains no font library, no rasterizer and no compositor: a
label is a string on the wire. That asymmetry is the whole reason the
toolkit is this small, and it is what makes the remote case cost what the
local one does.

App code in `main`: 31 lines as rustfmt wraps it.

### What the introspection socket cost, and why it is not small

The spec expected the introspection code to be a small delta, and asked
for the reason if it was not. It is **+77 424 bytes of binary (+17 %)
and +80 kB of RSS**, and the honest attribution is that essentially all
of it is the socket rather than the widgets. Building the same example
three ways:

| build | binary | delta |
|---|---|---|
| this tree | 522 032 | — |
| `App::run` never binding the socket (`introspect(false)` hard-coded) | 514 952 | −7 080 |
| the `introspect` and `shot` modules removed from the crate | 453 600 | −68 432 |

Summing the symbol sizes of an unstripped build agrees: `nitro_ui::
introspect::*` is **39 322 bytes** of text and `nitro_ui::shot::*` 1 001,
against 10 960 for *all eleven* widgets — only 558 more than the five
that were there before this task. `TextField`, `Checkbox`, `Slider`,
`Scroll`, `Separator` and `Image` are nearly free because they share the
paint, measure and theme helpers the earlier widgets already had.

Two reasons the socket is as big as it is, and one of them is fixable:

1. **It is monomorphised per app-state type.** `Socket::serve<S>`,
   `list<S>`, `get<S>`, `invoke<S>` and `path_of<S>` are all generic over
   `S` because they hold a `&mut Ui<S>`, so every generic function in the
   module is instantiated afresh for each app. `Socket::serve` alone is
   the largest single symbol in the binary at 24 335 bytes. Routing the
   protocol through a small `dyn` interface over a `&mut dyn
   IntrospectTree` would collapse that to one copy for the whole program,
   at the cost of one virtual call per request — a request rate measured
   in tens per second. **That is the M3 change**, and it is a mechanical
   one: the protocol code already touches the tree through eight methods.
2. **Text formatting is not free.** The protocol prints numbers, and
   `core::fmt`'s float path (`flt2dec`, 16 997 bytes) is linked whether
   or not anything formats a float — it is in the baseline binary too, so
   it is not part of the delta, but it is why the `format_number` helper
   avoids `{:?}` and the `{:.4}`-then-trim form is the only float
   formatting the protocol does.

The RSS delta (+80 kB, roughly the binary growth) is entirely
resident text: the socket allocates nothing until something connects, and
its `snapshot` vector is freed the moment the last watcher goes away —
`Socket::serve` shrinks it to fit, so an app nobody is watching pays for
the code and not the data.

Whether 77 KB is worth it is a product question rather than a technical
one, and the answer this milestone takes is **yes, on by default**: an
app that has to opt in to being scriptable is an app that nothing can
drive, and goal 5 is that *every* nitro app is scriptable with one
mechanism. `App::introspect(false)` is there for the app that disagrees,
and it saves the 7 KB of binding code but not the 68 KB of protocol —
until the M3 de-monomorphisation, at which point the whole thing should
fall to roughly the size of one instantiation.

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
* **No pointer grab.** A press followed by a release outside the widget
  is not routed back to it, so `Button` drops its pressed state on
  `PointerLeave` instead. A real grab is a server-side concept and M3.
* **`Ui::remove` does not recycle the node ids under the destroyed
  subtree.** One `DestroyNode` on the outermost group frees them
  server-side; reusing them locally would mean proving that commit had
  been applied. Ids are a monotonic `u32` per client.
* **`set_theme` re-marks the root's subtree**, so a widget built but not
  yet attached keeps the old theme's measurements until it is attached
  (which marks it anyway).
* **No wrapping, no `order`, no baselines** in layout; no `Adaptive`
  widget yet (M3, and it is a widget, not a new mechanism).
* **`Ui` owns exactly one window.** Multi-window is a shell concern and
  M3; nothing in the arena assumes one window, only `open_window` does.
* **The introspection protocol is monomorphised per app-state type**, so
  it costs ~68 KB of binary in each app rather than being shared. The
  `dyn`-interface fix is argued under *Measured* above and is M3.
* **`watch` detects a value or focus change by diffing snapshots**, not
  by being told. That is why it reports a change however it was made —
  the app's own code, real input or another client all look identical —
  but it means a value that changes and changes back within one loop
  turn is not reported, and the diff is O(widgets) per turn *while a
  watcher is connected*. An unwatched app does no work at all.
  Activation (`click`) is the exception and *is* announced, by
  `EventCx::report_activation`, because running a callback leaves no
  trace in the tree to diff.
* **`shot` screenshots the whole output and crops.** The window's
  position comes from `Configure`; a window that has moved without the
  client being told would crop the wrong rectangle. The server always
  sends a `Configure` on a move, so this is a statement about what the
  code relies on rather than a known bug.
* **Security is the socket directory's mode**, `0700`, and nothing else:
  any process of the same user can drive any app completely. Same
  boundary as the X11 socket or `$XDG_RUNTIME_DIR/wayland-0`; a per-app
  allow policy is M3+ and belongs with the session manager that would
  issue the tokens.
* **`TextField` is single-line, and has no clipboard, no undo and no
  IME.** Each is a real feature rather than a missing case, and each
  wants a server-side concept (a selection owner, a text-input protocol)
  that M2 does not have.
* **`Scroll` is vertical only** and scrolls by translating its content
  group, so its child is laid out at full height. A list of ten thousand
  rows therefore costs ten thousand widgets; virtualisation is M3 and is
  a widget, not a new mechanism.
* **`Button::on_click` is `Fn`, not `FnMut`** — it is taken out of the
  button for the call (same trick as the arena), and a `FnMut` would need
  either a second take-out or interior mutability. `&mut S` is where the
  mutation belongs anyway.

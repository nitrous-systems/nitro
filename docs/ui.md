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

### `ui.defer`, for the callback that really does mean itself

"To change yourself, use the `&mut self` you already have" is the right
answer for a widget's own `event` and `action`, where there *is* a `&mut
self`. It is not available in the third place a callback runs: an **app**
callback like `on_click` or `on_activate` is handed `&mut S` and `&mut
Ui<S>` and no `self` at all, because it is the app's code rather than the
widget's.

Most of the time that is fine, because such a callback changes something
else. A list is where it is not. "Activate this row" means "show
different rows **here**", and `nitro-files` (M4-D) shipped exactly that:
its `on_activate` navigated, wrote the new rows with `if let Ok(mut l) =
ui.widget_mut(list)`, and the `Err(Busy)` went into the `if let`. The
path bar updated, the rows on screen did not, and **nothing anywhere
returned an error anybody read**. The tests passed; a test that called
the same function directly saw it work, because directly is not through
the widget.

`ui.defer(|s, ui| ..)` queues the work and `Ui::run_deferred` runs it
once dispatch is over and every widget is back in its slot. It is not a
new mechanism: `Ui::focus` has always queued its `FocusChanged` for
precisely this reason — a widget out of its slot cannot receive a
notification either — so this is that answer generalised rather than a
second one invented beside it. The queue drains wherever
`deliver_focus_events` does (after an event batch, after an introspection
action) and also after `run_timers` and `run_fd`, since a timer and a
descriptor hook are app code with the same rights. A deferred callback
may defer again — a navigation that triggers a re-listing does — and the
chain drains in the same pass, bounded by `MAX_DEFER_ROUNDS` so a
callback that re-queues itself unconditionally is cut off with a message
instead of hanging the loop.

The rule is one sentence: **if an app callback needs to change the widget
whose callback it is, defer it.**
`a_widget_callback_changes_that_widget_by_deferring` in `tests/ui.rs`
asserts both halves — that the reach-yourself case really is `Busy`, and
that deferring really does land.

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
every ancestor, **stopping as soon as every one of them is already lit** —
so marking the thousandth widget in a settled tree is O(depth) the first
time and O(1) afterwards. A pass that sees neither `X` nor `SUB_X` on a
node skips the entire subtree without walking it. That is the whole
mechanism behind "work is proportional to what changed".

The word *every* in that sentence is load-bearing, and it cost a bug to
find out (#3691). A mark is usually a **pair** — `set_text` is
`LAYOUT | PAINT` — so the walk is setting `SUB_LAYOUT | SUB_PAINT`, and
the early stop has to ask whether the ancestor already carries **both**.
Stopping at the first ancestor that carried *one* of them (an earlier
layout-only change had lit `SUB_LAYOUT` all the way up) left `SUB_PAINT`
unset to the root: `pass_paint` then skipped a subtree holding a `PAINT`
widget, and the symptom was the nastiest kind there is — the tree said
one thing, the screen said another, and nothing anywhere returned an
error. `Dirty::has` is "any" for the passes, `Dirty::has_all` is "every"
for the mark, and `a_combined_mark_lights_every_sub_flag_on_the_way_up`
in `tests/ui.rs` fails without the distinction.

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

### A window's content is clipped to the window

`Ui` sets `SetClip` on the **root** widget's group, once, and the scene
clips every descendant to it. The rectangle it clips to is the root's own
bounds, which the layout pass keeps equal to the window's content area on
every `Configure`, so a resize needs no second message and the clip
cannot go stale.

So **overflow is a layout bug you *see* as cut-off, never as spill.** A
row wider than its window ends at the window's edge; it does not paint
its last widgets onto the desktop beside the frame. That is worth
stating as a rule because the toolkit shipped without it and a real app
found out: `nitro-settings`' display row grew to ~700 px in a 560-px
window (#3718 added a long string to a row that was already full, and
the content floor below means a `Label` will not shrink below its text),
and the slider, the checkbox and both position fields were painted on the
wallpaper. `SetClip` is opt-in on the wire — it has to be, a `Scroll`
needs it and a shadow must not have it — and nothing was setting it on a
window's root.

The compositor will eventually forbid a window painting outside its own
frame whatever its client asks for. The toolkit does not wait for that:
a toolkit that relies on the compositor to contain it is one whose bugs
are invisible until they are somebody else's.
`a_child_moved_past_the_window_paints_nothing_on_the_desktop` in
`crates/nitro-ui/tests/ui.rs` is the pin, and it is a pixel census of the
desktop strip beside the window: 0 changed pixels with the clip, 1998
without it.

The same census on the real box, against the settings dialog that found
this: **1910 changed pixels in a 27-row band** — one row's worth of
widgets on the wallpaper — before, **0** after (`docs/settings.md`).

## The widget set

| widget | role | value | actions | notes |
|---|---|---|---|---|
| `Flex` (`column()`, `row()`) | `container` | — | — | draws nothing; everything is its `LayoutStyle` |
| `Panel` | `container` | — | — | background, radius, border, each `None` = the theme's |
| `Label` | `label` | its text | `set_value` | remembers the width it was measured at; `.elide(true)` shortens with `…` instead of overflowing |
| `Button` | `button` | — | `click`, `activate`, `focus`, `alt_click` | hover/pressed/focused faces; `alt_click` is the middle button |
| `TextField` | `textfield` | its contents | `set_value`, `submit`, `clear`, `focus` | caret, selection, click-to-place, h-scroll |
| `Checkbox` | `checkbox` | `true`/`false` | `toggle`, `set_value`, `focus` | Space toggles |
| `Slider` | `slider` | the number | `set_value`, `focus` | drag, arrows, Home/End, optional step |
| `Scroll` | `scroll` | the offset | `scroll_to`, `scroll_by`, `focus` | wheel, arrows, PgUp/PgDn, Home/End |
| `List` | `list` | the **visible** rows, one per line | `activate`, `select`, `scroll_to`, `scroll_by`, `focus` | virtualised: `visible + 2` rows materialised, whatever the model holds; a row's icon is a **name** |
| `Separator` | `separator` | — | — | spans its container on the other axis |
| `Image` | `image` | `WxH` | — | an `ARGB` buffer, uploaded once in a memfd |
| `Icon` | `icon` | the icon **name** | `set_icon`, `set_value` | named, never drawn: the server owns the artwork (`docs/icons.md`) |
| `Spacer` | `spacer` | — | — | `.grow(1.0)` and nothing else |

`Role::Terminal` exists too, and has no widget in this crate: it is what
`nitro-term`'s grid answers, and it is here because the role vocabulary
is the introspection protocol's rather than the widget set's. A screen
reader has to know that this widget's text is a *screen* — rewritten in
place, addressed by row and column — rather than a document that grows,
which is why AT-SPI has had the role since the beginning.

Four of them are worth a paragraph, because each makes a claim about
cost that the rest of the design has to hold up — and a fifth, `List`,
wants a section of its own, because its claim is the one this document
spent two milestones deferring.

**`Icon` measures without a round trip, because icons are square by
contract.** `icon("gear").size(16.0)` measures to exactly 16 × 16 and
asks the server nothing. A `Label` cannot do that — it has no fonts, so
it has to ask how wide its string is — but every icon in the set is drawn
on a 16-unit grid, so one number is both dimensions and the layout is
arithmetic rather than a question.

What crosses the wire is a **name and a palette role**, never pixels, and
that is the whole design: `docs/icons.md` makes the argument in full, but
the three-line version is that it is the only form that survives a remote
link (a buffer is a file descriptor and cannot cross TCP), a scheme flip
(the server resolves the role at paint time, so a `theme.scheme = dark`
recolours every icon with no client message at all) and a scale change
(the server re-rasterises at the output's device size, so a 2× screen
gets a real 2× icon rather than a doubled 16 px tile).

There is deliberately **no** `.color(Color)` on an `IconBuilder`, only
`.color_role(Role)`: a literal is a colour the user's switch could never
reach, and `deploy/lint-colors.sh` enforces the same rule from outside.
Without `caps::ICONS` the widget measures the identical box and paints
nothing, so an icon-less server costs a gap in a row and never a broken
layout — the same bargain `Label` makes with `caps::TEXT`.

**`.coloured()` is the other icon set, not the other colour.** Since
M4-H the server owns two: the symbolic one compiled into it, and the
machine's XDG icon theme. `icon("gear")` names the first and takes a
role; `icon("firefox").coloured()` names the second and takes none — an
application icon has its own colours, and no role to resolve. The
`IconTint` enum is the toolkit's name for that choice
(`IconTint::Role(r)` or `IconTint::Coloured`), and the two are mutually
exclusive with the last call winning.

Nothing falls back between the sets, which is why a coloured icon wants
`.fallback_tinted(name, tint)`: a name that came from outside the program
— a `.desktop` file's `Icon=`, a window's app id — is a claim about the
box rather than a fact about it, and the icon to reach for when it is
wrong has to come from the set that **cannot** be missing. That is the
mixed case and it is the common one:

```rust,ignore
icon("firefox").coloured()
    .fallback_tinted("window", IconTint::Role(ColorRole::Text))
```

`.fallback(name)` is the same-tint shorthand, right for a theme icon
falling back to a more generic theme icon. Both are **exactly once**: the
server's `BadIcon` re-sends through the normal paint path, and a
`BadIcon` for the fallback itself is the end of it. `Icon::fell_back()`
is how a test — or a caller — asks which name is actually on screen.

One wrinkle worth knowing, because it shows in the code and will look
like a shortcut otherwise: `Error { BadIcon }` carries **no node id**, so
the toolkit routes it to a widget by parsing the quoted icon name out of
the error's message and offering the fallback to every widget whose slot
holds a `SetIcon` for that name. It is honest and bounded, and it is a
weaker guarantee than the wire could give — issue #564 is the wire change
that would fix it.

**`Button::icon` is the same guard with a better fallback.** A button
given an icon falls back to painting its *label* when the server has
none, which is why a button keeps its `text` even when it shows a glyph:
the glyph is for the eye, the word is for everything else, and a blank
face would be worse than either. Both `measure` and `paint` ask the
capability, and they must agree — an icon box is a square and a label box
is not, so a button that measured one and painted the other would clip.

A button has **two icon modes**, and `IconMode::Replace` — the glyph
*instead of* the word — is the original and still the default.
`IconMode::Leading` puts the icon in front of the label with `ICON_GAP`
between them and measures to the sum, which is what a row in a list
wants: a launcher entry or a window-list button, where the word is the
content and the icon is the hint and neither is redundant. The builders
are `.icon(n)`, `.icon_leading(n)` and `.icon_coloured(n)`, with
`.icon_size(px)`, `.icon_tint(t)`, `.icon_fallback(n)` and
`.icon_fallback_tinted(n, t)` beside them, and a `WidgetMut<Button>`
setter for each — because a task list creates its buttons before it knows
which application they are for. In every mode the **text** stays the
accessible name, so `hey` and a screen reader are unaffected.

`.icon_size` exists because a button's icon otherwise takes the label's
font size, which is right when the glyph stands in for the word and wrong
when it stands beside it: a 13 px application logo next to a 16 px
symbolic one is visibly odd, and 16/24/32/48 are the sizes the artwork is
drawn for.

`List` rows carry icons too, since M4-I, and on the same terms: a row's
`icon` field is an icon **name**, painted into an `Icon` node in a fixed
column, guarded by the same capability check and diffed against the same
slot cache. What a row does *not* get is `.coloured()` or a fallback —
there is no `IconMode` to choose and no `.fallback(…)` to latch — because
a row's icon is the app's own furniture rather than a name that came out
of a `.desktop` file, so there is nothing for it to fall back *from*. The
`icon_tint` field is an `IconTint` rather than a role so that the door is
open; the widget has no second name to try. See *A row's icon is a name*
under `List` below.

The guard is not cosmetic, and this is the reason it is spelled out
here rather than left to the reader. Painting an icon creates a node of
`NodeKind::Icon`; on a server older than the icon set that kind is a
**decode error**, which is fatal — so an unguarded icon button does not
lose a glyph against an old server, it loses the application.
`crates/nitro-ui/tests/icons.rs` pins both halves, and pins them by
masking the capability *before the first paint*, because once the node
exists the damage is already done.

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

### `List`, the one widget that is not the size of what it shows

Every other widget in this crate is the size of its content. A list is
the first that is not: a directory of a hundred thousand files is a
hundred thousand rows, and a toolkit that turned each into a widget would
allocate a hundred thousand arena slots, measure them all, and hand the
server a hundred thousand scene nodes in order to draw a screenful of
text. This document used to carry that under *Deviations* as a known
cost — "a list of ten thousand rows costs ten thousand widgets;
virtualisation is M3 and is a widget, not a new mechanism" — and `List`
is that widget. `nitro-files` (M4-D) is the app that needed it, because a
directory is the only model whose size the user chooses and nothing
bounds; `docs/files.md` has that argument.

Its rows are **data, not widgets**: a `Row` is an optional **icon name**,
a primary text and a right-aligned secondary text, and the widget owns the
model. There is no per-row widget, no per-row callback and no per-row
state — a row is addressed by its index, and that index is what
`on_activate` and `on_select` are handed. An app with a model of its own
pays for the `Vec<Row>` it hands over, which is the deliberate trade: a
borrowed model would have to be reachable from `paint`, which sees
`&mut Ui<S>` and not `&mut S`, so the alternative is interior mutability
in the one place this crate has none. `ListModel` is the door left open
for an app that would rather generate a row than store it; `row(i)` is
called only for the rows that are materialised, which is a screenful.

Three cost claims, each named with the test that checks it, because a
cost claim nothing checks stops being true.

**The scene holds a screenful, whatever the model holds.** The widget
materialises `visible + 2` rows into paint slots of its own — using the
`PaintCx::keep` mechanism `nitro-term` introduced, which is what
*Slots that are content rather than parts* below is about — so a
100 000-row model and a 100-row model create the same number of nodes.
`a_hundred_thousand_rows_create_a_screenful_of_nodes` asserts that a
200 px viewport over 100 000 rows materialises `rows_that_fit() + 2` and
fewer than thirty rows in total, *and* that the rows are really drawn —
a virtualisation that drew nothing would pass every count on its own.
`the_node_count_does_not_depend_on_the_models_length` makes the same
assertion over a hundred rows, which is the other half of the claim.

**A scroll that does not change the visible set is one `SetTransform`.**
The rows hang under a clipping group of the widget's own and the group's
transform is the scroll offset, so scrolling inside the two spare rows
costs one mutation and no repaint at all — the same trick `Scroll` plays
one level up, and the reason the spare rows exist at all.
`scrolling_one_row_is_one_set_transform` counts the mutations from
outside and demands exactly `[SetTransform, Commit]`.

**A row that did not change sends nothing.** Slots are addressed
`row_index % ring_len` rather than `row_index - first_row`, so
re-anchoring the materialised window moves only the rows that actually
changed: a full page down re-emits a page, and a single row of
re-anchoring re-emits a single row.
`scrolling_a_page_repaints_only_the_rows_that_changed` jumps five
thousand rows and asserts that the `SetText` count is bounded by the ring
rather than by the distance travelled, with **zero** `CreateNode` and
zero `DestroyNode` — the ring is reused, so scrolling costs mutations and
not allocations. `replacing_the_model_costs_a_screenful_not_a_model`
makes the same assertion for a wholesale model replacement, which is what
a directory refresh is.

Two smaller decisions follow from the same reasoning. **Selection is the
row's background and nothing else**, so moving it is exactly two
`SetFill`s — one for the row that lost it, one for the row that gained it
(`moving_the_selection_is_two_set_fills`). Tinting the text as well would
have been prettier and would have cost a `SetText` per run per row on
every arrow key, which in a toolkit whose whole claim is that work is
proportional to change is the wrong kind of pretty. And the **type-ahead
prefix expires by elapsed time checked on the next key, not by a timer**,
because a list that armed a timer on every keystroke would wake the loop
half a second after the user stopped typing in order to do nothing;
`a_settled_list_sends_nothing_while_idle` asserts `next_timeout() ==
None` and then that the app is silent.

#### A row's icon is a name, in a column that costs width and never height

`Row::icon("folder-fill")` names an icon from the server's symbolic set —
the same namespace and the same `SetIcon` a `Button` or an `Icon` widget
uses, so `docs/icons.md`'s three properties come with it: it survives a
remote link, it recolours itself when the scheme flips with no client
message, and it is rasterised at the output's device scale rather than
doubled. `.icon_tint(t)` and `.icon_size(px)` are beside it; the tint
defaults to `ColorRole::Text` and the size to **16**, which is one of the
four sizes the artwork is drawn for.

It used to be a *glyph* — `"/"`, `"~"` — painted as text, because when the
widget was written the server had no icons. It has had them since M4-G,
and the substitution is not cosmetic: the glyph came from whatever font on
the box happened to carry that character, at whatever weight, and on a box
whose fonts lacked it from nothing at all.

**The column is width, not height**, and that is the layout contract worth
stating because breaking it would change every app at once: a 16 px icon
in a row whose text line is ~13 px does *not* make the row taller — the
icon is centred in the row's own height, the row height is still
`line + 8`, and `a_row_that_gained_an_icon_is_the_same_height` asserts
that a list with icons and one without have the same `row_height()` and
fit the same number of rows.

The column is **this row's own icon side**, not the widest in the model.
Finding that maximum means walking every row, which is the one thing a
virtualised list must never do; every list in this tree uses one size, so
the labels line up, and a list that mixed sizes would indent per row
rather than centre in a shared column. That is the trade, and it is here
rather than left to be discovered.

**Without `caps::ICONS` the column collapses** and the label starts where
it would have, which is the same bargain a leading-icon `Button` makes: a
row with a gap where an icon would be is a worse answer than a row without
one. The guard is not cosmetic, for the reason spelled out above — an
`Icon` node against a server predating the icon set is a decode error and
therefore fatal — and
`without_the_icons_capability_a_row_is_its_label_and_no_icon_node` pins it
by masking the bit *before the first paint*.

Two failures that the per-slot cache cannot see on its own, and both are
pinned. A **scheme flip** must cost the rows' text and nothing for their
icons, because the node holds a role index and the server resolves the
colour per frame: `a_row_icon_survives_a_scheme_flip_with_no_set_icon_at_all`
asserts zero `SetIcon`s *and* that the server's `icon_renders` did not
budge. And the **capability going away** moves every label while leaving
"same row, same generation, same selection" true, which is exactly the
shape of the palette bug the `painted_with` field exists for — so
`has_icons()` is a field of `RowPaint` beside the colours, and
`the_capability_going_away_moves_the_labels_and_the_cache_notices` checks
that the labels move and the icon nodes are destroyed rather than left
drawing stale artwork.

**What it costs is the icons that changed, and the comparison is against
what was *requested*.** A `List` reuses its ring slots, so a refresh that
produced the same listing must cost nothing, and a refresh that changed
one row's type must cost exactly one `SetIcon`. Both fall out of the paint
slot caching the last `SetIcon` it *sent* rather than what is displayed —
which is the lesson #3714's review paid for: a slot that cached the
displayed name would re-send a row's icon on every repaint the moment a
fallback or a capability mask put something else on screen.
`set_rows_with_identical_rows_sends_no_set_icon_and_one_changed_row_sends_one`
is the pair of numbers, **0 and 1**, with the `SetText` count beside them
as the control that makes the zero mean something.

Scrolling is the other half, because slots are addressed `row % ring`: a
re-anchor hands slot *k* a different row, and if that row's icon name
matches the slot's cached one, nothing is sent. Twenty whole-window
scrolls over a model whose icons alternate cost **0 `SetIcon`s against 400
`SetText`s** on the harness's ten-slot ring — the rows all changed, the
icons did not
(`scrolling_does_not_re_send_an_icon_for_a_row_that_merely_moved`).

Its introspection value is the **visible** rows, one per line, with the
detail column tab-separated, which is the honest answer rather than a
convenient one: a hundred thousand rows down a socket is not a value
anybody wanted, and the widget genuinely does not draw them.
`the_visible_rows_are_what_a_script_reads` checks the line count against
`rows_that_fit()` and the role against `list`, and
`a_script_can_select_and_activate_a_row_by_index` drives `select` and
`activate` through the socket and checks that an index past the end is an
error value rather than a panic.

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
as `Length` (Auto/Px/Percent), `min_`/`max_` on both axes, `flex_grow`,
`flex_shrink` and `shrink_floor`.

Two passes, as CSS does it: measure every child at its intrinsic size,
divide positive free space by `flex_grow`, take negative free space back
weighted by `flex_shrink × basis`, then `MainAlign` places whatever is
still left over and `CrossAlign` sizes and positions on the other axis.

**A child never shrinks below what it measured unless it says it can.**
That is `shrink_floor`, whose default is `Content` — CSS's `min-size:
auto` on a flex item — and it is the one place the model departs from
CSS's defaults rather than its arithmetic. A container with more content
than room therefore *overflows*: the children run past its end and the
parent, or the window, clips them. It does not squash them, because most
widgets have no smaller honest version of themselves: a label laid out
below its measured height is still painted with the full glyphs and
loses its descenders, and a column laid out below its children's total
draws its last row over whatever follows. Overflow ends the window
early; squashing corrupts a line the user is still reading. Because a
container's basis is its own measured size, which already sums its
children, the container rule falls out of the child rule for free.

The opt-out is `.shrink_to_zero()`, for a widget that is honestly
smaller when it is given less room: a viewport over a scrolled or
virtualised child. `List` takes it — showing fewer rows is what
virtualisation is *for* — and so do `Scroll`, `TextField` (a viewport
over its own string) and `Slider` (whose measured size is a default, not
a claim; give it a `min_width` for the narrowest still-draggable track).
`flex_shrink` keeps its meaning and now only divides an overflow between
the children that may take it; `.shrink(0.0)` still means "never shrink
at all".

### A third answer: a floor the widget computes

Two of the three widgets in a crowded row are covered by the rules above
— the ones with no smaller honest version (the default floor) and the
viewports (`shrink_to_zero`). An **eliding label** is neither. It *is*
honestly smaller than its text: it drops characters and says so with an
ellipsis. But it is not honestly *arbitrarily* small — below `…` plus a
few characters it has stopped communicating anything, and `HDM…` versus
`VGA…` is the question a truncated label has to be able to answer.

So a widget may report a floor of its own from `measure`, with
`cx.report_floor(size)`. The solver reads it as `FlexItem::floor` and it
only ever *raises* a `Zero` floor; a `max_*` still caps it and a `min_*`
still beats it. It is for the floor an author cannot write down as a
`min_width`, because only the widget can compute it: `Label`'s depends on
the server's fonts and on the string.

`.elide(true)` is the whole of it from the outside. An eliding label
takes the `Zero` floor automatically, reports `…` plus its first three
characters as its floor, and never wraps (the two policies contradict
each other — a wrapped label gets *taller* when given less width, which
is the opposite of what a row needs). The search is the server's own
title elision (`Text::elide`, `docs/wm.md`) run client-side: a binary
search over char boundaries for the longest prefix that fits, `log(len)`
measurements rather than one per prefix, memoized on the width it ran at
so a relayout at an unchanged width costs nothing. `Label::text()` is
still the whole string — that is what `hey get` and an accessibility
client read — and `Label::painted_text()` is what is on screen.

**The search runs from `layout`, not from `measure`, and that is the
load-bearing detail.** What `measure` is offered is the *container's*
available width; `solve` has not yet taken the row's overflow back out
of it. A label that elided against the offer would paint a string chosen
for 208 px into the 122-px box the solver gave it, and `PaintKind::Text`
clips a run to its item's bounds — so it would be cut mid-glyph with the
ellipsis itself clipped away, which is precisely the truncation the
ellipsis exists to replace. (That was the first version of this feature,
and every assertion about `painted_text()` passed while it was broken;
the test that catches it asks the server whether the painted string fits
the box, which is the property `.elide(true)` actually sells.)

So an eliding label's **basis is its whole text** — which is also what
makes the solver's arithmetic right, since the deficit it divides is the
difference between what the row wants and what it has — and `layout` is
where it learns what it got. That is the first moment a leaf knows its
own width and, because nothing re-measures a leaf afterwards, the only
one. It requests a *paint* there and never a layout: the string it picks
cannot be wider than the box it was picked for, so it cannot change the
label's size, and asking for a layout would invite a measure/layout
loop. A settled eliding label has searched exactly **once**.

The floor applies to whichever axis is the **parent's** main axis, and
`shrink_floor` is one field rather than one per axis. So a widget that
opted out for a horizontal reason has also opted out vertically: the
`TextField` and `Slider` arguments above are both about width, and in a
`Column` the same flag lets them be laid out shorter than they measured.
Neither is normally a column's flexible child, so this is a sharp edge
rather than a live bug, and `min_height` is the defence — the floor is a
`max` against it.

What it does **not** do: wrapping, `order`, baseline alignment, and the
iteration CSS performs when a min/max clamp puts free space back on the
table (we clamp once and accept the second-order error). An overflow no
child will absorb is left as overflow rather than iterated on, and the
leftover a `MainAlign` distributes is clamped at zero — a negative
leftover is overflow, so `SpaceBetween` degenerates to `Start` rather
than spacing children *backwards* on top of each other, which is what
CSS does with negative free space too. `Adaptive` and breakpoints are M3.

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
  nobody consumed that produced text is re-offered as `Event::Text`, so a
  widget matches on characters rather than on keycodes and layouts.
* **App-level key handlers run last.** `ui.on_key(|state, ui, key| ..)`
  registers a handler that is offered every **press** the focused chain
  declined — both the `KeyDown` and the `Event::Text` it produced — in
  registration order, until one answers `Handled::Yes`. Releases are not
  offered: a shortcut that fired on the press and again on the release
  would run twice, and the signature has no way to tell the two apart.
  `ui.set_shortcut(mods, keycode, |state, ui| ..)` is sugar over it for
  the common case, matching the modifier bits in `mods::MASK` exactly
  (so `Ctrl-Q` does not fire a plain `Q`, and Caps Lock changes
  nothing).

  A handler is handed `&mut S` and `&mut Ui<S>`, exactly like a button's
  `on_click`, so it edits the tree rather than only setting a flag.

  This is a filter list and says so. The alternative — a zero-sized
  widget hung off the root — cannot work: keys bubble *upward* from the
  focused widget, so a sibling of the root's other children is on
  nobody's ancestor chain and is never offered anything (#535). The
  ordering is what makes both halves true at once: a focused `TextField`
  types a `q`, and the app's `q` quits everywhere else.
  `crates/nitro-ui/tests/shortcuts.rs` asserts each of those.
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

**The token is an opaque id that is never reused, not the descriptor
number**, and the difference cost a box run to find. The obvious
implementation is the raw fd of our duplicate: unique among live hooks,
and already what the loop's `epoll` set is keyed on, so the two cannot
drift. What it is not is unique over *time*. Closing a descriptor
returns its number to the kernel, which hands out the lowest free one —
so a hook removed and another added in the same turn take the **same
number**, which is precisely what re-arming looks like. The loop keeps a
list of what it has registered so it does not `epoll_ctl` on every
wakeup; keyed on the number, that list said "already registered" about a
descriptor that had been closed (and so silently dropped from the set)
and replaced. The new hook existed, was never in the `epoll` set, and
never fired.

The app that found it was `nitro-files`, which re-arms an inotify watch
on every navigation: it refreshed its listing in the first directory and
in no directory afterwards, with nothing anywhere returning an error.
Nothing in the test suite could see it either, because tests drive
`Ui::run_fd` directly and `run_fd` was correct — only the loop was
wrong. `FdToken` is now a monotonic `u64`, `Ui::hook_fds` hands the loop
the id *and* the descriptor, and
`a_re_armed_fd_hook_gets_a_fresh_token` in `tests/ui.rs` fails against
the old scheme.

The introspection listener joins the same set, and `Socket::serve` runs
after the timers and before the flush. One caveat to the "blocks in
`epoll_wait`" claim above: **connected** introspection clients are not
registered in the set, only the listener is, so while one is attached the
loop clamps its timeout to 10 ms and wakes 100×/s. With nothing connected
— the normal case — the app blocks indefinitely exactly as before. See
`docs/introspection.md`; registering the client streams is M3.

### Frame callbacks, for an app whose input outruns the screen

The loop above assumes the normal shape: something changes, the widget is
marked dirty, the next flush sends it. An app whose *input* arrives
faster than the screen can show it wants the opposite, and `nitro-term`
(M4-A) is the case that made this real — a pty delivers a line every
microsecond and does not care that a compositor exists.

`ui.request_frame()` asks the server for one `Frame` callback carrying
the deadline for the next flip; `ui.on_frame(|s, ui, frame| ..)`
registers the handler, which is handed `&mut S` and `&mut Ui<S>` exactly
like a button's `on_click`. The app absorbs everything into its own model
as it arrives and touches the *tree* only in the handler — so a hundred
thousand lines of output cost one commit per refresh rather than one per
line, each showing the state as it stood at that moment.

**Do not make the callback the only thing that can paint.** The
temptation, once the hook exists, is to move `request_paint` into the
handler so the scene is touched exactly once per `Frame`. `nitro-term`
tried it and **froze its screen**: `request_frame` is one-in-flight, so
painting then depends on the answer arriving, and a server coalescing
flips under load is exactly when it does not. Four frames in twelve
seconds of steady output, with consecutive framebuffer readbacks
byte-identical.

The rule that survives contact is narrower. Use the callback to *pace
work you would otherwise do per input event* — recompute a model, rebuild
a list — and bound the input itself (`nitro-term` reads at most 256 KiB
of pty per turn). "At most one change per refresh" is the **server's**
guarantee, delivered by its flip coalescing, and a client that tries to
enforce it a second time adds a dependency rather than a guarantee.

Three properties make it safe to build on:

* **One request, one answer.** Asking twice before the answer arrives
  sends one request; two callbacks per frame is exactly the free-running
  loop this exists to avoid. `ui.frame_pending()` is the flag.
* **Asking is conditional, so idle stays free.** An app that requests a
  frame only when its model is dirty — which is what `nitro-term`'s
  `request_frame_if_dirty` does — has nothing outstanding when nothing is
  happening, and blocks in `epoll_wait` with no timer. That is the same
  idle contract every other app has, kept by the loudest client on the
  machine.
* **An answer is not guaranteed promptly.** One request is outstanding at
  a time and the server answers after a flip, so under load the gap
  between callbacks is the *flip* rate, not the refresh rate. Anything
  that must happen while output is flowing cannot be parked behind it.
* **It is a handler list, not a widget**, for the same reason `on_key`
  and `on_shell` are: a frame deadline is news about the *output*, with
  no position to hit-test and no focus to follow. Every handler sees
  every frame; there is nothing to consume.

In a test, `Harness::frame()` delivers one, so a test can choose the
moment — the whole point of pacing is that many changes happen between
two frames, and a test that could not say when a frame lands could not
assert that.

### Long work off the loop

The frame callback above paces work that arrives *too fast*. The
opposite problem is work that takes *too long*, and it arrived with
`nitro-files` (M4-D): reading a directory is a `read_dir` plus a `stat`
each, against a disk, an NFS mount or an automounter that has gone to
sleep. `/usr/bin` is two thousand entries; a directory on a sleeping
mount is a call that returns in thirty seconds. Done between two
`epoll_wait`s it does not make the window slow, it **freezes** it: no
repaint, no keystroke, no answer to the server, for as long as the kernel
takes.

The toolkit has **no thread integration**, and it does not need one. A
descriptor is already something the loop waits on, so the whole pattern
is a worker thread, a pipe and `Ui::add_fd`:

```text
  worker thread                          app loop
  ─────────────                          ────────
  result = the long thing
  tx.send(result)          ── channel ──►  take()        the payload
  write(pipe_w, [1])       ── pipe ─────►  epoll wakeup  the fact there is one
```

**A channel *and* a pipe, because neither half can do the other's job.**
A `std::sync::mpsc` channel alone cannot wake a process sleeping in
`epoll_wait`, so the result would sit there until the user happened to
move the mouse. A pipe alone would mean pushing the payload through a
descriptor, which means choosing a serialisation — and a serialisation
between two threads of one process is work done for nobody. So the
channel carries the data and the pipe is a **doorbell**: one byte,
written *after* the result is in the channel, so a wakeup never arrives
ahead of its payload.

The hook registered with `add_fd` runs from exactly where a wire message
is handled — between events, with the tree settled, holding the same
`&mut S` and `&mut Ui<S>` every callback gets — so the code that applies
the result is ordinary app code and not a special case.
`nitro-files`'s `dir::Scan` is the worked example: it reads *and sorts*
on the thread (sorting fifty thousand rows is the same kind of work as
reading them), remembers which directory it was reading so a result for a
directory the user has already left can be dropped, and keeps its read
end non-blocking so a spurious wakeup costs an `EAGAIN` rather than a
stall. Measured on a dev box: a 50 000-entry directory is listed in
0.18 s and the app answers its introspection socket throughout, which is
the only observable difference that matters.

**A hook whose descriptor stays readable must be removed.** This is the
hazard of the pattern, and it is not optional. The app loop's `epoll` is
**level-triggered**, so a descriptor that remains readable dispatches its
hook on every turn of the loop, for ever — an app at 100 % CPU with
nothing on screen, which is exactly what the "idle costs nothing"
contract forbids. A pipe holding an unread byte is readable for ever, so
`nitro-files` calls `Ui::remove_fd` the moment a scan's result is taken.
An exited process's pidfd is readable for ever too, which is the same
hazard one crate over: `nitro-launcher`'s reaping hook removes the token
of every child it reaps (`crates/nitro-launcher/src/spawn.rs`
§Reaping). And a descriptor that is *kept* — `nitro-files`'s inotify
watch lives as long as the directory is on screen — must have its events
**drained** on every wakeup for the same reason, even when the app does
not care what they say.

The rule, then, is one sentence: a hook is registered for as long as its
descriptor can go quiet, and removed the moment it cannot.

### Two window properties

`ui.set_window_title(s)` and `ui.set_window_limits(min, max)` are queued
mutations like everything else, and both **drop an unchanged value**.
That matters more than it sounds for the title: a shell whose prompt
carries an OSC sequence sets the same string on every command, and a
terminal that forwarded each one would commit — and make the server
relist its windows — once per command the user runs. Limits set before
the window exists ride its first commit, for the reason a shell surface's
anchor does.

### Resizes, for an app whose content has its own units

`ui.on_resize(|s, ui, size| ..)` is offered every `Configure` that
actually changed the window's size, after it has been applied — so the
handler sees the new `window_size` and a tree already marked for layout.

Laying the tree out again is the framework's job and needs no hook. This
one exists for the *other* thing a resize can mean: an app whose content
is measured in its own units has to recompute how much content fits, and
may have to tell something outside the process. `nitro-term` turns the
new pixel size into a column and row count, reflows its grid, and sends
`TIOCSWINSZ` so the child gets `SIGWINCH` — none of which is expressible
as a `measure`, because `measure` answers "how big would you like to be"
and a resize says "you are this big now".

A handler list rather than a widget hook, for the reason `on_frame` and
`on_shell` are: a new size is news about the *window*, with no position
to hit-test and no focus to follow. A `Configure` that only moved the
window or changed its scale fires nothing, so an app does not reflow its
content because the user dragged its titlebar.

In a test, `Harness::configure(size)` goes through the ordinary dispatch
path, so it fires the hook — which is what lets a resize test assert that
*a resize* works rather than that the app's resize function works.

## Shell surfaces

A bar, a dock, a launcher and a wallpaper are `nitro-ui` apps like any
other, with one difference the toolkit has to express: they connect to
`shell.sock` rather than `wire.sock`, and that connection may name its
own layer, stick to its output's edges and reserve screen space. The
model is `docs/shell.md`; `crate::shell` is the client's half of it, and
`crates/nitro-bar` is the consumer.

`App::shell(name)` connects to the privileged socket and
`App::surface(Surface::bar(32))` says what kind of window to open.
Three decisions are worth stating.

**The surface is described up front, not configured afterwards.** The
layer, the flags, the anchor and the zone all ride the **same commit** as
the `CreateWindow`. That is not tidiness: the server buffers `SetAnchor`
and `SetExclusiveZone` to the sender's commit precisely so a bar can
create and anchor in one transaction — it is the M3-B hardware probe's
regression, where an anchor answered on receipt named a window `Commit`
had not created yet — and a bar that anchored a frame later would paint
once at its placeholder size and then jump.

**`App::shell` fails rather than falling back.** A bar that silently
downgraded to the ordinary socket would come up looking right and be
killed by the first shell op it sent, which is a far harder failure to
read than "could not connect". `Ui::is_shell` is the check for code that
wants to degrade deliberately instead.

**Shell events are a separate hook from widget events.** `Ui::on_shell`
registers a handler for `ShellEvent` — the window list, output hotplug,
hotkeys — offered `&mut S` and `&mut Ui<S>` exactly like a button's
`on_click`, so a bar's task list is ordinary tree code. It is a list
rather than a widget hung off the root for the same reason `on_key` is:
a `WindowInfo` is news about *somebody else's* window, with no position
to hit-test and no focus to follow. Unlike a key, every handler sees
every event — there is nothing to consume, and a "handled" answer would
only let the first handler silently starve the second.

The questions (`window_list`, `outputs`) and the ops on other clients'
windows (`focus_window`, `close_window`, `set_window_state_for`,
`bind_key`) are sent **immediately** rather than queued as mutations,
because they are not mutations: queuing them would make the next flush
commit, and a shell that merely asked a question would break the idle
contract. Nothing here polls — `WindowList` and `Outputs` subscribe — so
a bar with nothing changing sits in `epoll_wait` like any other app.

**Hiding is a mutation, and that is what makes a launcher cheap.**
`Ui::set_window_visible` and `Ui::grab_keyboard` are *queued* rather than
sent at once, unlike the questions above, because they are exactly the
opposite kind of thing: they change the window's state, and the two have
to arrive **together**. The server drops a keyboard grab on a window that
is not showing (`docs/shell.md`), so a launcher that un-hid itself in one
transaction and grabbed in the next would have the grab taken away again
between them.

That pairing is what lets `crates/nitro-launcher` (M3-D) be built once
and hidden rather than rebuilt: showing it is one `SetVisible` plus the
grab in a single commit, and hiding it is one `SetVisible` that releases
the grab and any exclusive zone with it — no second message to forget.
An unchanged `set_window_visible` sends nothing, so the flag is cached
next to the paint-slot cache in `wire.rs` rather than in `Ui`, where it
would have been a fourth `bool` and a second copy of the same fact.

The toolkit also sends `SetAppId` for every app now, from the name it was
constructed with, in the window's own first commit. The app id is what a
window list names a *program* by (the title names the document), so a
window that existed without one would appear in a bar as an anonymous
row — and the bar could not recognise and skip its own window, which is
exactly the bug the first version of it had.

**A click in a `NO_FOCUS` window acts without focusing.** Every shell
surface is `NO_FOCUS` — a bar, a dock, a launcher overlay, a wallpaper —
and the server will never route a key to one. Toolkit focus inside such
a window therefore buys nothing and costs something visible: the clicked
button keeps its focus ring, and `hey nitro-bar list` reported a
window-list entry as `focused,hovered` after a click, which is a lie
about a surface that cannot be focused. So `EventCx::request_focus` — how
a widget asks for focus from inside its own event handling, and the
reason clicking a button focuses it at all — is a **no-op** when the
window's flags carry `NO_FOCUS`. The click still activates the widget;
only the focus move is dropped. `Ui::click_takes_focus` is the predicate,
and widget code needs no `if` for it.

`Ui::focus` itself is *not* gated, deliberately: the launcher is
`NO_FOCUS` and still focuses its query field, because it reads the
keyboard through a grab rather than through focus, and a toolkit that
refused would have broken it. The rule is about focus a *click* takes on
the user's behalf, not about focus an app places on purpose.

## Colours come from roles

A widget never writes a colour down. It asks for a **role** — a name for
a *meaning* (`Accent`, `TextDim`, `Surface`) — and the server, which owns
the desktop's palette, decides what colour that is.

```rust,ignore
fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
    cx.fill_rect(0, cx.bounds, cx.color(ColorRole::Surface));
}
```

`Theme` is still there and still what the built-in widgets read, but it
is now a **view** on the palette: `Theme::from_palette` maps roles onto
its colour fields, and its non-colour fields (font, radius, paddings)
stay app state that a scheme switch leaves alone. So a `Button` needs no
changes to follow the user's scheme, and `ui.theme().accent` is still the
right thing to read inside one.

What is *not* right is `.color(ui.theme().text_disabled)` on a label.
That reads the palette **once, at build time**, and freezes the answer —
so the label keeps its light-scheme grey for ever. Use
`.color_role(ColorRole::TextDim)`, which resolves at paint time.

A palette push costs **one commit**: `Ui::set_palette` marks every widget
and the next flush sends the lot as one transaction. An app that was idle
before a switch is idle again after it, and an unchanged palette is
dropped without marking anything. `ui.on_theme(..)` is for a widget that
caches something *derived* from a colour — `nitro-term` rebuilds its
per-cell table there.

The rule is enforced, not remembered: `deploy/lint-colors.sh` fails the
build on a `Color::rgb(` or a bare `0xRRGGBB` outside a short allow-list,
and it runs from `just clippy` and the default recipe. **If a colour you
need has no role, add one** to `nitro_core::palette` — do not work around
it. See `docs/theme.md` for the role table, the `server.conf` keys and
how to add one.

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
* Colours come from `cx.color(ColorRole::X)` or the theme, never from a
  literal — see above, and `deploy/lint-colors.sh`, which fails the build
  for one.
* If it wants pointer events, it must paint something.

### Slots that are content rather than parts

The paragraphs above describe a widget whose slots are its **parts** —
slot 0 the background, slot 1 the label — and for those, "a slot the
paint did not emit is destroyed" is exactly right: a button that stops
drawing its focus ring wants the ring gone.

`nitro-term` (M4-A) is the first widget whose slots are its **content**.
It gives every same-style run on every row a slot of its own, so the
per-slot cache does the diffing and a run whose bytes did not change
costs nothing. That needed two things the toolkit did not have, and both
are general rather than terminal-shaped:

* **`cx.keep(slot)`** — a third answer beside emit and omit:
  *unchanged*. It marks the slot used without diffing it, so a widget can
  skip re-deriving a part of itself it knows did not change. A terminal
  asks `Grid::row_dirty` and calls `keep` for a clean row, which is
  forty-nine rows out of fifty on a keystroke. Without it the choice
  would be to re-derive every row's runs on every paint (a string
  comparison per run, in order to send nothing) or to omit them, which
  deletes the screen.
* **The slot index is a `Slot` (`u16`), not a `u8`.** Five is plenty for
  a widget's parts; a 200-column row can hold more than 256 runs.

A widget defined **outside `nitro-ui`** cannot write `impl WidgetMut<'_,
W, S>` — an inherent `impl` has to live where the type does. Declare a
local trait carrying the setters and implement it for `WidgetMut<'_, W,
S>` instead (`nitro_term::widget::TermGridMut` is the worked example).
The contract is unchanged, which is the point: `WidgetMut`'s `DerefMut`
gives the trait methods their `&mut W`, and each still calls
`request_paint` or `request_layout`, so invalidation still cannot be
forgotten. The caller pays one `use`.

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

`Harness::shell(name, state, surface, size, build)` is the same thing on
the **shell** socket, for a bar or a launcher: a shell surface cannot be
tested over the ordinary socket at all, since the first shell op would be
a fatal protocol error, so this is not a convenience but the only way in.

Timers are part of what the harness runs, because they are part of what
the app loop runs: `settle` and `assert_idle` both fire due timers, so a
test sees the tree the real app has rather than one whose clock never
ticks. `advance_timers(ms)` fast-forwards every pending deadline (it
saturates rather than panicking on an `Instant` underflow, so an
over-large fast-forward means "fire everything"). That
last one exists because a timer's deadline is an `Instant` from the
**monotonic** clock, which no amount of faking an app's *wall* clock
moves — so a test of a minute-aligned tick would otherwise have to wait a
real minute to see it. Shifting the deadlines preserves the timers'
relative order and fires exactly the ones the elapsed time would have.

`Harness::frame()` delivers one frame callback, and `Harness::parts()`
hands out `(&mut Ui<S>, &mut S)` together. The second one exists because
an app's own loop functions take exactly that pair — it is the signature
of every callback the toolkit hands out — so a test that wants to call
one cannot get there through `ui()` and `state_mut()` separately. Without
it a test ends up moving the state out and back around every call, which
is noise at best and, for a state that owns a descriptor, a different
object at worst.

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
| binary | **522 112 bytes** (522 KB) | 444 608 (444 KB) |
| RSS / HWM | **2 576 kB** | 2 500 kB |
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
for the reason if it was not. It is **+77 504 bytes of binary (+17 %)
and +76 kB of RSS**, and the honest attribution is that essentially all
of it is the socket rather than the widgets. Building the same example
three ways:

| build | binary | delta |
|---|---|---|
| this tree | 522 112 | — |
| `App::run` never binding the socket (`introspect(false)` hard-coded) | 514 952 | −7 160 |
| the `introspect` and `shot` modules removed from the crate | 453 600 | −68 512 |

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

The RSS delta (+76 kB, roughly the binary growth) is entirely
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
  to its siblings. An overflow that no child will absorb — the usual
  case now that `shrink_floor` defaults to `Content` — is likewise left
  as overflow rather than iterated on, and the leftover `MainAlign`
  spends is clamped at zero so a negative one cannot be distributed.
* **`shrink_floor` is one field, not one per axis.** It applies to the
  parent's main axis, so a widget that opted out for a horizontal reason
  (`TextField`, `Slider`) is also unfloored vertically. `min_height` is
  the defence; splitting the flag per axis is the thorough fix and is
  not done.
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
* **An `Image` holds two server-side buffers across a replacement.**
  `set_pixels` cannot release the old one itself (a setter has no
  connection), so the release is deferred to the next paint. A widget
  that replaces its pixels and is never painted again keeps one buffer
  alive until it is dropped.
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
  group, so its child is laid out at full height and a list of ten
  thousand rows put *inside a `Scroll`* still costs ten thousand widgets.
  **`List` is the answer to that** and is argued above: it holds
  `visible + 2` rows in the scene however long its model is, a scroll
  inside the spare rows is one `SetTransform`, a re-anchor re-emits only
  the rows that changed, and a selection move is two `SetFill`s. What it
  costs is that its rows are *data it owns* rather than widgets — a
  `Vec<Row>`, three small allocations a row — and that a row therefore
  has no per-row callbacks or per-row state, only an index. A screenful
  of arbitrary widgets is still a screenful of widgets; `List` virtualises
  rows, not the tree.
* **A pointer event carries no modifier mask.** `Event::PointerDown` has
  a position and a button and nothing else, so Ctrl-click and
  Shift-click cannot be distinguished from a plain click. A `List`'s
  multi-selection is therefore **keyboard-only** (`Ctrl+Space`, Shift and
  the arrows), and a pointer selects exactly one row. Fixing it means
  carrying the mask on every pointer event on the wire, which is a
  protocol change made for one widget's two extra gestures; it is worth
  doing when a second widget wants it.
* **Scrolling under a stationary pointer does not re-hover.** Hit testing
  and `window_bounds` *do* account for a content transform — a scrolled
  button is clicked and reported where it is drawn — but the hover chain
  is only recomputed on a pointer event. Scroll a list under a still
  pointer and the widget that was under it stays hovered until the
  pointer moves. Re-running the hover walk after a transform change is
  the fix and it is M3; it needs `&mut S` to deliver the enter/leave
  events, which `set_content_transform` does not have.
* **Only the translation of a content transform is honoured** by hit
  testing and `window_bounds` (`e` and `f`). M2 produces nothing else —
  `Scroll` is the only caller — but a widget that rotated or scaled its
  children would be hit-tested as though it had not.
* **`Button::on_click` is `Fn`, not `FnMut`** — it is taken out of the
  button for the call (same trick as the arena), and a `FnMut` would need
  either a second take-out or interior mutability. `&mut S` is where the
  mutation belongs anyway.

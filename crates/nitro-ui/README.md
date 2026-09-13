# nitro-ui

The toolkit: a **retained widget tree in an arena** that maps widgets to
scene nodes and sends only mutations for what changed. This is the crate
an app is written against.

The architecture and the reasoning behind every decision below are in
[`docs/ui.md`](../../docs/ui.md).

```rust,no_run
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::widgets::{Label, button, column, label};
use nitro_ui::{App, Ui};

struct State {
    clicks: u32,
}

fn main() -> Result<(), nitro_ui::Error> {
    App::new("counter")?.run(State { clicks: 0 }, |ui: &mut Ui<State>| {
        let count = ui.build(label("0"));
        let root = ui.build(column().gap(8.0).padding(12.0).child(
            button("Click me").on_click(move |s: &mut State, ui: &mut Ui<State>| {
                s.clicks += 1;
                let text = s.clicks.to_string();
                ui.widget_mut::<Label>(count).unwrap().set_text(text);
            }),
        ));
        ui.attach(root, count).unwrap();
        root
    })
}
```

## Shape

* Widgets live in an **arena indexed by `WidgetId`** (a generational
  index). No `Rc`, no parent pointers, no interior mutability. A stale id
  is `Error::StaleWidget`, never a panic.
* A widget is **taken out of its slot** while its own method runs, so a
  callback gets a full `&mut Ui<S>` and can mutate *other* widgets.
  Re-entering the same widget is `Error::Busy`.
* **`WidgetMut<'_, W, S>` is the only way to mutate a widget.** Its
  setters mark layout or paint dirty, so invalidation cannot be
  forgotten.
* Callbacks take **`&mut S` and `&mut Ui<S>`** — the app's state is a
  plain struct, routed by widget id.
* **Builders build once**: `column().gap(8.0).child(label("Hi"))` reads
  declaratively and materialises into the arena exactly once.

## Passes

`Ui::flush` runs `TREE` → `LAYOUT` → `PAINT` and sends **one** `Commit`.
Each pass is a tree walk driven by dirty flags, and each skips a clean
subtree without walking it. **Nothing dirty means no commit**, so an idle
app puts zero bytes on the socket.

Each widget owns one scene `Group` positioned by its bounds, so a moved
widget costs one `SetBounds` and no repaint of its content. Painting goes
into numbered slots that diff against the last values sent, so a repaint
producing the same values costs nothing.

`introspect` is the fifth pass: `Ui::introspect` fills a `Vec<Node>` with
id, role, name/value/actions and bounds. The socket that serves it is the
next task.

## Widgets

`Flex` (`column()`, `row()`), `Panel`, `Label`, `Button`, `Spacer`. Each
has a builder and a `WidgetMut` impl with setters. Colours, sizes, radius
and padding come from a [`Theme`], overridable at `App` level.

Layout is a flex subset — direction, main/cross alignment, gap, padding,
margin, `Auto`/`Px`/`Percent` sizes with min/max clamps, grow and shrink.
The solver in `layout.rs` is pure functions over styles and measured
sizes, unit-tested without a server.

**Text measurement is synchronous** in M2: a label asks the server to
shape its string and waits, because the server owns the fonts. The result
is cached by `(text, style, max_width)`, so a settled UI does no round
trips. Reasoning and the async path in `docs/ui.md`.

## Testing

`nitro_ui::test::Harness` (feature `test-support`) starts a real server on
the fake backend in-process, runs the `Ui` on the test thread and pumps
it. It injects synthetic input through the server's fake `InputSource`,
takes screenshots for pixel assertions, and taps the wire so a test can
assert that a text change cost exactly one commit and one `SetText`.

`tests/ui.rs` covers layout with gaps and real glyphs, click → callback →
`WidgetMut` → one commit, `Configure` relayout, zero traffic while idle,
stale/busy/wrong-type errors, Tab order, hover and press visuals, hit
testing, scroll routing, the introspection tree, fd hooks and timers.

## Example

`examples/hello_dialog.rs` — title, message, Cancel/OK, `q` quits, in 31
lines of app code. Release, stripped, against a fake server: **444 KB**
binary, **2.4 MB** RSS, one thread, **zero context switches over 5 s
idle**, and `ldd` shows only libc. No font library, no rasterizer, no
compositor: a label is a string on the wire.

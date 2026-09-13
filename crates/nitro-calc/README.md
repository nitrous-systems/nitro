# nitro-calc

A calculator: **the first application written against `nitro-ui`**, and
M2's exit criterion. It is here to answer one question with a running
program rather than a claim — *what does an app on nitro actually look
like?* — and to be driven from a shell, because goal 5 of `DESIGN.md` is
that every nitro app is scriptable with one mechanism.

```text
┌───────────────────────┐
│                 7 + 8 │  history  (Label, named `history`)
│                    15 │  display  (Label, named `display`)
├─────┬─────┬─────┬─────┤
│  C  │  ⌫  │  ±  │  ÷  │
│  7  │  8  │  9  │  ×  │
│  4  │  5  │  6  │  −  │
│  1  │  2  │  3  │  +  │
│  0  │  .  │  =        │
└─────┴─────┴─────┴─────┘
```

Run it against a server (`just fake` in another terminal):

```console
$ cargo run -p nitro-calc
```

`q` quits. The keyboard mirrors the buttons exactly — `0-9 . + - * /
=`, `Enter` for `=`, `Backspace` for `⌫`, `Escape` for `C` — because both
go through the same `engine::Key`. A key and a button that disagreed
would be two implementations of the same calculator.

## Driving it with `hey`

This is the agentic-use demonstration. Nothing below cooperates with the
app's code: `hey` is a separate binary that has never linked
`nitro-calc`, talking to the introspection socket every nitro app opens.

```console
$ hey                                          # which apps are up
nitro-calc    12873

$ hey nitro-calc do window/7 click
$ hey nitro-calc do window/plus click
$ hey nitro-calc do window/8 click
$ hey nitro-calc do window/equals click
$ hey nitro-calc get window/display value
15
$ hey nitro-calc get window/history value
7 + 8 =

$ hey nitro-calc shot -o tmp/calc.png          # a PNG the size of the window
$ hey nitro-calc list                          # the whole tree, one line per widget
$ hey nitro-calc watch '*'                     # stream every change until Ctrl-C
```

Those commands are not decoration: `crates/nitro-hey/tests/calc_end_to_end.rs`
runs them against a real server with the real `hey` binary as a child
process, and asserts the answers above. A README that went stale would
fail the build.

Every button carries a `.name()`, which is what gives it a path. The
names are the token the button carries — `7`, `plus`, `equals`, `clear`,
`backspace`, `negate`, `point`, `times`, `minus`, `divide` — because
`window/+` is not a path anyone wants to quote in a shell.

`window/7` addresses the button wherever the layout puts it. Its
canonical path goes through the row it lives in
(`window/container[1]/7`, which is what `hey … get window/7 path`
reports), but a name that is unique in the subtree resolves from above
it — so a script names the *widget* and a rearrangement of the keypad
cannot break it. An ambiguous name is refused rather than guessed.

## Shape

| file | what is in it |
|---|---|
| `src/engine.rs` | the state machine and the formatter. No UI, no toolkit, 22 unit tests. |
| `src/lib.rs` | the widget tree, the state struct, one callback per button. |
| `src/main.rs` | three lines: connect and run. |

`engine.rs` is where a calculator's bugs live, so it is a pure module
that has never heard of a widget: `Engine::press(Key)` in, `display()`
and `history()` out. The UI half is then small enough to read in one
sitting — a `Calc` state struct, a `[[(&str, &str, Key); 4]; 5]` table
for the keypad, and a closure per button that presses a key and pushes
the result into two labels.

**Left to right, like a desk calculator.** `2 + 3 × 4 =` is **20**, not
14: each operator key completes the operation already pending before
starting the next. That is what a four-function calculator does, it is
what a keypad with no parentheses implies, and it is a decision rather
than a missing feature — precedence without brackets is a calculator
that cannot express what it evaluates.

**The formatter is written, not borrowed.** `{}` on an `f64` prints the
shortest string that round-trips, which is how `0.1 + 0.2` becomes
`0.30000000000000004` on a calculator whose user is entitled to `0.3`.
So a value is rounded **once**, to 15 significant digits (the most an
`f64` always carries), and the fixed-point form is assembled by moving
the point rather than formatted a second time — rounding twice is how a
digit that was correct stops being so. Outside `1e-6 … 1e15` it switches
to exponential rather than printing a screenful of zeros.

**Division by zero and overflow are an `Error` state**, not `inf` or
`NaN` on the screen, and only `C` (or `⌫`) leaves it. A calculator that
went on computing with a number nobody can see would be worse than one
that stops.

## What it costs

One keypress changes one thing — the display's string — and what that
costs on the wire is **one `SetText` and the `Commit` that carries it**.
No button repaints, no node is created, and the history line is silent
because its text did not change (`WidgetMut::set_text` returns early on a
no-op). `tests/calc.rs::one_keypress_is_one_set_text` asserts it from
outside by counting mutations, because a cost claim nothing checks stops
being true.

A settled calculator sends **nothing at all** — `nothing_is_sent_while_it_sits_there`
watches the socket for 200 ms after a sum. Sizes, RSS and the
keypress-to-photon number measured on the test box are in
[`docs/budget.md`](../../docs/budget.md).

## Testing

`tests/calc.rs` drives the tree the binary builds, through a real server
on the fake backend: clicks travel evdev code → server hit test → wire →
widget, and keys go through the real keymap. It covers the click path,
the key path, the two mixing freely, `C`, error recovery, the
fifteen-digit cap, a `Configure` resize stretching the keypad, real ink
on real pixels, Tab order, the mutation count and the idle silence.

`crates/nitro-hey/tests/calc_end_to_end.rs` does the same through the
socket with the real `hey` binary, asserting the commands in this README.

The engine's own tests are in `src/engine.rs` and need nothing: no
server, no window, no toolkit.

## A note for the next app

The root widget implements `Widget::event` itself, rather than attaching
a zero-sized handler as a child of a `column()` root the way
`docs/ui.md` and `examples/hello_dialog.rs` suggest. That pattern does
not work — keys bubble *upward*, so a sibling of the root's children is
never on the path — and the cost of working around it here is a
hand-written `measure` that reimplements `Flex`'s. Issue **#535**.

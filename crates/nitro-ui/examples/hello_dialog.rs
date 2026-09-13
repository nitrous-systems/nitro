//! A small dialog: a title, a line of text, and Cancel/OK buttons.
//!
//! This is the M2 size-and-RSS benchmark as much as it is a demo: the app
//! code below the imports is under thirty lines, and the binary it builds
//! carries no font library, no rasterizer and no compositor — a label is
//! a string on the wire, and the server owns the glyphs.
//!
//! * **OK** toggles the message label between two strings.
//! * **Cancel** and **q** quit.
//! * Tab and Shift-Tab walk the two buttons; Space or Enter activates the
//!   focused one.
//!
//! The three interesting widgets carry a `.name()`, which is what gives
//! them a stable path on the introspection socket — so the same dialog
//! is drivable from a shell without changing a line of it:
//!
//! ```text
//! hey hello-dialog do window/ok click
//! hey hello-dialog get window/message value
//! ```
//!
//! Run it against a server (`just fake` in one terminal):
//!
//! ```text
//! cargo run -p nitro-ui --example hello_dialog
//! ```

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::{Event, Handled, key};
use nitro_ui::widgets::{Label, button, column, label, row, spacer};
use nitro_ui::{App, Built, Error, Ui, Widget};

/// Everything the dialog remembers.
struct State {
    ok: bool,
}

fn main() -> Result<(), Error> {
    App::new("hello-dialog")?.run(State { ok: false }, |ui: &mut Ui<State>| {
        let message = ui.build(label("Nothing has happened yet.").name("message"));
        let ok = ui.build(button("OK").name("ok").on_click(
            move |s: &mut State, ui: &mut Ui<State>| {
                s.ok = !s.ok;
                let text = if s.ok { "OK pressed." } else { "Toggled back." };
                ui.widget_mut::<Label>(message).unwrap().set_text(text);
            },
        ));
        let root = ui.build(
            column()
                .gap(12.0)
                .padding(16.0)
                .child(label("Hello, nitro").size(20.0).weight(700))
                .child(label("A dialog in under thirty lines.")),
        );
        let buttons = ui.build(
            // The spacer is what pushes the pair to the right edge.
            row().gap(8.0).child(spacer()).child(
                button("Cancel")
                    .name("cancel")
                    .on_click(|_: &mut State, ui: &mut Ui<State>| ui.quit()),
            ),
        );
        ui.attach(buttons, ok).unwrap();
        ui.attach(root, message).unwrap();
        ui.attach(root, buttons).unwrap();
        let quit = ui.build(Built::new(QuitOnQ));
        ui.attach(root, quit).unwrap();
        root
    })
}

/// A zero-size widget that turns `q` into a quit.
///
/// A key that no focused widget consumed bubbles to the root, so an
/// invisible child of the root is all an app needs for a global
/// shortcut — no event filter, no hook list.
struct QuitOnQ;

impl Widget<State> for QuitOnQ {
    fn event(&mut self, cx: &mut nitro_ui::EventCx<'_, State>, ev: &Event) -> Handled {
        match ev {
            Event::Text { text } if text == "q" => {
                cx.ui.quit();
                Handled::Yes
            }
            Event::KeyDown(k) if k.keycode == key::ESC => {
                cx.ui.quit();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

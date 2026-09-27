//! A button that opens the toolkit's file picker, and a label that shows
//! what came back.
//!
//! * **Open…** picks one or more images (`image/*`, or "All files").
//! * **Folder…** picks a directory.
//! * **Save…** picks a path to save `untitled.txt` to.
//!
//! Run it against a server (`just fake` in one terminal):
//!
//! ```text
//! cargo run -p nitro-ui --example pick
//! ```

use std::path::PathBuf;

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::widgets::{Label, button, column, label, row};
use nitro_ui::{App, Error, FilePicker, Ui, WidgetId};

struct State {
    result: Option<WidgetId>,
}

fn show(s: &mut State, ui: &mut Ui<State>, picked: Option<Vec<PathBuf>>) {
    let text = match picked {
        None => "Cancelled.".to_owned(),
        Some(paths) => paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    if let Some(id) = s.result
        && let Ok(mut l) = ui.widget_mut::<Label>(id)
    {
        l.set_text(text);
    }
}

fn open(ui: &mut Ui<State>, picker: FilePicker<State>) {
    if let Err(e) = picker.on_done(show).open_in(ui) {
        eprintln!("pick: {e}");
    }
}

fn main() -> Result<(), Error> {
    App::new("pick")?.run(State { result: None }, |ui: &mut Ui<State>| {
        let result = ui.build(label("Nothing picked yet.").name("result"));
        let buttons =
            ui.build(
                row()
                    .gap(8.0)
                    .child(button("Open…").name("open").on_click(
                        |_s: &mut State, ui: &mut Ui<State>| {
                            open(ui, FilePicker::open().multiple(true).mime(["image/*"]));
                        },
                    ))
                    .child(button("Folder…").name("folder").on_click(
                        |_s: &mut State, ui: &mut Ui<State>| open(ui, FilePicker::folder()),
                    ))
                    .child(button("Save…").name("save").on_click(
                        |_s: &mut State, ui: &mut Ui<State>| {
                            open(ui, FilePicker::save("untitled.txt"));
                        },
                    )),
            );
        let root = ui.build(column().gap(12.0).padding(16.0).width(420.0));
        ui.attach(root, buttons).expect("fresh ids");
        ui.attach(root, result).expect("fresh ids");
        ui.defer(move |s: &mut State, _ui: &mut Ui<State>| s.result = Some(result));
        root
    })
}

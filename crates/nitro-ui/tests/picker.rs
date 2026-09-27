//! The file picker: `FilePicker` opens a second window laid out like
//! `nitro-files`, and hands `on_done` exactly one answer.
//!
//! Every test uses a scratch directory, a fixture places list and the
//! built-in MIME table (`globs(vec![])`), so nothing depends on the
//! developer's `$HOME` or `/usr/share/mime`.

use std::path::{Path, PathBuf};

use nitro_core::Size;
use nitro_fs::places::{Place, Section};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::introspect;
use nitro_ui::picker::Picker;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Button, Label, button, column};
use nitro_ui::{FilePicker, SidebarRow, Ui, WidgetId, WindowId};

#[derive(Default)]
struct St {
    answers: Vec<Option<Vec<PathBuf>>>,
    main_clicks: u32,
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("nitro-picker-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("sub")).expect("scratch");
    for f in ["a.txt", "b.png", "c.png", ".hidden.txt"] {
        std::fs::write(root.join(f), f).expect("fixture file");
    }
    std::fs::write(root.join("sub").join("inner.txt"), "x").expect("fixture file");
    root
}

fn main_tree(ui: &mut Ui<St>) -> WidgetId {
    ui.build(
        column().padding(8.0).child(
            button("Main")
                .name("main_ok")
                .on_click(|s: &mut St, _ui: &mut Ui<St>| s.main_clicks += 1),
        ),
    )
}

fn fixture_places(dir: &Path) -> Vec<Place> {
    vec![
        Place {
            key: "fix",
            label: "Fixture".to_owned(),
            icon: "house",
            path: dir.to_path_buf(),
            section: Section::Places,
        },
        Place {
            key: "sub",
            label: "Sub".to_owned(),
            icon: "folder-fill",
            path: dir.join("sub"),
            section: Section::Places,
        },
        Place {
            key: "trash",
            label: "Trash".to_owned(),
            icon: "trash3",
            path: dir.join("sub"),
            section: Section::System,
        },
    ]
}

/// Open `picker` over a fresh main window, in `dir`.
fn open(dir: &Path, picker: FilePicker<St>) -> (Harness<St>, WindowId) {
    let mut h = Harness::sized("picker", St::default(), Size::new(240.0, 160.0), main_tree);
    let win = picker
        .start_dir(dir)
        .places(fixture_places(dir))
        .globs(Vec::new())
        .size(Size::new(640.0, 400.0))
        .on_done(|s: &mut St, _ui: &mut Ui<St>, r| s.answers.push(r))
        .open_in(h.ui())
        .expect("open the picker");
    h.settle();
    (h, win)
}

fn named(h: &mut Harness<St>, name: &str) -> WidgetId {
    let path = format!("window[1]/{name}");
    introspect::resolve(h.ui(), &path).unwrap_or_else(|| panic!("no widget at {path}"))
}

fn shown(h: &mut Harness<St>, win: WindowId) -> Vec<String> {
    let root = h.ui().root_of(win).expect("the dialog's root");
    h.widget::<Picker<St>>(root)
        .shown()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn cwd(h: &mut Harness<St>, win: WindowId) -> PathBuf {
    let root = h.ui().root_of(win).expect("the dialog's root");
    h.widget::<Picker<St>>(root)
        .cwd()
        .expect("the model")
        .to_path_buf()
}

fn act(h: &mut Harness<St>, id: WidgetId, action: &str, arg: Option<&str>) {
    let (ui, st) = h.parts();
    ui.action(st, id, action, arg).expect("action");
    h.settle();
}

/// The footer sits below the harness's 320×240 output, where a pointer
/// click cannot reach it, so its buttons are driven by their `click`
/// action — the same callback a real click runs (`Ui::action`).
fn status(h: &mut Harness<St>) -> String {
    let id = named(h, "picker_status");
    h.widget::<Label>(id).text().to_owned()
}

#[test]
fn open_lists_dirs_first_and_activating_a_file_answers_it() {
    let dir = scratch("open");
    let (mut h, win) = open(&dir, FilePicker::open());
    assert_eq!(h.ui().window_title_of(win), "Open File");
    assert_eq!(shown(&mut h, win), ["sub", "a.txt", "b.png", "c.png"]);
    assert_eq!(status(&mut h), "4 items");
    // The cursor row counts as picked, so Open is live in a non-empty
    // directory and dead in an empty one.
    let ok = named(&mut h, "picker_ok");
    assert!(h.widget::<Button<St>>(ok).is_enabled());

    let list = named(&mut h, "picker_list");
    act(&mut h, list, "activate", Some("2"));
    assert_eq!(h.state().answers, [Some(vec![dir.join("b.png")])]);
    assert!(!h.ui().has_window(win), "the dialog closed");
    assert!(!h.ui().should_quit(), "the app did not");
    h.quit();
}

#[test]
fn activating_a_directory_navigates_and_back_and_up_return() {
    let dir = scratch("nav");
    let (mut h, win) = open(&dir, FilePicker::open());
    let back = named(&mut h, "picker_back");
    assert!(!h.widget::<Button<St>>(back).is_enabled());
    let list = named(&mut h, "picker_list");
    act(&mut h, list, "activate", Some("0"));
    assert_eq!(cwd(&mut h, win), dir.join("sub"));
    assert_eq!(shown(&mut h, win), ["inner.txt"]);
    assert!(h.state().answers.is_empty());
    assert!(h.widget::<Button<St>>(back).is_enabled());
    // The place row for the directory on screen is the selected one.
    let sub_row = named(&mut h, "place_sub");
    assert!(h.widget::<SidebarRow<St>>(sub_row).is_selected());

    h.click(back);
    assert_eq!(cwd(&mut h, win), dir);
    assert!(!h.widget::<SidebarRow<St>>(sub_row).is_selected());

    let up = named(&mut h, "picker_up");
    h.click(up);
    assert_eq!(cwd(&mut h, win), dir.parent().unwrap());

    // The path bar, written from outside, goes where it says.
    let path = named(&mut h, "picker_path");
    let target = dir.join("sub").display().to_string();
    act(&mut h, path, "set_text", Some(&target));
    assert_eq!(cwd(&mut h, win), dir.join("sub"));

    // A place row goes to its place.
    let fix = named(&mut h, "place_fix");
    h.click(fix);
    assert_eq!(cwd(&mut h, win), dir);
    assert!(h.widget::<SidebarRow<St>>(fix).is_selected());
    h.quit();
}

#[test]
fn open_is_disabled_in_an_empty_directory() {
    let dir = scratch("empty");
    let empty = dir.join("sub").join("nothing");
    std::fs::create_dir_all(&empty).unwrap();
    let (mut h, _win) = open(&empty, FilePicker::open());
    let ok = named(&mut h, "picker_ok");
    assert!(!h.widget::<Button<St>>(ok).is_enabled());
    assert_eq!(status(&mut h), "0 items");
    h.quit();
}

#[test]
fn the_trash_is_not_a_place_to_pick_from() {
    let dir = scratch("trash");
    let (mut h, _win) = open(&dir, FilePicker::open());
    assert!(introspect::resolve(h.ui(), "window[1]/place_trash").is_none());
    h.quit();
}

#[test]
fn cancel_escape_and_a_server_close_each_answer_none_once() {
    let dir = scratch("cancel");

    let (mut h, win) = open(&dir, FilePicker::open());
    let cancel = named(&mut h, "picker_cancel");
    act(&mut h, cancel, "click", None);
    assert_eq!(h.state().answers, [None]);
    assert!(!h.ui().has_window(win));
    h.quit();

    let (mut h, win) = open(&dir, FilePicker::open());
    h.key_in(win, key::ESC);
    assert_eq!(h.state().answers, [None]);
    assert!(!h.ui().has_window(win));
    h.quit();

    let (mut h, win) = open(&dir, FilePicker::open());
    h.close_from_server(win);
    assert_eq!(h.state().answers, [None]);
    assert!(!h.ui().has_window(win));
    assert!(!h.ui().should_quit());
    // The main window keeps working.
    let main_ok = introspect::resolve(h.ui(), "window/main_ok").unwrap();
    h.click(main_ok);
    assert_eq!(h.state().main_clicks, 1);
    h.quit();
}

#[test]
fn a_mime_filter_hides_other_files_and_all_files_brings_them_back() {
    let dir = scratch("mime");
    let (mut h, win) = open(&dir, FilePicker::open().mime(["image/*"]));
    assert_eq!(shown(&mut h, win), ["sub", "b.png", "c.png"]);
    let filter = named(&mut h, "picker_filter");
    assert_eq!(h.widget::<Button<St>>(filter).text(), "Images");
    act(&mut h, filter, "click", None);
    assert_eq!(h.widget::<Button<St>>(filter).text(), "All files");
    assert_eq!(shown(&mut h, win), ["sub", "a.txt", "b.png", "c.png"]);
    act(&mut h, filter, "click", None);
    assert_eq!(shown(&mut h, win), ["sub", "b.png", "c.png"]);
    h.quit();
}

#[test]
fn without_a_filter_there_is_no_filter_button_and_ctrl_h_shows_dot_files() {
    let dir = scratch("hidden");
    let (mut h, win) = open(&dir, FilePicker::open());
    assert!(introspect::resolve(h.ui(), "window[1]/picker_filter").is_none());
    h.key_with(key::LEFT_CTRL, key::H);
    assert_eq!(
        shown(&mut h, win),
        ["sub", ".hidden.txt", "a.txt", "b.png", "c.png"]
    );
    h.quit();
}

#[test]
fn multiple_returns_every_selected_file_and_single_only_the_cursor() {
    let dir = scratch("multi");
    let (mut h, win) = open(&dir, FilePicker::open().multiple(true));
    assert_eq!(h.ui().window_title_of(win), "Open Files");
    let list = named(&mut h, "picker_list");
    act(&mut h, list, "select", Some("1"));
    h.key_with(key::LEFT_SHIFT, key::DOWN);
    assert_eq!(status(&mut h), "4 items, 2 selected");
    let ok = named(&mut h, "picker_ok");
    act(&mut h, ok, "click", None);
    assert_eq!(
        h.state().answers,
        [Some(vec![dir.join("a.txt"), dir.join("b.png")])]
    );
    h.quit();

    let (mut h, _win) = open(&dir, FilePicker::open());
    let list = named(&mut h, "picker_list");
    act(&mut h, list, "select", Some("1"));
    h.key_with(key::LEFT_SHIFT, key::DOWN);
    let ok = named(&mut h, "picker_ok");
    act(&mut h, ok, "click", None);
    assert_eq!(h.state().answers, [Some(vec![dir.join("b.png")])]);
    h.quit();
}

#[test]
fn open_on_a_directory_goes_into_it() {
    let dir = scratch("open-dir");
    let (mut h, win) = open(&dir, FilePicker::open());
    let list = named(&mut h, "picker_list");
    act(&mut h, list, "select", Some("0"));
    let ok = named(&mut h, "picker_ok");
    act(&mut h, ok, "click", None);
    assert!(h.state().answers.is_empty());
    assert_eq!(cwd(&mut h, win), dir.join("sub"));
    h.quit();
}

#[test]
fn a_folder_picker_lists_folders_and_selects_the_current_one_by_default() {
    let dir = scratch("folder");
    let (mut h, win) = open(&dir, FilePicker::folder());
    assert_eq!(h.ui().window_title_of(win), "Select Folder");
    assert_eq!(shown(&mut h, win), ["sub"]);
    let ok = named(&mut h, "picker_ok");
    assert_eq!(h.widget::<Button<St>>(ok).text(), "Select");
    act(&mut h, ok, "click", None);
    assert_eq!(h.state().answers, [Some(vec![dir.clone()])]);
    h.quit();

    let (mut h, _win) = open(&dir, FilePicker::folder().mime(["image/*"]));
    assert!(
        introspect::resolve(h.ui(), "window[1]/picker_filter").is_none(),
        "a folder has no MIME type to filter by"
    );
    let list = named(&mut h, "picker_list");
    act(&mut h, list, "select", Some("0"));
    let ok = named(&mut h, "picker_ok");
    act(&mut h, ok, "click", None);
    assert_eq!(h.state().answers, [Some(vec![dir.join("sub")])]);
    h.quit();
}

#[test]
fn save_answers_the_typed_name_and_asks_twice_before_replacing() {
    let dir = scratch("save");
    let (mut h, win) = open(&dir, FilePicker::save("out.txt"));
    assert_eq!(h.ui().window_title_of(win), "Save As");
    let name = named(&mut h, "picker_name");
    assert_eq!(h.ui().focused_in(win), Some(name), "the name field has focus");
    assert_eq!(
        h.widget::<nitro_ui::widgets::TextField<St>>(name)
            .selected_text(),
        "out",
        "the stem is selected, the extension is not"
    );
    h.key_in(win, key::ENTER);
    assert_eq!(h.state().answers, [Some(vec![dir.join("out.txt")])]);
    h.quit();

    let (mut h, win) = open(&dir, FilePicker::save("x"));
    let name = named(&mut h, "picker_name");
    act(&mut h, name, "set_text", Some("a.txt"));
    let ok = named(&mut h, "picker_ok");
    act(&mut h, ok, "click", None);
    assert!(h.state().answers.is_empty(), "an existing file needs a yes");
    assert!(status(&mut h).contains("press Save again"), "{}", status(&mut h));
    act(&mut h, ok, "click", None);
    assert_eq!(h.state().answers, [Some(vec![dir.join("a.txt")])]);
    assert!(!h.ui().has_window(win));
    h.quit();
}

#[test]
fn activating_a_file_in_a_save_dialog_copies_its_name() {
    let dir = scratch("save-copy");
    let (mut h, _win) = open(&dir, FilePicker::save(""));
    let ok = named(&mut h, "picker_ok");
    assert!(!h.widget::<Button<St>>(ok).is_enabled(), "no name, no Save");
    let list = named(&mut h, "picker_list");
    act(&mut h, list, "activate", Some("3"));
    let name = named(&mut h, "picker_name");
    assert_eq!(
        h.widget::<nitro_ui::widgets::TextField<St>>(name).text(),
        "c.png"
    );
    assert!(h.state().answers.is_empty());
    assert!(h.widget::<Button<St>>(ok).is_enabled());
    h.quit();
}

#[test]
fn the_mime_matcher_takes_families_and_ignores_case_and_parameters() {
    use nitro_ui::picker::mime_matches;
    assert!(mime_matches("image/*", "image/png"));
    assert!(mime_matches("IMAGE/PNG", "image/png; x=1"));
    assert!(mime_matches("*/*", "text/plain"));
    assert!(!mime_matches("image/*", "text/plain"));
    assert!(!mime_matches("image/png", "image/jpeg"));
    assert!(!mime_matches("image/*", "imagex/png"));
}


#[test]
fn the_host_window_keeps_working_while_the_picker_is_open() {
    let dir = scratch("host");
    let (mut h, win) = open(&dir, FilePicker::open());
    let main_ok = introspect::resolve(h.ui(), "window/main_ok").unwrap();
    let (ui, st) = h.parts();
    ui.action(st, main_ok, "click", None).unwrap();
    h.settle();
    assert_eq!(h.state().main_clicks, 1);
    assert!(h.ui().has_window(win), "the picker is still open");
    assert!(h.state().answers.is_empty());
    h.quit();
}

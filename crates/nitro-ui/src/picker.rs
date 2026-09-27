//! [`FilePicker`] — the toolkit's file and folder dialog.
//!
//! A second window ([`Ui::add_window`]) laid out like `nitro-files`: a
//! places sidebar, a back/up/path header, a virtualised list with rounded
//! inset rows and a footer with a status line, a filter button, Cancel
//! and the action button. The directory model — reading, sorting,
//! MIME types, icons, the XDG places — is `nitro-fs`, the crate
//! `nitro-files` reads it from too.
//!
//! ```no_run
//! # use nitro_ui::picker::FilePicker;
//! # use nitro_ui::Ui;
//! # struct State { chosen: Vec<std::path::PathBuf> }
//! # fn demo(ui: &mut Ui<State>) -> Result<(), nitro_ui::Error> {
//! FilePicker::open()
//!     .multiple(true)
//!     .mime(["image/*"])
//!     .on_done(|s: &mut State, _ui: &mut Ui<State>, picked| {
//!         if let Some(paths) = picked {
//!             s.chosen = paths;
//!         }
//!     })
//!     .open_in(ui)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Where the dialog keeps its state
//!
//! Callbacks are handed the *app's* `&mut S`, so the picker cannot keep
//! its directory, history and result callback there, and the crate has
//! no `Rc<RefCell>` to hide them in. They live in the dialog window's
//! root widget, a [`Picker`], which is a plain column container that
//! owns the model. Every callback inside the dialog captures that root's
//! [`WidgetId`] and reaches the model through [`Ui::widget_mut`]; one
//! whose window has gone finds a stale id and does nothing.
//!
//! # The result, exactly once
//!
//! `on_done` is called with `Some(paths)` when the user accepts and with
//! `None` when they cancel — the Cancel button, Escape, or the window
//! being closed from outside. It is an `FnOnce` held in the model and
//! *taken* by whichever of those happens first, so it cannot run twice.
//! On accept it runs **before** the window is removed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use nitro_core::Size;
use nitro_fs::dir::{self, Entry, Kind};
use nitro_fs::mime::{self, Glob};
use nitro_fs::places::{self, Place, Section};

use crate::build::{Built, ContainerBuilder as _, StyleBuilder as _};
use crate::event::{Event, Handled, key, mods};
use crate::layout::{Constraints, CrossAlign, Direction, Length};
use crate::list::{List, Row};
use crate::split::{
    SIDEBAR_ROW_INSET, SidebarRow, sidebar_row, sidebar_section, sidebar_separator, split_view,
};
use crate::ui::{FdToken, Ui, WindowId};
use crate::widget::{EventCx, MeasureCx, Role, Widget};
use crate::widgets::{Button, Label, TextField, button, column, label, row, separator, text_field};
use crate::{ColorRole, Error, WidgetId};

/// The addressing names of the dialog's widgets, for `hey` and tests.
///
/// They only have to be unique inside the dialog: introspection paths
/// are rooted per window (`window[1]/picker_ok`), so a host app's own
/// `ok` cannot collide with them.
pub mod names {
    /// The path bar.
    pub const PATH: &str = "picker_path";
    /// The file list.
    pub const LIST: &str = "picker_list";
    /// The name field (save dialogs only).
    pub const NAME: &str = "picker_name";
    /// The status line.
    pub const STATUS: &str = "picker_status";
    /// The action button: Open, Save or Select.
    pub const OK: &str = "picker_ok";
    /// Cancel.
    pub const CANCEL: &str = "picker_cancel";
    /// The filter button, when there is more than one filter.
    pub const FILTER: &str = "picker_filter";
    /// The sidebar's rows column.
    pub const PLACES: &str = "picker_places";
    /// Prefix of a place row's name: `place_home`, `place_root`, ….
    pub const PLACE_PREFIX: &str = "place_";
    /// The Back button.
    pub const BACK: &str = "picker_back";
    /// The Up button.
    pub const UP: &str = "picker_up";
}

/// Where `globs2` lives on a freedesktop system.
const GLOBS2: &str = "/usr/share/mime/globs2";
/// How many directories Back remembers.
const HISTORY: usize = 64;
/// The default window size.
const DEFAULT_SIZE: Size = Size::new(760.0, 480.0);

/// What the dialog picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    /// One or more existing files.
    Open,
    /// A path to write to, which may not exist yet.
    Save,
    /// One or more directories.
    Folder,
}

/// A named set of MIME patterns the list can be narrowed to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Filter {
    label: String,
    /// `image/png`, `image/*`; empty means "everything".
    mimes: Vec<String>,
}

type DoneFn<S> = Box<dyn FnOnce(&mut S, &mut Ui<S>, Option<Vec<PathBuf>>)>;

/// Builder for a file or folder dialog; see the [module docs](self).
pub struct FilePicker<S> {
    kind: PickKind,
    multiple: bool,
    filters: Vec<Filter>,
    start_dir: Option<PathBuf>,
    title: Option<String>,
    hidden: bool,
    size: Size,
    name: String,
    on_done: Option<DoneFn<S>>,
    places: Option<Vec<Place>>,
    globs: Option<Vec<Glob>>,
}

impl<S: 'static> FilePicker<S> {
    fn new(kind: PickKind) -> Self {
        Self {
            kind,
            multiple: false,
            filters: Vec::new(),
            start_dir: None,
            title: None,
            hidden: false,
            size: DEFAULT_SIZE,
            name: String::new(),
            on_done: None,
            places: None,
            globs: None,
        }
    }

    /// Pick existing file(s).
    #[must_use]
    pub fn open() -> Self {
        Self::new(PickKind::Open)
    }

    /// Pick a path to save to; the name field starts with `name`, its
    /// stem selected.
    #[must_use]
    pub fn save(name: impl Into<String>) -> Self {
        let mut p = Self::new(PickKind::Save);
        p.name = name.into();
        p
    }

    /// Pick directory(ies).
    #[must_use]
    pub fn folder() -> Self {
        Self::new(PickKind::Folder)
    }

    /// Which kind of dialog this is.
    #[must_use]
    pub fn kind(&self) -> PickKind {
        self.kind
    }

    /// Allow more than one file or folder (Shift+arrows, Ctrl+Space).
    /// Ignored for a save dialog.
    #[must_use]
    pub fn multiple(mut self, on: bool) -> Self {
        self.multiple = on && self.kind != PickKind::Save;
        self
    }

    /// Show only files of these MIME types: exact (`image/png`) or a
    /// whole family (`image/*`), case-insensitively. The user can still
    /// switch to "All files". Calling it again adds another filter.
    #[must_use]
    pub fn mime(mut self, types: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mimes: Vec<String> = types.into_iter().map(Into::into).collect();
        let label = describe(&mimes);
        self.filters.push(Filter { label, mimes });
        self
    }

    /// A named filter the user can cycle to with the filter button, in
    /// the order they were added; "All files" is always last.
    #[must_use]
    pub fn filter(
        mut self,
        label: impl Into<String>,
        types: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.filters.push(Filter {
            label: label.into(),
            mimes: types.into_iter().map(Into::into).collect(),
        });
        self
    }

    /// The directory to start in. Default: `$HOME`, else the current
    /// directory.
    #[must_use]
    pub fn start_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.start_dir = Some(dir.into());
        self
    }

    /// The window title. Default: "Open File", "Open Files", "Save As",
    /// "Select Folder" or "Select Folders".
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Start with dot-files shown (Ctrl+H toggles).
    #[must_use]
    pub fn show_hidden(mut self, on: bool) -> Self {
        self.hidden = on;
        self
    }

    /// The window's size. Default 760×480.
    #[must_use]
    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    /// What to do with the answer: `Some(paths)` on accept, `None` on
    /// cancel. Called exactly once.
    #[must_use]
    pub fn on_done(
        mut self,
        f: impl FnOnce(&mut S, &mut Ui<S>, Option<Vec<PathBuf>>) + 'static,
    ) -> Self {
        self.on_done = Some(Box::new(f));
        self
    }

    /// Replace the sidebar's places (default: `$HOME` and the XDG user
    /// directories, then Root). For tests, which must not depend on the
    /// developer's home directory.
    #[doc(hidden)]
    #[must_use]
    pub fn places(mut self, places: Vec<Place>) -> Self {
        self.places = Some(places);
        self
    }

    /// Replace the `globs2` table (default: the system's). For tests.
    #[doc(hidden)]
    #[must_use]
    pub fn globs(mut self, globs: Vec<Glob>) -> Self {
        self.globs = Some(globs);
        self
    }

    fn default_title(&self) -> &'static str {
        match (self.kind, self.multiple) {
            (PickKind::Open, false) => "Open File",
            (PickKind::Open, true) => "Open Files",
            (PickKind::Save, _) => "Save As",
            (PickKind::Folder, false) => "Select Folder",
            (PickKind::Folder, true) => "Select Folders",
        }
    }

    /// Build the dialog and open it as a new window of `ui`.
    ///
    /// # Errors
    /// A wire error from creating the window.
    pub fn open_in(mut self, ui: &mut Ui<S>) -> Result<WindowId, Error> {
        let title = self
            .title
            .take()
            .unwrap_or_else(|| self.default_title().to_owned());
        let globs = self
            .globs
            .take()
            .unwrap_or_else(|| mime::load_globs2(Path::new(GLOBS2)));
        // The trash is a place for a file manager, not somewhere a file
        // is opened from or saved to.
        let places: Vec<Place> = self
            .places
            .take()
            .unwrap_or_else(|| places::from_env(Path::new("")))
            .into_iter()
            .filter(|p| p.key != "trash")
            .collect();
        let start = self
            .start_dir
            .take()
            .or_else(places::home_dir)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut filters = std::mem::take(&mut self.filters);
        if self.kind == PickKind::Folder {
            filters.clear();
        }
        filters.push(Filter {
            label: "All files".to_owned(),
            mimes: Vec::new(),
        });

        let mut built: Built<S> = Built::new(Picker::<S> { model: None });
        {
            let st = built.state_mut();
            st.style.direction = Direction::Column;
            st.style.cross_align = CrossAlign::Stretch;
            st.style.width = Length::Percent(1.0);
            st.style.height = Length::Percent(1.0);
        }
        let root = ui.build(built);
        let (ids, place_rows) = build_tree(ui, root, &self, &title, &filters, &places);
        let win = ui.add_window(&title, Some(self.size), root)?;
        let model = Model {
            root,
            win,
            kind: self.kind,
            multiple: self.multiple,
            filters,
            active: 0,
            hidden: self.hidden,
            globs,
            cwd: start.clone(),
            entries: Vec::new(),
            types: Vec::new(),
            shown: Vec::new(),
            history: Vec::new(),
            listings: 0,
            scan: None,
            message: None,
            confirm: None,
            on_done: self.on_done.take(),
            ids,
            place_rows,
        };
        if let Ok(mut p) = ui.widget_mut::<Picker<S>>(root) {
            p.model = Some(Box::new(model));
        }
        ui.on_window_closed(win, move |s: &mut S, ui: &mut Ui<S>| finish(s, ui, root, None));
        navigate(ui, root, start, false);
        if let Some(name) = ids.name {
            ui.focus(name);
            let stem = match self.name.rsplit_once('.') {
                Some((stem, _)) if !stem.is_empty() => stem.len(),
                _ => self.name.len(),
            };
            if let Ok(mut f) = ui.widget_mut::<TextField<S>>(name) {
                f.select_range(0, stem);
            }
        } else {
            ui.focus(ids.list);
        }
        Ok(win)
    }
}

/// A short label for a filter made from bare MIME patterns.
fn describe(mimes: &[String]) -> String {
    match mimes {
        [one] => match one.to_ascii_lowercase().as_str() {
            "image/*" => "Images".to_owned(),
            "audio/*" => "Audio".to_owned(),
            "video/*" => "Videos".to_owned(),
            "text/*" => "Text files".to_owned(),
            _ => one.clone(),
        },
        [a, b] => format!("{a}, {b}"),
        _ => "Supported files".to_owned(),
    }
}

/// Whether the MIME type `mime` matches `pattern`: equal, or in the
/// pattern's `major/*` family, or anything for `*` and `*/*`. Case and
/// parameters (`; charset=…`) are ignored.
#[must_use]
pub fn mime_matches(pattern: &str, mime: &str) -> bool {
    let clean = |s: &str| s.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let (pattern, mime) = (clean(pattern), clean(mime));
    if pattern == "*" || pattern == "*/*" {
        return true;
    }
    match pattern.strip_suffix("/*") {
        Some(major) => mime.split_once('/').is_some_and(|(m, _)| m == major),
        None => pattern == mime,
    }
}

// ---------------------------------------------------------------------
// The root widget and its model
// ---------------------------------------------------------------------

/// The ids of the dialog's widgets the model writes to.
#[derive(Debug, Clone, Copy)]
struct Ids {
    list: WidgetId,
    path: WidgetId,
    name: Option<WidgetId>,
    status: WidgetId,
    ok: WidgetId,
    filter: Option<WidgetId>,
    back: WidgetId,
}

struct Model<S> {
    root: WidgetId,
    win: WindowId,
    kind: PickKind,
    multiple: bool,
    /// The user's filters, then "All files" (empty `mimes`).
    filters: Vec<Filter>,
    active: usize,
    hidden: bool,
    globs: Vec<Glob>,
    cwd: PathBuf,
    entries: Vec<Entry>,
    /// Each entry's MIME type, parallel to `entries`; `None` for a
    /// directory or an unknown type. Computed once per listing.
    types: Vec<Option<String>>,
    /// The entries on screen, as indices into `entries`: row `i` of the
    /// list is `entries[shown[i]]`.
    shown: Vec<usize>,
    history: Vec<PathBuf>,
    listings: u64,
    scan: Option<(dir::Scan, FdToken)>,
    message: Option<String>,
    /// A save target that exists and has been warned about once.
    confirm: Option<PathBuf>,
    on_done: Option<DoneFn<S>>,
    ids: Ids,
    /// Each place row and where it goes, so the row for the directory on
    /// screen can be selected.
    place_rows: Vec<(WidgetId, PathBuf)>,
}

/// The dialog window's root: a column container that owns the picker's
/// model. See the [module docs](self) for why the state lives here.
pub struct Picker<S> {
    model: Option<Box<Model<S>>>,
}

impl<S: 'static> Picker<S> {
    /// The directory on screen, or `None` while the model is in use.
    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        self.model.as_ref().map(|m| m.cwd.as_path())
    }

    /// The names on screen, in list order.
    #[must_use]
    pub fn shown(&self) -> Vec<&str> {
        self.model.as_ref().map_or_else(Vec::new, |m| {
            m.shown
                .iter()
                .map(|i| m.entries[*i].name.as_str())
                .collect()
        })
    }

    /// Whether dot-files are shown.
    #[must_use]
    pub fn shows_hidden(&self) -> bool {
        self.model.as_ref().is_some_and(|m| m.hidden)
    }

    /// The label of the active filter.
    #[must_use]
    pub fn filter_label(&self) -> Option<&str> {
        self.model
            .as_ref()
            .and_then(|m| m.filters.get(m.active))
            .map(|f| f.label.as_str())
    }

    /// Whether a big directory is being read in the background.
    #[must_use]
    pub fn scanning(&self) -> bool {
        self.model.as_ref().is_some_and(|m| m.scan.is_some())
    }
}

impl<S: 'static> Widget<S> for Picker<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        crate::widgets::measure_container(cx, constraints)
    }

    fn role(&self) -> Role {
        Role::Container
    }

    /// The dialog's keys, offered after the focused widget declined
    /// them. Everything is deferred: this widget is out of its slot
    /// while its own `event` runs, and every operation reads the model
    /// out of it.
    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        let Event::KeyDown(k) = ev else {
            return Handled::No;
        };
        let root = cx.id;
        let m = k.mods & mods::MASK;
        match (m, k.keycode) {
            (mods::NONE, key::ESC) => {
                cx.ui
                    .defer(move |s: &mut S, ui: &mut Ui<S>| finish(s, ui, root, None));
            }
            (mods::NONE, key::ENTER) => {
                cx.ui.defer(move |s: &mut S, ui: &mut Ui<S>| accept(s, ui, root));
            }
            (mods::CTRL, key::H) => {
                cx.ui.defer(move |_s: &mut S, ui: &mut Ui<S>| {
                    with_model(ui, root, |m, ui| {
                        m.hidden = !m.hidden;
                        refresh(m, ui, true);
                    });
                });
            }
            (mods::CTRL, key::L) => {
                cx.ui.defer(move |_s: &mut S, ui: &mut Ui<S>| {
                    let Some(path) = with_model(ui, root, |m, _| m.ids.path) else {
                        return;
                    };
                    ui.focus(path);
                    if let Ok(mut f) = ui.widget_mut::<TextField<S>>(path) {
                        f.select_all();
                    }
                });
            }
            (mods::NONE, key::BACKSPACE) | (mods::ALT, key::UP) => {
                cx.ui.defer(move |_s: &mut S, ui: &mut Ui<S>| go_up(ui, root));
            }
            (mods::ALT, key::LEFT) => {
                cx.ui.defer(move |_s: &mut S, ui: &mut Ui<S>| go_back(ui, root));
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }
}

/// Take the model out of the root, run `f` with it and the whole tree,
/// and put it back. `None` when the dialog has gone (or the model is
/// already out, which only a re-entrant call could see).
fn with_model<S: 'static, R>(
    ui: &mut Ui<S>,
    root: WidgetId,
    f: impl FnOnce(&mut Model<S>, &mut Ui<S>) -> R,
) -> Option<R> {
    let mut m = ui.widget_mut::<Picker<S>>(root).ok()?.model.take()?;
    let r = f(&mut m, ui);
    if let Ok(mut p) = ui.widget_mut::<Picker<S>>(root) {
        p.model = Some(m);
    } else if let Some((_, token)) = m.scan.take() {
        ui.remove_fd(token);
    }
    Some(r)
}

// ---------------------------------------------------------------------
// Building the tree
// ---------------------------------------------------------------------

// One function because it is one tree, read in the order it is built —
// the allow `nitro-files::build` takes for the same reason.
#[allow(clippy::too_many_lines)]
fn build_tree<S: 'static>(
    ui: &mut Ui<S>,
    root: WidgetId,
    p: &FilePicker<S>,
    title: &str,
    filters: &[Filter],
    places: &[Place],
) -> (Ids, Vec<(WidgetId, PathBuf)>) {
    let start = p
        .start_dir
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let path = ui.build(
        text_field(start)
            .name(names::PATH)
            .placeholder("Path")
            .grow(1.0)
            .height(34.0)
            .on_submit(move |_s: &mut S, ui: &mut Ui<S>, text: &str| {
                let text = text.to_owned();
                ui.defer(move |_s: &mut S, ui: &mut Ui<S>| {
                    with_model(ui, root, |m, ui| {
                        let to = dir::resolve(&text, &m.cwd);
                        navigate_in(m, ui, to, true);
                        let list = m.ids.list;
                        ui.focus(list);
                    });
                });
            })
            // A change navigates only when the field is not focused: a
            // script's `set picker_path value /tmp` goes somewhere, a
            // user typing does not jump at every letter. The same rule,
            // for the same reason, as `nitro-files`' path bar.
            .on_change(move |_s: &mut S, ui: &mut Ui<S>, text: &str| {
                let text = text.to_owned();
                ui.defer(move |_s: &mut S, ui: &mut Ui<S>| {
                    with_model(ui, root, |m, ui| {
                        if ui.is_focused(m.ids.path) {
                            return;
                        }
                        let to = dir::resolve(&text, &m.cwd);
                        if to != m.cwd {
                            navigate_in(m, ui, to, true);
                        }
                    });
                });
            }),
    );
    let up = ui.build(button("↑").name(names::UP).icon("arrow-up").on_click(
        move |_s: &mut S, ui: &mut Ui<S>| {
            ui.defer(move |_s: &mut S, ui: &mut Ui<S>| go_up(ui, root));
        },
    ));
    let back = ui.build(
        button("←")
            .name(names::BACK)
            .icon("arrow-left")
            .disabled()
            .on_click(move |_s: &mut S, ui: &mut Ui<S>| {
                ui.defer(move |_s: &mut S, ui: &mut Ui<S>| go_back(ui, root));
            }),
    );
    let list = ui.build(
        crate::list::list()
            .name(names::LIST)
            .row_inset(SIDEBAR_ROW_INSET)
            .row_radius(6.0)
            .grow(1.0)
            .width_percent(1.0)
            .on_activate(move |_s: &mut S, ui: &mut Ui<S>, index: usize| {
                // Deferred: activating a directory writes new rows into
                // this very list, which is out of its slot right now.
                ui.defer(move |s: &mut S, ui: &mut Ui<S>| activate(s, ui, root, index));
            })
            .on_select(move |_s: &mut S, ui: &mut Ui<S>, _index: usize| {
                ui.defer(move |_s: &mut S, ui: &mut Ui<S>| {
                    with_model(ui, root, |m, ui| show_status(m, ui));
                });
            }),
    );
    let body = ui.build(
        column()
            .gap(6.0)
            .padding_xy(SIDEBAR_ROW_INSET, 0.0)
            .width_percent(1.0)
            .grow(1.0)
            .cross_align(CrossAlign::Stretch),
    );
    ui.attach(body, list).expect("fresh ids");
    let name = if p.kind == PickKind::Save {
        let field = ui.build(
            text_field(p.name.clone())
                .name(names::NAME)
                .placeholder("File name")
                .grow(1.0)
                .on_submit(move |_s: &mut S, ui: &mut Ui<S>, _text: &str| {
                    ui.defer(move |s: &mut S, ui: &mut Ui<S>| accept(s, ui, root));
                })
                .on_change(move |_s: &mut S, ui: &mut Ui<S>, _text: &str| {
                    ui.defer(move |_s: &mut S, ui: &mut Ui<S>| {
                        with_model(ui, root, |m, ui| {
                            m.confirm = None;
                            m.message = None;
                            show_status(m, ui);
                        });
                    });
                }),
        );
        let line = ui.build(
            row()
                .gap(8.0)
                .padding_xy(crate::split::SIDEBAR_ROW_PAD_X, 4.0)
                .cross_align(CrossAlign::Center)
                .width_percent(1.0)
                .child(label("Name").color_role(ColorRole::TextDim)),
        );
        ui.attach(line, field).expect("fresh ids");
        ui.attach(body, line).expect("fresh ids");
        Some(field)
    } else {
        None
    };

    let status = ui.build(
        label("")
            .name(names::STATUS)
            .size(crate::split::SMALL_PX)
            .color_role(ColorRole::TextDim)
            .elide(true)
            .grow(1.0),
    );
    let filter = if filters.len() > 1 {
        Some(ui.build(button(filters[0].label.clone()).name(names::FILTER).on_click(
            move |_s: &mut S, ui: &mut Ui<S>| {
                ui.defer(move |_s: &mut S, ui: &mut Ui<S>| cycle_filter(ui, root));
            },
        )))
    } else {
        None
    };
    let cancel = ui.build(button("Cancel").name(names::CANCEL).on_click(
        move |_s: &mut S, ui: &mut Ui<S>| {
            ui.defer(move |s: &mut S, ui: &mut Ui<S>| finish(s, ui, root, None));
        },
    ));
    let ok_text = match p.kind {
        PickKind::Open => "Open",
        PickKind::Save => "Save",
        PickKind::Folder => "Select",
    };
    let ok = ui.build(button(ok_text).name(names::OK).on_click(
        move |_s: &mut S, ui: &mut Ui<S>| {
            ui.defer(move |s: &mut S, ui: &mut Ui<S>| accept(s, ui, root));
        },
    ));
    let bar = ui.build(
        row()
            .gap(8.0)
            .padding_xy(crate::split::CONTENT_GUTTER, 8.0)
            .cross_align(CrossAlign::Center)
            .width_percent(1.0),
    );
    for c in [Some(status), filter, Some(cancel), Some(ok)]
        .into_iter()
        .flatten()
    {
        ui.attach(bar, c).expect("fresh ids");
    }
    let footer_line = ui.build(
        separator()
            .color_role(ColorRole::Hairline)
            .width_percent(1.0),
    );
    let footer = ui.build(column().width_percent(1.0));
    ui.attach(footer, footer_line).expect("fresh ids");
    ui.attach(footer, bar).expect("fresh ids");

    let parts = split_view()
        .sidebar_name(names::PLACES)
        .sidebar_header(title)
        .sidebar_child(sidebar_section("Places"))
        .content_header_leading_id(back)
        .content_header_leading_id(up)
        .content_header_leading_id(path)
        .content_id(body)
        .content_footer_id(footer)
        .build(ui);
    let mut prev = Section::Places;
    let mut place_rows = Vec::new();
    for place in places {
        if place.section != prev {
            let _ = ui.add_child(parts.sidebar, sidebar_separator());
            prev = place.section;
        }
        let to = place.path.clone();
        let added = ui.add_child(
            parts.sidebar,
            sidebar_row(place.icon, place.label.clone())
                .name(format!("{}{}", names::PLACE_PREFIX, place.key))
                .on_click(move |_s: &mut S, ui: &mut Ui<S>| {
                    // Deferred: navigating selects the row clicked.
                    let to = to.clone();
                    ui.defer(move |_s: &mut S, ui: &mut Ui<S>| navigate(ui, root, to, true));
                }),
        );
        if let Ok(id) = added {
            place_rows.push((id, place.path.clone()));
        }
    }
    ui.attach(root, parts.root).expect("fresh ids");
    let ids = Ids {
        list,
        path,
        name,
        status,
        ok,
        filter,
        back,
    };
    (ids, place_rows)
}

// ---------------------------------------------------------------------
// Navigation and listing
// ---------------------------------------------------------------------

fn navigate<S: 'static>(ui: &mut Ui<S>, root: WidgetId, to: PathBuf, remember: bool) {
    with_model(ui, root, |m, ui| navigate_in(m, ui, to, remember));
}

fn go_up<S: 'static>(ui: &mut Ui<S>, root: WidgetId) {
    with_model(ui, root, |m, ui| {
        let to = dir::parent_of(&m.cwd);
        navigate_in(m, ui, to, true);
    });
}

fn go_back<S: 'static>(ui: &mut Ui<S>, root: WidgetId) {
    with_model(ui, root, |m, ui| {
        // A pop, not a push: going back must not remember the directory
        // being left on the history it came from.
        if let Some(to) = m.history.pop() {
            navigate_in(m, ui, to, false);
        }
    });
}

fn navigate_in<S: 'static>(m: &mut Model<S>, ui: &mut Ui<S>, to: PathBuf, remember: bool) {
    match std::fs::metadata(&to) {
        Ok(md) if md.is_dir() => {}
        Ok(_) => {
            m.message = Some(format!("not a directory: {}", to.display()));
            show_status(m, ui);
            return;
        }
        Err(e) => {
            m.message = Some(format!("{}: {e}", to.display()));
            show_status(m, ui);
            return;
        }
    }
    if remember && m.listings > 0 && m.cwd != to {
        m.history.push(std::mem::replace(&mut m.cwd, to));
        if m.history.len() > HISTORY {
            m.history.remove(0);
        }
    } else {
        m.cwd = to;
    }
    m.message = None;
    m.confirm = None;
    if let Ok(mut b) = ui.widget_mut::<Button<S>>(m.ids.back) {
        b.set_enabled(!m.history.is_empty());
    }
    if let Ok(mut f) = ui.widget_mut::<TextField<S>>(m.ids.path) {
        f.set_text(m.cwd.display().to_string());
    }
    for (row, path) in &m.place_rows {
        if let Ok(mut r) = ui.widget_mut::<SidebarRow<S>>(*row) {
            r.set_selected(*path == m.cwd);
        }
    }
    relist(m, ui);
}

fn relist<S: 'static>(m: &mut Model<S>, ui: &mut Ui<S>) {
    drop_scan(m, ui);
    let cwd = m.cwd.clone();
    // A directory too big to read inside a frame is read on a thread,
    // as `nitro-files` does; the count itself costs no `stat`.
    if dir::count_at_most(&cwd, dir::BIG_DIR + 1) > dir::BIG_DIR {
        start_scan(m, ui, &cwd);
        return;
    }
    match dir::read_dir(&cwd, &m.globs) {
        Ok(mut entries) => {
            dir::sort(&mut entries, dir::Sort::Name);
            set_entries(m, entries);
        }
        Err(e) => {
            set_entries(m, Vec::new());
            m.message = Some(format!("{}: {e}", cwd.display()));
        }
    }
    refresh(m, ui, true);
}

fn start_scan<S: 'static>(m: &mut Model<S>, ui: &mut Ui<S>, cwd: &Path) {
    let scan = match dir::Scan::start(cwd.to_path_buf(), dir::Sort::Name, m.globs.clone()) {
        Ok(scan) => scan,
        Err(e) => {
            m.message = Some(format!("background read failed ({e}), reading inline"));
            match dir::read_dir(cwd, &m.globs) {
                Ok(mut entries) => {
                    dir::sort(&mut entries, dir::Sort::Name);
                    set_entries(m, entries);
                }
                Err(e) => {
                    set_entries(m, Vec::new());
                    m.message = Some(format!("{}: {e}", cwd.display()));
                }
            }
            refresh(m, ui, true);
            return;
        }
    };
    let root = m.root;
    let token = match ui.add_fd(scan.as_fd(), move |_s: &mut S, ui: &mut Ui<S>| {
        with_model(ui, root, |m, ui| scan_ready(m, ui));
    }) {
        Ok(t) => t,
        Err(e) => {
            m.message = Some(format!("cannot watch the scan: {e}"));
            show_status(m, ui);
            return;
        }
    };
    m.message = Some(format!("reading {}…", cwd.display()));
    m.scan = Some((scan, token));
    show_status(m, ui);
}

fn scan_ready<S: 'static>(m: &mut Model<S>, ui: &mut Ui<S>) {
    let Some((scan, _)) = &mut m.scan else {
        return;
    };
    let Some(result) = scan.take() else {
        return;
    };
    let stale = scan.path() != m.cwd;
    drop_scan(m, ui);
    if stale {
        return;
    }
    match result {
        Ok(entries) => {
            set_entries(m, entries);
            m.message = None;
        }
        Err(e) => {
            set_entries(m, Vec::new());
            m.message = Some(format!("{}: {e}", m.cwd.display()));
        }
    }
    refresh(m, ui, true);
}

/// Forget the scan in flight and unregister its hook — which matters:
/// the loop's `epoll` is level-triggered, so a hook left on a pipe
/// holding an unread byte would spin it.
fn drop_scan<S: 'static>(m: &mut Model<S>, ui: &mut Ui<S>) {
    if let Some((_, token)) = m.scan.take() {
        ui.remove_fd(token);
    }
}

fn set_entries<S>(m: &mut Model<S>, entries: Vec<Entry>) {
    m.types = entries
        .iter()
        .map(|e| {
            if e.opens_a_directory() {
                None
            } else {
                mime::type_of(Path::new(&e.name), &m.globs)
            }
        })
        .collect();
    m.entries = entries;
    m.listings += 1;
}

/// Whether entry `i` belongs on screen under the current settings.
fn passes<S>(m: &Model<S>, index: usize) -> bool {
    let entry = &m.entries[index];
    if !m.hidden && entry.name.starts_with('.') {
        return false;
    }
    // Directories always show, or there would be no way anywhere.
    if entry.opens_a_directory() {
        return true;
    }
    if m.kind == PickKind::Folder {
        return false;
    }
    let Some(filter) = m.filters.get(m.active) else {
        return true;
    };
    if filter.mimes.is_empty() {
        return true;
    }
    let Some(ty) = m.types.get(index).and_then(Option::as_deref) else {
        return false;
    };
    filter.mimes.iter().any(|p| mime_matches(p, ty))
}

/// The list row for one entry: the same shape `nitro-files` shows.
fn row_of(e: &Entry) -> Row {
    let detail = match e.kind {
        Kind::Dir => "<dir>".to_owned(),
        Kind::Symlink if e.symlink_dir => "→ <dir>".to_owned(),
        Kind::Symlink => format!("→ {}   {}", dir::format_size(e), dir::format_mtime(e.mtime)),
        _ => format!("{}   {}", dir::format_size(e), dir::format_mtime(e.mtime)),
    };
    Row::new(e.name.clone()).icon(e.icon).detail(detail)
}

/// Recompute what is shown and push it into the list. `reset` puts the
/// cursor back at the top with nothing selected: the rows are a
/// different set, and a selection by index would name the wrong files.
fn refresh<S: 'static>(m: &mut Model<S>, ui: &mut Ui<S>, reset: bool) {
    m.shown = (0..m.entries.len()).filter(|i| passes(m, *i)).collect();
    let rows: Vec<Row> = m.shown.iter().map(|i| row_of(&m.entries[*i])).collect();
    if let Ok(mut l) = ui.widget_mut::<List<S>>(m.ids.list) {
        l.set_rows(rows);
        if reset {
            l.reset_cursor();
        }
    }
    show_status(m, ui);
}

/// The entries the user has picked, as indices into `entries`: the
/// list's selection (the cursor row when nothing is selected), or for a
/// single-choice dialog the one selected row the cursor is on.
fn picks<S: 'static>(m: &Model<S>, ui: &Ui<S>) -> Vec<usize> {
    let Ok(l) = ui.widget::<List<S>>(m.ids.list) else {
        return Vec::new();
    };
    let sel = l.selection();
    // An empty selection means the cursor row: the list's cursor is
    // always somewhere, and Open with a row under the cursor should not
    // depend on whether the user clicked it first.
    // A folder dialog is the exception: there, nothing selected means
    // "this folder", which is the common case and must stay reachable.
    if sel.is_empty() && m.kind == PickKind::Folder {
        return Vec::new();
    }
    let rows: Vec<usize> = if sel.is_empty() || (!m.multiple && sel.contains(&l.cursor())) {
        vec![l.cursor()]
    } else if m.multiple {
        sel
    } else {
        sel.into_iter().take(1).collect()
    };
    rows.into_iter()
        .filter_map(|r| m.shown.get(r).copied())
        .collect()
}

fn name_text<S: 'static>(m: &Model<S>, ui: &Ui<S>) -> String {
    m.ids
        .name
        .and_then(|n| ui.widget::<TextField<S>>(n).ok())
        .map(|f| f.text().trim().to_owned())
        .unwrap_or_default()
}

/// The status line and whether the action button is live.
fn show_status<S: 'static>(m: &mut Model<S>, ui: &mut Ui<S>) {
    let picked = picks(m, ui);
    let text = if let Some(msg) = &m.message {
        msg.clone()
    } else {
        let n = m.shown.len();
        let mut s = format!("{n} item{}", if n == 1 { "" } else { "s" });
        if m.multiple && picked.len() > 1 {
            let _ = write!(s, ", {} selected", picked.len());
        }
        s
    };
    if let Ok(mut l) = ui.widget_mut::<Label>(m.ids.status) {
        l.set_text(text);
    }
    let live = match m.kind {
        PickKind::Open => !picked.is_empty(),
        PickKind::Folder => true,
        PickKind::Save => !name_text(m, ui).is_empty(),
    };
    if let Ok(mut b) = ui.widget_mut::<Button<S>>(m.ids.ok) {
        b.set_enabled(live);
    }
}

fn cycle_filter<S: 'static>(ui: &mut Ui<S>, root: WidgetId) {
    with_model(ui, root, |m, ui| {
        if m.filters.len() < 2 {
            return;
        }
        m.active = (m.active + 1) % m.filters.len();
        if let Some(id) = m.ids.filter
            && let Ok(mut b) = ui.widget_mut::<Button<S>>(id)
        {
            b.set_text(m.filters[m.active].label.clone());
        }
        refresh(m, ui, true);
    });
}

// ---------------------------------------------------------------------
// Accepting and cancelling
// ---------------------------------------------------------------------

/// What a user action came to.
enum Outcome {
    /// The dialog is done, with these paths.
    Done(Vec<PathBuf>),
    /// Something changed inside the dialog; it stays open.
    Stay,
}

/// A row was activated (Enter, double-click, `do … activate`).
fn activate<S: 'static>(s: &mut S, ui: &mut Ui<S>, root: WidgetId, index: usize) {
    let outcome = with_model(ui, root, |m, ui| {
        let Some(&i) = m.shown.get(index) else {
            return Outcome::Stay;
        };
        let e = &m.entries[i];
        let path = m.cwd.join(&e.name);
        if e.opens_a_directory() {
            navigate_in(m, ui, path, true);
            return Outcome::Stay;
        }
        match m.kind {
            PickKind::Open => Outcome::Done(vec![path]),
            PickKind::Save => {
                let name = e.name.clone();
                if let Some(id) = m.ids.name
                    && let Ok(mut f) = ui.widget_mut::<TextField<S>>(id)
                {
                    f.set_text(name);
                }
                m.confirm = None;
                m.message = None;
                show_status(m, ui);
                Outcome::Stay
            }
            PickKind::Folder => Outcome::Stay,
        }
    });
    if let Some(Outcome::Done(paths)) = outcome {
        finish(s, ui, root, Some(paths));
    }
}

/// The action button, or Enter where nothing else took it.
fn accept<S: 'static>(s: &mut S, ui: &mut Ui<S>, root: WidgetId) {
    let outcome = with_model(ui, root, |m, ui| match m.kind {
        PickKind::Open => accept_open(m, ui),
        PickKind::Folder => {
            let dirs: Vec<PathBuf> = picks(m, ui)
                .into_iter()
                .filter(|i| m.entries[*i].opens_a_directory())
                .map(|i| m.cwd.join(&m.entries[i].name))
                .collect();
            if dirs.is_empty() {
                Outcome::Done(vec![m.cwd.clone()])
            } else {
                Outcome::Done(dirs)
            }
        }
        PickKind::Save => accept_save(m, ui),
    });
    if let Some(Outcome::Done(paths)) = outcome {
        finish(s, ui, root, Some(paths));
    }
}

fn accept_open<S: 'static>(m: &mut Model<S>, ui: &mut Ui<S>) -> Outcome {
    let picked = picks(m, ui);
    let files: Vec<PathBuf> = picked
        .iter()
        .filter(|i| !m.entries[**i].opens_a_directory())
        .map(|i| m.cwd.join(&m.entries[*i].name))
        .collect();
    if !files.is_empty() {
        return Outcome::Done(files);
    }
    // Only directories picked: Open goes into the first, as a file
    // dialog's Open does on a folder.
    if let Some(&i) = picked.first() {
        let to = m.cwd.join(&m.entries[i].name);
        navigate_in(m, ui, to, true);
    }
    Outcome::Stay
}

fn accept_save<S: 'static>(m: &mut Model<S>, ui: &mut Ui<S>) -> Outcome {
    let name = name_text(m, ui);
    if name.is_empty() {
        m.message = Some("type a file name".to_owned());
        show_status(m, ui);
        return Outcome::Stay;
    }
    let target = dir::resolve(&name, &m.cwd);
    if target.is_dir() {
        // A directory typed into the name field is somewhere to go.
        if let Some(id) = m.ids.name
            && let Ok(mut f) = ui.widget_mut::<TextField<S>>(id)
        {
            f.set_text(String::new());
        }
        navigate_in(m, ui, target, true);
        return Outcome::Stay;
    }
    // There are no dialogs within this dialog, so replacing a file is
    // confirmed on the status line: the second press of the same name
    // is the yes.
    if target.exists() && m.confirm.as_ref() != Some(&target) {
        let shown = target
            .file_name()
            .map_or_else(|| name.clone(), |n| n.to_string_lossy().into_owned());
        m.message = Some(format!("“{shown}” exists — press Save again to replace it"));
        m.confirm = Some(target);
        show_status(m, ui);
        return Outcome::Stay;
    }
    Outcome::Done(vec![target])
}

/// End the dialog: hand `result` to `on_done` (if it has not run yet)
/// and close the window (if it is still open).
///
/// Every way out goes through here, including the window's close
/// handler, which is what makes `on_done` exactly-once: the callback is
/// taken out of the model before it runs, and the close handler that
/// `remove_window` triggers finds it gone.
fn finish<S: 'static>(s: &mut S, ui: &mut Ui<S>, root: WidgetId, result: Option<Vec<PathBuf>>) {
    let Some((cb, win)) = with_model(ui, root, |m, ui| {
        drop_scan(m, ui);
        (m.on_done.take(), m.win)
    }) else {
        return;
    };
    if let Some(cb) = cb {
        cb(s, ui, result);
    }
    if ui.has_window(win)
        && let Err(e) = ui.remove_window(s, win)
    {
        eprintln!("nitro-ui: closing the file picker: {e}");
    }
}

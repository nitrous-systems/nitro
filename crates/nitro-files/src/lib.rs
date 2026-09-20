//! `nitro-files` — a file manager, and the app the virtualised
//! [`List`](nitro_ui::List) was written for.
//!
//! ```text
//! ┌──────────────┬────────────────────────────────────────────┐
//! │ Files        │ [←] [↑] [ /home/kaspar/src               ] │  back, up, path
//! │              ├────────────────────────────────────────────┤
//! │  PLACES      │ ▸ nitro                            <dir>   │
//! │ ▣ Home       │ ▸ old                              <dir>   │  the List
//! │   Documents  │   notes.txt                        912 B   │
//! │ ─────────    │   photo.png                        4.2 MB  │
//! │   Root       ├────────────────────────────────────────────┤
//! │   Trash      │ 4 items, 1 selected                        │  status line
//! └──────────────┴────────────────────────────────────────────┘
//! ```
//!
//! The window is the split-view blueprint (`nitro_ui::split`,
//! `docs/ui.md`): a sidebar of [`places`] on the left, the list and its
//! header on the right.
//!
//! The model is [`dir`], [`mime`], [`trash`], [`ops`] and [`places`] — five modules
//! with no widget in them, tested without a display server. This file is
//! the other half: the tree, the keys and the three things that make a
//! file manager different from a list of strings.
//!
//! # A directory is not read on the event loop
//!
//! `/usr/bin` is two thousand entries and a `stat` each; a directory on
//! a sleeping NFS mount is a `read_dir` that returns in thirty seconds.
//! Doing either between two `epoll_wait`s would freeze the window, so a
//! directory of more than [`dir::BIG_DIR`] entries is read on a
//! **thread**, and the result arrives through a pipe registered with
//! [`Ui::add_fd`]. The toolkit has no thread integration of its own and
//! does not need one: a descriptor is already something the loop waits
//! on, so "long work off the loop" is a worker plus a pipe plus
//! `add_fd`, and `docs/ui.md` records it as the general pattern.
//!
//! Small directories are read inline, because a thread and a wakeup cost
//! more than reading forty entries.
//!
//! # The listing refreshes itself
//!
//! An inotify watch on the directory on screen is registered the same
//! way, so a `touch` in a terminal appears here without a poll, a timer
//! or a refresh key. When nothing is happening the app sits in
//! `epoll_wait` with two extra descriptors in the set and sends nothing
//! at all — the same idle contract every other nitro app has.
//!
//! # Opening a file is the launcher's job, not ours
//!
//! The extension picks a MIME type (a built-in table, plus the system's
//! `globs2` when it is installed), the type picks a `.desktop` id
//! (`mimeapps.list`, then `mimeinfo.cache`), and the id picks an argv —
//! which is then handed to [`nitro_launcher::spawn`], the same code the
//! launcher starts applications with. Not a copy of it: it already
//! detaches the child into its own process group, drops the privileged
//! socket from its environment and reaps it through a pidfd, and a
//! second implementation would be a second place for those to be
//! forgotten.
//!
//! # Everything is addressable
//!
//! ```text
//! hey nitro-files set path value /tmp     # navigate
//! hey nitro-files get list text           # the visible rows
//! hey nitro-files do list activate        # enter the selected row
//! hey nitro-files get status value        # "4 items, 1 selected"
//! ```
//!
//! The list answers its **visible** rows rather than its model, which is
//! the honest answer for a widget that draws a screenful of a hundred
//! thousand: see [`nitro_ui::List`].

pub mod dir;
pub mod mime;
pub mod ops;
pub mod places;
pub mod trash;

use std::path::{Path, PathBuf};

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::{Handled, KeyEvent, key, mods};
use nitro_ui::split::{SidebarRow, sidebar_row, sidebar_section, sidebar_separator, split_view};
use nitro_ui::widgets::{Button, Label, TextField, button, column, label, text_field};
use nitro_ui::{App, Error, List, Row, Ui, WidgetId};

use dir::{Entry, Kind, Sort};

/// The app id and the name `hey` addresses. **Not** the window title:
/// the bar's icon rule and `StartupWMClass` in `deploy/nitro-files.desktop`
/// key on this, so it has to stay the binary name; what the user reads is
/// [`title_for`].
pub const APP_NAME: &str = "nitro-files";

/// The human name, matching `Name=` in the `.desktop` file — what the
/// window is called when the directory has no name of its own (`/`).
pub const TITLE: &str = "Files";

/// The window title for a directory: its basename, or [`TITLE`] when it
/// has none. Finder-style — `src`, not `/home/k/src` and not
/// `src — Files` — because the bar has one line and the path bar already
/// says where you are.
///
/// `cwd` is always normalised by the time it gets here: [`navigate`] only
/// sees [`dir::resolve`] output or the process's `current_dir`, both
/// lexically normal and absolute, so `file_name()` is `None` only for `/`.
#[must_use]
pub fn title_for(cwd: &Path) -> String {
    cwd.file_name()
        .map_or_else(|| TITLE.to_owned(), |n| n.to_string_lossy().into_owned())
}

/// Initial window size: wide enough for a name, a size and a date.
pub const WIDTH: f32 = 860.0;
/// Initial window height.
pub const HEIGHT: f32 = 520.0;
/// How many directories Back remembers.
pub const HISTORY: usize = 64;

/// The names every widget carries, so `hey` can address them.
///
/// Constants rather than string literals at the use sites because these
/// are a **public interface**: a script, a test and this file must agree
/// on them, and a typo in one of the three would be a widget nothing can
/// find.
pub mod names {
    /// The editable path bar.
    pub const PATH: &str = "path";
    /// The "up one directory" button.
    pub const UP: &str = "up";
    /// The "back to the previous directory" button.
    pub const BACK: &str = "back";
    /// The sidebar's rows column (`nitro_ui::split::names::SIDEBAR`
    /// renamed): a place is `places/place_<key>`, e.g. `places/place_home`.
    pub const PLACES: &str = "places";
    /// The prefix of a place row's name: `place_home`, `place_root`, …
    pub const PLACE_PREFIX: &str = "place_";
    /// The file list.
    pub const LIST: &str = "list";
    /// The status line at the bottom.
    pub const STATUS: &str = "status";
    /// The rename / new-folder field.
    pub const EDIT: &str = "edit";
}

/// A pending question the status line is asking, waiting for `y`/`n`.
///
/// A one-line confirm rather than a modal dialog, and that is a design
/// decision rather than a shortcut: the toolkit has no modal windows, a
/// dialog would need one, and a file manager whose delete key can be
/// answered without leaving the keyboard is the better interaction
/// anyway. The status line says what will happen and the next key
/// decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Move these paths to the trash.
    Trash(Vec<PathBuf>),
}

impl Confirm {
    /// The question, as the status line asks it.
    #[must_use]
    pub fn prompt(&self) -> String {
        match self {
            Confirm::Trash(paths) => match paths.as_slice() {
                [one] => format!(
                    "Move {} to the trash? [y/n]",
                    one.file_name().unwrap_or_default().to_string_lossy()
                ),
                many => format!("Move {} items to the trash? [y/n]", many.len()),
            },
        }
    }
}

/// What the app is doing with the row the user is editing.
///
/// `F2` swaps a text field into the layout below the list rather than
/// into the row itself: a row is not a widget here (that is the whole
/// point of the virtualised list), so there is nothing to swap *into*.
/// The field is placed where the eye already is — under the list, above
/// the status line — and carries the old name, which is what an inline
/// rename actually gives you.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Editing {
    /// Nothing; the edit row is hidden.
    None,
    /// Renaming the file at this path.
    Rename(PathBuf),
    /// Creating a new folder in the current directory.
    NewFolder,
}

/// The app's state.
pub struct Files {
    /// The directory on screen.
    cwd: PathBuf,
    /// Everything in it, in the order it was read and sorted.
    entries: Vec<Entry>,
    /// Whether dotfiles are shown (`Ctrl+H`).
    hidden: bool,
    /// The sort order (`Ctrl+S` cycles).
    sort: Sort,
    /// The last thing worth telling the user, shown in the status line
    /// until the next navigation replaces it.
    message: Option<String>,
    /// A question waiting for `y`/`n`.
    confirm: Option<Confirm>,
    /// The inline edit in progress.
    editing: Editing,
    /// Paths copied with `Ctrl+C`, pasted with `Ctrl+V`.
    ///
    /// The app's own clipboard, and **only** the app's: there is no
    /// clipboard protocol yet, so copying here cannot be pasted into
    /// another program and a file copied in another program cannot be
    /// pasted here. Recorded in `docs/files.md`.
    clipboard: Vec<PathBuf>,
    /// The system MIME glob table, loaded once at start.
    globs: Vec<mime::Glob>,
    /// Where the `.desktop` associations live.
    assoc: mime::Assoc,
    /// The trash.
    trash: trash::Trash,
    /// Launched children, reaped through their pidfds.
    children: nitro_launcher::spawn::Children,
    /// The background scan in flight, if any, and the hook watching it.
    scan: Option<(dir::Scan, nitro_ui::FdToken)>,
    /// The inotify watch on `cwd`, and its hook.
    watch: Option<(std::os::fd::OwnedFd, nitro_ui::FdToken)>,
    /// The widget ids, filled in by [`build`].
    ids: Option<Ids>,
    /// How many directories have been listed, for the tests and for
    /// `hey nitro-files get window value`.
    listings: u64,
    /// The terminal binary the text fallback opens an editor in.
    term: String,
    /// The sidebar's places, in row order.
    places: Vec<places::Place>,
    /// The sidebar rows, one per place, found by [`start`].
    place_rows: Vec<WidgetId>,
    /// Where Back goes: the directories left behind, newest last.
    history: Vec<PathBuf>,
}

impl Files {
    /// A file manager showing `cwd`, with the real environment's MIME
    /// tables and trash.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            entries: Vec::new(),
            hidden: false,
            sort: Sort::Name,
            message: None,
            confirm: None,
            editing: Editing::None,
            clipboard: Vec::new(),
            globs: mime::load_globs2(Path::new("/usr/share/mime/globs2")),
            assoc: mime::Assoc::from_env(),
            trash: trash::Trash::from_env(),
            children: nitro_launcher::spawn::Children::new(),
            scan: None,
            watch: None,
            ids: None,
            listings: 0,
            term: term_binary(),
            places: Vec::new(),
            place_rows: Vec::new(),
            history: Vec::new(),
        }
    }

    /// The same, in the process's working directory.
    #[must_use]
    pub fn here() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
    }

    /// Point the MIME lookup and the trash somewhere else.
    ///
    /// For the tests, which must not read the developer's own
    /// `~/.config` and must not put anything in their real trash. An
    /// app that could only be tested by mutating the environment of a
    /// threaded test binary could not be tested at all.
    #[must_use]
    pub fn with_env(
        mut self,
        globs: Vec<mime::Glob>,
        assoc: mime::Assoc,
        trash: trash::Trash,
    ) -> Self {
        self.globs = globs;
        self.assoc = assoc;
        self.trash = trash;
        self
    }

    /// The sidebar's places. The default is [`places::from_env`] with the
    /// trash's root, resolved when the tree is built; a test injects a
    /// fixture so the row set does not depend on the developer's home.
    #[must_use]
    pub fn with_places(mut self, places: Vec<places::Place>) -> Self {
        self.places = places;
        self
    }

    /// The sidebar's places, in row order.
    #[must_use]
    pub fn places(&self) -> &[places::Place] {
        &self.places
    }

    /// The directories Back would go to, newest last.
    #[must_use]
    pub fn history(&self) -> &[PathBuf] {
        &self.history
    }

    /// Use `term` as the terminal the text fallback opens an editor in.
    #[must_use]
    pub fn with_term(mut self, term: impl Into<String>) -> Self {
        self.term = term.into();
        self
    }

    /// The directory on screen.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Everything in it, hidden files included.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The rows actually shown, in order.
    #[must_use]
    pub fn shown(&self) -> Vec<&Entry> {
        dir::visible(&self.entries, self.hidden)
    }

    /// Whether dotfiles are shown.
    #[must_use]
    pub fn shows_hidden(&self) -> bool {
        self.hidden
    }

    /// The sort order.
    #[must_use]
    pub fn sort_order(&self) -> Sort {
        self.sort
    }

    /// The status line's text.
    #[must_use]
    pub fn status(&self) -> String {
        if let Some(c) = &self.confirm {
            return c.prompt();
        }
        if let Some(m) = &self.message {
            return m.clone();
        }
        String::new()
    }

    /// The last message, without the confirm prompt in front of it.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The question waiting for an answer, if any.
    #[must_use]
    pub fn pending_confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }

    /// The inline edit in progress.
    #[must_use]
    pub fn editing(&self) -> &Editing {
        &self.editing
    }

    /// The paths `Ctrl+C` remembered.
    #[must_use]
    pub fn clipboard(&self) -> &[PathBuf] {
        &self.clipboard
    }

    /// How many directories have been listed.
    #[must_use]
    pub fn listings(&self) -> u64 {
        self.listings
    }

    /// Launched children that have not been reaped.
    #[must_use]
    pub fn children(&self) -> &nitro_launcher::spawn::Children {
        &self.children
    }

    /// Whether a background scan is in flight.
    #[must_use]
    pub fn scanning(&self) -> bool {
        self.scan.is_some()
    }
    /// The background scan's wakeup descriptor and its hook, if one is
    /// in flight.
    ///
    /// For a test driving the loop by hand: the harness runs the `Ui` on
    /// the test thread and has no `epoll`, so a test `poll`s this
    /// descriptor exactly as `epoll_wait` would and then calls
    /// [`nitro_ui::Ui::run_fd`] with the token — the same pair of steps
    /// the app loop takes on a wakeup, with the shipped dispatch after
    /// it. Without it a test could only fake the *whole* handoff, which
    /// is the half that is worth asserting.
    #[must_use]
    pub fn scan_hook(&self) -> Option<(std::os::fd::BorrowedFd<'_>, nitro_ui::FdToken)> {
        self.scan
            .as_ref()
            .map(|(scan, token)| (scan.as_fd(), *token))
    }

    /// The inotify watch's descriptor and its hook, if the watch is
    /// armed. Driven by a test exactly as [`Files::scan_hook`] is.
    #[must_use]
    pub fn watch_hook(&self) -> Option<(std::os::fd::BorrowedFd<'_>, nitro_ui::FdToken)> {
        use std::os::fd::AsFd as _;

        self.watch.as_ref().map(|(fd, token)| (fd.as_fd(), *token))
    }

    /// The full path of the row at `index` of the *shown* rows.
    #[must_use]
    pub fn path_at(&self, index: usize) -> Option<PathBuf> {
        self.shown().get(index).map(|e| self.cwd.join(&e.name))
    }

    /// The entry at `index` of the shown rows.
    #[must_use]
    pub fn entry_at(&self, index: usize) -> Option<&Entry> {
        self.shown().get(index).copied()
    }

    /// The rows as the list widget takes them.
    ///
    /// A directory gets `<dir>` for its size; everything else gets its
    /// size and its date. The icon column is a **name**, not a glyph:
    /// `folder-fill` for a directory, one of the `file-earmark-*` family
    /// for a file, `hdd` for a device node — and the server draws the
    /// artwork, at the output's scale, in the colour the palette's `Text`
    /// role currently names (`docs/icons.md`).
    ///
    /// **The icon is not computed here.** It is
    /// [`dir::Entry::icon`](crate::dir::Entry::icon), resolved once when
    /// the listing was read, because resolving it means a MIME glob match
    /// per file and this function runs again every time the selection
    /// moves. What is left here is formatting.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        self.shown()
            .into_iter()
            .map(|e| {
                let detail = match e.kind {
                    Kind::Dir => "<dir>".to_owned(),
                    // The link marker stays in the *detail* column now
                    // that the icon column says what the target is: a
                    // symlinked directory and a real one draw the same
                    // folder, and `→` is the only thing that
                    // distinguishes them. Dropping it would make a link
                    // indistinguishable from what it points at.
                    Kind::Symlink if e.symlink_dir => "→ <dir>".to_owned(),
                    Kind::Symlink => {
                        format!("→ {}   {}", dir::format_size(e), dir::format_mtime(e.mtime))
                    }
                    _ => format!("{}   {}", dir::format_size(e), dir::format_mtime(e.mtime)),
                };
                Row::new(e.name.clone()).icon(e.icon).detail(detail)
            })
            .collect()
    }
}

/// Which terminal the text fallback opens an editor in.
///
/// `nitro-term` next to this binary if there is one, else the bare name
/// and `PATH`'s opinion: a deployed `~/nitro-bin` and a `target/debug`
/// build should each find *their own* terminal rather than yesterday's
/// copy of the other's, which is the same reasoning
/// [`nitro_launcher::spawn::exe_dir`] exists for.
fn term_binary() -> String {
    nitro_launcher::spawn::exe_dir()
        .map(|d| d.join("nitro-term"))
        .filter(|p| p.exists())
        .map_or_else(|| "nitro-term".to_owned(), |p| p.display().to_string())
}

/// The widgets the app writes to.
///
/// Found from the tree rather than stored by [`build`], for the reason
/// `nitro_term::grid_of` exists: the builder has a `&mut Ui` and no
/// state, so there is nowhere to put them until [`start`] runs with
/// both. The lookup is by **name**, which is the same addressing `hey`
/// uses — so a widget that a script can find is a widget the app can
/// find, and a renamed widget breaks both at once rather than one
/// silently.
#[derive(Debug, Clone, Copy)]
pub struct Ids {
    /// The path bar.
    pub path: WidgetId,
    /// The list.
    pub list: WidgetId,
    /// The status line.
    pub status: WidgetId,
    /// The rename / new-folder field, hidden unless something is being
    /// edited.
    pub edit: WidgetId,
    /// The back button.
    pub back: WidgetId,
}

impl Ids {
    /// Locate the four widgets in a tree [`build`] made.
    ///
    /// `None` if any of them is missing, which in practice means the
    /// tree was not built by [`build`].
    #[must_use]
    pub fn of(ui: &Ui<Files>) -> Option<Ids> {
        let find = |name: &str| nitro_ui::introspect::resolve(ui, name);
        Some(Ids {
            path: find(names::PATH)?,
            list: find(names::LIST)?,
            status: find(names::STATUS)?,
            edit: find(names::EDIT)?,
            back: find(names::BACK)?,
        })
    }
}

/// Build the tree.
///
/// # Panics
/// Never in practice: every `attach` names an id built a few lines
/// above, and a fresh id cannot be stale.
// One function because it is one tree, and the order the widgets are
// created in is the order they are read in — the same allow
// `nitro-settings::build` takes for the same reason.
#[allow(clippy::too_many_lines)]
pub fn build(ui: &mut Ui<Files>) -> WidgetId {
    // The tree is built before any callback has seen the state, so the
    // path bar starts with the process's directory; [`start`] writes the
    // real one with the first listing, a few microseconds later.
    let start = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("/"))
        .display()
        .to_string();
    let path = ui.build(
        text_field(start)
            .name(names::PATH)
            .placeholder("Path")
            .grow(1.0)
            .height(34.0)
            .on_submit(|s: &mut Files, ui: &mut Ui<Files>, text: &str| {
                // Deferred, like every callback in this file that writes
                // back to the widget it came from: `navigate` rewrites
                // this very field with the normalised path, and a write
                // from inside the field's own dispatch would land in an
                // `Error::Busy` nobody reads. See `Ui::defer`.
                let to = dir::resolve(text, &s.cwd.clone());
                ui.defer(move |s: &mut Files, ui: &mut Ui<Files>| navigate(s, ui, to));
            })
            // A change navigates **only when the field is not focused**,
            // and that distinction is what makes `hey nitro-files set
            // path value /tmp` work without breaking typing.
            //
            // `set <prop>` goes through the widget's `set_<prop>` action,
            // which is the `WidgetMut` setter, which fires `on_change` —
            // the same callback a keystroke fires, because the toolkit's
            // whole point is that a script and a user take one path.
            // Navigating on every change would therefore navigate on
            // every letter: typing `/home/kaspar` would jump to `/home`
            // at the fifth character and rewrite the field underneath the
            // caret. Refusing to navigate at all would mean a path bar a
            // script cannot drive, which the spec asks for by name.
            //
            // Focus is the honest discriminator, not a heuristic: a user
            // typing has the caret in this field by definition, and a
            // `set` from outside moves no focus (`Ui::action` runs the
            // setter and nothing else). So the field navigates when it is
            // written to from outside, and waits for Enter when it is
            // being typed into.
            .on_change(|s: &mut Files, ui: &mut Ui<Files>, text: &str| {
                let Some(ids) = s.ids else { return };
                if ui.focused() == Some(ids.path) {
                    return;
                }
                let to = dir::resolve(text, &s.cwd.clone());
                if to == s.cwd {
                    return;
                }
                ui.defer(move |s: &mut Files, ui: &mut Ui<Files>| navigate(s, ui, to));
            }),
    );
    let up = ui.build(button("↑").name(names::UP).icon("arrow-up").on_click(
        |s: &mut Files, ui: &mut Ui<Files>| {
            let to = dir::parent_of(&s.cwd.clone());
            navigate(s, ui, to);
        },
    ));
    let back = ui.build(
        button("←")
            .name(names::BACK)
            .icon("arrow-left")
            .disabled()
            .on_click(|s: &mut Files, ui: &mut Ui<Files>| {
                // A pop, not a push: going back must not put the
                // directory being left onto the history it just came
                // from. Deferred, because `navigate` disables this very
                // button when the history runs out. A directory that has
                // gone since is a message in the status line, and the
                // entry is consumed rather than kept: retrying it would
                // only say the same thing again.
                if let Some(to) = s.history.pop() {
                    ui.defer(move |s: &mut Files, ui: &mut Ui<Files>| {
                        navigate_with(s, ui, to, false);
                    });
                }
            }),
    );

    let list = ui.build(
        nitro_ui::list()
            .name(names::LIST)
            .row_inset(nitro_ui::split::SIDEBAR_ROW_INSET)
            .row_radius(6.0)
            .grow(1.0)
            .width_percent(1.0)
            .on_activate(|s: &mut Files, ui: &mut Ui<Files>, index: usize| {
                // **Deferred, and this is the one that proves the rule.**
                // Activating a row means "show a different directory in
                // this list", so the work writes rows into the very
                // widget whose callback is running — which take-out
                // dispatch has moved out of its slot. Called inline it
                // updated the path bar and left the old rows on screen,
                // because `widget_mut` answered `Error::Busy` and the
                // app dropped it. `Ui::defer` runs it a moment later
                // with the tree whole.
                let _ = s;
                ui.defer(move |s: &mut Files, ui: &mut Ui<Files>| activate(s, ui, index));
            }),
    );

    // The edit field is built once and kept, rather than created and
    // destroyed around every rename: a widget that comes and goes costs
    // a tree pass and a scene node each way, and hiding it is a style
    // change. It is `height(0)` when idle, which the flex solver honours
    // exactly.
    let edit = ui.build(
        text_field("")
            .name(names::EDIT)
            .placeholder("name")
            .width_percent(1.0)
            .height(0.0)
            .on_submit(|s: &mut Files, ui: &mut Ui<Files>, text: &str| {
                // Deferred: `commit_edit` closes this field (clears it
                // and takes its height back to zero), which is this
                // field writing to itself.
                let _ = s;
                let text = text.to_owned();
                ui.defer(move |s: &mut Files, ui: &mut Ui<Files>| commit_edit(s, ui, &text));
            }),
    );

    let status = ui.build(
        label("")
            .name(names::STATUS)
            .size(nitro_ui::split::SMALL_PX)
            .color_role(nitro_ui::ColorRole::TextDim)
            .width_percent(1.0),
    );
    let footer_line = ui.build(
        nitro_ui::widgets::separator()
            .color_role(nitro_ui::ColorRole::Hairline)
            .width_percent(1.0),
    );
    let footer_pad = ui.build(
        column()
            .width_percent(1.0)
            .padding_xy(nitro_ui::split::CONTENT_GUTTER, 6.0),
    );
    ui.attach(footer_pad, status)
        .expect("attach the status line");
    let footer = ui.build(column().width_percent(1.0));
    for child in [footer_line, footer_pad] {
        ui.attach(footer, child).expect("attach the footer");
    }

    // The body: the list scrolls itself, so it is not in a `Scroll`; the
    // edit field sits under it at height 0 until a rename opens it.
    let body = ui.build(
        column()
            .gap(6.0)
            .padding_xy(nitro_ui::split::SIDEBAR_ROW_INSET, 0.0)
            .width_percent(1.0),
    );
    for child in [list, edit] {
        ui.attach(body, child).expect("attach a child of the body");
    }

    // The sidebar holds only its section header here: the rows come
    // from the *state's* places, which `build` cannot see, so `start`
    // attaches them (`sync_places`) before the first commit — `run` and
    // the harness both call `start` before flushing. Building a default
    // set here and replacing it later was tried and rejected: two sets
    // that share their keys but not their paths are told apart by
    // nothing a row carries, and a test fixture's `place_documents`
    // then navigated to the developer's real `~/Documents`.
    let view = split_view()
        .sidebar_name(names::PLACES)
        .sidebar_header(TITLE)
        .sidebar_child(sidebar_section("Places"))
        .content_header_leading_id(back)
        .content_header_leading_id(up)
        .content_header_leading_id(path)
        .content_id(body)
        .content_footer_id(footer);
    let parts = view.build(ui);
    install(ui);
    parts.root
}

/// One sidebar row for a place. Its `on_click` navigates — deferred,
/// because `navigate` selects the row that was clicked.
fn place_row(p: &places::Place) -> nitro_ui::split::SidebarRowBuilder<Files> {
    let to = p.path.clone();
    sidebar_row(p.icon, p.label.clone())
        .name(format!("{}{}", names::PLACE_PREFIX, p.key))
        .on_click(move |_s: &mut Files, ui: &mut Ui<Files>| {
            let to = to.clone();
            ui.defer(move |s: &mut Files, ui: &mut Ui<Files>| navigate(s, ui, to));
        })
}

/// Build the sidebar rows from `s.places`, and remember their ids.
///
/// Called by [`start`], and **unconditionally**: whatever rows the
/// sidebar holds (none, after `build`; a previous set, if `start` ran
/// twice) go, and the state's places come back. A row's `on_click`
/// captures its path, so a row can only be trusted to go where
/// `s.places` says if it was built from `s.places`.
fn sync_places(s: &mut Files, ui: &mut Ui<Files>) {
    let Some(sidebar) = nitro_ui::introspect::resolve(ui, names::PLACES) else {
        return;
    };
    // Everything after the section header goes.
    for c in ui.children(sidebar).into_iter().skip(1) {
        let _ = ui.remove(c);
    }
    s.place_rows.clear();
    let mut prev = places::Section::Places;
    for p in &s.places {
        if p.section != prev {
            let _ = ui.add_child(sidebar, sidebar_separator());
            prev = p.section;
        }
        if let Ok(row) = ui.add_child(sidebar, place_row(p)) {
            s.place_rows.push(row);
        }
    }
}

/// Select the sidebar row whose place is `cwd`, and deselect the rest.
fn refresh_places(s: &Files, ui: &mut Ui<Files>) {
    for (row, place) in s.place_rows.iter().zip(&s.places) {
        if let Ok(mut r) = ui.widget_mut::<SidebarRow<Files>>(*row) {
            r.set_selected(place.path == s.cwd);
        }
    }
}

/// The shortcuts and the window title.
///
/// Separate from [`run`] so a test installs the same handlers on a
/// harness-built tree rather than a copy of them — the bug that idiom
/// prevents is a shortcut that works in the app and not in the test, or
/// the reverse.
///
/// # A pending confirm swallows the shortcuts too
///
/// Every `Ctrl+…` closure below early-returns while `s.confirm.is_some()`
/// (issue #559). `nitro-ui` offers app-level handlers in **registration
/// order**, and the shortcuts are registered ahead of [`app_key`], which is
/// where the pending-confirm swallow lives — so without this a `Ctrl+N` with
/// a delete confirm on screen would open the new-folder field, focus it, and
/// on commit hand focus back to the list, where `y`/`n` are eaten as
/// type-ahead and the question can no longer be answered. That is the same
/// bug the confirm-blur fixed, through the other door.
///
/// The alternative was to register `ui.on_key(app_key)` **first** so the
/// swallow outranks the shortcuts. It is two lines rather than five, and it
/// was not taken: it changes the app-level ordering for *every* key, not just
/// while a confirm is pending, so it trades a narrow fix for a global
/// behaviour change in the handler this file relies on seeing keys last.
fn install(ui: &mut Ui<Files>) {
    ui.set_shortcut(
        mods::CTRL,
        key::H,
        move |s: &mut Files, ui: &mut Ui<Files>| {
            if s.confirm.is_some() {
                return;
            }
            s.hidden = !s.hidden;
            refresh_rows(s, ui);
        },
    );
    ui.set_shortcut(
        mods::CTRL,
        key::S,
        move |s: &mut Files, ui: &mut Ui<Files>| {
            if s.confirm.is_some() {
                return;
            }
            s.sort = s.sort.next();
            dir::sort(&mut s.entries, s.sort);
            s.message = Some(format!("sorted by {}", sort_name(s.sort)));
            refresh_rows(s, ui);
        },
    );
    ui.set_shortcut(
        mods::CTRL,
        key::N,
        move |s: &mut Files, ui: &mut Ui<Files>| {
            if s.confirm.is_some() {
                return;
            }
            start_edit(s, ui, Editing::NewFolder, "");
        },
    );
    ui.set_shortcut(
        mods::CTRL,
        key::C,
        move |s: &mut Files, ui: &mut Ui<Files>| {
            if s.confirm.is_some() {
                return;
            }
            copy_selection(s, ui);
        },
    );
    ui.set_shortcut(
        mods::CTRL,
        key::V,
        move |s: &mut Files, ui: &mut Ui<Files>| {
            if s.confirm.is_some() {
                return;
            }
            paste(s, ui);
        },
    );
    // The rest are plain keys, and they have to be offered *after* the
    // focused widget has had them: `y` is an answer to a confirm only
    // when the path bar is not the thing being typed into, and the
    // toolkit's ordering gives that for free — `on_key` handlers see
    // only what the focused chain declined.
    ui.on_key(move |s: &mut Files, ui: &mut Ui<Files>, k: &KeyEvent| app_key(s, ui, k));
}

/// The name of a sort order, for the status line.
fn sort_name(sort: Sort) -> &'static str {
    match sort {
        Sort::Name => "name",
        Sort::Size => "size",
        Sort::Mtime => "date",
    }
}

/// Keys the focused widget did not want: the confirm answer, `F2`,
/// `Delete`, and `Escape`.
///
/// # Why the confirm arm can be reached at all
///
/// App-level handlers are offered only what the focused chain declined,
/// which is the right order for everything else in this file and was
/// very nearly fatal here. The list answers `Handled::Yes` to any
/// printable key it can type-ahead with, so with the list focused an
/// `n` is *not* "no" — it is a jump to the first row beginning with
/// `n`. On the box the prompt "Move notes.txt to the trash? [y/n]" ate
/// its own `n` (the file starts with one) and would have accepted `y`
/// only because no row happened to start with `y`: a confirmation whose
/// meaning depended on the file names in the directory.
///
/// So a pending confirm **takes the keyboard**, by dropping the focus
/// when the question is asked ([`ask`]) and giving it back when it is
/// answered. With nothing focused, keys bubble from the root and reach
/// this handler first, which is what makes a one-line prompt behave
/// like a dialog without being one.
fn app_key(s: &mut Files, ui: &mut Ui<Files>, k: &KeyEvent) -> Handled {
    if s.confirm.is_some() {
        return match k.keycode {
            key::Y => {
                let c = s.confirm.take();
                if let Some(c) = c {
                    run_confirm(s, ui, &c);
                }
                restore_focus(s, ui);
                Handled::Yes
            }
            key::N | key::ESC => {
                s.confirm = None;
                s.message = Some("cancelled".to_owned());
                restore_focus(s, ui);
                show_status(s, ui);
                Handled::Yes
            }
            // A key that is neither is ignored rather than passed on: a
            // question on screen that the next keystroke silently
            // dismissed would be worse than one that waits.
            _ => Handled::Yes,
        };
    }
    match k.keycode {
        key::F2 => {
            let Some(ids) = s.ids else {
                return Handled::No;
            };
            let index = ui
                .widget::<List<Files>>(ids.list)
                .map(List::cursor)
                .unwrap_or_default();
            let Some(path) = s.path_at(index) else {
                return Handled::No;
            };
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            start_edit(s, ui, Editing::Rename(path), &name);
            Handled::Yes
        }
        key::DELETE => {
            let paths = selected_paths(s, ui);
            if paths.is_empty() {
                return Handled::No;
            }
            ask(s, ui, Confirm::Trash(paths));
            Handled::Yes
        }
        key::ESC if s.editing != Editing::None => {
            cancel_edit(s, ui);
            Handled::Yes
        }
        _ => Handled::No,
    }
}

/// Ask a question in the status line, and take the keyboard while it is
/// unanswered.
///
/// Dropping the focus is the whole mechanism: see [`app_key`] for the
/// box run that found out what happens without it.
pub fn ask(s: &mut Files, ui: &mut Ui<Files>, what: Confirm) {
    s.confirm = Some(what);
    ui.blur(s);
    show_status(s, ui);
}

/// Give the keyboard back to the list once a question is answered.
fn restore_focus(s: &mut Files, ui: &mut Ui<Files>) {
    if let Some(ids) = s.ids {
        ui.focus(ids.list);
        ui.deliver_focus_events(s);
    }
}

/// Every selected row's path, or the cursor's when nothing is selected.
fn selected_paths(s: &Files, ui: &Ui<Files>) -> Vec<PathBuf> {
    let Some(ids) = s.ids else {
        return Vec::new();
    };
    let Ok(list) = ui.widget::<List<Files>>(ids.list) else {
        return Vec::new();
    };
    let mut indices = list.selection();
    if indices.is_empty() {
        indices.push(list.cursor());
    }
    indices.iter().filter_map(|i| s.path_at(*i)).collect()
}

/// Run a confirmed operation.
fn run_confirm(s: &mut Files, ui: &mut Ui<Files>, what: &Confirm) {
    match what {
        Confirm::Trash(paths) => {
            let mut done = 0usize;
            let mut failure = None;
            for p in paths {
                match s.trash.send(p) {
                    Ok(_) => done += 1,
                    Err(e) => failure = Some(format!("{}: {}", short(p), trash_error(&e))),
                }
            }
            s.message = Some(match failure {
                Some(e) => e,
                None if done == 1 => "moved 1 item to the trash".to_owned(),
                None => format!("moved {done} items to the trash"),
            });
            relist(s, ui);
        }
    }
}

/// A trash failure as a sentence rather than as an errno.
///
/// `EXDEV` is the one that needs translating, and the box run is why it
/// is here: trashing a file under `/tmp` on a box whose home is a
/// different filesystem put "Invalid cross-device link (os error 18)"
/// in the status line — an accurate message that tells a user nothing
/// about what happened or what to do about it. The behaviour is
/// deliberate (see [`trash::Trash::send`]: copy-then-delete wearing the
/// same name is a different operation, and the spec's answer is a trash
/// on the other filesystem, which this does not have), so the fix is to
/// say so rather than to change it.
fn trash_error(e: &std::io::Error) -> String {
    if e.raw_os_error() == Some(18) {
        return "on another filesystem; only the home trash is supported".to_owned();
    }
    e.to_string()
}

/// A path's file name, for a one-line message.
fn short(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

// -- navigation -------------------------------------------------------

/// Show `to`, or say in the status line why it cannot be shown.
///
/// The one entry point for changing directory: the path bar, the up
/// button, Enter on a directory and `hey set path value` all go through
/// it, so there is exactly one place that re-lists, re-arms the watch
/// and resets the selection.
pub fn navigate(s: &mut Files, ui: &mut Ui<Files>, to: PathBuf) {
    navigate_with(s, ui, to, true);
}

/// [`navigate`], with a say in whether the directory being left is
/// pushed onto the Back history. Back itself passes `false`.
fn navigate_with(s: &mut Files, ui: &mut Ui<Files>, to: PathBuf, remember: bool) {
    // A path that is not a directory is a message, not a state change:
    // leaving the user in the directory they could see is better than
    // showing them an empty list of somewhere that does not exist.
    match std::fs::metadata(&to) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            s.message = Some(format!("not a directory: {}", to.display()));
            show_status(s, ui);
            return;
        }
        Err(e) => {
            s.message = Some(format!("{}: {e}", to.display()));
            show_status(s, ui);
            return;
        }
    }
    if remember && s.listings > 0 && s.cwd != to {
        s.history.push(std::mem::replace(&mut s.cwd, to));
        if s.history.len() > HISTORY {
            s.history.remove(0);
        }
    } else {
        s.cwd = to;
    }
    s.message = None;
    if let Some(ids) = s.ids
        && let Ok(mut b) = ui.widget_mut::<Button<Files>>(ids.back)
    {
        b.set_enabled(!s.history.is_empty());
    }
    refresh_places(s, ui);
    // The title follows the directory, and this is the only place it is
    // set — so every way of getting somewhere agrees on what it is
    // called. The setter drops an unchanged title, and `run` opens the
    // window with this directory's title already, so the first pass here
    // sends nothing.
    let _ = ui.set_window_title(title_for(&s.cwd));
    cancel_edit(s, ui);
    // Rewrite the path bar with the **normalised** path, whatever the
    // user typed: submitting `~/src/../src/` should leave `/home/…/src`
    // in the bar, because the bar is a statement about where you are
    // rather than a record of what you pressed. The setter drops an
    // unchanged string, so navigating from the list costs nothing here.
    if let Some(ids) = s.ids {
        let shown = s.cwd.display().to_string();
        match ui.widget_mut::<TextField<Files>>(ids.path) {
            Ok(mut f) => f.set_text(shown),
            Err(e) => complain("the path bar", &e),
        }
    }
    relist(s, ui);
}

/// Re-read the current directory, inline or on a thread, and re-arm the
/// inotify watch.
pub fn relist(s: &mut Files, ui: &mut Ui<Files>) {
    arm_watch(s, ui);
    drop_scan(s, ui);
    let cwd = s.cwd.clone();
    // The count is a `getdents` walk with no `stat` at all, so asking it
    // of a fifty-thousand-entry directory costs a few milliseconds and
    // answers the only question that matters here: is reading this thing
    // going to stall the loop?
    if dir::count_at_most(&cwd, dir::BIG_DIR + 1) > dir::BIG_DIR {
        start_scan(s, ui, &cwd);
        return;
    }
    match dir::read_dir(&cwd, &s.globs) {
        Ok(mut entries) => {
            dir::sort(&mut entries, s.sort);
            s.entries = entries;
            s.listings += 1;
        }
        Err(e) => {
            s.entries.clear();
            s.message = Some(format!("{}: {e}", cwd.display()));
        }
    }
    refresh_rows(s, ui);
}

/// Start reading a big directory on a thread and register its pipe.
///
/// The list is **not** cleared while the scan runs: showing the previous
/// directory for the fifty milliseconds it takes is better than a blank
/// window, and the status line says what is happening.
fn start_scan(s: &mut Files, ui: &mut Ui<Files>, cwd: &Path) {
    let scan = match dir::Scan::start(cwd.to_path_buf(), s.sort, s.globs.clone()) {
        Ok(scan) => scan,
        Err(e) => {
            // A thread we could not start is not a reason to show
            // nothing: fall back to reading it here, slow but correct.
            s.message = Some(format!("background read failed ({e}), reading inline"));
            match dir::read_dir(cwd, &s.globs) {
                Ok(mut entries) => {
                    dir::sort(&mut entries, s.sort);
                    s.entries = entries;
                    s.listings += 1;
                }
                Err(e) => {
                    s.entries.clear();
                    s.message = Some(format!("{}: {e}", cwd.display()));
                }
            }
            refresh_rows(s, ui);
            return;
        }
    };
    let token = match ui.add_fd(scan.as_fd(), scan_ready) {
        Ok(t) => t,
        Err(e) => {
            s.message = Some(format!("cannot watch the scan: {e}"));
            return;
        }
    };
    s.message = Some(format!("reading {}…", cwd.display()));
    s.scan = Some((scan, token));
    show_status(s, ui);
}

/// The scan's pipe woke us: take the listing if it is there.
///
/// Registered with [`Ui::add_fd`], so this is called from the app loop
/// exactly where a wire message would be handled — between events, with
/// the tree settled, and with the same `&mut Files` and `&mut Ui` every
/// callback gets.
fn scan_ready(s: &mut Files, ui: &mut Ui<Files>) {
    let Some((scan, _)) = &mut s.scan else {
        return;
    };
    let Some(result) = scan.take() else {
        // A spurious wakeup, or a byte for a result already taken.
        return;
    };
    // A listing for a directory the user has already left is dropped: it
    // is not wrong, it is just answering a question nobody is asking any
    // more.
    let stale = scan.path() != s.cwd;
    drop_scan(s, ui);
    if stale {
        return;
    }
    match result {
        Ok(entries) => {
            s.entries = entries;
            s.listings += 1;
            s.message = None;
        }
        Err(e) => {
            s.entries.clear();
            s.message = Some(format!("{}: {e}", s.cwd.display()));
        }
    }
    refresh_rows(s, ui);
}

/// Forget the scan in flight and unregister its hook.
///
/// **Unregistering matters**: the loop's `epoll` is level-triggered and
/// a pipe holding an unread byte stays readable for ever, so a hook left
/// behind would spin the loop — the same hazard the launcher's pidfd
/// reaping has, and the same answer.
fn drop_scan(s: &mut Files, ui: &mut Ui<Files>) {
    if let Some((_, token)) = s.scan.take() {
        ui.remove_fd(token);
    }
}

/// Watch the current directory, so a file created elsewhere appears.
fn arm_watch(s: &mut Files, ui: &mut Ui<Files>) {
    use rustix::fs::inotify;

    if let Some((_, token)) = s.watch.take() {
        ui.remove_fd(token);
    }
    let Ok(fd) = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
    else {
        // No inotify is a file manager that does not refresh itself, not
        // a file manager that does not work.
        return;
    };
    let flags = inotify::WatchFlags::CREATE
        | inotify::WatchFlags::DELETE
        | inotify::WatchFlags::MOVED_FROM
        | inotify::WatchFlags::MOVED_TO
        | inotify::WatchFlags::ATTRIB;
    if inotify::add_watch(&fd, &s.cwd, flags).is_err() {
        return;
    }
    if let Ok(token) = ui.add_fd(std::os::fd::AsFd::as_fd(&fd), watch_fired) {
        s.watch = Some((fd, token));
    }
}

/// The directory changed under us: drain the events and re-read it.
fn watch_fired(s: &mut Files, ui: &mut Ui<Files>) {
    let Some((fd, _)) = &s.watch else {
        return;
    };
    // The events are drained but not read for meaning. Every flag we
    // registered means the same thing to this app — "the listing is out
    // of date" — and a `read_dir` is the only way to find out what it is
    // now anyway. Draining is not optional: a level-triggered `epoll`
    // over a descriptor still holding events would wake the loop for
    // ever.
    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
    let mut reader = rustix::fs::inotify::Reader::new(fd, &mut buf);
    let mut saw = false;
    while reader.next().is_ok() {
        saw = true;
    }
    if !saw {
        return;
    }
    // The watch is still armed and still on the same directory, so this
    // re-reads without re-arming: `relist` would drop and re-add the
    // watch, which is two syscalls and a window in which a change is
    // missed.
    let cwd = s.cwd.clone();
    if dir::count_at_most(&cwd, dir::BIG_DIR + 1) > dir::BIG_DIR {
        drop_scan(s, ui);
        start_scan(s, ui, &cwd);
        return;
    }
    if let Ok(mut entries) = dir::read_dir(&cwd, &s.globs) {
        dir::sort(&mut entries, s.sort);
        s.entries = entries;
        s.listings += 1;
        refresh_rows(s, ui);
    }
}

// -- the tree ---------------------------------------------------------

/// Push the model into the list and the count into the status line.
///
/// The one place the tree is written to for a listing change. What it
/// costs is a screenful: [`List::set_rows`](nitro_ui::List) bumps a
/// generation and the next paint re-emits the rows that are materialised
/// — a few dozen — whatever the directory holds.
///
/// **A write that fails is reported, not dropped.** The failure this
/// guards against has already happened once: called from inside the
/// list's own `on_activate`, the list is out of its arena slot and
/// `widget_mut` answers [`nitro_ui::Error::Busy`] — so an `if let
/// Ok(…)` here left the previous directory's rows on screen while the
/// path bar said something else, with nothing anywhere returning an
/// error anybody read. The callbacks now defer (see `Ui::defer`), so
/// this should not be reachable; [`complain`] is what makes the next
/// one audible instead of invisible.
pub fn refresh_rows(s: &mut Files, ui: &mut Ui<Files>) {
    let rows = s.rows();
    if let Some(ids) = s.ids {
        match ui.widget_mut::<List<Files>>(ids.list) {
            Ok(mut l) => l.set_rows(rows),
            Err(e) => complain("the file list", &e),
        }
    }
    show_status(s, ui);
}

/// Say that a write to the tree failed, once, on stderr.
///
/// Every write in this file goes to a widget the app built and still
/// owns, so a failure is a bug in this file rather than a condition to
/// handle: the honest response is to be loud in the journal and carry
/// on. Not `unwrap` — a file manager should not die because a label did
/// not update — and not silence, which is the bug this exists because
/// of.
fn complain(what: &str, e: &nitro_ui::Error) {
    eprintln!("nitro-files: could not update {what}: {e}");
}

/// Write the status line: the confirm prompt, the last message, or the
/// counts.
pub fn show_status(s: &mut Files, ui: &mut Ui<Files>) {
    let Some(ids) = s.ids else {
        return;
    };
    let selected = ui
        .widget::<List<Files>>(ids.list)
        .map(|l| l.selection().len())
        .unwrap_or_default();
    let counts = format!("{} items, {selected} selected", s.shown().len());
    let text = match s.status() {
        t if t.is_empty() => counts,
        t => format!("{counts} — {t}"),
    };
    match ui.widget_mut::<Label>(ids.status) {
        Ok(mut l) => l.set_text(text),
        Err(e) => complain("the status line", &e),
    }
}

// -- activation and opening -------------------------------------------

/// Enter a directory, or open a file with whatever claims it.
pub fn activate(s: &mut Files, ui: &mut Ui<Files>, index: usize) {
    let Some(entry) = s.entry_at(index).cloned() else {
        return;
    };
    let path = s.cwd.join(&entry.name);
    // A symlink's target was resolved when the listing was read (one
    // `metadata` per link, `dir::read_dir`), which is also what put a
    // folder in its icon column — so "enter the directory this points at"
    // and "show it as a folder" cannot disagree, and activating a row
    // costs no syscall at all.
    if entry.opens_a_directory() {
        navigate(s, ui, path);
        return;
    }
    open(s, ui, &path);
}

/// Open one file with its registered handler, or the text fallback.
pub fn open(s: &mut Files, ui: &mut Ui<Files>, path: &Path) {
    let what = mime::open_with(path, &s.globs, &s.assoc, &s.term);
    let (argv, how) = match what {
        mime::Open::Argv(argv) => (argv, "opened"),
        mime::Open::Editor(argv) => (argv, "editing"),
        mime::Open::None => {
            s.message = Some(format!("nothing opens {}", short(path)));
            show_status(s, ui);
            return;
        }
    };
    match s.children.spawn(&argv) {
        Ok(_) => {
            // Watch the child's pidfd, so it is reaped the moment it
            // exits rather than at the next launch. The hook needs to
            // find this `Children` inside the app state, which is what
            // the accessor is for.
            s.children.watch(ui, |s: &mut Files| &mut s.children);
            s.message = Some(format!(
                "{how} {} with {}",
                short(path),
                argv.first().map_or("?", String::as_str)
            ));
        }
        Err(e) => s.message = Some(format!("{}: {e}", short(path))),
    }
    show_status(s, ui);
}

// -- inline editing ---------------------------------------------------

/// Show the edit field with `initial` in it, focused.
fn start_edit(s: &mut Files, ui: &mut Ui<Files>, what: Editing, initial: &str) {
    let Some(ids) = s.ids else { return };
    s.editing = what;
    match ui.widget_mut::<TextField<Files>>(ids.edit) {
        Ok(mut f) => {
            f.set_text(initial);
            f.select_all();
            let mut style = f.ui().style(ids.edit);
            style.height = nitro_ui::Length::Auto;
            f.set_style(style);
        }
        Err(e) => complain("the edit field", &e),
    }
    ui.focus(ids.edit);
    s.message = Some(match &s.editing {
        Editing::Rename(_) => "rename: type a name, Enter to confirm".to_owned(),
        Editing::NewFolder => "new folder: type a name, Enter to create".to_owned(),
        Editing::None => String::new(),
    });
    show_status(s, ui);
}

/// Hide the edit field and forget what was being edited.
fn cancel_edit(s: &mut Files, ui: &mut Ui<Files>) {
    if s.editing == Editing::None {
        return;
    }
    s.editing = Editing::None;
    let Some(ids) = s.ids else { return };
    match ui.widget_mut::<TextField<Files>>(ids.edit) {
        Ok(mut f) => {
            f.set_text("");
            let mut style = f.ui().style(ids.edit);
            style.height = nitro_ui::Length::Px(0.0);
            f.set_style(style);
        }
        Err(e) => complain("the edit field", &e),
    }
    ui.focus(ids.list);
    show_status(s, ui);
}

/// Apply the edit: a rename, or a new folder.
fn commit_edit(s: &mut Files, ui: &mut Ui<Files>, text: &str) {
    let what = s.editing.clone();
    let result = match &what {
        Editing::Rename(path) => {
            ops::rename(path, text).map(|p| format!("renamed to {}", short(&p)))
        }
        Editing::NewFolder => {
            ops::create_dir(&s.cwd.clone(), text).map(|p| format!("created {}", short(&p)))
        }
        Editing::None => return,
    };
    cancel_edit(s, ui);
    s.message = Some(match result {
        Ok(m) => m,
        Err(e) => e.to_string(),
    });
    relist(s, ui);
}

// -- copy and paste ---------------------------------------------------

/// Remember the selection for a later paste.
fn copy_selection(s: &mut Files, ui: &mut Ui<Files>) {
    let paths = selected_paths(s, ui);
    if paths.is_empty() {
        return;
    }
    s.message = Some(match paths.len() {
        1 => format!("copied {}", short(&paths[0])),
        n => format!("copied {n} items"),
    });
    s.clipboard = paths;
    show_status(s, ui);
}

/// Copy what `Ctrl+C` remembered into the current directory.
fn paste(s: &mut Files, ui: &mut Ui<Files>) {
    if s.clipboard.is_empty() {
        s.message = Some("nothing to paste".to_owned());
        show_status(s, ui);
        return;
    }
    let (mut done, mut failure) = (0usize, None);
    for src in s.clipboard.clone() {
        match ops::copy_into(&src, &s.cwd.clone()) {
            Ok(_) => done += 1,
            Err(e) => failure = Some(format!("{}: {e}", short(&src))),
        }
    }
    s.message = Some(match failure {
        Some(e) => e,
        None if done == 1 => "copied 1 item".to_owned(),
        None => format!("copied {done} items"),
    });
    relist(s, ui);
}

// -- addressing -------------------------------------------------------

/// Find the widgets, list the starting directory and focus the list.
///
/// Public because [`run`] and every test call it on a tree [`build`]
/// made: the app and its tests must drive the same wiring, or the tests
/// are about a different program.
///
/// # Errors
/// [`Error::NoRoot`] if the tree was not built by [`build`] and the
/// named widgets are not in it.
pub fn start(ui: &mut Ui<Files>, state: &mut Files) -> Result<(), Error> {
    state.ids = Some(Ids::of(ui).ok_or(Error::NoRoot)?);
    if state.places.is_empty() {
        state.places = places::from_env(state.trash.root());
    }
    sync_places(state, ui);
    let cwd = state.cwd.clone();
    navigate(state, ui, cwd);
    if let Some(ids) = state.ids {
        ui.focus(ids.list);
        ui.deliver_focus_events(state);
    }
    Ok(())
}

/// Open the window and run the loop.
///
/// # Errors
/// If the server cannot be reached or the window cannot be opened.
pub fn run() -> Result<(), Error> {
    let mut state = Files::here();
    let mut ui = App::new(APP_NAME)?
        .title(title_for(&state.cwd))
        .size(nitro_ui::Size::new(WIDTH, HEIGHT))
        .build(build)?;
    start(&mut ui, &mut state)?;
    let socket = nitro_ui::introspect::Socket::bind(APP_NAME).ok();
    nitro_ui::app::event_loop_with(&mut ui, &mut state, socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_is_the_directory_name_or_the_app_name() {
        assert_eq!(title_for(Path::new("/home/k/src")), "src");
        assert_eq!(title_for(Path::new("/home/k")), "k");
        // The root has no name of its own, so the window is called what
        // the `.desktop` file calls the program.
        assert_eq!(title_for(Path::new("/")), TITLE);
        assert_eq!(TITLE, "Files");
    }
}

//! The file manager, driven through a real server on the fake backend.
//!
//! Every test here builds the tree the binary builds
//! ([`nitro_files::build`]), runs the same [`nitro_files::start`] on it,
//! and drives it the way a user or a script would: evdev keycodes travel
//! server → wire → widget, a click on the `↑` button is a real click, and
//! the assertions are made on what the *widgets* then say — the list's
//! visible rows, the path bar's text, the status line's label. The model
//! modules (`dir`, `mime`, `trash`, `ops`) have thorough tests of their
//! own and are not re-tested here; what is tested here is the wiring
//! between them and the tree, which is exactly where a passing model can
//! still add up to a broken program.
//!
//! Three things are injected rather than taken from the machine, through
//! [`Files::with_env`] and [`Files::with_term`]: the MIME glob table (an
//! empty one, so the built-in extension table answers and a box with
//! `shared-mime-info` installed and one without run the same test), the
//! `.desktop` association roots, and the trash. A test that read the
//! developer's `~/.config` would be asserting on their desktop, and one
//! that used the real trash would put their files in it. Nothing calls
//! `std::env::set_var`: a test binary is threaded, and a variable set in
//! one test is a variable every other test sees.
//!
//! The two places a wakeup is faked are the background scan's pipe and
//! the inotify watch. The harness runs the `Ui` on the test thread and
//! has no `epoll`, so a test `poll`s the descriptor itself and then calls
//! [`nitro_ui::Ui::run_fd`] with the token — which is precisely the pair
//! of steps the app loop takes on a wakeup, and everything after that
//! point is the shipped dispatch path. Every such wait is bounded and
//! fails with a message rather than hanging.

use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nitro_files::{Confirm, Editing, Files, Ids, dir, mime, names, trash};
use nitro_ui::event::key;
use nitro_ui::split::SidebarRow;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Button, Label, TextField};
use nitro_ui::{List, Size};

/// The terminal the text fallback is told to open an editor in.
///
/// A real program that exists on every box and does nothing, so the
/// fallback's `spawn` really runs and the status line really says what it
/// started. `nitro-term` would be the app's own answer and is not built
/// in a `-p nitro-files` test run.
const TERM: &str = "/bin/echo";

/// Evdev keycodes of `a`..`z`, so a test types what a user types.
const LETTERS: [u32; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];

/// The window every test opens; see [`app`].
const WINDOW: Size = Size::new(560.0, 280.0);

/// A fresh scratch root, unique to this test *and* this process, and the
/// directory inside it the app will show.
///
/// The layout is `<root>/dir` for the files under test and `<root>/xdg`
/// for the environment: `<root>/xdg/config/mimeapps.list`,
/// `<root>/xdg/data/applications/*.desktop` and `<root>/xdg/Trash`. Both
/// halves are under `std::env::temp_dir()`, which matters for the trash:
/// a trash on another filesystem is an `EXDEV` rename, by design.
fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("nitro-files-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("dir");
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    (root, dir)
}

/// A harness showing `dir`, with the MIME tables and the trash pointed at
/// temp directories so a test never reads the developer's `~/.config` and
/// never puts anything in their real trash.
///
/// The window is 560×280 on the harness's 320×240 output: beside the
/// 200 px sidebar the list gets a few rows and ~350 px of width, which
/// is the awkward case rather than a soft one — the list is
/// virtualised, so a test that gave it room for everything would never
/// see the window into the model. The rows are read through the widget
/// API rather than pixels, so the part off the output does not matter;
/// the header buttons a test clicks are at the top and on it.
///
/// The sidebar's places are a **fixture** — a home next to `dir` with
/// only Root and Trash after it — so the row set never depends on the
/// developer's `~` or `user-dirs.dirs`.
fn app(dir: &Path, xdg: &Path) -> (Harness<Files>, Ids) {
    let assoc = mime::Assoc::at(vec![xdg.join("config")], vec![xdg.join("data")]);
    let home = dir.parent().expect("the scratch root").join("home");
    std::fs::create_dir_all(home.join("Documents")).expect("the fixture home");
    let trash = trash::Trash::at(xdg.join("Trash"));
    let places = nitro_files::places::places(Some(&home), &[], &trash.root().join("files"));
    let state = Files::new(dir)
        // An empty glob table, so the built-in extension table is what
        // answers `foo.txt` and the result does not depend on whether
        // this box has `/usr/share/mime/globs2`.
        .with_env(Vec::new(), assoc, trash)
        .with_term(TERM)
        .with_places(places);
    let mut h = Harness::sized(nitro_files::APP_NAME, state, WINDOW, nitro_files::build);
    // The same `start` the binary runs, on the tree `build` produced: a
    // test that wired the widgets up itself would be testing a different
    // file manager.
    let (ui, state) = h.parts();
    nitro_files::start(ui, state).expect("start the app");
    h.settle();
    let ids = Ids::of(h.ui()).expect("the five named widgets");
    (h, ids)
}

/// How many distinct icons the **server's own decorations** cache.
///
/// Since #3715 a decorated window's title bar carries an application
/// icon and three symbolic button glyphs, all of them the server's own
/// nodes — so `icons_cached` is not "what this client asked for".
/// Measured on a harness with a window and no icons at all rather than
/// written down as a constant, which would go stale silently: the number
/// would still look plausible.
///
/// A name of its own, not the file manager's: a `TestServer`'s socket
/// directory is keyed on the name, so two harnesses sharing one share a
/// directory and the second's teardown takes the first's socket with it.
///
/// Measured at the **app's window size**: the app's window is wider than
/// the harness's output, so the title bar's button glyphs at its right
/// end are off the output and never rasterised — a baseline taken on a
/// narrower window would count three glyphs the app's frame never draws.
fn frame_icons_cached() -> usize {
    let h = Harness::sized(
        "files-icon-baseline",
        (),
        WINDOW,
        |ui: &mut nitro_ui::Ui<()>| ui.build(nitro_ui::widgets::label("no icons here")),
    );
    let n = h.server().stat("icons_cached") as usize;
    h.quit();
    n
}

/// Write `text` to `path`, creating the parents.
fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir -p");
    }
    std::fs::write(path, text).expect("write a fixture file");
}

/// The rows the list is actually showing, as `(icon, text, detail)`.
///
/// Through the widget rather than through `dir::read_dir`: the claim
/// under test is what is on screen, and a listing the app read but never
/// put in the list would pass every assertion made on the model.
fn rows(h: &Harness<Files>, ids: Ids) -> Vec<(String, String, String)> {
    h.widget::<List<Files>>(ids.list)
        .visible_rows()
        .into_iter()
        .map(|(_, r)| (r.icon.unwrap_or_default(), r.text, r.detail))
        .collect()
}

/// Just the names of the visible rows, in order.
fn names_of(h: &Harness<Files>, ids: Ids) -> Vec<String> {
    rows(h, ids).into_iter().map(|(_, t, _)| t).collect()
}

/// The status line's text.
fn status(h: &Harness<Files>, ids: Ids) -> String {
    h.widget::<Label>(ids.status).text().to_owned()
}

/// The path bar's text.
fn path_text(h: &Harness<Files>, ids: Ids) -> String {
    h.widget::<TextField<Files>>(ids.path).text().to_owned()
}

/// Put the path bar's text there without typing it.
///
/// A temp path is thirty characters of digits and dashes, and typing it
/// evdev code by evdev code would make this a test of the keymap. The
/// submission itself — the part the app wires up — is a real `Enter` on
/// the focused field.
fn submit_path(h: &mut Harness<Files>, ids: Ids, text: &str) {
    h.ui()
        .widget_mut::<TextField<Files>>(ids.path)
        .expect("the path bar")
        .set_text(text);
    h.ui().focus(ids.path);
    h.key(key::ENTER);
    h.settle();
}

/// Type `text`, one real key press per character.
fn type_text(h: &mut Harness<Files>, text: &str) {
    for c in text.chars() {
        let code = match c {
            'a'..='z' => LETTERS[c as usize - 'a' as usize],
            '.' => 52,
            other => panic!("no evdev code for {other:?} in this test"),
        };
        h.key(code);
    }
    h.settle();
}

/// Wait up to `secs` for `fd` to become readable, the way the app loop's
/// `epoll_wait` does. `false` on timeout, so a caller fails rather than
/// hangs.
fn poll_readable(fd: BorrowedFd<'_>, secs: i64) -> bool {
    let mut fds = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
    )];
    let ts = rustix::event::Timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    matches!(rustix::event::poll(&mut fds, Some(&ts)), Ok(n) if n > 0)
}

/// The wakeup the app loop would have had for the background scan: poll
/// the scan's own pipe, then dispatch its token.
fn drive_scan(h: &mut Harness<Files>) {
    let token = {
        let (fd, token) = h.state().scan_hook().expect("a scan in flight");
        assert!(poll_readable(fd, 10), "the scan never woke its pipe");
        token
    };
    let (ui, state) = h.parts();
    ui.run_fd(state, token);
    h.settle();
}

/// The same for the inotify watch: poll the descriptor, then dispatch.
fn drive_watch(h: &mut Harness<Files>, what: &str) {
    let token = {
        let (fd, token) = h.state().watch_hook().expect("the watch is armed");
        assert!(poll_readable(fd, 10), "no inotify event for {what}");
        token
    };
    let (ui, state) = h.parts();
    ui.run_fd(state, token);
    h.settle();
}

/// Whether `s` has the shape `YYYY-MM-DD HH:MM`.
fn looks_like_a_timestamp(s: &str) -> bool {
    s.len() == 16
        && s.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            10 => b == b' ',
            13 => b == b':',
            _ => b.is_ascii_digit(),
        })
}

#[test]
fn a_directory_lists_its_folders_first_then_its_files_by_name() {
    // The headline, and the one assertion made through the list widget's
    // visible rows rather than the model: directories first, then names
    // case-insensitively, each row carrying its icon name and either
    // `<dir>` or a size and a date. If this failed, every other test in
    // this file would be asserting about rows the user cannot see.
    let (root, dir) = fixture("listing");
    std::fs::create_dir_all(dir.join("alpha")).expect("a subdirectory");
    std::fs::create_dir_all(dir.join("Beta")).expect("a subdirectory");
    std::fs::write(dir.join("notes.txt"), vec![b'x'; 912]).expect("a file");
    write(&dir.join("Photo.png"), "not really a png");
    let (h, ids) = app(&dir, &root.join("xdg"));

    let rows = rows(&h, ids);
    assert_eq!(
        rows.iter().map(|(_, t, _)| t.as_str()).collect::<Vec<_>>(),
        ["alpha", "Beta", "notes.txt", "Photo.png"],
        "directories first, then case-insensitive name"
    );
    assert_eq!(
        rows[0].0, "folder-fill",
        "a directory carries the folder icon's name"
    );
    assert_eq!(rows[0].2, "<dir>", "and no size");
    assert_eq!(rows[1].2, "<dir>");

    let (size, when) = rows[2]
        .2
        .split_once("   ")
        .expect("a file's detail is a size and a date");
    assert_eq!(size, "912 B", "exact below a kilobyte");
    assert!(
        looks_like_a_timestamp(when),
        "the date column is YYYY-MM-DD HH:MM, got {when:?}"
    );
    assert_eq!(
        status(&h, ids),
        "4 items, 0 selected",
        "and the status line counts what is shown"
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn entering_a_directory_follows_the_path_bar_and_up_comes_back() {
    // Enter on a directory row is the whole navigation path: the list's
    // `on_activate`, `activate`, `navigate`, the re-listing and the write
    // back into the path bar. A break anywhere in it leaves the user
    // looking at one directory with another one's name above it.
    let (root, dir) = fixture("navigate");
    let sub = dir.join("sub");
    write(&sub.join("inner.txt"), "inner");
    write(&dir.join("outer.txt"), "outer");
    let (mut h, ids) = app(&dir, &root.join("xdg"));
    assert_eq!(path_text(&h, ids), dir.display().to_string());

    // The cursor starts on row 0, which is the directory.
    h.key(key::ENTER);
    h.settle();
    assert_eq!(h.state().cwd(), sub, "Enter entered the directory");
    assert_eq!(
        path_text(&h, ids),
        sub.display().to_string(),
        "and the path bar followed"
    );
    assert_eq!(names_of(&h, ids), ["inner.txt"]);

    let up = nitro_ui::introspect::resolve(h.ui(), "window/up").expect("the up button");
    assert_eq!(h.widget::<Button<Files>>(up).text(), "↑");
    h.click(up);
    h.settle();
    assert_eq!(h.state().cwd(), dir, "the up button went back");
    assert_eq!(names_of(&h, ids), ["sub", "outer.txt"]);

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn the_window_is_titled_after_the_directory_on_screen() {
    // Issue #571: the bar read `nitro-files` next to `Calculator` and
    // `Settings` — the binary name, because that was what the app was
    // titled with. The title is the directory's basename, set from
    // `navigate` and nowhere else, so every way of moving agrees with it;
    // `/` has no name and gets the `.desktop` file's `Name=`.
    let (root, dir) = fixture("title");
    let sub = dir.join("sub");
    write(&sub.join("inner.txt"), "inner");
    let (mut h, ids) = app(&dir, &root.join("xdg"));
    assert_eq!(
        h.ui().window_title(),
        "dir",
        "the starting directory's name"
    );

    submit_path(&mut h, ids, &sub.display().to_string());
    assert_eq!(h.state().cwd(), sub);
    assert_eq!(
        h.ui().window_title(),
        "sub",
        "the title followed the navigation"
    );

    // A navigation that is refused leaves the title alone with the
    // listing: the user is still looking at `sub`.
    submit_path(&mut h, ids, &sub.join("inner.txt").display().to_string());
    assert_eq!(h.state().cwd(), sub);
    assert_eq!(
        h.ui().window_title(),
        "sub",
        "a refused navigation changes nothing"
    );

    submit_path(&mut h, ids, "/");
    assert_eq!(h.state().cwd(), Path::new("/"));
    assert_eq!(
        h.ui().window_title(),
        nitro_files::TITLE,
        "the root has no name of its own, so the app's name stands in"
    );
    assert_eq!(
        nitro_files::TITLE,
        "Files",
        "which is `Name=` in the .desktop file"
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn a_path_submitted_in_the_bar_navigates_and_a_bad_one_only_complains() {
    // The failure mode this pins: a path bar that empties the list when
    // the user fat-fingers a directory name. A path that is missing, or
    // that is a file, must leave the user looking at the directory they
    // could see, with a message saying why.
    let (root, dir) = fixture("pathbar");
    let sub = dir.join("sub");
    write(&sub.join("inner.txt"), "inner");
    write(&dir.join("outer.txt"), "outer");
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    // A path the user typed loosely is normalised, and the *field* is
    // rewritten with the answer — `navigate` writes the path bar on every
    // navigation, and this one arrives from inside that same field's
    // `on_submit`. What a user sees if it does not: they typed
    // `~/src/../src/`, they are in `~/src`, and the bar still shows the
    // detour, so the next Enter re-resolves a path that no longer means
    // what is on screen.
    let messy = format!("{}/sub/../sub/", dir.display());
    submit_path(&mut h, ids, &messy);
    assert_eq!(h.state().cwd(), sub, "the messy path resolved to sub");
    assert_eq!(
        path_text(&h, ids),
        sub.display().to_string(),
        "and the path bar was rewritten with the path it actually went to"
    );
    assert_eq!(names_of(&h, ids), ["inner.txt"]);

    submit_path(&mut h, ids, &dir.display().to_string());
    assert_eq!(h.state().cwd(), dir);

    submit_path(&mut h, ids, &sub.display().to_string());
    assert_eq!(h.state().cwd(), sub, "a typed path navigates");
    assert_eq!(names_of(&h, ids), ["inner.txt"]);

    let missing = dir.join("nowhere");
    submit_path(&mut h, ids, &missing.display().to_string());
    assert_eq!(h.state().cwd(), sub, "a missing path changed nothing");
    assert!(
        status(&h, ids).contains(&missing.display().to_string()),
        "and the status line says which path: {:?}",
        status(&h, ids)
    );
    assert_eq!(names_of(&h, ids), ["inner.txt"], "the listing is untouched");

    let file = sub.join("inner.txt");
    submit_path(&mut h, ids, &file.display().to_string());
    assert_eq!(h.state().cwd(), sub, "a file is not a directory to enter");
    assert!(
        status(&h, ids).contains("not a directory"),
        "and it says so: {:?}",
        status(&h, ids)
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn ctrl_h_shows_and_hides_dotfiles_and_the_count_follows() {
    // Both halves matter: the rows *and* the count. A toggle that showed
    // the hidden files without re-running the status line would leave "1
    // item" over a list of two.
    let (root, dir) = fixture("hidden");
    write(&dir.join(".secret"), "shh");
    write(&dir.join("plain.txt"), "hello");
    let (mut h, ids) = app(&dir, &root.join("xdg"));
    assert_eq!(names_of(&h, ids), ["plain.txt"]);
    assert!(!h.state().shows_hidden());
    assert_eq!(status(&h, ids), "1 items, 0 selected");

    h.key_with(key::LEFT_CTRL, key::H);
    h.settle();
    assert!(h.state().shows_hidden(), "Ctrl+H showed the dotfiles");
    assert_eq!(names_of(&h, ids), [".secret", "plain.txt"]);
    assert_eq!(status(&h, ids), "2 items, 0 selected");

    h.key_with(key::LEFT_CTRL, key::H);
    h.settle();
    assert!(!h.state().shows_hidden(), "and hid them again");
    assert_eq!(names_of(&h, ids), ["plain.txt"]);
    assert_eq!(status(&h, ids), "1 items, 0 selected");

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn ctrl_s_cycles_name_size_date_and_the_rows_reorder() {
    // Three files whose name, size and date orders are all different, so
    // a cycle that changed the label without re-sorting the rows — or
    // re-sorted without pushing the new order into the list — fails here
    // rather than looking right in two of the three states.
    let (root, dir) = fixture("sort");
    std::fs::write(dir.join("a.txt"), vec![b'x'; 1000]).expect("a file");
    std::fs::write(dir.join("b.txt"), vec![b'x'; 10]).expect("a file");
    std::fs::write(dir.join("c.txt"), vec![b'x'; 100]).expect("a file");
    // Distinct modification times, oldest first: `Entry::mtime` is whole
    // seconds, so three files written in the same second would tie and
    // the date order would silently be the name order.
    let base = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    for (name, age) in [("a.txt", 0), ("c.txt", 86_400), ("b.txt", 172_800)] {
        let f = std::fs::File::options()
            .write(true)
            .open(dir.join(name))
            .expect("open to set the time");
        f.set_modified(base + Duration::from_secs(age))
            .expect("set the modification time");
    }
    let (mut h, ids) = app(&dir, &root.join("xdg"));
    assert_eq!(h.state().sort_order(), dir::Sort::Name);
    assert_eq!(names_of(&h, ids), ["a.txt", "b.txt", "c.txt"]);

    h.key_with(key::LEFT_CTRL, key::S);
    h.settle();
    assert_eq!(h.state().sort_order(), dir::Sort::Size);
    assert_eq!(
        names_of(&h, ids),
        ["a.txt", "c.txt", "b.txt"],
        "biggest first"
    );
    assert!(status(&h, ids).contains("sorted by size"));

    h.key_with(key::LEFT_CTRL, key::S);
    h.settle();
    assert_eq!(h.state().sort_order(), dir::Sort::Mtime);
    assert_eq!(
        names_of(&h, ids),
        ["b.txt", "c.txt", "a.txt"],
        "newest first"
    );
    assert!(status(&h, ids).contains("sorted by date"));

    h.key_with(key::LEFT_CTRL, key::S);
    h.settle();
    assert_eq!(h.state().sort_order(), dir::Sort::Name, "and back to name");
    assert_eq!(names_of(&h, ids), ["a.txt", "b.txt", "c.txt"]);

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn a_big_directory_is_read_off_the_loop_and_the_ui_answers_while_it_is() {
    // The claim the whole `Scan` machinery exists for. Fifty thousand
    // entries is the number `docs/files.md` quotes, and creating them is
    // most of this test's runtime — the part that is timed is the
    // listing, which must not block the loop for a perceptible moment.
    //
    // What is faked is only the wakeup: `drive_scan` polls the scan's own
    // pipe exactly as `epoll_wait` would and then calls `run_fd` with the
    // token, which is the call the app loop makes. Everything after that
    // is the shipped path.
    const BIG: usize = 50_000;
    let (root, dir) = fixture("bigdir");
    write(&dir.join("small.txt"), "a small directory to start in");
    let big = root.join("big");
    std::fs::create_dir_all(&big).expect("the big directory");
    for i in 0..BIG {
        std::fs::File::create(big.join(format!("f{i:06}"))).expect("a file in the big directory");
    }
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    let started = Instant::now();
    let (ui, state) = h.parts();
    nitro_files::navigate(state, ui, big.clone());
    assert!(
        h.state().scanning(),
        "a directory past dir::BIG_DIR is read on a thread, not here"
    );
    assert_eq!(
        h.state().cwd(),
        big,
        "the app moved even though the listing has not arrived"
    );

    // The loop is not blocked: introspection answers while the scan is in
    // flight, which is the observable form of "the window is still
    // responding".
    let said = nitro_ui::introspect::get_prop(h.ui(), "window/status", "value")
        .expect("the status line answers during a scan");
    assert!(
        said.contains("reading"),
        "and it says what it is doing: {said:?}"
    );

    drive_scan(&mut h);
    let took = started.elapsed();
    assert!(!h.state().scanning(), "the scan finished and was unhooked");
    assert_eq!(h.state().entries().len(), BIG, "every entry arrived");
    assert!(
        took < Duration::from_secs(1),
        "listing {BIG} entries took {took:?}, which is not off the loop"
    );

    let list = h.widget::<List<Files>>(ids.list);
    let fits = list.rows_that_fit();
    assert_eq!(list.len(), BIG, "the model is the whole directory");
    assert_eq!(
        list.materialised(),
        fits + 2,
        "but only a screenful plus two spare rows exists as nodes"
    );
    assert!(fits < 30, "a 220 px window shows tens of rows, not {fits}");
    let first = names_of(&h, ids);
    assert_eq!(first.first().map(String::as_str), Some("f000000"));

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn activating_a_file_runs_the_handler_its_mime_type_names() {
    // Extension → MIME type → `mimeapps.list` → `.desktop` → argv →
    // `nitro_launcher::spawn`, end to end, with a real child at the end
    // of it. The marker file is how the test knows the program really
    // ran rather than that a `Command` was built.
    let (root, dir) = fixture("open");
    let xdg = root.join("xdg");
    let marker = root.join("marker");
    write(&dir.join("foo.txt"), "hello");
    write(
        &xdg.join("config/mimeapps.list"),
        "[Default Applications]\ntext/plain=marker.desktop\n",
    );
    write(
        &xdg.join("data/applications/marker.desktop"),
        &format!(
            "[Desktop Entry]\nType=Application\nName=Marker\nExec=/bin/sh -c \"touch {}\"\n",
            marker.display()
        ),
    );
    let (mut h, ids) = app(&dir, &xdg);
    assert_eq!(names_of(&h, ids), ["foo.txt"]);

    h.key(key::ENTER);
    h.settle();
    assert_eq!(
        h.state().children().len(),
        1,
        "the handler was spawned and is being reaped through its pidfd"
    );
    let said = status(&h, ids);
    assert!(
        said.contains("opened foo.txt with /bin/sh"),
        "the status line names what opened it: {said:?}"
    );
    // Bounded: a child that never starts fails the test rather than
    // hanging it.
    nitro_ui::test::until("the handler to touch its marker", || marker.exists());

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn a_text_file_nobody_claims_falls_back_to_an_editor() {
    // `mime::open_with` decides this and has its own tests; what is
    // asserted here is the app's half — that the fallback is reached with
    // no association registered at all, that it really spawns, and that
    // the status line distinguishes "editing" from "opened".
    let (root, dir) = fixture("editor");
    write(&dir.join("notes.txt"), "hello");
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    h.key(key::ENTER);
    h.settle();
    let said = status(&h, ids);
    assert!(
        said.contains(&format!("editing notes.txt with {TERM}")),
        "the text fallback opens an editor in the terminal: {said:?}"
    );
    assert_eq!(h.state().children().len(), 1, "and it really started one");

    // A type nobody claims and that is not text opens nothing at all,
    // rather than showing its bytes in an editor.
    write(&dir.join("thing.pdf"), "%PDF-1.4");
    let (ui, state) = h.parts();
    nitro_files::relist(state, ui);
    h.settle();
    let pdf = dir.join("thing.pdf");
    let (ui, state) = h.parts();
    nitro_files::open(state, ui, &pdf);
    h.settle();
    assert!(
        status(&h, ids).contains("nothing opens thing.pdf"),
        "{:?}",
        status(&h, ids)
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn delete_asks_first_and_n_leaves_the_file_where_it_is() {
    // The file the prompt is about, named so its first letter is also the
    // key that answers the question. That collision is the whole point of
    // this test rather than a detail of it, so it is stated as a premise
    // rather than buried in a literal: rename it to `list.txt` and every
    // assertion below still passes while the test has silently stopped
    // testing anything.
    const VICTIM: &str = "notes.txt";

    // A delete key that deleted would be the one destructive key in the
    // program with no way back. The question lives in the status line and
    // the next key answers it; `n` must leave the file alone.
    //
    // Why the name matters: app-level key handlers are offered only what
    // the focused chain declined, and a focused `List` consumes any
    // printable key as type-ahead. With the list still focused, `n` would
    // not be "no" — it would be "jump to the first row starting with n",
    // which is this one — and the question would eat its own answer. A
    // pending confirm therefore takes the keyboard (`nitro_files::ask`
    // blurs), and this asserts that property from the outside: against
    // the version without the blur, the file is trashed on the next `y`
    // and this test fails.
    assert!(
        VICTIM.starts_with('n'),
        "this test is about `n` being both an answer and a type-ahead prefix; \
         a fixture whose name does not start with `n` makes it vacuous"
    );
    let (root, dir) = fixture("confirm-no");
    write(&dir.join(VICTIM), "still here");
    write(&dir.join("keep.txt"), "untouched");
    let (mut h, ids) = app(&dir, &root.join("xdg"));
    assert_eq!(names_of(&h, ids), ["keep.txt", VICTIM]);

    // Select it — and check that the row really is the one the premise is
    // about, rather than trusting the sort order to have put it here.
    h.key(key::DOWN);
    h.settle();
    let selected = h
        .state()
        .path_at(h.widget::<List<Files>>(ids.list).cursor())
        .expect("a row under the cursor");
    assert_eq!(
        selected,
        dir.join(VICTIM),
        "the cursor is on the file the prompt will be about"
    );

    h.key(key::DELETE);
    h.settle();
    assert_eq!(
        h.state().pending_confirm(),
        Some(&Confirm::Trash(vec![dir.join(VICTIM)])),
        "Delete asks about the row under the cursor"
    );
    assert!(
        status(&h, ids).contains(&format!("Move {VICTIM} to the trash? [y/n]")),
        "and asks it in the status line: {:?}",
        status(&h, ids)
    );
    assert_eq!(
        h.ui().focused(),
        None,
        "a pending question takes the keyboard, or the list types the answer"
    );

    h.key(key::N);
    h.settle();
    assert!(
        h.state().pending_confirm().is_none(),
        "`n` answered the question rather than jumping to {VICTIM}"
    );
    assert!(dir.join(VICTIM).exists(), "and the file is still here");
    assert_eq!(h.state().message(), Some("cancelled"));
    assert_eq!(names_of(&h, ids), ["keep.txt", VICTIM]);
    assert_eq!(
        h.ui().focused(),
        Some(ids.list),
        "and the keyboard went back to the list"
    );
    // The list is live again: type-ahead works, which is the other half
    // of "the confirm borrowed the focus" rather than kept it.
    h.key(key::N);
    h.settle();
    assert_eq!(
        h.widget::<List<Files>>(ids.list).cursor(),
        1,
        "with no question pending, `n` is type-ahead again"
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn a_pending_confirm_swallows_the_ctrl_shortcuts() {
    // Issue #559, the other door into the room the confirm-blur closed.
    // `install()` registers the five `Ctrl+…` shortcuts *before*
    // `on_key(app_key)`, and `nitro-ui` offers app-level handlers in
    // registration order — so a shortcut outranks the pending-confirm
    // swallow. The four-step repro from the issue:
    //
    //   1. Delete   → confirm pending, focus dropped, prompt in the status line
    //   2. Ctrl+N   → (was) the new-folder field opens and takes the focus
    //   3. a name + Enter → commit_edit → cancel_edit hands focus to the list
    //   4. y/n      → eaten by the focused list as type-ahead; the prompt is
    //                 now unanswerable, and the file is in limbo
    //
    // Nothing is destroyed either way, which is why this was a review nit
    // rather than a bug report — but a question on screen that cannot be
    // answered is exactly what the blur exists to prevent.
    const VICTIM: &str = "notes.txt";
    let (root, dir) = fixture("confirm-shortcut");
    write(&dir.join(VICTIM), "still here");
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    h.key(key::DELETE);
    h.settle();
    assert_eq!(
        h.state().pending_confirm(),
        Some(&Confirm::Trash(vec![dir.join(VICTIM)])),
        "Delete asks first"
    );
    assert_eq!(h.ui().focused(), None, "and the question took the keyboard");

    // Step 2: the shortcut must do nothing at all.
    h.key_with(key::LEFT_CTRL, key::N);
    h.settle();
    assert_eq!(
        h.state().editing(),
        &Editing::None,
        "Ctrl+N must not open the new-folder field while a question is pending"
    );
    assert_eq!(
        h.ui().focused(),
        None,
        "and must not give the keyboard away: that is what strands the prompt"
    );
    assert_eq!(
        h.state().pending_confirm(),
        Some(&Confirm::Trash(vec![dir.join(VICTIM)])),
        "the question is still the same question"
    );

    // The other four, for the same reason and in one sweep: none may act,
    // and none may move the focus. Ctrl+H would toggle hidden files, Ctrl+S
    // would re-sort, Ctrl+C would copy the selection, Ctrl+V would paste.
    let hidden = h.state().shows_hidden();
    for k in [key::H, key::S, key::C, key::V] {
        h.key_with(key::LEFT_CTRL, k);
        h.settle();
        assert_eq!(h.ui().focused(), None, "a shortcut gave the keyboard away");
        assert!(
            h.state().pending_confirm().is_some(),
            "a shortcut cleared the pending question"
        );
    }
    assert_eq!(
        h.state().shows_hidden(),
        hidden,
        "and Ctrl+H specifically did nothing"
    );

    // And the question is still answerable, which is the whole point.
    h.key(key::N);
    h.settle();
    assert!(h.state().pending_confirm().is_none(), "`n` cancelled it");
    assert_eq!(h.state().message(), Some("cancelled"));
    assert!(dir.join(VICTIM).exists(), "and the file is still here");
    assert_eq!(
        h.ui().focused(),
        Some(ids.list),
        "the keyboard went back to the list"
    );

    // With nothing pending the shortcuts work exactly as before: the
    // early-return is conditional, not a disablement. (The alternative fix
    // — registering `app_key` first — would have changed the ordering for
    // every key, which is why this assertion is here in either case.)
    h.key_with(key::LEFT_CTRL, key::H);
    h.settle();
    assert!(
        h.state().shows_hidden() != hidden,
        "with no question pending, Ctrl+H toggles hidden files again"
    );
    h.key_with(key::LEFT_CTRL, key::N);
    h.settle();
    assert_eq!(
        h.state().editing(),
        &Editing::NewFolder,
        "and Ctrl+N opens the new-folder field again"
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn answering_y_moves_the_file_into_the_trash_with_its_info_file() {
    // The trash is only a trash if what it holds can be put back, which
    // means `files/<name>` and `info/<name>.trashinfo` both exist. A move
    // that wrote one without the other would look right in the file
    // manager and be unrecoverable from anywhere else.
    //
    // Both halves of the move are under `std::env::temp_dir()` — the
    // directory and the trash root — because the move is a `rename` and a
    // trash on another filesystem is an `EXDEV` by design (the app says
    // "on another filesystem; only the home trash is supported" and does
    // not copy). A fixture that straddled two filesystems would be
    // testing that message instead of the trash.
    let (root, dir) = fixture("confirm-yes");
    let xdg = root.join("xdg");
    write(&dir.join("doomed.txt"), "goodbye");
    write(&dir.join("keep.txt"), "untouched");
    let (mut h, ids) = app(&dir, &xdg);

    h.key(key::DELETE);
    h.settle();
    h.key(key::Y);
    h.settle();

    assert!(h.state().pending_confirm().is_none());
    assert_eq!(
        h.state().message(),
        Some("moved 1 item to the trash"),
        "the move succeeded rather than reporting a cross-device rename"
    );
    assert!(!dir.join("doomed.txt").exists(), "the file left the folder");
    let trash = xdg.join("Trash");
    assert_eq!(
        std::fs::read_to_string(trash.join("files/doomed.txt")).expect("the trashed file"),
        "goodbye",
        "and arrived intact"
    );
    let info = std::fs::read_to_string(trash.join("info/doomed.txt.trashinfo"))
        .expect("the info file next to it");
    assert!(
        info.starts_with("[Trash Info]"),
        "the info file is the spec's shape: {info:?}"
    );
    assert!(
        info.contains(&dir.join("doomed.txt").display().to_string()),
        "and records where it came from: {info:?}"
    );
    assert_eq!(
        names_of(&h, ids),
        ["keep.txt"],
        "the listing refreshed itself after the move"
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn f2_renames_the_row_under_the_cursor_and_the_list_follows() {
    // The rename field is the one widget that comes and goes, and the
    // thing that makes it an *inline* rename is that it opens carrying
    // the old name selected — so typing replaces it and Enter is one
    // keystroke away.
    let (root, dir) = fixture("rename");
    write(&dir.join("old.txt"), "contents");
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    h.key(key::F2);
    h.settle();
    assert_eq!(
        h.state().editing(),
        &Editing::Rename(dir.join("old.txt")),
        "F2 edits the row under the cursor"
    );
    assert_eq!(
        h.widget::<TextField<Files>>(ids.edit).text(),
        "old.txt",
        "and the field carries the old name"
    );
    assert_eq!(
        h.ui().focused(),
        Some(ids.edit),
        "with the keyboard already in it"
    );

    // The old name is selected, so the first character replaces it.
    type_text(&mut h, "new.txt");
    h.key(key::ENTER);
    h.settle();

    assert_eq!(h.state().editing(), &Editing::None, "the field closed");
    // The *widget* closed too, not just the state that describes it.
    // `commit_edit` calls `cancel_edit`, which empties the field and
    // collapses it back to `height: Px(0.0)` — and it runs from inside
    // that same field's `on_submit`. What a user sees if the write is
    // dropped: the rename worked, the list refreshed, and an edit box is
    // still sitting under the list holding the name they just committed,
    // with no way to tell it is no longer live.
    assert_eq!(
        h.widget::<TextField<Files>>(ids.edit).text(),
        "",
        "the edit field emptied itself after committing"
    );
    assert_eq!(
        h.ui().style(ids.edit).height,
        nitro_ui::Length::Px(0.0),
        "and collapsed back out of the layout"
    );
    assert_eq!(
        h.ui().focused(),
        Some(ids.list),
        "with the keyboard back on the list"
    );
    assert!(!dir.join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).expect("the renamed file"),
        "contents"
    );
    assert_eq!(
        names_of(&h, ids),
        ["new.txt"],
        "and the list re-read the directory"
    );
    assert!(status(&h, ids).contains("renamed to new.txt"));

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn escape_abandons_an_edit_without_touching_the_file() {
    // The other half of the edit field: a rename you thought better of
    // must leave nothing behind, neither a renamed file nor a field that
    // stays on screen eating keys.
    let (root, dir) = fixture("escape");
    write(&dir.join("old.txt"), "contents");
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    h.key(key::F2);
    h.settle();
    type_text(&mut h, "other");
    h.key(key::ESC);
    h.settle();

    assert_eq!(h.state().editing(), &Editing::None);
    assert_eq!(
        h.ui().focused(),
        Some(ids.list),
        "and the keyboard went back to the list"
    );
    assert!(dir.join("old.txt").exists(), "the file was not renamed");
    assert!(!dir.join("other").exists());

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn ctrl_n_creates_a_folder_and_it_appears_in_the_list() {
    let (root, dir) = fixture("newfolder");
    write(&dir.join("a.txt"), "a");
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    h.key_with(key::LEFT_CTRL, key::N);
    h.settle();
    assert_eq!(h.state().editing(), &Editing::NewFolder);
    assert_eq!(
        h.widget::<TextField<Files>>(ids.edit).text(),
        "",
        "a new folder starts with an empty name, not the row's"
    );

    type_text(&mut h, "made");
    h.key(key::ENTER);
    h.settle();

    assert!(dir.join("made").is_dir(), "the directory was created");
    assert_eq!(
        names_of(&h, ids),
        ["made", "a.txt"],
        "and it is in the list, sorted with the directories"
    );
    assert!(status(&h, ids).contains("created made"));

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn ctrl_c_then_ctrl_v_copies_the_file_into_the_new_directory() {
    // The app's own clipboard: copy here, navigate, paste there. What
    // would break if this failed is not just the copy — `Ctrl+C` reads
    // the *list's* selection, so a wiring bug puts the wrong file on the
    // clipboard and the paste is silently of something else.
    let (root, dir) = fixture("copy");
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).expect("a subdirectory");
    write(&dir.join("note.txt"), "copy me");
    let (mut h, ids) = app(&dir, &root.join("xdg"));
    assert_eq!(names_of(&h, ids), ["sub", "note.txt"]);

    // Down to the file, then copy it.
    h.key(key::DOWN);
    h.settle();
    h.key_with(key::LEFT_CTRL, key::C);
    h.settle();
    assert_eq!(
        h.state().clipboard(),
        [dir.join("note.txt")],
        "Ctrl+C remembered the selected row"
    );
    assert!(status(&h, ids).contains("copied note.txt"));

    // Into the subdirectory, and paste.
    h.key(key::HOME);
    h.key(key::ENTER);
    h.settle();
    assert_eq!(h.state().cwd(), sub);
    h.key_with(key::LEFT_CTRL, key::V);
    h.settle();

    assert_eq!(
        std::fs::read_to_string(sub.join("note.txt")).expect("the copy"),
        "copy me"
    );
    assert!(
        dir.join("note.txt").exists(),
        "a copy is not a move: the original stayed"
    );
    assert_eq!(names_of(&h, ids), ["note.txt"], "and the list refreshed");
    assert!(status(&h, ids).contains("copied 1 item"));

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn the_watch_follows_the_directory_on_screen_across_navigations() {
    // A `touch` in a terminal shows up here with no refresh key, no poll
    // and no timer. Only the wakeup is faked: the inotify descriptor is
    // polled the way `epoll_wait` would poll it, and the dispatch is the
    // app's own hook.
    //
    // Three directories, not one, and that is the point of the test.
    // `relist` re-arms the watch on every navigation — it removes the old
    // hook and adds a new one in the same turn — and a toolkit whose
    // `FdToken` was the raw descriptor number handed the new hook the
    // token the closed one had just freed, so the app loop's registered
    // set concluded it was already in the `epoll` set. The visible
    // failure was a file manager that refreshed in the directory it
    // started in and in no directory afterwards, which a single-directory
    // test passes happily. Walking A → B → A is what catches it.
    let (root, dir) = fixture("inotify");
    let a = dir.join("a");
    let b = dir.join("b");
    write(&a.join("first.txt"), "one");
    write(&b.join("other.txt"), "other");
    let (mut h, ids) = app(&a, &root.join("xdg"));
    assert_eq!(names_of(&h, ids), ["first.txt"]);
    let listings = h.state().listings();

    write(&a.join("second.txt"), "two");
    drive_watch(&mut h, "a file created in the first directory");
    assert_eq!(
        names_of(&h, ids),
        ["first.txt", "second.txt"],
        "the new file appeared without anything asking for it"
    );
    assert!(
        h.state().listings() > listings,
        "and it appeared because the directory was re-read"
    );

    // Navigate, which re-arms the watch on the new directory.
    submit_path(&mut h, ids, &b.display().to_string());
    assert_eq!(h.state().cwd(), b);
    assert_eq!(names_of(&h, ids), ["other.txt"]);
    write(&b.join("fresh.txt"), "three");
    drive_watch(&mut h, "a file created after navigating");
    assert_eq!(
        names_of(&h, ids),
        ["fresh.txt", "other.txt"],
        "the re-armed watch fires in the second directory too"
    );

    // And back: the watch must follow again rather than having been
    // spent, and a change in the directory we *left* must not be
    // reported as a change in this one.
    submit_path(&mut h, ids, &a.display().to_string());
    assert_eq!(h.state().cwd(), a);
    write(&a.join("third.txt"), "four");
    drive_watch(&mut h, "a file created after navigating back");
    assert_eq!(
        names_of(&h, ids),
        ["first.txt", "second.txt", "third.txt"],
        "and again in the directory we came back to"
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn a_settled_file_manager_schedules_no_timer_and_sends_nothing() {
    // The idle contract every nitro app has, and the reason the refresh
    // is an inotify watch rather than a poll: with the watch armed and a
    // scan hook available, a settled file manager must still sit in
    // `epoll_wait` with no deadline and send nothing at all.
    let (root, dir) = fixture("idle");
    write(&dir.join("a.txt"), "a");
    write(&dir.join("b.txt"), "b");
    let (mut h, _ids) = app(&dir, &root.join("xdg"));

    h.settle();
    assert_eq!(
        h.next_timeout(),
        None,
        "a file manager arms no timer, not even with a watch running"
    );
    h.assert_idle(200);

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn every_part_is_addressable_the_way_hey_addresses_it() {
    // The four commands the module documentation promises, through the
    // same `introspect` entry points the socket serves:
    //
    //   hey nitro-files set path value /tmp
    //   hey nitro-files get list text
    //   hey nitro-files do list activate
    //   hey nitro-files get status value
    //
    // A widget a script cannot find is a widget the app cannot find
    // either — `Ids::of` resolves by the same names.
    let (root, dir) = fixture("hey");
    let sub = dir.join("sub");
    write(&sub.join("inner.txt"), "inner");
    write(&dir.join("outer.txt"), "outer");
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    // `set path value <dir>` navigates: the field is not focused, so its
    // `on_change` treats the write as a script's and follows it.
    {
        let (ui, state) = h.parts();
        nitro_ui::introspect::set(
            ui,
            state,
            &format!("window/{}", names::PATH),
            "value",
            &dir.display().to_string(),
        )
        .expect("set the path");
    }
    h.settle();
    assert_eq!(h.state().cwd(), dir);

    // The other side of that discriminator: typing into the field fires
    // the *same* `on_change`, and must not navigate on every letter — a
    // path bar that jumped to `/home` at the fifth character of
    // `/home/kaspar` would rewrite the field under the caret.
    h.ui().focus(ids.path);
    h.ui()
        .widget_mut::<TextField<Files>>(ids.path)
        .expect("the path bar")
        .set_text(sub.display().to_string());
    type_text(&mut h, "x");
    assert_eq!(
        h.state().cwd(),
        dir,
        "typing navigates on Enter, not on every keystroke"
    );

    // `get list text` is the *visible* rows, one per line, tab-separated
    // within a row — and the protocol escapes the newlines.
    submit_path(&mut h, ids, &dir.display().to_string());
    let text = nitro_ui::introspect::get_prop(h.ui(), &format!("window/{}", names::LIST), "text")
        .expect("the list's text");
    let text = nitro_ui::introspect::unescape(&text);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "one line per visible row: {lines:?}");
    assert_eq!(lines[0], "sub\t<dir>");
    assert!(lines[1].starts_with("outer.txt\t"), "{:?}", lines[1]);

    // `do list activate` enters the selected row.
    {
        let (ui, state) = h.parts();
        nitro_ui::introspect::invoke(
            ui,
            state,
            &format!("window/{}", names::LIST),
            "activate",
            None,
        )
        .expect("activate the row");
    }
    h.settle();
    assert_eq!(h.state().cwd(), sub, "activate entered the directory");
    assert_eq!(
        nitro_ui::introspect::get_prop(h.ui(), &format!("window/{}", names::PATH), "value")
            .expect("the path bar's value"),
        sub.display().to_string(),
        "and the path bar a script reads followed"
    );

    // `get status value` is the counts, and the confirm or message after
    // them.
    let said =
        nitro_ui::introspect::get_prop(h.ui(), &format!("window/{}", names::STATUS), "value")
            .expect("the status line");
    assert_eq!(said, "1 items, 0 selected");

    // The split view's parts: `places/place_<key>` for every place, and
    // the back button, all by the documented names.
    for (path, role) in [
        (names::PLACES.to_owned(), "container"),
        (
            format!("{}/{}home", names::PLACES, names::PLACE_PREFIX),
            "button",
        ),
        (
            format!("{}/{}root", names::PLACES, names::PLACE_PREFIX),
            "button",
        ),
        (
            format!("{}/{}trash", names::PLACES, names::PLACE_PREFIX),
            "button",
        ),
        (names::BACK.to_owned(), "button"),
        (names::UP.to_owned(), "button"),
    ] {
        let got = nitro_ui::introspect::get_prop(h.ui(), &format!("window/{path}"), "role")
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(got, role, "{path}");
    }

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn every_type_gets_its_own_icon_and_the_server_draws_them() {
    // The icon column end to end: one file of each family in a real
    // directory, read by the real `read_dir`, formatted by the real
    // `rows`, and drawn by a real server. Ten distinct names in one
    // listing, which is what the screenshot on the box shows.
    //
    // The MIME table is **empty** (see `app`), so every answer here comes
    // from the built-in extension table — which is exactly the box with
    // no `shared-mime-info`, and the arm that has to work.
    let (root, dir) = fixture("icons");
    std::fs::create_dir_all(dir.join("adir")).expect("a subdirectory");
    write(&dir.join("b.txt"), "text");
    write(&dir.join("c.rs"), "fn main() {}");
    write(&dir.join("d.png"), "not really a png");
    write(&dir.join("e.mp3"), "not really an mp3");
    write(&dir.join("f.mp4"), "not really an mp4");
    write(&dir.join("g.zip"), "not really a zip");
    write(&dir.join("h.qqq"), "an unknown extension");
    std::os::unix::fs::symlink(dir.join("adir"), dir.join("i-to-dir")).expect("symlink");
    std::os::unix::fs::symlink(dir.join("b.txt"), dir.join("j-to-file")).expect("symlink");
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    let got: Vec<(String, String)> = rows(&h, ids)
        .into_iter()
        .map(|(icon, text, _)| (text, icon))
        .collect();
    let want = [
        ("adir", "folder-fill"),
        ("b.txt", "file-earmark-text"),
        ("c.rs", "file-earmark-code"),
        ("d.png", "file-earmark-image"),
        ("e.mp3", "file-earmark-music"),
        ("f.mp4", "file-earmark-play"),
        ("g.zip", "file-earmark-zip"),
        ("h.qqq", "file-earmark"),
        // A symlink shows what it points at, because that is what
        // activating it does — and the arrow in the detail column is the
        // only thing left that says it is a link.
        ("i-to-dir", "folder-fill"),
        ("j-to-file", "file-earmark"),
    ];
    // The list is virtualised, so only what fits is on screen; assert on
    // the prefix that is, and that it is long enough to be a test.
    assert!(
        got.len() >= 6,
        "too few visible rows to say anything: {got:?}"
    );
    for (name, icon) in want.iter().take(got.len()) {
        let row = got
            .iter()
            .find(|(t, _)| t == name)
            .unwrap_or_else(|| panic!("no row {name} in {got:?}"));
        assert_eq!(&row.1, icon, "the icon for {name}");
    }

    // The **distinct** names on screen are what the server cached, one
    // entry per `(name, px)`. That is the arithmetic that separates "the
    // rows name artwork" from "each row carries its own".
    //
    // Measured against a **baseline**, not against zero: since #3715 a
    // decorated window's own title bar carries an application icon and
    // three symbolic button glyphs, so `icons_cached` counts those too.
    // The baseline is taken from a harness with a window and no icons of
    // its own rather than written down as a constant — a hard-coded 4
    // would go stale the day the frame changes, and stale silently,
    // because the number would still look plausible.
    //
    // Only the rows the **output** shows: the window is taller than the
    // harness's 320×240 output, and a row below its bottom edge is never
    // rasterised, so its icon is never cached.
    // The window is taller than the harness's 320×240 output, and an
    // icon below the output's bottom edge is never rasterised, so the
    // count is bracketed: every icon wholly on the output is cached,
    // and nothing wholly off it is. What sits on the edge may go
    // either way, which is the server's business.
    let origin = h.ui().window_position();
    let output_h = f32::from(u16::try_from(nitro_ui::test::OUTPUT.1).unwrap());
    let list_b = h.bounds(ids.list);
    let row_h = h.widget::<List<Files>>(ids.list).row_height();
    let mut surely: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut maybe: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut classify = |top: f32, icon: &str| {
        if top + 16.0 <= output_h {
            surely.insert(icon.to_owned());
        }
        if top < output_h {
            maybe.insert(icon.to_owned());
        }
    };
    for (i, (_, icon)) in got.iter().take(want.len()).enumerate() {
        classify(
            origin.y + list_b.y + i as f32 * row_h + (row_h - 16.0) / 2.0,
            icon,
        );
    }
    // Plus the split view's own: the two header buttons and the
    // fixture's sidebar rows (`docs/files.md`, "Places").
    classify(0.0, "arrow-left");
    classify(0.0, "arrow-up");
    for place in h.state().places().to_vec() {
        let row =
            nitro_ui::introspect::resolve(h.ui(), &format!("{}{}", names::PLACE_PREFIX, place.key))
                .expect("the place row");
        let icon = h.ui().children(row)[0];
        let top = origin.y + h.bounds(icon).y;
        classify(top, place.icon);
    }
    let cached = h.server().stat("icons_cached") as usize;
    let baseline = frame_icons_cached();
    assert!(
        (baseline + surely.len()..=baseline + maybe.len()).contains(&cached),
        "cached entries are the distinct icons on screen, not the rows: \
         {cached} cached, {baseline} for the frame, surely {surely:?}, maybe {maybe:?}"
    );
    assert!(
        cached < baseline + got.len() + h.state().places().len() + 2,
        "fewer entries than icon-carrying rows: {cached}"
    );
    assert!(
        h.server().stat("icon_renders") > 0,
        "the server rasterised no icon at all"
    );
    assert_eq!(
        h.server().stat("icon_refusals"),
        0,
        "every name the app used is in the server's set"
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn a_symlinked_directory_is_a_folder_icon_and_still_marked_as_a_link() {
    // The one row where the icon deliberately lies about the *kind*: a
    // symlink to a directory draws a folder, because activating it enters
    // one. The arrow in the detail column is what keeps it honest, and
    // dropping it would make a link indistinguishable from its target.
    let (root, dir) = fixture("symlink-icon");
    std::fs::create_dir_all(dir.join("real")).expect("a subdirectory");
    write(&dir.join("target.txt"), "x");
    std::os::unix::fs::symlink(dir.join("real"), dir.join("to-real")).expect("symlink");
    std::os::unix::fs::symlink(dir.join("target.txt"), dir.join("to-file")).expect("symlink");
    std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("dangling")).expect("symlink");
    let (mut h, ids) = app(&dir, &root.join("xdg"));

    let by = |h: &Harness<Files>, name: &str| -> (String, String) {
        rows(h, ids)
            .into_iter()
            .find(|(_, t, _)| t == name)
            .map_or_else(
                || panic!("no row {name}"),
                |(icon, _, detail)| (icon, detail),
            )
    };
    let (icon, detail) = by(&h, "to-real");
    assert_eq!(icon, "folder-fill", "a link to a directory is a folder");
    assert_eq!(detail, "→ <dir>", "and the arrow says it is a link");
    let (icon, detail) = by(&h, "to-file");
    assert_eq!(icon, "file-earmark");
    assert!(detail.starts_with('→'), "still marked: {detail:?}");
    // A dangling link cannot be followed, so it is a file rather than a
    // folder — which is the honest answer and is not a panic.
    let (icon, _) = by(&h, "dangling");
    assert_eq!(icon, "file-earmark");

    // And activation follows the cached answer rather than re-`stat`ing:
    // entering the row really enters the directory.
    let index = names_of(&h, ids)
        .iter()
        .position(|n| n == "to-real")
        .expect("the row");
    {
        let (ui, state) = h.parts();
        nitro_ui::introspect::invoke(
            ui,
            state,
            &format!("window/{}", names::LIST),
            "activate",
            Some(&index.to_string()),
        )
        .expect("activate the link");
    }
    h.settle();
    assert_eq!(
        h.state().cwd(),
        dir.join("to-real"),
        "activating a link to a directory entered it"
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn a_selection_move_does_not_re_resolve_a_single_mime_type() {
    // The cost claim, and it is asserted on syscalls rather than on a
    // clock: `rows()` is re-run whenever the selection changes (see
    // `refresh_rows`), so a MIME lookup inside it would mean a glob match
    // per file per keystroke and a `stat` per symlink per keystroke.
    //
    // The instrument is the `Entry` itself: the icon is a field, so the
    // rows a second `rows()` produces must be *identical* to the first
    // without the model having been re-read. A `listings` counter that did
    // not move is what says the directory was not re-read.
    let (root, dir) = fixture("icon-cost");
    for i in 0..20 {
        write(&dir.join(format!("f{i:02}.txt")), "x");
    }
    std::os::unix::fs::symlink(dir.join("f00.txt"), dir.join("link")).expect("symlink");
    let (mut h, ids) = app(&dir, &root.join("xdg"));
    let before = h.state().listings();
    let first = rows(&h, ids);

    // Walk the selection down the list. Every one of these calls
    // `refresh_rows` → `rows()`.
    for _ in 0..10 {
        h.key(key::DOWN);
    }
    h.settle();
    assert_eq!(
        h.state().listings(),
        before,
        "moving the selection re-listed the directory"
    );
    // The rows are unchanged, which is what makes the toolkit's `SetIcon`
    // diff answer zero: the icon names are the same objects they were.
    let after = rows(&h, ids);
    assert_eq!(
        first.iter().map(|(i, _, _)| i).collect::<Vec<_>>(),
        after.iter().map(|(i, _, _)| i).collect::<Vec<_>>(),
        "the icon column changed when only the selection moved"
    );

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn the_sidebar_lists_the_places_that_exist_and_highlights_the_one_on_screen() {
    // The fixture home has a `Documents` and nothing else, so the sidebar
    // is Home, Documents, a separator, Root, Trash. Clicking a row
    // navigates, and the row whose place is `cwd` is the selected one.
    let (root, dir) = fixture("places");
    write(&dir.join("a.txt"), "a");
    let (mut h, _ids) = app(&dir, &root.join("xdg"));
    let keys: Vec<&str> = h.state().places().iter().map(|p| p.key).collect();
    assert_eq!(keys, ["home", "documents", "root", "trash"]);

    let row = |h: &mut Harness<Files>, key: &str| {
        nitro_ui::introspect::resolve(
            h.ui(),
            &format!("window/{}/{}{key}", names::PLACES, names::PLACE_PREFIX),
        )
        .unwrap_or_else(|| panic!("the {key} row"))
    };
    let home = row(&mut h, "home");
    let documents = row(&mut h, "documents");
    assert_eq!(h.ui().role(home).unwrap(), nitro_ui::Role::Button);
    assert!(
        !h.widget::<SidebarRow<Files>>(home).is_selected(),
        "the scratch dir is no place"
    );

    // The rows are built from the **state's** places, never from the
    // environment's: on a box whose real `~` happens to hold exactly a
    // `Documents`, the two sets share every key and only the paths tell
    // them apart. A row that kept an environment-built closure would
    // navigate to the developer's real directory here.
    h.click(documents);
    h.settle();
    let want = root.join("home/Documents");
    assert_eq!(h.state().cwd(), want, "the row navigated to the fixture");
    assert_ne!(
        h.state().cwd(),
        nitro_launcher::spawn::home_dir()
            .unwrap_or_default()
            .join("Documents"),
        "and not to the real home's"
    );
    let rows: Vec<_> = h.state().places().iter().map(|p| p.path.clone()).collect();
    assert!(
        rows.iter()
            .all(|p| p == Path::new("/") || p.starts_with(&root)),
        "every place is the fixture's: {rows:?}"
    );
    assert!(h.widget::<SidebarRow<Files>>(documents).is_selected());
    assert!(!h.widget::<SidebarRow<Files>>(home).is_selected());

    // Navigating any other way moves the highlight too.
    submit_path_to(&mut h, &root.join("home"));
    assert!(h.widget::<SidebarRow<Files>>(home).is_selected());
    assert!(!h.widget::<SidebarRow<Files>>(documents).is_selected());
    h.assert_idle(100);

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

/// Navigate through `hey set path value`, from any page.
fn submit_path_to(h: &mut Harness<Files>, to: &Path) {
    let (ui, state) = h.parts();
    nitro_ui::introspect::set(
        ui,
        state,
        &format!("window/{}", names::PATH),
        "value",
        &to.display().to_string(),
    )
    .expect("set the path");
    h.settle();
}

#[test]
fn back_returns_to_the_previous_directory_and_is_disabled_at_the_start() {
    let (root, dir) = fixture("back");
    let sub = dir.join("sub");
    write(&sub.join("inner.txt"), "inner");
    let (mut h, ids) = app(&dir, &root.join("xdg"));
    assert!(
        !h.widget::<Button<Files>>(ids.back).is_enabled(),
        "nowhere to go back to yet"
    );
    assert!(h.state().history().is_empty());

    h.key(key::ENTER);
    h.settle();
    assert_eq!(h.state().cwd(), sub);
    assert_eq!(h.state().history(), std::slice::from_ref(&dir));
    assert!(h.widget::<Button<Files>>(ids.back).is_enabled());

    h.click(ids.back);
    h.settle();
    assert_eq!(h.state().cwd(), dir, "back went back");
    assert!(h.state().history().is_empty(), "and consumed the entry");
    assert!(!h.widget::<Button<Files>>(ids.back).is_enabled());

    // Up is a navigation like any other: it is remembered.
    let up = nitro_ui::introspect::resolve(h.ui(), "window/up").expect("up");
    h.click(up);
    h.settle();
    assert_eq!(h.state().history(), std::slice::from_ref(&dir));

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

#[test]
fn the_trash_row_opens_the_trash_files_directory() {
    let (root, dir) = fixture("trash-row");
    write(&dir.join("doomed.txt"), "x");
    let (mut h, ids) = app(&dir, &root.join("xdg"));
    let trash = nitro_ui::introspect::resolve(h.ui(), "window/place_trash").expect("trash row");
    // Nothing has been trashed, so `files/` does not exist yet: the row
    // is there anyway, and the status line says why it cannot be shown.
    h.click(trash);
    h.settle();
    assert_eq!(h.state().cwd(), dir, "a missing directory is not entered");
    assert!(
        h.state()
            .message()
            .is_some_and(|m| m.contains("Trash/files")),
        "{:?}",
        h.state().message()
    );

    // Trash something, and the row goes there.
    h.key(key::DELETE);
    h.settle();
    h.key(key::Y);
    h.settle();
    h.click(trash);
    h.settle();
    assert_eq!(h.state().cwd(), root.join("xdg/Trash/files"));
    assert!(h.widget::<SidebarRow<Files>>(trash).is_selected());
    assert_eq!(names_of(&h, ids), ["doomed.txt"]);

    let _ = std::fs::remove_dir_all(&root);
    h.quit();
}

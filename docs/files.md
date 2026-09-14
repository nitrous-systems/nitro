# nitro-files — the file manager

`nitro-files` is the M4-D application: a file manager written against
`nitro-ui`. Like `nitro-term` it exists for two reasons, and like
`nitro-term` the second is the interesting one.

It is **the app that made the toolkit's last "this is M3" deviation into
real work**. `docs/ui.md` carried a bullet saying that `Scroll` lays its
child out at full height, so "a list of ten thousand rows costs ten
thousand widgets; virtualisation is M3 and is a widget, not a new
mechanism". That was a comfortable thing to write while every app on the
stack showed a calculator's worth of widgets. A directory is not a
comfortable model: `/usr/bin` is two thousand entries, `~/.cache` on a
developer's machine is tens of thousands, and a `find`-happy user will
open a directory with a hundred thousand files in it. Either the bullet
was a plan or it was an excuse, and the way to find out was to write the
app that cannot avoid it.

The second thing it forced is smaller in code and larger in consequence.
Reading a directory is the first piece of work in this tree that is
**unbounded and not ours** — a `read_dir` plus a `stat` each, against a
disk, an NFS mount or an automounter that has gone to sleep. A toolkit
whose whole claim is "idle costs nothing, and work is proportional to
what changed" has to have an answer for work that is proportional to
nothing the user did, and `nitro-files` is where that answer was
written down.

```text
        ┌──────────────────────────────────────────────┐
        │ [ /home/kaspar/src                  ] [  ↑ ] │  TextField `path`
        ├──────────────────────────────────────────────┤     + Button `up`
        │ / nitro                            <dir>     │
        │ / old                              <dir>     │  List `list`
        │   notes.txt         912 B   2024-03-01 09:15 │  (virtualised)
        │   photo.png         4.2 MB  2024-02-11 21:40 │
        ├──────────────────────────────────────────────┤
        │ [ name                                     ] │  TextField `edit`
        ├──────────────────────────────────────────────┤     (height 0 when idle)
        │ 4 items, 1 selected — copied notes.txt       │  Label `status`
        └──────────────────────────────────────────────┘
             │                                    ▲
    activate │                                    │ entries
             ▼                                    │
   nitro_launcher::spawn            dir::read_dir / dir::Scan (thread)
```

Five widgets, four modules and no dialogs. The modules — `dir`, `mime`,
`trash`, `ops` — contain no widget code at all and are tested without a
display server; `src/lib.rs` is the tree, the keys and the wiring. That
split is not tidiness: the interesting properties of a file manager
("directories first, whatever you sorted by", "912 B but 4.2 kB", "a
`stat` that failed is still a row", "the info file is written before the
move") are properties of a *directory listing* rather than of a list
widget, and they should be assertable without a compositor in the
process.

## The model

### A row is four fields, and none of them is a path

`dir::Entry` is a name, a `Kind`, a size and an mtime. The name is the
**file name only**, never a path: the directory it is in is the one the
app is showing, and carrying it per row would be a hundred thousand
copies of the same string. `Kind` is the four cases `symlink_metadata`
can answer without following anything — `Dir`, `File`, `Symlink`,
`Other` — and the last one absorbs fifos, sockets, device nodes and,
deliberately, entries whose `stat` failed.

Two decisions in that sentence are worth defending.

**A symlink is a symlink even when it points at a directory.** Resolving
it would be a second `stat` per link on every listing, and a hang on the
one that points into a dead mount — which is the failure this app spends
most of its design avoiding. The visible consequence is that a symlinked
directory sorts among the files rather than with the directories.
Entering it still works, because the kernel follows the link when the
app `read_dir`s it, and `activate` does one `metadata` call on the one
row the user actually chose: one syscall for one deliberate action is a
different proposition from one per row per listing.

**A `stat` that failed is still a row**, with a size and time of zero. A
dangling symlink, a file deleted between the `getdents` and the `stat`, a
link into a directory we may not search — all of them stay visible. A
file manager that silently omitted files it could not stat would be one
you cannot use to find out *why* a file is broken, which is most of what
a file manager is for.  Only the `read_dir` itself is an error, because a
directory that cannot be opened has no rows at all and the app has to say
so. `a_broken_symlink_is_a_row_rather_than_a_dropped_file` and
`reading_a_missing_directory_is_an_error_value` pin the two halves.

A name that is not UTF-8 — which Unix allows, a name being a byte string
with no encoding — comes through with replacement characters. It is
visible in the list and cannot be opened or renamed by that text, which
is awkward; dropping the row would make the file invisible, which is
worse.

### Directories first, then the key, then the name

`dir::sort` orders by `(is_dir, key, lowercase name, raw name)`, and
every term earns its place.

**Directories first whatever the key**, because a file manager's list is
a place you navigate as much as a place you read. The folders are the
part you click through; scattering them through a size-ordered list of
files turns moving around the filesystem into a search problem. Every
file manager does this and it is worth saying why.
`directories_come_first_whatever_the_key` asserts it under all three
orders.

**Size and date sort largest and newest first.** The reason to sort by
either is to find an extreme, and the extreme you want should be at the
top rather than a page-down away.

**The name is always the last comparison, so the order is total.** Two
files of the same size in the same second would otherwise come out in
whatever order the filesystem handed them over — which changes between
listings of the same unchanged directory, and makes the list jump under
the user's cursor on a refresh that changed nothing.
`the_order_is_total_so_a_re_sort_does_not_reshuffle` sorts the same rows
from two different starting permutations and demands the same answer.

**Case-insensitively**, because `Downloads` sorting before `apps` is an
ASCII artefact nobody means, with the raw name breaking the tie so
`README` and `readme` keep a fixed order rather than swapping between
runs.

Hidden files are the dot rule and nothing cleverer: `Ctrl+H` toggles, and
`dir::visible` filters by a leading `.` over **borrowed** entries rather
than cloning, because it runs on every toggle and every refresh of a
directory that may hold fifty thousand rows.

### Sizes are 1000-based and times are UTC

Sizes are `kB`, `MB`, `GB` — powers of a thousand, not of 1024 — because
that is what `k` means, what the disk in the test box was sold in, and
what `ls -h --si` and every file manager written this decade shows. One
decimal above a kilobyte and none below it: `912 B` is exact and `4.2 kB`
is the precision anyone reads at a glance. The rollover is checked
against `999.95` rather than `1000.0`, so 999 999 bytes prints as
`1.0 MB` and never as `1000.0 kB`, which is a unit nobody writes
(`sizes_are_1000_based_with_one_decimal_above_a_kilobyte`).

Times are `YYYY-MM-DD HH:MM` **in UTC**, computed arithmetically from the
Unix timestamp with Howard Hinnant's days-from-civil algorithm. This is a
limitation rather than a preference and it is listed as one below. The
tree links no libc time functions and carries no timezone database:
`localtime_r` would mean a libc dependency, and parsing `/etc/localtime`
means implementing `TZif`, which is a project rather than a line. A user
east of Greenwich sees a timestamp a few hours off their clock — wrong,
but wrong by a *constant*, which keeps the column's ordering honest and
still lets you find the file you saved this morning.  The same
arithmetic, in different punctuation, writes the trash's `DeletionDate`;
`the_civil_calendar_gets_the_leap_years_right` checks 1900 (not a leap
year), 2000 (one) and the December 31st where the shifted-year
arithmetic has to put the day back into the right civil year.

## The virtual list, and why a file manager is the app that needed it

The rows go into `nitro_ui::List`, which is new in this milestone and is
argued in `docs/ui.md`. The short version of what it buys, stated here
because it is the reason this app is possible at all: **the scene holds a
screenful whatever the model holds**. A hundred thousand rows and a
hundred rows materialise the same `visible + 2` scene nodes, a scroll
that stays inside the two spare rows is exactly one `SetTransform`, and
replacing the model re-emits a screenful rather than a model.

That last one is what a directory listing actually costs here.
`refresh_rows` hands the whole `Vec<Row>` to `List::set_rows`, which
bumps a generation and lets the next paint re-derive the few dozen rows
that are materialised. So a refresh of `/usr/bin` — two thousand rows,
replaced wholesale because inotify said something changed — is a few
dozen `SetText`s on the wire, not two thousand, and not two thousand
widget allocations either.

A file manager is the app that forced this rather than, say, a task list
or a launcher, because it is the only one whose model size is chosen by
the user and unbounded. The launcher's result list is capped at what a
human will read; a bar's window list is the number of windows. A
directory is whatever is in it, and "whatever is in it" is the input the
design has to survive.

What the app pays for the widget is one `Vec<Row>` — three small
allocations a row — because a `List` owns its rows. `paint` sees
`&mut Ui<S>` and not `&mut S`, which is the toolkit's central rule, so a
borrowed model would need interior mutability threaded through the one
place this crate has none. `ListModel` is the door left open for an app
that would rather generate a row than store one; `nitro-files` does not
walk through it, because it already holds the `Vec<Entry>` and formatting
a row is cheaper than keeping a second index into it.

One consequence of virtualisation shows up in the interaction rather than
the numbers, and it is why `F2` works the way it does. **A row is not a
widget**, so there is nothing to swap a text field *into* for an inline
rename. The edit field is instead a permanent widget in the layout,
placed under the list and above the status line — where the eye already
is — carrying the old name and selected, which is what an inline rename
actually gives you. It is built once and kept at `height(0)` when idle
rather than created and destroyed around each rename: a widget that comes
and goes costs a tree pass and a scene node each way, and hiding it is a
style change.

## Long work off the loop

A directory read is not bounded by anything this program controls.
`/usr/bin` is two thousand entries and a `stat` each; a directory on a
sleeping NFS mount is a `read_dir` that returns in thirty seconds. Doing
either between two `epoll_wait`s freezes the window — not "is slow", but
*freezes*: no repaint, no keystroke, no response to the server, for as
long as the kernel takes.

So the rule is a number, and the number is **2 000**
(`dir::BIG_DIR`). Below it the directory is read inline, because a thread
and a wakeup cost more than reading forty entries and the synchronous
path finishes inside one frame. Above it the read goes on a thread. Two
thousand is roughly where a `read_dir` plus a `stat` each stops being
free on a warm cache and starts being a visible pause on a cold one, and
`/usr/bin` sits about there — which matters, because `/usr/bin` is the
directory people open when they want to know whether a file manager is
slow.

"About there" is the honest phrasing rather than a hedge: the dev box's
`/usr/bin` is **1 765 entries**, so it comes in just *under* the
threshold and is read inline, with no perceptible pause. That is the
threshold working rather than missing — the synchronous path is the
simpler one and should be taken whenever it is safe — but it does mean
the directory people reach for first is not the one that exercises the
thread. The 50 000-entry measurement below is, and it exists precisely
because the obvious test case turned out to be on the other side of the
line.

Deciding which path to take must itself be cheap, which is what
`dir::count_at_most` is for: names only, no `stat`, nothing but the
`getdents` the kernel is doing anyway, and **capped** at `BIG_DIR + 1`.
The only question is "at least this many?", and counting a 200 000-entry
directory to the end to discover it is big would be precisely the pause
being avoided.

### A worker, a channel and a pipe

`dir::Scan::start` spawns a thread that reads *and sorts* the directory —
sorting fifty thousand rows is the same kind of work as reading them, and
doing it back on the loop would give back the pause the thread exists to
avoid — and then delivers through two things at once:

```text
  worker thread                         app loop
  ─────────────                         ────────
  entries = read_dir(); sort(entries)
  tx.send(entries)         ── channel ──►  scan.take()      (the payload)
  write(pipe_w, [1])       ── pipe ─────►  epoll wakeup     (the fact)
```

**The channel carries the payload; the pipe carries the fact that there
is one.** Each half is there because the other cannot do its job.

A `std::sync::mpsc` channel alone has no way to wake a process sleeping
in `epoll_wait`: the result would sit in the channel until the user
happened to move the mouse. A pipe alone would mean pushing a
`Vec<Entry>` of arbitrary size through a descriptor, which means choosing
a serialisation — and a serialisation between two threads of the same
process is work done for nobody. So the pipe is a **doorbell**: one byte,
written *after* the result is in the channel, so a wakeup never arrives
before its payload and `take` never has to return "not yet" for a result
that is really there.

The read end is registered with `Ui::add_fd`, and that is the whole of
the toolkit's thread integration — there is none, and none is needed. A
descriptor is already something the app loop waits on, so the callback
runs from exactly where a wire message would be handled: between events,
with the tree settled, holding the same `&mut Files` and `&mut Ui<Files>`
every other callback gets. **This is the toolkit's general pattern for
long work off the loop now**, and `docs/ui.md` records it as such with
this scan as the worked example.

Four details are the difference between the pattern working and the
pattern looking like it works:

* **The read end is non-blocking**, so `Scan::take` on a spurious wakeup
  — a callback dispatched for a byte already consumed — costs an `EAGAIN`
  rather than a stall.
* **A stale result is dropped.** `Scan` remembers the path it is reading,
  and a listing for a directory the user has already left is discarded.
  It is not wrong; it is answering a question nobody is asking.
* **The list is not cleared while a scan runs.** Showing the previous
  directory for the fifty milliseconds it takes is better than a blank
  window, and the status line says `reading …`.
* **A thread that cannot be started is not a failure**, it is a slower
  path: the directory is read inline, the status line says so, and the
  user gets a pause instead of an empty window.

The worker writing into a pipe whose read end has gone gets `EPIPE`,
which it ignores — Rust's runtime ignores `SIGPIPE`, so abandoning a scan
by walking into another directory mid-read cannot kill the app.
`dropping_a_scan_does_not_take_the_process_with_it` asserts exactly that,
and `a_background_scan_wakes_a_poll_and_hands_over_sorted_entries` sleeps
on the descriptor the way the app loop does and checks that the rows come
back sorted.

## The live refresh

A `touch` in a terminal appears in the list without a poll, a timer or a
refresh key, because the directory on screen carries an **inotify** watch
registered the same way the scan's pipe is: `inotify::init` with
`CLOEXEC | NONBLOCK`, one `add_watch`, and `Ui::add_fd`. When nothing is
happening the app sits in `epoll_wait` with two extra descriptors in the
set and sends nothing at all — the same idle contract every other nitro
app has, kept by the app that has the most excuses not to.

The watch registers `CREATE`, `DELETE`, `MOVED_FROM`, `MOVED_TO` and
`ATTRIB`, and the events are **drained without being read for meaning**.
That is not laziness: every flag registered means exactly the same thing
to this application — "the listing you are showing is out of date" — and
a `read_dir` is the only way to find out what the directory holds now
anyway. An event-by-event model would have to reproduce the sort order
incrementally, get `MOVED_TO` of a name that already exists right, and
still re-read on overflow; it would be a second, subtly different
implementation of the listing, which is how the two drift.

**Draining is not optional**, and this is the one place where "we do not
care what the events say" could have become a bug. The app loop's `epoll`
is **level-triggered**, so a descriptor still holding unread events stays
readable and the hook is dispatched on every turn of the loop, for ever —
a file manager at 100 % CPU with nothing on screen. Reading the events
and throwing them away is what makes the descriptor quiet again. It is
the same hazard, and the same answer, as the launcher's pidfd reaping
(`crates/nitro-launcher/src/spawn.rs` §Reaping) and as the scan's own
hook, which is *removed* with `Ui::remove_fd` the moment its result is
taken — a pipe holding an unread byte is readable for ever too.

A watch that fires re-reads **without re-arming**: it is still on the
same directory, and `relist` would drop and re-add it, which is two
syscalls and a window during which a change is missed. Navigation is the
only thing that re-arms.

No inotify at all — the descriptor limit reached, a filesystem that does
not support it — is a file manager that does not refresh itself, not one
that does not work: the `init` failure returns and everything else
proceeds.

## Opening a file is the launcher's job

The resolution order is the freedesktop one, read partially and
deliberately, and it is worth writing out precisely because "it uses the
system associations" is the kind of sentence that hides four different
behaviours:

```text
  path
   └─ extension  ─► dir/mime: type_of
        ├─ /usr/share/mime/globs2       (system table; highest weight wins,
        │                                ties broken by the longer suffix)
        └─ builtin table                (only when globs2 has no rule)
   └─ MIME type ─► Assoc::handler_for
        ├─ every mimeapps.list [Default Applications]
        ├─ every mimeapps.list [Added Associations]
        └─ every mimeinfo.cache [MIME Cache]
           …skipping any id named in [Removed Associations]
   └─ .desktop id ─► Assoc::argv_for
        └─ nitro_launcher::desktop::parse  → argv, with the path appended
   └─ argv ─► nitro_launcher::spawn::Children::spawn
```

**The type comes from the extension, not the content.**
`shared-mime-info` also carries magic rules — byte patterns at offsets,
with priorities — and sniffing content is what `file(1)` does. Doing it
here would mean opening and reading every file in a directory to draw the
list, which is the cost the whole of `dir` exists to avoid. Getting a
type wrong on a file the user explicitly asked to open is recoverable in
a way that a slow listing is not. The visible consequence is that an
extensionless script has no type and falls through to nothing.

**The system table outranks the built-in one**, because it is the
machine's own answer and two orders of magnitude bigger. The built-in
table is thirty-odd entries for the machine that has no
`shared-mime-info` — which is the test box with a bare rootfs, and the
case where a hundred-line table is the difference between "opens your
notes" and "does nothing". Within `globs2` the highest weight wins and a
tie goes to the **longer** extension, so `.tar.gz` is compressed tar
rather than plain gzip when both rules carry the default weight of 50
(`the_system_table_outranks_the_builtin_one_and_the_longest_suffix_wins`).

The `globs2` reader keeps only the `*.ext` shape of rule. `Makefile`,
`*README*` and `core.[0-9]*` are skipped, because matching them means
implementing `fnmatch` and the types they name are ones where guessing
wrong costs a user nothing and guessing at all costs us a glob engine. A
rule carrying flags is skipped too: the flag that actually appears is
`cs`, "case-sensitive", and it exists precisely to say that `*.C` (C++)
is not `*.c` (C) — a distinction that cannot be honoured while matching
case-insensitively, so the honest thing is to decline the rule rather
than apply it in the wrong case. A malformed line is skipped rather than
failing the parse: the file belongs to the distribution, and a file
manager that guessed no types at all because line 900 was odd would be
strictly worse than one missing a rule.

Associations are searched most-important-first —
`$XDG_CONFIG_HOME/mimeapps.list`, then `$XDG_CONFIG_DIRS`, then the
`applications/mimeapps.list` and `applications/mimeinfo.cache` of
`$XDG_DATA_HOME` and `$XDG_DATA_DIRS` — which is the opposite of the
order `nitro_launcher::desktop::search_dirs` returns, for a reason: the
launcher's scan overwrites as it walks and therefore wants the winner
last, while this walks until it finds an answer and stops. One
simplification is taken and stated: a `[Removed Associations]` entry is
treated as **global**, where the spec scopes it to the files below the
one that states it. The visible difference is only the case where two
files disagree about a removal, and the direction it resolves in — the
user's own `~/.config` wins — is the direction a user's file should win
in anyway.

**The spawn path is reused, not copied.** `Assoc::argv_for` resolves the
id with `nitro_launcher::desktop::parse` and the argv goes to
`nitro_launcher::spawn::Children::spawn`. That code already detaches the
child into its own process group, sends its stdio to `/dev/null`, drops
`NITRO_SHELL_SOCKET` from its environment so an application started from
a file manager is not handed the privileged socket, and reaps the child
through a pidfd. A second implementation would be a second place for each
of those four to be forgotten, and the one most likely to be forgotten is
the one nobody sees: the environment subtraction.

Sharing the launcher's `.desktop` parser costs one known inaccuracy,
recorded here because it is a real behavioural difference rather than a
hypothetical. That parser **strips** the `%f`/`%u` field codes — a
launcher opens a program with no document, so it removes the
placeholders rather than leaving `%U` to be opened as a file called
`%U` — so this appends the path instead of substituting it. For
`Exec=prog %U` and `Exec=prog %f`, which is the overwhelming majority,
the argv is identical. For `Exec=prog %f --flag` the path lands after the
flag instead of before it. Against that: the launcher's parser already
handles `[Desktop Action …]` and `Name[de]` correctly and is already
tested, and this app gets that for free.

**The fallback is the useful half.** A `text/*` file that nothing claims
opens in a terminal running `$EDITOR`, or `vi` when that is unset — `vi`
and not `nano` because it is the one editor a Unix machine is close to
guaranteed to have. It applies to `text/*` only: an editor started on a
PDF shows its bytes, which is a worse answer than "nothing opens this".
The argv is `term -e EDITOR path`, xterm's convention and every terminal
emulator's since; `nitro-term` does not honour `-e` yet, so today this
opens a terminal in which the user types the command themselves, and the
day it does the shape is already right. Which terminal is
`nitro-term` next to this binary if there is one, else the bare name and
`PATH`'s opinion — the same reasoning `nitro_launcher::spawn::exe_dir`
exists for, so a deployed `~/nitro-bin` and a `target/debug` build each
find their own.

A file nothing claims is **not an error**: a `.iso` on a machine with no
image mounter is a file with no handler, and the honest answer is a
status line saying so rather than an invented one.

Every one of the paths above is injectable — `Assoc::at`,
`load_globs2(path)`, `Trash::at`, `Files::with_env` — and nothing in
`mime` reads the environment except `Assoc::from_env`. That matters more
than usual here: the test binary is threaded, so `std::env::set_var`
would race every other test in it, and the alternative — testing against
whatever the developer happens to have installed — is a test that passes
on one machine.

## The trash

Deleting in a file manager should be undoable, and the desktop's answer
is a directory: `$XDG_DATA_HOME/Trash`, holding `files/` (the things) and
`info/` (a `.trashinfo` per thing, saying where it came from and when it
left). Anything implementing the same spec — including whatever other
desktop the user has — can then restore what this put there, which is the
entire point of implementing a spec rather than a bin directory of our
own.

What is implemented is the **home trash** and nothing else. The spec also
describes per-filesystem trash directories (`.Trash/$uid` at the mount
point, or `.Trash-$uid`), which exist because a file cannot be `rename`d
across filesystems: a delete on a USB stick has to go to a trash *on* the
stick. Also not implemented: `DeletionDate` in local time (no timezone
database, as above), the `directorysizes` cache (an optimisation for a
trash browser this does not have), and restoring (this crate deletes;
`trash-cli` restores).

### The info file is written first, and that is the whole order

`Trash::send` writes the `.trashinfo` **before** it moves the file. The
spec requires that a file in `files/` always have its info file, because
a trash containing a file nobody knows the origin of cannot be restored —
and a crash between the two operations is exactly when that would happen.
Written first, the failure window holds an *orphan info file* instead,
which is the recoverable direction; and the orphan is cleaned up here
anyway when the move then fails, because litter in `info/` is litter a
restore trips over.

The info file is created with `O_EXCL`, which is what makes the name
reservation **atomic**. Two file managers trashing `notes.txt` at the
same moment cannot both decide the name is free, because the first
`create_new` wins and the second gets `AlreadyExists` and tries the next
name. Collisions take a numbered suffix inserted *before* the extension —
`notes 2.txt`, not `notes.txt 2` — so a restored duplicate still opens in
the right program. Ten thousand of one name is where the loop gives up
with "too many files of that name in the trash"; that is not a state to
keep searching, it is a trash that needs emptying.

### EXDEV is an error, not a silent copy

The move is a `rename`, so trashing a file that is not on the trash's
filesystem fails with `EXDEV` — "Invalid cross-device link" in the status
line. The alternative, copy-then-delete, is a **different operation
wearing the same name**: it is not atomic, it can half-finish on a full
disk and leave the user with neither the original nor a trashed copy, and
on a large directory it takes minutes with no progress to show. The
spec's answer is a trash on the other filesystem, which is the feature
this does not have. Failing loudly is the honest version of not having
it, and it is one status line away from the user understanding what
happened.

`Path=` in the info file is percent-encoded per the spec's reading of RFC
2396 — the unreserved set and `/` pass through, everything else becomes
`%XX` over the path's **bytes**, so a name that is not UTF-8 survives the
round trip. The separators are deliberately left alone, because the value
is a path and a restore has to read it as one.

## Operations

`ops` is three functions and an error type, and most of the thinking is
in the error type: these run from a status line with no modal dialogs, so
every way they can fail has to be a value with a short sentence in it.
Two of the four cases are not I/O errors at all — a name with a `/` in
it and a copy into its own subtree are refused *before* any syscall — and
dressing them up as `InvalidInput` would lose the sentence the status
line wants to show.

**Rename takes a single path component, by design.** No `/`, not `.` or
`..`, not empty, no interior NUL. This is the whole design of the
function rather than a validation detail: an inline rename box that
accepted a path would be a **move**. Typing `../elsewhere/notes.txt` into
it would take the file out of the directory the user is looking at, with
no confirmation and no visible destination, and moving files is a
different feature with a different interaction. Refusing is a sentence in
the status line; allowing it is a file that vanished. The NUL is refused
for a related reason: it truncates the name at the syscall boundary, so
`"a\0b"` would create a file called `a` while the user looks for one
called `a\0b`.

An existing target is an error rather than a silent overwrite, because
`rename(2)` would replace it and losing a file to a typo in a rename box
is not recoverable. The check is a `try_exists` before the `rename`, so
it races a second process creating the target in between; the alternative
is `renameat2(RENAME_NOREPLACE)`, which is Linux-only and outside the
rustix feature set this crate takes. The race needs two programs writing
one directory in the same millisecond, and it is not a new window —
every file manager has it.

**New folder** applies the same single-component rule for the same
reason: a box that accepted `a/b/c` would create a tree somewhere the
user is not looking, and `mkdir -p` semantics hide a typo
(`/home/u` for `home u`) as a successful creation.

**Copy is recursive, and names the copy in words.** `foo`, then
`foo copy`, then `foo copy 2` — words rather than `foo.1`, because the
result is a file name a person reads, and the suffix goes before the
extension so `notes copy.txt` still opens in the right program. Symlinks
are **followed**: a link is copied as its target's contents, which is the
paste a user means when they copy a link to a document, and which
`std::fs::copy` and `cp` without `-d` both do. What it costs is a
recursive copy of a directory containing a link to something large — the
large thing is copied. Copying a directory into itself is refused before
anything is written, by lexical path prefix; the depth is bounded at 100
anyway, so the worst a devious symlink back into the source can do is
fill a disk with a bounded amount of data rather than loop forever.

Move and delete are deliberately *not* here. Deleting is the trash,
because a file manager one mis-keypress from an unrecoverable delete is a
bad afternoon waiting to happen; moving is a `rename` the app can do
directly, and a cross-filesystem move has the copy-then-delete problem
the trash module argues about above.

### The keys, and why a confirm is a status line

| key | what |
|---|---|
| ↑ ↓ PgUp PgDn Home End | move the cursor (the `List`'s own) |
| letters | type-ahead to the first matching row |
| Enter, double-click | enter a directory, or open a file |
| Ctrl+Space | toggle the cursor's row in the selection |
| `F2` | rename the cursor's row |
| `Delete` | move the selection to the trash, after a `y`/`n` |
| `Ctrl+N` | new folder |
| `Ctrl+C` / `Ctrl+V` | copy paths / paste them here (**this app only**) |
| `Ctrl+H` | show hidden files |
| `Ctrl+S` | cycle the sort: name → size → date |
| `Escape` | cancel an edit, or answer a confirm with "no" |

The delete confirmation is **one line in the status bar**, not a dialog,
and that is a decision rather than a shortcut. The toolkit has no modal
windows; a dialog would need one. A file manager whose delete key can be
answered without leaving the keyboard is the better interaction anyway:
the status line says what will happen and the next key decides. While a
question is pending it swallows every key, which is what makes a one-line
prompt behave like a dialog without being one — a key that is neither `y`
nor `n` is *ignored* rather than passed on, because a question on screen
that the next keystroke silently dismissed would be worse than one that
waits.

The plain keys (`y`, `n`, `F2`, `Delete`, `Escape`) are `on_key`
handlers rather than shortcuts, which means the focused widget sees them
first. That is what makes `y` an answer to a confirm only when the path
bar is not the thing being typed into, and the toolkit's ordering gives
it for free.

**And it is what made the first version of the confirm answer the wrong
question.** A focused `List` consumes any printable key as type-ahead,
so with the list focused `n` was not "no" — it was "jump to the first
row beginning with `n`", and the file being deleted was called
`notes.txt`. `y` worked only because no row happened to begin with `y`:
a confirmation whose meaning depended on the file names in the
directory. A pending question therefore **takes the keyboard** — `ask`
drops the focus, answering gives it back — so the keys bubble from the
root and reach the app handler first. That is the whole of what makes a
one-line prompt behave like a dialog without being one, and
`delete_asks_first_and_n_leaves_the_file_where_it_is` pins it, including
that the focus is *borrowed* rather than kept.

## A callback that changes its own widget has to defer

One more thing this app had to learn, and it is general enough that
`docs/ui.md` now carries the rule.

The toolkit's take-out dispatch moves a widget out of its arena slot for
the duration of its own callback — which is what makes `Fn(&mut S, &mut
Ui<S>)` possible at all — so the one widget a callback cannot reach is
itself: `widget_mut` answers `Error::Busy`, a value rather than a panic.
For a button that is invisible, because `on_click` changes something
else. For a list it is the common case: *activating a row means showing
different rows in that same list*.

The first version of this app wrote the new rows straight from
`on_activate`, with `if let Ok(mut l) = ui.widget_mut(list)`. The `Err`
went into the `if let`. `cwd` moved, the path bar updated, the **rows on
screen stayed as they were**, and nothing anywhere returned an error
anybody read — while calling the same function directly worked
perfectly, because directly is not through the widget. The same shape
hid three more: a path bar that did not normalise what it showed, and a
rename box that stayed open holding the name you had just used.

So every callback here that writes back to its own widget goes through
`Ui::defer`, which runs it once dispatch is over and the tree is whole;
and every `widget_mut` in this file reports a failure (`complain`)
instead of dropping it. A write in this app always goes to a widget the
app built and still owns, so a failure is a bug in this file rather than
a condition to handle: be loud in the journal and carry on. Not
`unwrap` — a file manager should not die because a label did not update
— and not silence, which is the bug that started this.

## Everything is addressable

```text
hey nitro-files set path value /tmp     # navigate
hey nitro-files get list text           # the visible rows
hey nitro-files do list activate        # enter the selected row
hey nitro-files get status value        # "4 items, 1 selected"
```

The widget names are constants (`names::PATH`, `names::LIST`, …) rather
than string literals at the use sites, because they are a **public
interface**: a script, a test and the app must agree on them, and a typo
in one of the three would be a widget nothing can find. The app finds its
own widgets by the same names, through `introspect::resolve`, so a widget
a script can address is a widget the app can address and a rename breaks
both at once rather than one silently.

`get list text` answers the **visible** rows, one per line, with the
detail column tab-separated. That is the honest answer rather than a
convenient one: a hundred thousand rows down a socket is not a value
anybody wanted, and the widget genuinely does not draw them — the model
is not on screen, the window into it is. A script that wants a specific
row scrolls to it (`do list scroll_to`, `do list select N`) and reads
again, which is what a user does too.

### Why `set path value` navigates and typing does not

The first line of that block is subtler than it looks, and the subtlety
is the toolkit's doing rather than this app's. `set <prop>` runs the
widget's setter — the `WidgetMut` one — which fires `on_change`, *the
same callback a keystroke fires*, because the whole point of the design
is that a script and a user take one path through the app rather than
two. So a path bar that navigated on every change would navigate on
every letter: typing `/home/kaspar` would jump to `/home` at the fifth
character and rewrite the field underneath the caret. A path bar that
never navigated on change would be one no script can drive, which is a
thing the spec asks for by name.

**Focus is the discriminator**, and it is an honest one rather than a
heuristic. A user typing has the caret in the field by definition; a
`set` from outside runs the setter and moves no focus. So `on_change`
navigates only when the field is *not* focused, and a person typing
navigates on Enter, through `on_submit`, which is what Enter in a path
bar has always meant. The reasoning is in the comment at the
`text_field(start)` builder in `build()`, next to the code it explains.

## Measured

Two runs, and it matters which is which. The first table is a **dev-box,
fake-backend run**: a release build, `nitro-server` on the fake backend
(`NITRO_BACKEND=fake NITRO_FAKE_SIZE=1280x720`) on the development
machine, a real `nitro-files` client, driven through `hey`. The second
is the **test box** — real hardware, real input, real pixels
(`docs/testbox.md`), which is where the damage and idle claims are
settled, because both are statements about a compositor driving a
screen.

### Dev box, fake backend

| what | measured | spec target |
|---|---|---|
| binary, release, stripped | **816 192 bytes** (816 KB) | ≤ 800 KB — **2 % over** |
| RSS / HWM, `/usr/bin` listed (1 765 entries) | **3 628 kB** | ≤ 4 MB — **ok**, 91 % |
| RSS / HWM, 50 000-entry directory listed | **1 984 kB** | — |
| threads | **1** | — |
| `ldd` | `libc`, `libgcc_s`, vdso — nothing else | — |
| context switches, 5 s idle, `/usr/bin` listed, watch armed | **0** (voluntary and non-voluntary both) | 0 — **ok** |
| 50 000 entries: `hey set path value` → all 50 000 rows counted in the status line | **0.18 s** | < 1 s — **ok** |
| `/usr/bin` (1 765 entries) | listed **inline**, under the 2 000 threshold, no perceptible pause | — |

Three of those rows are the milestone's actual claims rather than
statistics, and each was verified live rather than reasoned about.

**The UI answers `hey` while a 50 000-entry scan is in flight.**
`get status value` replied throughout the read, which is the whole point
of doing it off the loop: the thread is reading and sorting fifty
thousand entries while the loop is still in `epoll_wait`, still
answering the introspection socket, and still able to repaint. A
synchronous read would have had the socket time out.

**Idle really is zero with inotify armed.** The watch adds a descriptor
to the `epoll` set and *no wakeups*: over five seconds with `/usr/bin`
listed and the watch live, neither the voluntary nor the non-voluntary
context-switch counter moved at all. That is the row to read first,
because it is the claim the watch puts at risk — a level-triggered
`epoll` over a descriptor that stays readable is a spin, and the drain in
`watch_fired` is the only thing standing between this app and a busy loop
with nothing on screen. Zero is the expected answer and anything else
would be a defect, not a tolerance.

**One thread, not two.** The scan thread is joined as soon as its result
is taken, so a session's worth of directory changes does not accumulate
detached threads and a settled app is single-threaded like every other
nitro client.

The **50 000-entry figure being *smaller* than the `/usr/bin` one** is
not a typo and is worth a sentence: the 50 000 rows in that test are
synthetic names in a scratch directory, short and uniform, while
`/usr/bin`'s are longer and more varied — and in both cases the *scene*
holds a screenful, so what is left is the `Vec<Entry>` and the `Vec<Row>`
and their strings. The model is the memory, which is exactly what
virtualisation was supposed to leave standing.

### The binary is 2 % over budget

816 192 bytes against a 800 KB target: **16 KB over, 2 %**. Stating it
rather than rounding it, because the honest attribution is available and
is not flattering to hide.

The app links the toolkit, which carries the introspection protocol —
~68 KB of it, **monomorphised per app-state type**, as `docs/ui.md`
§Measured argues at length and lists under *Deviations* as an M3 fix.
On top of that this app links `nitro-launcher` for the `.desktop` parser
and the spawn path, which is the reuse argued above and is cheaper than
the second copy it replaces but is not free. The de-monomorphisation
`docs/ui.md` already records as the fix — routing the protocol through a
small `dyn` interface, at the cost of one virtual call per request
against a request rate measured in tens per second — would collapse that
68 KB to one instantiation for the whole program and **more than cover
the overrun** on its own.

So the number is over, the cause is known, the fix is already written
down somewhere else, and none of that makes 816 KB anything other than
816 KB today.

### Test box, real hardware

Pentium G3240 (2 cores, no AVX2), 3.3 GB, HDMI 1920×1080 @60
(`docs/testbox.md`), release build deployed with `just deploy`, driven
through `hey` and real `ydotool` input.

| what | measured | budget |
|---|---|---|
| RSS / HWM, `/usr/bin` listed (1 860 entries) | **3 592 kB** | ≤ 4 MB — **ok** |
| binary | **817 352 bytes** | ≤ 800 KB — **2 % over** |
| idle 60 s, `/usr/bin` listed, watch armed | **0 app CPU ticks, 1 voluntary context switch, +2 server frames** | 0 |
| the same 60 s with the app **killed** (control) | **+2 server frames** | — |
| Page Down over `/usr/share`: pixels that changed | bbox **704×408 = 287 232 px** | ≈ the list area, not the 2 073 600 px screen |
| one Down (a selection move) | bbox 121×401 = 48 521 px | — |
| `damage_px_mean` over 120 frames of pure scrolling | **287 232** | agrees with the pixels |
| `paint_us_mean` while scrolling | **200 µs** | — |
| `/usr/bin` (1 860 entries), `set path value` → rows | **25 / 35 / 25 ms** | time to first paint |
| a `.txt` with no handler | `nitro-term -e vi …/notes.txt` spawned | opens a terminal |
| #555: the child after it exits | `ps --ppid` empty, **no zombie**, no second spawn | reaped |

Four of those rows are the milestone's claims rather than statistics.

**The damage claim is settled on pixels, not on `stats`.** `damage_px`
is *the server's own opinion about what it repainted*, which is exactly
the thing under test — quoting it alone would be marking one's own
homework. So the headline number is the bounding box of the pixels that
actually differ between two `nitro-shot --raw` framebuffer readbacks
taken either side of one Page Down, and `damage_px_mean` is the
corroborating instrument. They agree to the pixel: **704×408, which is
the list's bounds exactly**, out of a 1920×1080 screen. Scrolling a
directory of thousands of entries repaints the list and nothing else —
not the path bar, not the status line, not the bar at the top of the
screen.

**The idle claim has a control, and the control is why the number is
readable.** Sixty seconds with `/usr/bin` listed and the watch armed
cost the app zero CPU ticks and one voluntary context switch, while the
server advanced **two** frames. Two frames is not nothing, and the
tempting report is "0 app ticks, 2 frames". Running the same minute with
`nitro-files` killed gives **the same two frames**: they are the bar's
minute clock, not this app. A measurement with no control could not have
told those apart.

**The 25 ms for `/usr/bin` is the inline path, deliberately.** At 1 860
entries it is under the `BIG_DIR` threshold, so it is read on the loop —
and 25 ms is the answer to "is that acceptable?", measured rather than
assumed. The threshold is where it is because 2 000 entries is roughly
where that number starts to be felt.

**`nitro-term` really is spawned, and it really does ignore `-e`.** The
process table shows `nitro-term -e vi /…/notes.txt`, which is the argv
this app builds and hands to `nitro_launcher::spawn`; the window that
appears is a shell rather than `vi`, because `-e` is not implemented
yet. Both halves are the documented state, and the first half is the
half this app owns.

### Four defects that 78 passing tests could not see

Recorded because the *why* is transferable. Three were found by running
the program on hardware after every test in the workspace passed, and
the fourth by writing integration tests against it afterwards.

**1. A descriptor hook's token was a recycled descriptor number.** The
symptom was this app: the listing refreshed itself in the first
directory and in **no directory afterwards**. `hey get list text` was
correct, `relist` worked, nothing logged anything. `nitro-files` re-arms
its inotify watch on every navigation — drop one hook, add the next —
and `nitro_ui::FdToken` *was* the raw fd of the toolkit's own `dup`. A
descriptor number is recycled the instant it is closed and the kernel
hands back the lowest free one, so the new hook took the retired one's
number, and the app loop's list of what it had already registered with
`epoll` concluded it was already in the set. It was not: closing a
descriptor removes it from every epoll set. A hook that existed, was
never registered, and could never fire. `FdToken` is now an opaque
monotonic `u64`; `a_re_armed_fd_hook_gets_a_fresh_token` in
`crates/nitro-ui/tests/ui.rs` is the regression. It was invisible to
every test because the tests call `Ui::run_fd` directly and `run_fd`
worked perfectly — only the loop was wrong.

**2. A confirmation whose answer depended on the file names in the
directory.** `Delete` asks in the status line and the next key decides.
App-level key handlers are offered only what the focused chain declined,
and a focused `List` consumes any printable key as type-ahead — so `n`
was not "no", it was "jump to the first row starting with `n`", and the
file under the cursor was called `notes.txt`. `y` worked only because no
row happened to begin with `y`. A pending question now takes the
keyboard: `ask` drops the focus, answering restores it.

**3. `EXDEV` reached the user as "Invalid cross-device link (os error
18)".** Accurate and useless. Trashing a file under `/tmp` on a box
whose home is a different filesystem is the ordinary case, and the
behaviour is deliberate — so the fix was to say so in words rather than
to change it.

**4. A callback wrote to its own widget and the refusal went in the
bin.** Argued in full under *A callback that changes its own widget has
to defer* above; the short version is that `on_activate` navigated and
then wrote the new rows into an `Error::Busy` an `if let Ok(…)` threw
away, so `cwd` and the path bar moved and the rows did not. Found by
`crates/nitro-files/tests/files.rs` — the one of the four a test caught,
and only because the test asserted on *the rows the widget is showing*
rather than on the app's own state.

All four share a shape worth naming: **the instrument agreed with the
code because it was measuring the layer below the broken one.** `hey get
path value` reads the app's state and not the widget's. The fd tests
call `Ui::run_fd` directly and not the loop that dispatches to it. A
model can be perfect while the glass is wrong, which is `docs/term.md`'s
lesson arriving again in a third costume.

Two measurement traps this run walked into, in the spirit of the notes
in `docs/testbox.md`:

* **`damage_px_mean` is a rolling mean over `PAINT_WINDOW = 120` frames,
  not a cumulative total.** Computing an interval's damage as
  `mean₁·frames₁ − mean₀·frames₀` produces a *negative* number, which is
  how this was discovered. To measure one kind of frame, fill the window
  with that kind of frame and read the mean.
* **`pgrep -f nitro-files` matches the ssh command line that contains
  the string**, so a control that was supposed to run with the app
  absent reported "still up" three times while the app was in fact gone.
  `pgrep -x` on the binary name.

## Limitations

Each of these is a real feature rather than a missing case, and each is
recorded because the spec asks for them rather than because they are
regrets.

* **No drag and drop.** There is no drag protocol on the wire, and a
  drag between two clients is a server-side concept — a source, a target,
  a negotiated type and a cursor that follows the pointer across window
  boundaries. Copy and paste are the keyboard path to the same result
  within this app.
* **No clipboard integration with other programs.** `Ctrl+C` and
  `Ctrl+V` are the *app's own* clipboard, a `Vec<PathBuf>` in its state.
  A file copied here cannot be pasted into another program and a file
  copied elsewhere cannot be pasted here, because there is no clipboard
  protocol yet: a clipboard needs a selection owner and a protocol for
  offering and requesting types, which the server does not have. This is
  the same wall `nitro-term` records for `Ctrl+Shift+C`, and it closes
  for both apps on the same day.
* **No thumbnails.** A thumbnail means decoding images — untrusted bytes,
  a decoder dependency, a cache directory and a second thread pool — in a
  process whose whole design argument is that it does no work it was not
  asked for. `nitro-wallpaper` refused an image decoder for the same
  reason (`DEPENDENCIES.md`), and a file manager reading every file in a
  directory to draw the list is precisely what the extension-based MIME
  lookup exists to avoid.
* **No icon theme; glyphs only.** A directory gets `/`, a symlink `~`,
  everything else a space. An icon theme means an SVG rasterizer or a PNG
  decoder plus the freedesktop icon-theme lookup, and the server draws
  rectangles and text; a glyph is what it can draw with the fonts it has.
* **No mounts UI.** No removable-device list, no mount or unmount, no
  `udisks`. All three are D-Bus, and `DESIGN.md` spends its one D-Bus
  permission on the session rather than here.
* **Timestamps are UTC.** Argued above: no timezone database in the tree.
  Wrong by a constant offset, so the column's ordering stays honest.
* **Symlinks are not stat'ed through in the listing.** A symlinked
  directory shows as a symlink and sorts with the files. Following would
  be a `stat` per link per listing and a hang on the one pointing into a
  dead mount; `activate` follows the single row the user chose.
* **`nitro-term` does not honour `-e` yet**, so the `text/*` editor
  fallback opens a terminal rather than the editor. The argv is already
  the conventional `term -e EDITOR path`, so this closes with a change in
  `nitro-term` and none here.
* **Selection by pointer is single.** `Event::PointerDown` carries a
  position and a button and no modifier mask, so Ctrl-click and
  Shift-click are not the pointer half of the multi-selection the
  keyboard has (`Ctrl+Space`, `Shift`+arrows). Putting a modifier mask on
  every pointer event to give one widget two more gestures is a protocol
  change, and it is not a widget's to make; the limitation is recorded in
  `docs/ui.md` too.
* **The type comes from the extension, never the content**, so an
  extensionless script has no type and nothing opens it. Argued above.
* **`globs2` rules that are not plain suffixes are ignored**, as are
  case-sensitive (`cs`) rules. So `Makefile` has no type here.
* **The home trash only**, so a delete on another filesystem fails with
  `EXDEV` rather than copying. Argued above.
* **No restore-from-trash and no trash browser.** The trash is a
  directory another program can already browse, and `trash-cli` restores
  from it.
* **A `[Removed Associations]` entry is treated as global** rather than
  scoped to the files below the one stating it.
* **A `.desktop` field code that is not last gets the path in the wrong
  position** (`Exec=prog %f --flag`), because the path is appended rather
  than substituted — the price of sharing the launcher's parser.
* **No properties, no permissions editor, no free-space display.** Each
  is a feature, not a missing case; none is needed to prove anything this
  milestone claims.

## Dependencies

`nitro-ui`, `nitro-launcher` and `rustix` — and **no new external
crate**. The launcher is a workspace crate and is here for its
`.desktop` parser and its spawn path, both already argued; `rustix`
provides `inotify` (`fs`), the scan's pipe (`pipe`), `poll` (`event`) and
`getuid`/`getpid` for the temporary-path fallbacks (`process`), with the
feature set narrowed to those four. `DEPENDENCIES.md` carries the row.

## Testing

* `src/dir.rs` unit-tests the model with no display server: the four
  kinds, a dangling symlink surviving a failed `stat`, the
  directories-first rule under all three sort keys, case-insensitive
  names, the total order asserted from two starting permutations, the
  hidden-file filter, 1000-based sizes with the 999 999-byte rollover,
  UTC civil time including 1900 and 2000, the path bar's `~` and `..`
  resolution with `$HOME` handed in rather than set, the capped count,
  and the background scan — woken through a real `poll`, delivering
  sorted entries, delivering a failure as a value, and surviving being
  dropped mid-read.
* `src/mime.rs` unit-tests the resolution order against fixture
  directories: the built-in table, a system rule outranking it, the
  longest suffix breaking a weight tie, `[Default Applications]` over
  `[Added Associations]` over `mimeinfo.cache`, a config list outranking
  a system one, a removal skipped wherever it is offered, an id resolving
  to an argv with the path appended, an unlaunchable entry falling
  through, the `text/*` editor fallback with `$EDITOR` handed in, and a
  non-text file with no handler opening nothing.
* `src/trash.rs` unit-tests the spec subset: the move plus the info file,
  a numbered second copy, a directory going whole, a failed send leaving
  no orphan info file, the directories created on demand, the info body's
  shape, and percent-encoding that leaves the separators alone.
* `src/ops.rs` unit-tests the refusals as hard as the successes: a rename
  staying in its directory, a rename to a path refused rather than
  moving the file, no silent overwrite, a NUL refused, the copy-name
  sequence, a recursive copy with its permission bits, a symlink copied
  as its target's contents, a dangling one failing as a value, a
  directory into itself refused, and every error printing one short line
  for the status bar.
* `tests/files.rs` drives the assembled app through the harness — a real
  server on the fake backend, a real client, real pixels — which is where
  the widget half, the key handling and the addressing are checked.

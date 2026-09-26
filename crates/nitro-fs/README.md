# nitro-fs

The widget-free directory model shared by the file manager
(`nitro-files`) and the toolkit's file picker. It moved out of
`nitro-files` because that crate depends on `nitro-ui`, so the picker in
`nitro-ui` could not use it. Depends on `rustix` only.

## Layout

| file | what |
|---|---|
| `src/dir.rs` | `Entry`, `Kind`, `read_dir`, `sort`, `visible`, the background `Scan`, size/time formatting, `resolve`, `parent_of` |
| `src/mime.rs` | the extension → MIME type half: the built-in table, `globs2` parsing, `type_of`, `icon_for` |
| `src/places.rs` | the sidebar's places: home, the XDG user dirs, root and trash |

What opens a type (`mimeapps.list`, `.desktop` handlers), the trash and
the file operations stay in `nitro-files`. See `docs/files.md`.

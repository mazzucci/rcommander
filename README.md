# rcommander

A Total Commander-style dual-pane file manager written in Rust, with a terminal UI and a browser UI (Rust compiled to WebAssembly).

Runs on **Linux and macOS** (both are built and tested in CI). Deleting moves files to the system Trash (freedesktop trash on Linux, NSFileManager on macOS).

## Layout

| Crate | Purpose |
|-------|---------|
| `crates/rc-core` | Platform-independent model: locations, panel state (cursor, selection, sort, filter), side-by-side diff engine, directory compare, HTTP API types. Compiles to native and `wasm32`. |
| `crates/rc-vfs`  | Native back end: local filesystem listing, reading, copy/move/delete (to Trash)/mkdir. |
| `crates/rcmd`    | The `rcmd` binary: terminal UI, and `rcmd web`, a local server for the browser UI. |
| `crates/rc-web`  | Browser UI (Leptos), compiled to WebAssembly with [trunk](https://trunk-rs.github.io/trunk/). |

## Usage

Terminal UI:

```sh
cargo run --release -p rcmd -- [LEFT_DIR] [RIGHT_DIR]
```

Browser UI (opens your default browser; `--browser "Google Chrome"` to pick one, `--no-open` to just print the URL):

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk --locked                       # or download a release binary
(cd crates/rc-web && trunk build --release)        # builds crates/rc-web/dist
cargo run --release -p rcmd -- web [LEFT_DIR] [RIGHT_DIR]
```

Release builds embed the built front end, so the resulting `rcmd` binary is self-contained. The server listens on 127.0.0.1 only and every API call needs the random token from the URL it prints.

Both UIs share the key bindings below. In the browser, F4 opens the file with its default application.

| Key | Action |
|-----|--------|
| ↑ ↓ PgUp PgDn Home End | move cursor |
| Enter / → | open directory, view file |
| Backspace / ← | parent directory |
| Tab | switch panel |
| Space / Insert | select / deselect |
| `+` `-` `*` | select by mask, deselect by mask, invert selection |
| Esc | clear selection and filter |
| `/` Ctrl+F | quick filter |
| `.` | show / hide hidden files |
| `g` Ctrl+L | go to path |
| Ctrl+S, Ctrl+F3..F6 | cycle sort / sort by name, extension, date, size |
| Ctrl+R, Ctrl+U | refresh / swap panels |
| F2 | diff the files under both cursors (or two selected files) — `n`/`p` next/previous change, `f` fold |
| F3 / F4 | view / edit with `$VISUAL` or `$EDITOR` |
| F5 / F6 | copy / move or rename |
| F7 / F8 | new directory / delete to Trash (Shift+F8: permanent) |
| F9 | compare directories (toggle) |
| `1`..`9` | same as F1..F9, so Mac laptops don't need the fn key |
| `q` F10 Ctrl+C | quit |

## Development

```sh
cargo test          # unit tests
cargo clippy --all-targets
cargo fmt --all
```

## Process

Every change goes through a feature branch → code review → pull request → CI (fmt, clippy and tests on Linux and macOS; the wasm32 front end built with trunk and served by a release binary) → merge.

Building from source needs a Rust toolchain (`rustup`) and a C linker (Xcode Command Line Tools on macOS, `build-essential` or equivalent on Linux).

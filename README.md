# rcommander

A Total Commander-style dual-pane file manager written in Rust, with a terminal UI and a browser UI (Rust compiled to WebAssembly).

## Layout

| Crate | Purpose |
|-------|---------|
| `crates/rc-core` | Platform-independent model: locations, panel state (cursor, selection, sort, filter), side-by-side diff engine, directory compare, HTTP API types. Compiles to native and `wasm32`. |
| `crates/rc-vfs`  | Native back end: local filesystem listing, reading, copy/move/delete (to Trash)/mkdir. |

## Development

```sh
cargo test          # unit tests
cargo clippy --all-targets
cargo fmt --all
```

## Process

Every change goes through a feature branch → code review → pull request → CI (fmt, clippy and tests on Linux and macOS, plus a wasm32 build of `rc-core`) → merge.

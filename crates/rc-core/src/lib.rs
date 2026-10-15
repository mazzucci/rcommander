//! Shared model for rcommander. No I/O here, so it compiles for both native and wasm32.

pub mod api;
pub mod diff;
pub mod dircmp;
mod entry;
pub mod format;
mod location;
pub mod panel;

pub use diff::{DiffResult, diff_text};
pub use entry::{Entry, Listing};
pub use location::Location;
pub use panel::{Panel, Row, SortKey};

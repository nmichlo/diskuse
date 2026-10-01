//! Read-only disk usage scanning. Nothing here deletes, renames or writes
//! files, except saved scans in the cache dir ([`CacheDir`]).

#![deny(unsafe_code)]

mod json;
mod report;
mod scan;
mod store;
mod sys;
mod tree;

pub use json::json;
pub use report::report;
pub use scan::{Reader, ScanError, ScanOptions, scan};
pub use store::{CacheDir, Saved};
pub use tree::{ChildIndex, LARGEST, LargeFile, Record, Totals, Tree};

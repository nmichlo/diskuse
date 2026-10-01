//! Read-only disk usage scanning. Nothing here deletes, renames or writes
//! files.

#![deny(unsafe_code)]

mod report;
mod scan;
mod sys;
mod tree;

pub use report::report;
pub use scan::{Reader, ScanError, ScanOptions, scan};
pub use tree::{ChildIndex, Record, Totals, Tree};

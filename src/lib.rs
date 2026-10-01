//! Read-only disk usage scanning and browsing. Nothing here deletes,
//! renames or writes files, except saved scans in the cache dir
//! ([`CacheDir`]), and the only programs it runs are `open` and `xdg-open`
//! ([`reveal`]).

#![deny(unsafe_code)]

mod access;
mod app;
mod browse;
mod json;
mod labels;
mod report;
pub mod reveal;
mod scan;
mod store;
mod sys;
mod tree;
mod volumes;
mod watch;

pub use access::FullDiskAccess;
pub use app::{App, Preflight, browse};
pub use browse::{Browser, Env};
pub use json::json;
pub use report::report;
pub use scan::{Reader, ScanError, ScanOptions, scan, scan_live};
pub use store::{CacheDir, Saved};
pub use tree::{ChildIndex, LARGEST, LargeFile, Progress, Record, Totals, Tree};
pub use volumes::{Mount, mounts};
pub use watch::update;

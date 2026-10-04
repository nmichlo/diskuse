//! Read-only disk usage scanning and browsing. Nothing here deletes,
//! renames or writes files, except saved scans in the cache dir
//! ([`CacheDir`]), and the only programs it runs are `open` and `xdg-open`
//! ([`reveal`]).

#![deny(unsafe_code)]

mod access;
mod app;
mod browse;
mod cli;
mod json;
mod labels;
mod live;
mod report;
pub mod reveal;
mod scan;
mod store;
mod style;
mod sys;
mod tree;
mod volumes;
mod watch;

pub use access::FullDiskAccess;
pub use app::{App, Preflight, browse};
pub use browse::{Browser, Env};
pub use cli::cli;
pub use json::json;
pub use live::{Event, Live, LiveOptions, live};
pub use report::report;
pub use scan::{Reader, ScanError, ScanOptions, Stop, scan, scan_live};
pub use store::{CacheDir, Saved, SavedFile, SavedTree};
pub use tree::{LARGEST, LargeFile, Progress, ReadTree, Record, Tree};
pub use volumes::{Mount, mounts};

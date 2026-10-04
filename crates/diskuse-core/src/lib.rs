//! Read-only disk usage scanning: [`scan`] a folder into a [`Tree`], or
//! follow it [`live`]. The library of the `diskuse` command: everything
//! in it that does not draw on a terminal.
//!
//! Nothing here deletes, renames or changes the files it scans. The only
//! files it writes are saved scans, in the cache dir ([`CacheDir`]).

#![deny(unsafe_code)]

mod access;
mod json;
mod labels;
mod live;
mod report;
mod scan;
mod store;
mod sys;
mod tree;
mod volumes;
mod watch;

pub use access::{FullDiskAccess, preflight};
pub use json::json;
pub use labels::{Label, Labels, Tier};
pub use live::{Event, Handler, Live, LiveOptions, Reason, live};
pub use report::{Units, largest_first, report};
pub use scan::{Reader, ScanError, ScanOptions, Stop, scan, scan_live};
pub use store::{CacheDir, Saved, SavedFile, SavedTree};
pub use sys::{allocated, exists};
pub use tree::{File, LARGEST, LargeFile, Progress, ReadTree, Record, Tree};
pub use volumes::{Mount, mounts};

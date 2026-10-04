//! Read-only disk usage scanning: [`scan`] a folder into a [`Tree`], or
//! follow it [`live`]. Nothing here deletes, renames or writes files,
//! except saved scans in the cache dir (`CacheDir`, with the `cli`
//! feature), and the only programs it runs are `open` and `xdg-open`
//! (`reveal`, with `cli`).
//!
//! The `cli` feature, on by default, adds the `diskuse` command: the
//! browser, text and JSON output, and saved scans.

#![deny(unsafe_code)]

mod live;
mod scan;
mod sys;
mod tree;
mod volumes;
mod watch;

pub use live::{Event, Live, LiveOptions, Reason, Waker, live};
pub use scan::{Reader, ScanError, ScanOptions, Stop, scan, scan_live};
pub use tree::{File, LARGEST, LargeFile, Progress, ReadTree, Record, Tree};
pub use volumes::{Mount, mounts};

#[cfg(feature = "cli")]
mod access;
#[cfg(feature = "cli")]
mod app;
#[cfg(feature = "cli")]
mod browse;
#[cfg(feature = "cli")]
mod cli;
#[cfg(feature = "cli")]
mod json;
#[cfg(feature = "cli")]
mod labels;
#[cfg(feature = "cli")]
mod report;
#[cfg(feature = "cli")]
pub mod reveal;
#[cfg(feature = "cli")]
mod store;
#[cfg(feature = "cli")]
mod style;
#[cfg(feature = "cli")]
mod volume_list;

#[cfg(feature = "cli")]
pub use json::json;
#[cfg(feature = "cli")]
pub use report::{Units, report};
#[cfg(feature = "cli")]
pub use store::{CacheDir, Saved, SavedFile, SavedTree};
// the command's own parts: public for its tests and the Python console
// script, not for use as a library
#[cfg(feature = "cli")]
#[doc(hidden)]
pub use {
    access::FullDiskAccess,
    app::{App, Preflight, browse},
    browse::{Browser, Env},
    cli::cli,
};

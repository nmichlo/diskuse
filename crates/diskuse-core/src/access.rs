//! Whether a scan can read everything. On macOS a privacy layer (TCC)
//! guards some folders on top of Unix permissions: it denies with `EPERM`
//! where Unix permissions deny with `EACCES`. Full Disk Access, given to
//! the terminal app, lifts it.

use crate::sys;
use rustix::io::Errno;
use std::path::Path;

/// Folders whose first read makes macOS ask the user, one popup each.
const PROMPTED: [&str; 3] = ["Desktop", "Documents", "Downloads"];

/// Folders only Full Disk Access can read. Each may be missing.
const PROTECTED: [&str; 3] = [
    "Library/Application Support/com.apple.TCC",
    "Library/Safari",
    "Library/Mail",
];

/// Whether the terminal has Full Disk Access, as far as a probe can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullDiskAccess {
    Granted,
    Missing,
    /// No protected folder exists to probe, or Linux.
    Unknown,
}

/// Lists `~/Desktop`, `~/Documents` and `~/Downloads`, so any macOS
/// popups asking for them appear now, together, not one by one during the
/// scan. Then probes the [`PROTECTED`] folders for Full Disk Access.
pub fn preflight(home: &Path) -> FullDiskAccess {
    let list = |dir: &str| {
        let fd = sys::open_root(&home.join(dir))?;
        sys::read_dir(&fd, false, |_| {})
    };
    for dir in PROMPTED {
        // only asking matters, not the answer
        let _ = list(dir);
    }
    for dir in PROTECTED {
        match list(dir) {
            Ok(()) => return FullDiskAccess::Granted,
            Err(Errno::PERM) => return FullDiskAccess::Missing,
            Err(_) => {}
        }
    }
    FullDiskAccess::Unknown
}

//! The mounted filesystems, with their sizes.

use crate::sys;
use std::io;
use std::path::PathBuf;

/// One mounted filesystem, as the OS lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub point: PathBuf,
    /// The filesystem type: `apfs`, `ext4`, `proc`...
    pub fs: String,
    /// macOS `nobrowse`: hidden from Finder, like the VM and Preboot
    /// volumes, `devfs` and `autofs`. Never set on Linux.
    pub hidden: bool,
    /// Bytes in all.
    pub total: u64,
    /// Bytes in use: `f_blocks - f_bfree`. On APFS that is the whole
    /// container, all its volumes, so on macOS `/` counts the hidden Data
    /// volume, which a scan of `/` reaches through firmlinks.
    pub used: u64,
    /// Bytes free for users other than root.
    pub free: u64,
}

/// Every mounted filesystem. On macOS the sizes may be a few seconds old,
/// so a hung network mount cannot block the list.
pub fn mounts() -> io::Result<Vec<Mount>> {
    sys::mounts()
}

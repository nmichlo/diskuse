//! The only module that makes raw OS calls. Every open is read-only.
//!
//! Directory readers live in their own files and all yield [`Entry`].
//! [`read_dir`] picks the fastest one for the OS. [`watch`] reports the
//! changes below a path, on macOS only, and [`Inotify`] those in a few
//! dirs, on Linux only.

#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(target_os = "linux")]
mod inotify;
#[cfg(not(target_os = "macos"))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod portable;

use crate::volumes::Mount;
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, OFlags, Stat};
use rustix::io::{Errno, Result};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use std::ffi::CStr;
use std::io;
use std::path::Path;
use std::sync::mpsc::Sender;

/// The flags of every open: read-only, directories only, never through a
/// symlink, never inherited by a child process.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

/// One directory entry, as `lstat` sees it. Never `.` or `..`.
pub struct Entry<'a> {
    pub name: &'a CStr,
    pub kind: Kind,
    pub ino: u64,
    pub dev: u64,
    pub nlink: u64,
    /// Allocated bytes (`st_blocks * 512`), not the apparent length.
    pub bytes: u64,
    /// A mount point, as far as the reader can tell. On macOS the Data
    /// volume has the same `st_dev` as `/`, so only the macOS reader can
    /// see that boundary.
    pub mount: bool,
    /// Of `bytes`, those freed by deleting this entry alone: all of them,
    /// unless some are shared with a clone. Only the macOS reader fills it,
    /// and only when asked; otherwise 0.
    pub private: u64,
}

/// What `fstat` says about an open directory.
pub struct DirStat {
    pub dev: u64,
    pub bytes: u64,
}

/// Opens the scan root. Unlike children, a symlinked root is followed (like
/// `du -H`), so `disksweep scan /tmp` works on macOS where `/tmp` is a link.
pub fn open_root(path: &Path) -> Result<OwnedFd> {
    let flags = DIR_FLAGS.difference(OFlags::NOFOLLOW);
    // sys owns the only openat, with fixed read-only flags.
    #[allow(clippy::disallowed_methods)]
    rustix::fs::openat(rustix::fs::CWD, path, flags, rustix::fs::Mode::empty())
}

/// Opens `name` relative to its parent, so no full path is ever built.
pub fn open_child(parent: BorrowedFd<'_>, name: &CStr) -> Result<OwnedFd> {
    // sys owns the only openat, with fixed read-only flags.
    #[allow(clippy::disallowed_methods)]
    rustix::fs::openat(parent, name, DIR_FLAGS, rustix::fs::Mode::empty())
}

pub fn dir_stat(fd: BorrowedFd<'_>) -> Result<DirStat> {
    rustix::fs::fstat(fd).map(DirStat::of)
}

/// [`dir_stat`] of `name` in the open dir `parent`, by `lstat`, so of a
/// dir that cannot be opened too.
pub fn child_stat(parent: BorrowedFd<'_>, name: &CStr) -> Result<DirStat> {
    rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map(DirStat::of)
}

impl DirStat {
    // the fields are `u64` on Linux, signed on macOS
    #[cfg_attr(not(target_os = "macos"), allow(clippy::unnecessary_cast))]
    fn of(st: Stat) -> Self {
        Self {
            dev: st.st_dev as u64,
            bytes: st.st_blocks as u64 * 512,
        }
    }
}

/// One change the OS recorded, from [`watch`].
pub struct Event {
    /// Increases with every change on the system.
    pub id: u64,
    pub what: What,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum What {
    /// Something at this absolute path was created, removed, renamed or
    /// written to.
    Changed(Box<[u8]>),
    /// The OS merged the changes below this path: list all of it again.
    Rescan(Box<[u8]>),
    /// The end of the changes recorded before the watch started. Later ones
    /// are live.
    HistoryDone,
    /// Changes were lost, or the watched path itself moved: only a full
    /// scan is right.
    Lost,
}

/// Watches a path until dropped.
#[cfg(target_os = "macos")]
pub use fsevents::Stream;
#[cfg(not(target_os = "macos"))]
pub enum Stream {}

#[cfg(not(target_os = "macos"))]
impl Stream {
    // never built off macOS, as `watch` returns `None` there
    pub fn flush(&self) {
        match *self {}
    }
}

/// Sends the changes below `path`, an absolute path with no symlinks, with
/// event ids after `since`, first the recorded ones, then live ones, in
/// batches to `tx`. `None` where the OS keeps no record of changes, so on
/// Linux always.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn watch(path: &CStr, since: u64, tx: Sender<Vec<Event>>) -> Option<Stream> {
    #[cfg(target_os = "macos")]
    return fsevents::Stream::start(path, since, tx);
    #[cfg(not(target_os = "macos"))]
    None
}

/// A change in watched dirs, from [`Inotify::read`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum Note {
    /// Something in the dir of this watch changed.
    Changed(i32),
    /// The dir of this watch itself was deleted, renamed or unmounted.
    Gone(i32),
    /// Changes were lost: any watched dir may have changed.
    Lost,
}

/// Watches dirs for changes, inotify on Linux.
#[cfg(target_os = "linux")]
pub use inotify::Inotify;
#[cfg(not(target_os = "linux"))]
pub enum Inotify {}

/// Elsewhere there is never one.
#[cfg(not(target_os = "linux"))]
impl Inotify {
    pub fn new() -> Option<Self> {
        None
    }

    pub fn add(&self, _: &CStr, _: bool) -> Option<i32> {
        match *self {}
    }

    pub fn remove(&self, _: i32) {
        match *self {}
    }

    pub fn read(&self, _: impl FnMut(Note)) {
        match *self {}
    }
}

/// The id of the latest change on the system, for [`watch`], or 0 where
/// the OS keeps no record of changes.
pub fn event_id() -> u64 {
    #[cfg(target_os = "macos")]
    return fsevents::current_event_id();
    #[cfg(not(target_os = "macos"))]
    0
}

/// An id of the record of changes of device `dev`, or 0 if it keeps none.
/// When it differs from an earlier one, the record was purged, erased or
/// wrapped, so earlier event ids mean nothing in it.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn event_store(dev: u64) -> u128 {
    #[cfg(target_os = "macos")]
    return fsevents::store_uuid(dev);
    #[cfg(not(target_os = "macos"))]
    0
}

/// Whether `path` is still there, by one `lstat`, which never follows a
/// final symlink. Only "no such file" counts as gone; any other error
/// cannot tell, so counts as there.
pub fn exists(path: &Path) -> bool {
    !matches!(rustix::fs::lstat(path), Err(Errno::NOENT | Errno::NOTDIR))
}

/// Calls `f` for every entry of the open directory `fd`, with the fastest
/// reader for the OS. `private` asks for [`Entry::private`].
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn read_dir(fd: impl AsFd, private: bool, f: impl FnMut(Entry<'_>)) -> Result<()> {
    #[cfg(target_os = "macos")]
    return macos::read_dir(fd.as_fd(), private, f);
    #[cfg(not(target_os = "macos"))]
    portable::read_dir(fd.as_fd(), f)
}

/// [`read_dir`] with `readdir` plus `lstat`, the reader every unix has.
pub fn read_dir_portable(fd: impl AsFd, f: impl FnMut(Entry<'_>)) -> Result<()> {
    portable::read_dir(fd.as_fd(), f)
}

/// An id of the volume holding `path`, stable across mounts and reboots:
/// the volume UUID on macOS, `f_fsid` elsewhere.
pub fn volume_id(path: &Path) -> Result<u128> {
    #[cfg(target_os = "macos")]
    return macos::volume_uuid(path);
    #[cfg(not(target_os = "macos"))]
    Ok(u128::from(rustix::fs::statvfs(path)?.f_fsid))
}

/// Every mounted filesystem: `getfsstat` on macOS, `/proc/self/mountinfo`
/// elsewhere.
pub fn mounts() -> io::Result<Vec<Mount>> {
    #[cfg(target_os = "macos")]
    return macos::mounts();
    #[cfg(not(target_os = "macos"))]
    linux::mounts()
}

/// Stops this process from downloading iCloud placeholder files while it
/// reads, so a scan never fills the disk it measures. macOS only. Failure
/// only means such files may download, so it is ignored.
pub fn keep_placeholders_remote() {
    #[cfg(target_os = "macos")]
    macos::keep_placeholders_remote();
}

/// Raises the soft open-file limit to `min(hard, 65536)`. The walk keeps a
/// parent's fd open until all its children are opened, and the macOS default
/// soft limit is 256. Failure only means more `EMFILE` denials, so it is
/// ignored.
pub fn raise_fd_limit() {
    const WANT: u64 = 65536;
    // macOS refuses anything above OPEN_MAX (10240) even when the hard limit
    // is unlimited.
    const FALLBACK: u64 = 10240;
    let lim = getrlimit(Resource::Nofile);
    let want = lim.maximum.map_or(WANT, |hard| hard.min(WANT));
    if lim.current.is_none_or(|soft| soft >= want) {
        return;
    }
    for current in [want, FALLBACK.min(want)] {
        let new = Rlimit {
            current: Some(current),
            maximum: lim.maximum,
        };
        if setrlimit(Resource::Nofile, new).is_ok() {
            return;
        }
    }
}

fn kind(mode: u32) -> Kind {
    match FileType::from_raw_mode(mode as _) {
        FileType::Directory => Kind::Dir,
        FileType::RegularFile => Kind::File,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    }
}

/// Errno values the report names; everything else prints as a number.
pub fn errno_name(raw: u16) -> Option<&'static str> {
    let raw = i32::from(raw);
    if raw == Errno::ACCESS.raw_os_error() {
        Some("EACCES")
    } else if raw == Errno::PERM.raw_os_error() {
        Some("EPERM")
    } else {
        None
    }
}

//! The only module that makes raw OS calls. Every open is read-only.
//!
//! Directory readers live in their own files and all yield [`Entry`].
//! [`read_dir`] picks the fastest one for the OS. [`watch`] reports the
//! changes below a path, on macOS only, and [`Inotify`] those in a few
//! dirs, on Linux only. [`map`] maps a saved scan into memory, read-only.

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
use std::fs::File;
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

/// Calls `f` until it fails with something other than `EINTR`. A macOS
/// scan, which installs no signal handlers, still sees `EINTR` on a few
/// dirs, and it only means "try again". `readdir` is not retried, as
/// `rustix::fs::Dir` stops at its first error.
fn retry<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    loop {
        match f() {
            Err(Errno::INTR) => {}
            r => return r,
        }
    }
}

/// What `fstat` says about an open directory.
pub struct DirStat {
    pub dev: u64,
    pub bytes: u64,
}

/// Opens the scan root. Unlike children, a symlinked root is followed (like
/// `du -H`), so `diskuse scan /tmp` works on macOS where `/tmp` is a link.
pub fn open_root(path: &Path) -> Result<OwnedFd> {
    let flags = DIR_FLAGS.difference(OFlags::NOFOLLOW);
    // sys owns the only openat, with fixed read-only flags.
    #[allow(clippy::disallowed_methods)]
    retry(|| rustix::fs::openat(rustix::fs::CWD, path, flags, rustix::fs::Mode::empty()))
}

/// Opens `name` relative to its parent, so no full path is ever built.
pub fn open_child(parent: BorrowedFd<'_>, name: &CStr) -> Result<OwnedFd> {
    // sys owns the only openat, with fixed read-only flags.
    #[allow(clippy::disallowed_methods)]
    retry(|| rustix::fs::openat(parent, name, DIR_FLAGS, rustix::fs::Mode::empty()))
}

pub fn dir_stat(fd: BorrowedFd<'_>) -> Result<DirStat> {
    retry(|| rustix::fs::fstat(fd)).map(DirStat::of)
}

/// [`dir_stat`] of `name` in the open dir `parent`, by `lstat`, so of a
/// dir that cannot be opened too.
pub fn child_stat(parent: BorrowedFd<'_>, name: &CStr) -> Result<DirStat> {
    retry(|| rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)).map(DirStat::of)
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
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Event {
    /// Something at this absolute path was created, removed, renamed or
    /// written to.
    Changed(Box<[u8]>),
    /// The OS merged the changes below this path: list all of it again.
    Rescan(Box<[u8]>),
    /// The end of the changes recorded before the watch started. Later ones
    /// are live.
    HistoryDone,
    /// Changes were lost, or the watched path itself moved: only a full
    /// scan is right. Why, for the user.
    Lost(crate::live::Reason),
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
/// event ids after `since` ([`event_id`]), first the recorded ones, then
/// live ones, in batches to `tx`. `None` where the OS keeps no record of changes, so on
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

    pub fn remote(_: &CStr) -> bool {
        true
    }

    pub fn add(&self, _: &CStr, _: bool) -> Result<i32> {
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

/// Whether device `dev` keeps a record of its changes, which [`watch`]
/// needs: a network share or a read-only volume does not, so FSEvents sees
/// only the changes this Mac makes there. Never off macOS.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn records_changes(dev: u64) -> bool {
    #[cfg(target_os = "macos")]
    return fsevents::records_changes(dev);
    #[cfg(not(target_os = "macos"))]
    false
}

/// A firmlink and its target, as absolute paths.
pub type Firmlink = (Box<[u8]>, Box<[u8]>);

/// The macOS firmlinks: each joins a dir on the
/// Data volume, at `/System/Volumes/Data/<target>`, to the system volume,
/// at the firmlink's path. Read from `/usr/share/firmlinks`. Empty
/// elsewhere, or if it cannot be read.
pub fn firmlinks() -> Vec<Firmlink> {
    #[cfg(target_os = "macos")]
    return macos::firmlinks();
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

/// All of `file`, mapped read-only, so a saved scan is read in place
/// rather than copied.
#[allow(unsafe_code)]
pub fn map(file: &File) -> io::Result<memmap2::Mmap> {
    // SAFETY: the map is sound while no one changes the file in place.
    // diskuse never does: `CacheDir::save` writes a new file and renames
    // it over the old one, so a mapped file keeps its bytes. Only another
    // program writing into the owner-only cache dir could, as for any
    // mapped file.
    unsafe { memmap2::Mmap::map(file) }
}

/// The allocated bytes of `path` by one `lstat`, or `None` if it cannot be
/// read, as when it is gone.
#[cfg_attr(not(target_os = "macos"), allow(clippy::unnecessary_cast))]
pub fn allocated(path: &Path) -> Option<u64> {
    rustix::fs::lstat(path)
        .ok()
        .map(|st| st.st_blocks as u64 * 512)
}

/// Whether macOS protects `path` from changes (System Integrity
/// Protection's `restricted` flag), by one `lstat`. Never elsewhere.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn restricted(path: &Path) -> bool {
    // SF_RESTRICTED, from <sys/stat.h>
    #[cfg(target_os = "macos")]
    return rustix::fs::lstat(path).is_ok_and(|st| st.st_flags & 0x0008_0000 != 0);
    #[cfg(not(target_os = "macos"))]
    false
}

/// Whether the file at `path` starts with `prefix`, by reading only that
/// much. False if it cannot be read.
pub fn starts_with(path: &Path, prefix: &[u8]) -> bool {
    use std::io::Read;
    let mut head = vec![0; prefix.len()];
    let read = File::open(path).and_then(|mut f| f.read_exact(&mut head));
    read.is_ok() && head == prefix
}

/// Whether `path` is still there, by one `lstat`, which never follows a
/// final symlink. Only "no such file" counts as gone; any other error
/// cannot tell, so counts as there.
pub fn exists(path: &Path) -> bool {
    !matches!(rustix::fs::lstat(path), Err(Errno::NOENT | Errno::NOTDIR))
}

/// How to list the dirs of one filesystem, from [`Lister::of`] an open dir
/// on it. Dirs with the same `st_dev` are on the same filesystem, so a walk,
/// which never leaves its root's device, decides once. `Lister::PORTABLE`
/// is `readdir` plus `lstat`, the reader every unix has.
#[cfg(target_os = "macos")]
pub use macos::Lister;
#[cfg(not(target_os = "macos"))]
pub use portable::Lister;

/// Calls `f` for every entry of the open directory `fd`, with the fastest
/// reader for the OS. `private` asks for [`Entry::private`].
pub fn read_dir(fd: impl AsFd, private: bool, f: impl FnMut(Entry<'_>)) -> Result<()> {
    let lister = Lister::of(fd.as_fd(), private)?;
    read_dir_with(fd, lister, f)
}

/// [`read_dir`] with `lister`, which must be of the filesystem of `fd`.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn read_dir_with(fd: impl AsFd, lister: Lister, f: impl FnMut(Entry<'_>)) -> Result<()> {
    #[cfg(target_os = "macos")]
    return macos::read_dir(fd.as_fd(), lister, f);
    #[cfg(not(target_os = "macos"))]
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

/// The CPU time this process has used, on all its threads.
pub fn cpu_time() -> std::time::Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    std::time::Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
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

//! inotify: Linux reports the changes in a dir to whoever watches it, while
//! the watch lasts, and keeps no record of them. Each watch costs kernel
//! memory, and there are only so many (`max_user_watches`), so only a few
//! dirs are watched at a time.

use super::Note;
use rustix::fd::OwnedFd;
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, Reader, WatchFlags};
use std::ffi::CStr;
use std::mem::MaybeUninit;

/// What changes a dir's listing: an entry created, deleted, renamed in or
/// out, written to, closed after writing (writes through `mmap` show only
/// then) or with its mode or link count changed. Then the dir itself
/// deleted or renamed, which only its parent's listing shows. Never a
/// symlink, which `add` follows only for the root.
const WATCH: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::ATTRIB)
    .union(WatchFlags::DELETE_SELF)
    .union(WatchFlags::MOVE_SELF)
    .union(WatchFlags::ONLYDIR);

/// `f_type` of the filesystems inotify misses changes on, from
/// `linux/magic.h`: those made by other machines, on network and cluster
/// filesystems, and those made behind the kernel's back, on FUSE, which
/// sshfs, rclone and virtiofs (VM shares) use.
const REMOTE: [u32; 15] = [
    0x6969,      // NFS
    0xff53_4d42, // CIFS
    0xfe53_4d42, // SMB2
    0x517b,      // SMB
    0x6573_5546, // FUSE, virtiofs
    0x0102_1997, // 9p
    0x00c3_6400, // Ceph
    0x5346_414f, // AFS
    0x6b41_4653, // kAFS
    0x7375_7245, // Coda
    0x0bd0_0bd0, // Lustre
    0x4750_4653, // GPFS
    0x0116_1970, // GFS2
    0x7461_636f, // OCFS2
    0x2003_0528, // OrangeFS
];

pub struct Inotify(OwnedFd);

impl Inotify {
    /// `None` if the kernel has none to give (`max_user_instances`).
    pub fn new() -> Option<Self> {
        let flags = CreateFlags::CLOEXEC | CreateFlags::NONBLOCK;
        inotify::init(flags).ok().map(Self)
    }

    /// Watches dir `path`, following a symlink only if `follow`. `None` if
    /// it cannot, or its filesystem would not report every change.
    pub fn add(&self, path: &CStr, follow: bool) -> Option<i32> {
        let fs = rustix::fs::statfs(path).ok()?.f_type;
        // `f_type` is signed on some targets
        #[allow(clippy::unnecessary_cast)]
        if REMOTE.contains(&(fs as u32)) {
            return None;
        }
        let flags = match follow {
            true => WATCH,
            false => WATCH | WatchFlags::DONT_FOLLOW,
        };
        inotify::add_watch(&self.0, path, flags).ok()
    }

    /// Stops watch `wd`. It may have ended already, with its dir.
    pub fn remove(&self, wd: i32) {
        let _ = inotify::remove_watch(&self.0, wd);
    }

    /// Calls `f` for each change since the last read, without waiting.
    pub fn read(&self, mut f: impl FnMut(Note)) {
        // room for many events, and at least one with the longest name
        let mut buf = [MaybeUninit::uninit(); 4096];
        let mut reader = Reader::new(&self.0, &mut buf);
        // `AGAIN` once there are no more, or any error: the next read tries
        // again
        while let Ok(e) = reader.next() {
            let flags = e.events();
            let gone = ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF | ReadFlags::UNMOUNT;
            f(if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                Note::Lost
            } else if flags.contains(ReadFlags::IGNORED) {
                // the watch ended, with its dir or by `remove`. Watch ids
                // are handed out in turn, so its id is not seen again soon
                continue;
            } else if flags.intersects(gone) {
                Note::Gone(e.wd())
            } else {
                Note::Changed(e.wd())
            });
        }
    }
}

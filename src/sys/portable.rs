//! `readdir` plus one `lstat` per entry. Works on every unix.

use super::{Entry, kind, retry};
use rustix::fd::BorrowedFd;
use rustix::fs::{AtFlags, Dir, statat};
use rustix::io::{Errno, Result};

/// Every filesystem is listed the same way.
#[cfg(not(target_os = "macos"))]
#[derive(Clone, Copy)]
pub struct Lister;

#[cfg(not(target_os = "macos"))]
impl Lister {
    pub const PORTABLE: Self = Self;

    pub fn of(_: BorrowedFd<'_>, _: bool) -> Result<Self> {
        Ok(Self)
    }
}

pub fn read_dir(fd: BorrowedFd<'_>, mut f: impl FnMut(Entry<'_>)) -> Result<()> {
    // `Dir` takes ownership of the fd it reads, but the caller still needs
    // its fd to open children, so give `Dir` a duplicate.
    let mut dir = Dir::new(rustix::io::dup(fd)?)?;
    while let Some(ent) = dir.read() {
        let ent = ent?;
        let name = ent.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        let st = match retry(|| statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)) {
            Ok(st) => st,
            // deleted between readdir and lstat
            Err(Errno::NOENT) => continue,
            Err(e) => return Err(e),
        };
        f(Entry {
            name,
            kind: kind(st.st_mode as _),
            ino: st.st_ino as u64,
            dev: st.st_dev as u64,
            nlink: st.st_nlink as u64,
            bytes: st.st_blocks as u64 * 512,
            mount: false,
            private: 0,
            empty: false,
        });
    }
    Ok(())
}

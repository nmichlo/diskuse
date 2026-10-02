//! `getattrlistbulk`: one syscall returns the names and sizes of many
//! entries, so there is no `lstat` per entry. Apple's `libdarwin/dirstat.c`
//! parses the same buffer.
//!
//! Only APFS gets it. Other filesystems can answer with wrong sizes: FAT
//! reports every file as 0 B, and HFS+ more than `st_blocks`.

#![allow(unsafe_code)]

use super::{Entry, Kind, dir_stat, portable, retry};
use crate::volumes::Mount;
use rustix::fd::{AsRawFd, BorrowedFd};
use rustix::io::{Errno, Result};
use std::cell::RefCell;
use std::ffi::{CStr, CString, OsString, c_char, c_int};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

// Not in the libc crate.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;
const DIR_MNTSTATUS_MNTPOINT: u32 = 1;
/// An autofs trigger: opening it mounts something, maybe over the network.
const DIR_MNTSTATUS_TRIGGER: u32 = 2;

unsafe extern "C" {
    fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
}

// Byte offsets in one buffer entry. FSOPT_PACK_INVAL_ATTRS gives every
// requested attribute a slot, valid or not, so they are fixed. Dirs get the
// dir attributes and everything else the file ones, from COUNT on.
/// `u32`: the entry's length, including this field.
const LENGTH: usize = 0;
/// `attribute_set_t`: which slots hold valid values.
const RETURNED: usize = 4;
/// `u32` errno. Packed first, ahead of the other common attributes.
const ERROR: usize = 24;
/// `attrreference_t`: the name's offset from this field and its length.
const NAME: usize = 28;
const DEV: usize = 36;
const OBJTYPE: usize = 40;
/// `u64`.
const FILEID: usize = 44;
/// `u32`: ATTR_DIR_ENTRYCOUNT for a dir, ATTR_FILE_LINKCOUNT otherwise.
const COUNT: usize = 52;
/// `u32`: ATTR_DIR_MOUNTSTATUS, of a dir.
const MOUNT: usize = 56;
/// `off_t`: ATTR_DIR_ALLOCSIZE of a dir.
const DIR_ALLOC: usize = 60;
/// `off_t`: ATTR_FILE_ALLOCSIZE of anything else.
const FILE_ALLOC: usize = 56;
/// ATTR_CMNEXT_PRIVATESIZE, an `off_t` right after the allocated size, is
/// only there when requested.
const PRIVATE: usize = 8;

// `attribute_set_t` fields, from RETURNED.
const RETURNED_CMN: usize = RETURNED;
const RETURNED_DIR: usize = RETURNED + 8;

const BUF_LEN: usize = 256 * 1024;

thread_local! {
    // One per scan thread, reused for every directory.
    static BUF: RefCell<Box<[u8]>> = RefCell::new(vec![0; BUF_LEN].into());
}

/// How [`Entry::private`] is filled.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Private {
    /// Not asked for.
    Zero,
    /// From ATTR_CMNEXT_PRIVATESIZE.
    Measured,
    /// The filesystem has no clones, so every allocated byte is private.
    Allocated,
}

/// How to list the dirs of one filesystem.
#[derive(Clone, Copy)]
pub struct Lister {
    bulk: bool,
    private: Private,
}

impl Lister {
    pub const PORTABLE: Self = Self {
        bulk: false,
        private: Private::Zero,
    };

    /// Decided by one `fstatfs` of `fd`.
    pub fn of(fd: BorrowedFd<'_>, private: bool) -> Result<Self> {
        let fs = retry(|| rustix::fs::fstatfs(fd))?
            .f_fstypename
            .map(|c| c as u8);
        let fs = CStr::from_bytes_until_nul(&fs)
            .unwrap_or_default()
            .to_bytes();
        Ok(Self {
            bulk: fs == b"apfs",
            // only APFS has clones
            private: match (private, fs) {
                (false, _) => Private::Zero,
                (true, b"apfs") => Private::Measured,
                (true, _) => Private::Allocated,
            },
        })
    }
}

pub fn read_dir(fd: BorrowedFd<'_>, lister: Lister, mut f: impl FnMut(Entry<'_>)) -> Result<()> {
    let private = lister.private;
    if !lister.bulk {
        return read_portable(fd, private, f);
    }
    // the fork group means ATTR_CMNEXT_* only with FSOPT_ATTR_CMN_EXTENDED
    let (forkattr, extended) = match private {
        Private::Measured => (libc::ATTR_CMNEXT_PRIVATESIZE, libc::FSOPT_ATTR_CMN_EXTENDED),
        _ => (0, 0),
    };
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: libc::ATTR_DIR_ENTRYCOUNT | libc::ATTR_DIR_MOUNTSTATUS | libc::ATTR_DIR_ALLOCSIZE,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
        forkattr,
    };
    let options = u64::from(libc::FSOPT_PACK_INVAL_ATTRS | extended);
    BUF.with_borrow_mut(|buf| {
        let mut first = true;
        loop {
            let n = retry(|| {
                // SAFETY: `attrs` and `buf` are live, writable and sized as
                // passed for the whole call, and `fd` is an open directory.
                let n = unsafe {
                    libc::getattrlistbulk(
                        fd.as_raw_fd(),
                        (&raw mut attrs).cast(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        options,
                    )
                };
                match n {
                    0.. => Ok(n),
                    _ => Err(Errno::from_io_error(&io::Error::last_os_error()).unwrap()),
                }
            });
            let n = match n {
                Ok(n) => n,
                // a filesystem may refuse bulk listing or these attributes
                Err(Errno::NOTSUP | Errno::INVAL) if first => {
                    return read_portable(fd, private, &mut f);
                }
                Err(e) => return Err(e),
            };
            if n == 0 {
                return Ok(());
            }
            first = false;
            let mut rest = &buf[..];
            for _ in 0..n {
                let len = u32_at(rest, LENGTH) as usize;
                let (e, tail) = rest.split_at(len);
                rest = tail;
                match parse(e, private) {
                    Parsed::Skip => {}
                    Parsed::Entry(entry) => f(entry),
                    // yielded so the scan tries to open it and records why it
                    // cannot. Its own dev is unknown, so it gets its parent's.
                    Parsed::BrokenDir(name) => f(Entry {
                        name,
                        kind: Kind::Dir,
                        ino: 0,
                        dev: dir_stat(fd)?.dev,
                        nlink: 1,
                        bytes: 0,
                        mount: false,
                        private: 0,
                        empty: false,
                    }),
                }
            }
        }
    })
}

/// The portable reader, which cannot see clones.
fn read_portable(fd: BorrowedFd<'_>, private: Private, mut f: impl FnMut(Entry<'_>)) -> Result<()> {
    portable::read_dir(fd, |mut e| {
        if private == Private::Allocated {
            e.private = e.bytes;
        }
        f(e);
    })
}

enum Parsed<'a> {
    Skip,
    Entry(Entry<'a>),
    /// A dir with ATTR_CMN_ERROR set: only its name is known.
    BrokenDir(&'a CStr),
}

fn parse(e: &[u8], private: Private) -> Parsed<'_> {
    let returned = u32_at(e, RETURNED_CMN);
    if returned & libc::ATTR_CMN_NAME == 0 {
        return Parsed::Skip;
    }
    let name_at = NAME + u32_at(e, NAME) as usize;
    let Ok(name) = CStr::from_bytes_until_nul(&e[name_at..]) else {
        return Parsed::Skip;
    };
    let objtype = match returned & libc::ATTR_CMN_OBJTYPE {
        0 => 0,
        _ => u32_at(e, OBJTYPE),
    };
    let kind = match objtype {
        VDIR => Kind::Dir,
        VREG => Kind::File,
        VLNK => Kind::Symlink,
        _ => Kind::Other,
    };
    if u32_at(e, ERROR) != 0 {
        // sizes unknown: skip anything but a dir
        return match kind {
            Kind::Dir => Parsed::BrokenDir(name),
            _ => Parsed::Skip,
        };
    }
    let count = u32_at(e, COUNT);
    let (alloc, mount, nlink, empty) = match kind {
        Kind::Dir => {
            let known = u32_at(e, RETURNED_DIR);
            let mount = known & libc::ATTR_DIR_MOUNTSTATUS != 0
                && u32_at(e, MOUNT) & (DIR_MNTSTATUS_MNTPOINT | DIR_MNTSTATUS_TRIGGER) != 0;
            let empty = known & libc::ATTR_DIR_ENTRYCOUNT != 0 && count == 0;
            // dirs cannot be hard-linked, and the scan never asks
            (DIR_ALLOC, mount, 1, empty)
        }
        _ => (FILE_ALLOC, false, u64::from(count), false),
    };
    let bytes = u64_at(e, alloc);
    Parsed::Entry(Entry {
        name,
        kind,
        ino: u64_at(e, FILEID),
        dev: u64::from(u32_at(e, DEV)),
        nlink,
        bytes,
        mount,
        private: match private {
            Private::Zero => 0,
            Private::Measured => u64_at(e, alloc + PRIVATE),
            Private::Allocated => bytes,
        },
        empty,
    })
}

// Fields are only 4-byte aligned, so read them from bytes.
fn u32_at(e: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(e[at..at + 4].try_into().unwrap())
}

fn u64_at(e: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(e[at..at + 8].try_into().unwrap())
}

/// ATTR_VOL_UUID of the volume holding `path`, or 0 if the filesystem has
/// none. Any path on the volume works, not only its root as the man page
/// says.
pub fn volume_uuid(path: &Path) -> Result<u128> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::INVAL)?;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // u32 length of what was returned, then the 16 UUID bytes
    let mut buf = [0u8; 20];
    // SAFETY: `path` is NUL-terminated, and `attrs` and `buf` are live,
    // writable and sized as passed for the whole call.
    let n = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&raw mut attrs).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
        )
    };
    if n < 0 {
        return Err(Errno::from_io_error(&io::Error::last_os_error()).unwrap());
    }
    Ok(match u32_at(&buf, 0) as usize {
        20 => u128::from_be_bytes(buf[4..].try_into().unwrap()),
        _ => 0,
    })
}

/// Every mount, from the kernel's cached sizes (`MNT_NOWAIT`), so a hung
/// network mount cannot block the list.
pub fn mounts() -> io::Result<Vec<Mount>> {
    // SAFETY: a null buffer only asks for the count.
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // room for a few mounted between the two calls
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(n as usize + 8);
    let size = c_int::try_from(buf.capacity() * size_of::<libc::statfs>()).unwrap();
    // SAFETY: `buf` has room for `size` bytes of `statfs` entries.
    let n = unsafe { libc::getfsstat(buf.as_mut_ptr(), size, libc::MNT_NOWAIT) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getfsstat filled the first `n` entries, at most the capacity.
    unsafe { buf.set_len(n as usize) };
    let mounts = buf.iter().map(|s| {
        let block = u64::from(s.f_bsize);
        Mount {
            point: OsString::from_vec(c_str(&s.f_mntonname)).into(),
            fs: String::from_utf8_lossy(&c_str(&s.f_fstypename)).into(),
            hidden: s.f_flags & libc::MNT_DONTBROWSE as u32 != 0,
            total: s.f_blocks * block,
            used: s.f_blocks.saturating_sub(s.f_bfree) * block,
            free: s.f_bavail * block,
        }
    });
    Ok(mounts.collect())
}

/// The bytes of a NUL-terminated `char` array, without the NUL.
fn c_str(chars: &[c_char]) -> Vec<u8> {
    chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect()
}

pub fn keep_placeholders_remote() {
    const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
    const IOPOL_SCOPE_PROCESS: c_int = 0;
    const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;
    // SAFETY: takes three ints and touches no memory of ours.
    unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_PROCESS,
            IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
        );
    }
}

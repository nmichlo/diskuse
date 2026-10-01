//! The parallel walk. One rayon task per directory, never recursion, so deep
//! trees cannot overflow the stack. Every open is relative to the parent's
//! fd, so trees deeper than PATH_MAX work.

use crate::sys::{self, Kind};
use crate::tree::{Builder, Record, Tree};
use rustix::fd::{AsFd, OwnedFd};
use std::collections::HashSet;
use std::ffi::CString;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::{fmt, io};

#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    /// Worker threads. `None` means the available parallelism.
    pub threads: Option<NonZeroUsize>,
    /// Also measure [`Record::own_private`]. Only the macOS reader can see
    /// clones; the portable reader leaves it 0.
    pub reclaimable: bool,
    pub reader: Reader,
}

/// Which directory reader lists entries. Used by the differential test and
/// benchmarks; users want [`Reader::Auto`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Reader {
    /// The fastest reader for the OS (`getattrlistbulk` on macOS).
    #[default]
    Auto,
    /// `readdir` plus one `lstat` per entry, on every OS.
    Portable,
}

#[derive(Debug)]
pub enum ScanError {
    /// The root could not be opened as a directory.
    Root(io::Error),
    ThreadPool(rayon::ThreadPoolBuildError),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(e) => e.fmt(f),
            Self::ThreadPool(e) => write!(f, "cannot start threads: {e}"),
        }
    }
}

impl std::error::Error for ScanError {}

/// Scans `root` without crossing devices or mount points, or following
/// symlinks.
///
/// Also changes two process-wide settings: it raises the soft open-file
/// limit, since the walk holds many directory fds open at once, and on macOS
/// it stops iCloud placeholder files from downloading.
pub fn scan(root: &Path, opts: &ScanOptions) -> Result<Tree, ScanError> {
    sys::raise_fd_limit();
    sys::keep_placeholders_remote();
    let fd = sys::open_root(root).map_err(|e| ScanError::Root(e.into()))?;
    let st = sys::dir_stat(fd.as_fd()).map_err(|e| ScanError::Root(e.into()))?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads.map_or(0, NonZeroUsize::get))
        .build()
        .map_err(ScanError::ThreadPool)?;
    let walk = Walk {
        tree: Builder::new(),
        links: Mutex::default(),
        root_dev: st.dev,
        reclaimable: opts.reclaimable,
        reader: opts.reader,
    };
    let name = walk
        .tree
        .names
        .lock()
        .unwrap()
        .intern(root.as_os_str().as_bytes());
    let own = Own {
        bytes: st.bytes,
        // a dir's own blocks are never shared with a clone
        private: if opts.reclaimable { st.bytes } else { 0 },
    };
    pool.scope(|s| walk.list(s, fd, Record::NO_PARENT, name, own));
    Ok(walk.tree.finish())
}

struct Walk {
    tree: Builder,
    /// `(dev, ino)` of every multiply-linked file already counted.
    links: Mutex<HashSet<(u64, u64)>>,
    root_dev: u64,
    reclaimable: bool,
    reader: Reader,
}

/// [`Record::own`] and [`Record::own_private`], summed together.
#[derive(Clone, Copy)]
struct Own {
    bytes: u64,
    private: u64,
}

/// A subdirectory seen while listing its parent.
struct Child {
    name: CString,
    /// The subdirectory's own allocated bytes, from the parent's listing.
    own: Own,
    dev: u64,
    mount: bool,
}

impl Walk {
    fn visit<'s>(
        &'s self,
        s: &rayon::Scope<'s>,
        parent_fd: Arc<OwnedFd>,
        parent: u32,
        name: u32,
        child: Child,
    ) {
        let opened = sys::open_child(parent_fd.as_fd(), &child.name);
        // release the parent's fd as soon as possible to bound open fds
        drop(parent_fd);
        match opened {
            Ok(fd) => self.list(s, fd, parent, name, child.own),
            // like du: a denied directory still counts its own blocks
            Err(e) => self.deny(parent, name, child.own, e),
        }
    }

    fn deny(&self, parent: u32, name: u32, own: Own, e: rustix::io::Errno) {
        self.tree.push(Record {
            parent,
            name,
            flags: Record::DENIED,
            errno: e.raw_os_error() as u16,
            own: own.bytes,
            own_private: own.private,
        });
    }

    /// Lists the open directory `fd`, pushes its record, then spawns a task
    /// per subdirectory.
    fn list<'s>(&'s self, s: &rayon::Scope<'s>, fd: OwnedFd, parent: u32, name: u32, own: Own) {
        let mut own = own;
        let mut kids = Vec::new();
        let on_entry = |e: sys::Entry<'_>| match e.kind {
            Kind::Dir => kids.push(Child {
                name: e.name.to_owned(),
                own: Own {
                    bytes: e.bytes,
                    private: e.private,
                },
                dev: e.dev,
                mount: e.mount,
            }),
            _ if e.nlink > 1 && !self.links.lock().unwrap().insert((e.dev, e.ino)) => {}
            _ => {
                own.bytes += e.bytes;
                own.private += e.private;
            }
        };
        let listed = match self.reader {
            Reader::Auto => sys::read_dir(&fd, self.reclaimable, on_entry),
            Reader::Portable => sys::read_dir_portable(&fd, on_entry),
        };
        if let Err(e) = listed {
            return self.deny(parent, name, own, e);
        }
        let id = self.tree.push(Record {
            parent,
            name,
            flags: 0,
            errno: 0,
            own: own.bytes,
            own_private: own.private,
        });
        let ids: Vec<u32> = {
            let mut names = self.tree.names.lock().unwrap();
            kids.iter()
                .map(|k| names.intern(k.name.to_bytes()))
                .collect()
        };
        let fd = Arc::new(fd);
        for (kid, name) in kids.into_iter().zip(ids) {
            // like `du -x`: decided from the parent's listing, so a mount
            // point is never opened. The mount flag catches the macOS Data
            // volume, which shares the root's dev.
            if kid.mount || kid.dev != self.root_dev {
                self.tree.push(Record {
                    parent: id,
                    name,
                    flags: Record::OTHER_DEVICE,
                    errno: 0,
                    own: 0,
                    own_private: 0,
                });
                continue;
            }
            let fd = Arc::clone(&fd);
            s.spawn(move |s| self.visit(s, fd, id, name, kid));
        }
    }
}

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

/// Scans `root` without crossing devices or following symlinks.
///
/// Also raises the soft open-file limit, since the walk holds many directory
/// fds open at once.
pub fn scan(root: &Path, opts: &ScanOptions) -> Result<Tree, ScanError> {
    sys::raise_fd_limit();
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
    };
    let name = walk
        .tree
        .names
        .lock()
        .unwrap()
        .intern(root.as_os_str().as_bytes());
    pool.scope(|s| walk.list(s, fd, Record::NO_PARENT, name, st.bytes));
    Ok(walk.tree.finish())
}

struct Walk {
    tree: Builder,
    /// `(dev, ino)` of every multiply-linked file already counted.
    links: Mutex<HashSet<(u64, u64)>>,
    root_dev: u64,
}

/// A subdirectory seen while listing its parent.
struct Child {
    name: CString,
    /// The subdirectory's own allocated bytes, from the parent's lstat.
    bytes: u64,
    dev: u64,
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
            Ok(fd) => self.list(s, fd, parent, name, child.bytes),
            // like du: a denied directory still counts its own blocks
            Err(e) => self.deny(parent, name, child.bytes, e),
        }
    }

    fn deny(&self, parent: u32, name: u32, own: u64, e: rustix::io::Errno) {
        self.tree.push(Record {
            parent,
            name,
            flags: Record::DENIED,
            errno: e.raw_os_error() as u16,
            own,
        });
    }

    /// Lists the open directory `fd`, pushes its record, then spawns a task
    /// per subdirectory.
    fn list<'s>(&'s self, s: &rayon::Scope<'s>, fd: OwnedFd, parent: u32, name: u32, own: u64) {
        let mut own = own;
        let mut kids = Vec::new();
        let listed = sys::read_dir(&fd, |e| match e.kind {
            Kind::Dir => kids.push(Child {
                name: e.name.to_owned(),
                bytes: e.bytes,
                dev: e.dev,
            }),
            _ if e.nlink > 1 && !self.links.lock().unwrap().insert((e.dev, e.ino)) => {}
            _ => own += e.bytes,
        });
        if let Err(e) = listed {
            return self.deny(parent, name, own, e);
        }
        let id = self.tree.push(Record {
            parent,
            name,
            flags: 0,
            errno: 0,
            own,
        });
        let ids: Vec<u32> = {
            let mut names = self.tree.names.lock().unwrap();
            kids.iter()
                .map(|k| names.intern(k.name.to_bytes()))
                .collect()
        };
        let fd = Arc::new(fd);
        for (kid, name) in kids.into_iter().zip(ids) {
            // like `du -x`: decided from the parent's lstat, so a mount point
            // is never opened
            if kid.dev != self.root_dev {
                self.tree.push(Record {
                    parent: id,
                    name,
                    flags: Record::OTHER_DEVICE,
                    errno: 0,
                    own: 0,
                });
                continue;
            }
            let fd = Arc::clone(&fd);
            s.spawn(move |s| self.visit(s, fd, id, name, kid));
        }
    }
}

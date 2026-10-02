//! The parallel walk. One rayon task per directory, never recursion, so deep
//! trees cannot overflow the stack. Every open is relative to the parent's
//! fd, so trees deeper than PATH_MAX work.
//!
//! [`Tree::apply`] lists changed dirs again with the same reader, and scans
//! the subdirectories new in them with the same walk.

use crate::sys::{self, DirStat, Kind};
use crate::tree::{Builder, ChildIndex, LargeFile, Links, Progress, Record, Since, Tree};
use crate::watch::Changes;
use rayon::ThreadPool;
use rustix::fd::{AsFd, OwnedFd};
use rustix::io::Errno;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::ffi::{CString, OsStr};
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::{fmt, io};

#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    /// Worker threads. `None` means the number of cores, at most 8 (see
    /// `DEFAULT_THREADS` for why).
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
    scan_live(root, opts, |_| {})
}

/// [`scan`], first handing `progress` a [`Progress`] that reads the tree
/// while it is built, from any thread.
pub fn scan_live(
    root: &Path,
    opts: &ScanOptions,
    progress: impl FnOnce(Progress),
) -> Result<Tree, ScanError> {
    sys::raise_fd_limit();
    sys::keep_placeholders_remote();
    let fd = sys::open_root(root).map_err(|e| ScanError::Root(e.into()))?;
    let st = sys::dir_stat(fd.as_fd()).map_err(|e| ScanError::Root(e.into()))?;
    let lister = lister(&fd, opts).map_err(|e| ScanError::Root(e.into()))?;
    // taken first, so an update from here also sees the changes made while
    // the walk runs
    let since = Since {
        id: sys::event_id(),
        store: sys::event_store(st.dev),
    };
    let pool = pool(opts).map_err(ScanError::ThreadPool)?;
    let walk = Walk::new(root.as_os_str().as_bytes(), Links::new(), 0, st.dev, lister);
    let own = Own::of(&st, opts.reclaimable);
    progress(walk.tree.progress());
    pool.scope(|s| walk.list(s, fd, Record::NO_PARENT, 0, own));
    Ok(walk.finish(since))
}

/// The [`sys::Lister`] `opts` ask for, of the filesystem of `fd`.
fn lister(fd: &OwnedFd, opts: &ScanOptions) -> rustix::io::Result<sys::Lister> {
    match opts.reader {
        Reader::Auto => sys::Lister::of(fd.as_fd(), opts.reclaimable),
        Reader::Portable => Ok(sys::Lister::PORTABLE),
    }
}

/// The default cap on worker threads. Past it, threads mostly wait on
/// kernel locks: on a 10-core M4, 8 threads were as fast as or faster than
/// 10 on every tree measured, with 10% less CPU; fewer threads lose on
/// large trees, which also wait on the disk.
const DEFAULT_THREADS: usize = 8;

fn pool(opts: &ScanOptions) -> Result<ThreadPool, rayon::ThreadPoolBuildError> {
    let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    rayon::ThreadPoolBuilder::new()
        .num_threads(
            opts.threads
                .map_or(cores.min(DEFAULT_THREADS), NonZeroUsize::get),
        )
        .build()
}

struct Walk {
    tree: Builder,
    /// Moved in from the tree the walk adds to, if any.
    links: Mutex<Links>,
    /// Added to the walk's record ids in `links`: the length of the tree
    /// its records are appended to.
    base: u32,
    root_dev: u64,
    /// Of the root's filesystem, so of every dir the walk lists.
    lister: sys::Lister,
}

/// [`Record::own`] and [`Record::own_private`], summed together.
#[derive(Clone, Copy)]
struct Own {
    bytes: u64,
    private: u64,
}

impl Own {
    /// The own blocks of a dir, which are never shared with a clone.
    fn of(st: &DirStat, reclaimable: bool) -> Self {
        Self {
            bytes: st.bytes,
            private: if reclaimable { st.bytes } else { 0 },
        }
    }
}

/// A subdirectory seen while listing its parent.
struct Child {
    name: CString,
    /// The subdirectory's own allocated bytes, from the parent's listing.
    own: Own,
    dev: u64,
    mount: bool,
    /// Known to hold no entries.
    empty: bool,
}

/// What listing one dir found.
struct Listing {
    /// The dir's own blocks plus its files'.
    own: Own,
    kids: Vec<Child>,
    /// Candidates for the largest files, `(bytes, name)`.
    files: Vec<(u64, Box<[u8]>)>,
    /// The multiply-linked files it counts, held until it has an id.
    claimed: Vec<(u64, u64)>,
    /// Why the listing stopped short, if it did.
    denied: Option<Errno>,
}

impl Walk {
    /// A walk whose root is named `root`, adding to `links` of a tree of
    /// `base` records.
    fn new(root: &[u8], links: Links, base: u32, root_dev: u64, lister: sys::Lister) -> Self {
        Self {
            tree: Builder::new(root),
            links: Mutex::new(links),
            base,
            root_dev,
            lister,
        }
    }

    fn finish(self, since: Since) -> Tree {
        let links = self.links.into_inner().unwrap();
        self.tree.finish(links, since)
    }

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
            // opened only to learn it is not denied: listing finds nothing
            Ok(_) if child.empty => {
                self.tree.push(Record {
                    parent,
                    name,
                    flags: 0,
                    errno: 0,
                    own: child.own.bytes,
                    own_private: child.own.private,
                });
            }
            Ok(fd) => self.list(s, fd, parent, name, child.own),
            // like du: a denied directory still counts its own blocks
            Err(e) => {
                self.deny(parent, name, child.own, e);
            }
        }
    }

    fn deny(&self, parent: u32, name: u32, own: Own, e: Errno) -> u32 {
        self.tree.push(Record {
            parent,
            name,
            flags: Record::DENIED,
            errno: e.raw_os_error() as u16,
            own: own.bytes,
            own_private: own.private,
        })
    }

    /// Lists the open directory `fd`, whose own blocks are `own`.
    fn read(&self, fd: &OwnedFd, own: Own) -> Listing {
        let mut own = own;
        let mut kids = Vec::new();
        let mut files = Vec::new();
        let mut claimed = Vec::new();
        let on_entry = |e: sys::Entry<'_>| match e.kind {
            Kind::Dir => kids.push(Child {
                name: e.name.to_owned(),
                own: Own {
                    bytes: e.bytes,
                    private: e.private,
                },
                dev: e.dev,
                mount: e.mount,
                empty: e.empty,
            }),
            _ if e.nlink > 1 && !self.claim((e.dev, e.ino), &mut claimed) => {}
            _ => {
                own.bytes += e.bytes;
                own.private += e.private;
                if e.bytes > self.tree.largest.floor() {
                    files.push((e.bytes, e.name.to_bytes().into()));
                }
            }
        };
        let listed = sys::read_dir_with(fd, self.lister, on_entry);
        Listing {
            own,
            kids,
            files,
            claimed,
            denied: listed.err(),
        }
    }

    /// Whether the multiply-linked file `key` is not counted yet. If not,
    /// the dir being listed counts it, and `claimed` collects it for
    /// [`Walk::own`].
    fn claim(&self, key: (u64, u64), claimed: &mut Vec<(u64, u64)>) -> bool {
        match self.links.lock().unwrap().entry(key) {
            Entry::Occupied(_) => false,
            Entry::Vacant(v) => {
                v.insert(Record::NO_PARENT);
                claimed.push(key);
                true
            }
        }
    }

    /// Records dir `id` of the tree as counting the links it `claimed`.
    fn own(&self, id: u32, claimed: &[(u64, u64)]) {
        if claimed.is_empty() {
            return;
        }
        let mut links = self.links.lock().unwrap();
        for &key in claimed {
            links.insert(key, id);
        }
    }

    /// Lists the open directory `fd`, pushes its record, then spawns a task
    /// per subdirectory.
    fn list<'s>(&'s self, s: &rayon::Scope<'s>, fd: OwnedFd, parent: u32, name: u32, own: Own) {
        let listing = self.read(&fd, own);
        let id = match listing.denied {
            // files listed before the error are counted, so they compete too
            Some(e) => self.deny(parent, name, listing.own, e),
            None => self.tree.push(Record {
                parent,
                name,
                flags: 0,
                errno: 0,
                own: listing.own.bytes,
                own_private: listing.own.private,
            }),
        };
        self.own(self.base + id, &listing.claimed);
        self.tree.largest.offer(id, listing.files);
        if listing.denied.is_some() {
            return;
        }
        let kids = listing.kids;
        let ids = self.tree.intern(kids.iter().map(|k| k.name.to_bytes()));
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

impl Tree {
    /// Brings the tree up to date with `changes`: lists each dir they
    /// touched again, one bulk read each, and scans the subdirectories new
    /// in them. Returns how many dirs it listed, or `None`, with the tree
    /// left as it was, if only a full scan can bring it up to date.
    pub(crate) fn apply(&mut self, changes: &Changes, opts: &ScanOptions) -> Option<usize> {
        if changes.is_empty() {
            self.since.id = self.since.id.max(changes.id);
            return Some(0);
        }
        let index = self.child_index();
        let (dirty, fresh) = self.touched(&index, changes)?;
        let root = sys::open_root(Path::new(OsStr::from_bytes(self.name(0)))).ok()?;
        let pool = pool(opts).ok()?;
        sys::raise_fd_limit();
        sys::keep_placeholders_remote();
        self.since.id = self.since.id.max(changes.id);
        // listed afresh: everything below them goes, and comes back as new
        for &d in &fresh {
            for &k in index.children(d) {
                self.remove(k);
            }
        }
        self.prune();
        let dirty: Vec<u32> = dirty.into_iter().filter(|&d| !self.gone(d)).collect();
        // found again by listing them
        self.links.retain(|_, d| dirty.binary_search(d).is_err());
        self.largest
            .retain(|f| dirty.binary_search(&f.dir).is_err());
        let mut listed = 0;
        // parents first, so a dir gone with its parent is not listed
        for &d in &dirty {
            if !self.gone(d) {
                self.relist(&root, d, index.children(d), opts, &pool);
                listed += 1;
            }
        }
        self.prune();
        Some(listed)
    }

    /// The dirs `changes` touched or named, and those of them to list
    /// afresh, with everything below them. `None` if that is the root: a
    /// full scan.
    fn touched(&self, index: &ChildIndex, changes: &Changes) -> Option<(BTreeSet<u32>, Vec<u32>)> {
        // a lookup by name of the children of each dir walked through
        let mut lookups: HashMap<u32, HashMap<&[u8], u32>> = HashMap::new();
        // the deepest dir of `path` in the tree, and whether that is all of
        // it
        let mut find = |path: &[u8]| {
            let mut d = 0;
            if path.is_empty() {
                return (d, true);
            }
            for name in path.split(|&b| b == b'/') {
                let kids = lookups.entry(d).or_insert_with(|| {
                    let kids = index.children(d).iter();
                    kids.map(|&k| (self.name(self.record(k).name), k)).collect()
                });
                match kids.get(name) {
                    Some(&k) => d = k,
                    None => return (d, false),
                }
            }
            (d, true)
        };
        let mut dirty: BTreeSet<u32> = changes.dirs.iter().copied().collect();
        for path in &changes.changed {
            let (d, exact) = find(path);
            let r = self.record(d);
            // a dir is in its parent's listing too, by name
            if exact && d != 0 {
                dirty.insert(r.parent);
            }
            // nothing below a mount point is in the tree
            if r.flags & Record::OTHER_DEVICE == 0 {
                dirty.insert(d);
            }
        }
        let mut fresh = Vec::new();
        for path in &changes.rescan {
            let (d, _) = find(path);
            if d == 0 {
                return None;
            }
            if self.record(d).flags & Record::OTHER_DEVICE == 0 {
                dirty.insert(d);
                fresh.push(d);
            }
        }
        Some((dirty, fresh))
    }

    /// Lists dir `d` again: its own blocks and files anew, its
    /// subdirectories gone since removed, and new ones scanned. `kids` were
    /// its subdirectories, and `root` is the root's fd.
    fn relist(
        &mut self,
        root: &OwnedFd,
        d: u32,
        kids: &[u32],
        opts: &ScanOptions,
        pool: &ThreadPool,
    ) {
        let kids: Vec<u32> = (kids.iter().copied())
            .filter(|&k| self.record(k).flags & Record::REMOVED == 0)
            .collect();
        let opened = self
            .open(root, d)
            .and_then(|fd| Ok((sys::dir_stat(fd.as_fd())?, lister(&fd, opts)?, fd)));
        let (st, lister, fd) = match opened {
            Ok(opened) => opened,
            // listing the parent again finds that too
            Err(Errno::NOENT | Errno::NOTDIR) => return self.remove(d),
            // like a scan: a dir that cannot be opened counts its own blocks
            Err(e) => {
                let own = self.own_blocks(root, d, opts.reclaimable);
                return self.deny(d, &kids, own, e);
            }
        };
        let walk = Walk::new(b"", std::mem::take(&mut self.links), 0, st.dev, lister);
        let listing = walk.read(&fd, Own::of(&st, opts.reclaimable));
        walk.own(d, &listing.claimed);
        self.links = walk.links.into_inner().unwrap();
        let files = listing.files.into_iter();
        self.offer(files.map(|(bytes, name)| LargeFile {
            bytes,
            dir: d,
            name,
        }));
        if let Some(e) = listing.denied {
            return self.deny(d, &kids, listing.own, e);
        }
        let r = &mut self.records[d as usize];
        (r.flags, r.errno, r.own, r.own_private) = (0, 0, listing.own.bytes, listing.own.private);

        // the subdirectories before, by name
        let names = Arc::clone(&self.names);
        let mut before: HashMap<&[u8], u32> = (kids.iter())
            .map(|&k| (&names[self.record(k).name as usize][..], k))
            .collect();
        let mut new = Vec::new();
        for kid in listing.kids {
            let other = kid.mount || kid.dev != st.dev;
            match before.remove(kid.name.to_bytes()) {
                // still there, on the same side of a mount point
                Some(k) if (self.record(k).flags & Record::OTHER_DEVICE != 0) == other => {}
                Some(k) => {
                    self.remove(k);
                    new.push(kid);
                }
                None => new.push(kid),
            }
        }
        for k in before.into_values() {
            self.remove(k);
        }
        let fd = Arc::new(fd);
        for kid in new {
            if kid.mount || kid.dev != st.dev {
                let name = self.push_name(kid.name.to_bytes());
                self.records.push(Record {
                    parent: d,
                    name,
                    flags: Record::OTHER_DEVICE,
                    errno: 0,
                    own: 0,
                    own_private: 0,
                });
                continue;
            }
            let base = u32::try_from(self.len()).expect("over 2^32 directories");
            let links = std::mem::take(&mut self.links);
            let walk = Walk::new(kid.name.to_bytes(), links, base, st.dev, lister);
            let fd = Arc::clone(&fd);
            pool.scope(|s| walk.visit(s, fd, Record::NO_PARENT, 0, kid));
            let mut sub = walk.finish(Since::default());
            self.links = std::mem::take(&mut sub.links);
            self.graft(d, sub);
        }
    }

    /// Flags dir `d` [`Record::DENIED`] with `own` bytes, like a scan that
    /// could not list it, and removes its subdirectories `kids`.
    fn deny(&mut self, d: u32, kids: &[u32], own: Own, e: Errno) {
        for &k in kids {
            self.remove(k);
        }
        let errno = e.raw_os_error() as u16;
        let r = &mut self.records[d as usize];
        (r.flags, r.errno, r.own, r.own_private) = (Record::DENIED, errno, own.bytes, own.private);
    }

    /// The own blocks of dir `d`, from its parent, or 0 if that cannot be
    /// opened either.
    fn own_blocks(&self, root: &OwnedFd, d: u32, reclaimable: bool) -> Own {
        let r = self.record(d);
        let name = CString::new(self.name(r.name)).expect("names hold no NUL");
        let st = (d != 0)
            .then(|| self.open(root, r.parent).ok())
            .flatten()
            .and_then(|parent| sys::child_stat(parent.as_fd(), &name).ok());
        st.map_or(
            Own {
                bytes: 0,
                private: 0,
            },
            |st| Own::of(&st, reclaimable),
        )
    }

    /// Opens dir `d` from the root's fd down, one `openat` per name, so no
    /// path is built and no symlink below the root is followed.
    fn open(&self, root: &OwnedFd, d: u32) -> rustix::io::Result<OwnedFd> {
        let mut names = Vec::new();
        let mut i = d;
        while i != 0 {
            let r = self.record(i);
            names.push(self.name(r.name));
            i = r.parent;
        }
        // a fresh open of the root, which reads from the start
        let mut fd = sys::open_child(root.as_fd(), c".")?;
        for name in names.into_iter().rev() {
            let name = CString::new(name).expect("names hold no NUL");
            fd = sys::open_child(fd.as_fd(), &name)?;
        }
        Ok(fd)
    }
}

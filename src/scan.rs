//! The parallel walk. One rayon task per directory, never recursion, so deep
//! trees cannot overflow the stack. Every open is relative to the parent's
//! fd, so trees deeper than PATH_MAX work.
//!
//! [`Tree::apply`] lists changed dirs again with the same reader, and scans
//! the subdirectories new in them with the same walk, appending to the
//! tree.

use crate::sys::{self, DirStat, Kind};
use crate::tree::cache::Derived;
use crate::tree::{Builder, LargeFile, Links, Progress, ReadTree, Record, Tree, derive};
use crate::watch::Changes;
use rayon::ThreadPool;
use rustix::fd::{AsFd, OwnedFd};
use rustix::io::Errno;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::ffi::{CString, OsStr};
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use std::{fmt, io};

#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    /// Worker threads. `None` starts with 4 and adds threads, up to the
    /// number of cores, while they wait on the disk.
    pub threads: Option<NonZeroUsize>,
    /// Also measure [`ReadTree::own_private`]. Only the macOS reader can see
    /// clones; the portable reader leaves it 0.
    pub reclaimable: bool,
    pub reader: Reader,
    /// Stops the scan before it is done, as when the user quits.
    pub stop: Stop,
}

/// Asked before each directory below the root a scan lists, from any scan
/// thread: `true` leaves it out, with everything below it, as when the user
/// quits. The tree is then [`ReadTree::stopped`]. Never stops by default.
#[derive(Clone, Default)]
pub struct Stop(Option<Arc<dyn Fn() -> bool + Send + Sync>>);

impl Stop {
    pub fn new(stop: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(stop)))
    }

    fn now(&self) -> bool {
        self.0.as_ref().is_some_and(|stop| stop())
    }
}

impl fmt::Debug for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Stop")
    }
}

/// Which directory reader lists entries. Used by the differential test and
/// benchmarks; users want [`Reader::Auto`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
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

/// Why [`Tree::apply`] could not bring a tree up to date.
#[derive(Debug)]
pub(crate) enum ApplyError {
    /// The changes reach the root: only a full scan can.
    FullScan,
    Scan(ScanError),
}

/// What changed on disk: each folder whose own bytes changed, and each
/// folder that appeared or went, with its whole size, by path, summed per
/// path. `None` if nothing did, not even a folder of 0 bytes.
pub(crate) type Deltas = Option<Vec<(PathBuf, i64)>>;

/// What [`Tree::apply`] did.
#[derive(Debug, Default)]
pub(crate) struct Applied {
    pub deltas: Deltas,
    /// The ids from this one on are folders new to the tree.
    pub new: u32,
    /// Each old id's new one, `u32::MAX` for a folder gone, if the ids
    /// were numbered again ([`Tree::compact`]).
    pub moved: Option<Vec<u32>>,
}

/// Scans `root` without crossing devices or mount points, or following
/// symlinks.
///
/// Also changes two process-wide settings: it raises the soft open-file
/// limit, since the walk holds many directory fds open at once, and on macOS
/// it stops iCloud placeholder files from downloading.
pub fn scan(root: &Path, opts: &ScanOptions) -> Result<Tree, ScanError> {
    walk_root(root, opts, false, |_| {})
}

/// [`scan`], first handing `progress` a [`Progress`] that reads the tree
/// while it is built, from any thread. The tree can be watched from when
/// the scan started ([`Tree::since`]).
pub fn scan_live(
    root: &Path,
    opts: &ScanOptions,
    progress: impl FnOnce(Progress),
) -> Result<Tree, ScanError> {
    walk_root(root, opts, true, progress)
}

/// [`scan_live`], taking the OS's event id to watch from only if `watch`,
/// as that loads the frameworks for it on macOS.
fn walk_root(
    root: &Path,
    opts: &ScanOptions,
    watch: bool,
    progress: impl FnOnce(Progress),
) -> Result<Tree, ScanError> {
    sys::raise_fd_limit();
    sys::keep_placeholders_remote();
    let fd = sys::open_root(root).map_err(|e| ScanError::Root(e.into()))?;
    let st = sys::dir_stat(fd.as_fd()).map_err(|e| ScanError::Root(e.into()))?;
    let lister = lister(&fd, opts).map_err(|e| ScanError::Root(e.into()))?;
    // taken first, so a watch from here also sees the changes made while
    // the walk runs
    let since = if watch { sys::event_id() } else { 0 };
    let (pool, gate) = pool(opts).map_err(ScanError::ThreadPool)?;
    let tree = Builder::new(root.as_os_str().as_bytes(), opts.reclaimable);
    let walk = Walk::new(tree, Links::new(), st.dev, lister, opts.stop.clone(), gate);
    let own = Own::of(&st, opts.reclaimable);
    progress(walk.tree.progress());
    walk.run(&pool, |s| walk.list(s, fd, Record::NO_PARENT, 0, own));
    Ok(walk.finish(since, false))
}

/// The [`sys::Lister`] `opts` ask for, of the filesystem of `fd`.
fn lister(fd: &OwnedFd, opts: &ScanOptions) -> rustix::io::Result<sys::Lister> {
    match opts.reader {
        Reader::Auto => sys::Lister::of(fd.as_fd(), opts.reclaimable),
        Reader::Portable => Ok(sys::Lister::PORTABLE),
    }
}

/// A pool of `--threads` threads, or of every core with a [`Gate`] that
/// lets only some of them list dirs at once.
fn pool(opts: &ScanOptions) -> Result<(ThreadPool, Gate), rayon::ThreadPoolBuildError> {
    let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let (threads, gate) = match opts.threads {
        Some(n) => (n.get(), Gate::fixed(n.get())),
        None => (cores, Gate::adaptive(cores)),
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?;
    Ok((pool, gate))
}

/// How many of the pool's threads list dirs at once: those with an index
/// below `limit`. The others hand back any dir they take and wait.
///
/// The best number depends on the tree. Listing a dir is ~98% kernel time,
/// and past ~4 threads they mostly contend for kernel locks: on a 10-core
/// M4, a cached tree of 4k dirs took 0.41 s with 4 threads and 0.66 s with
/// 8, and one of 1M dirs 11 s and 17 s. But a tree that waits on the disk
/// needs more threads to hide the waits: a home dir of 1.3M dirs took 40 s
/// with 4 and 26 s with 8. So an adaptive gate starts at 4 and moves by how
/// busy its threads keep the CPU, see [`Gate::tune`].
struct Gate {
    limit: AtomicUsize,
    /// `Some((min, max))` of the limit when it adapts.
    range: Option<(usize, usize)>,
    /// Dirs spawned and not yet visited. Waiting threads stop waiting at 0,
    /// when the walk is done.
    pending: AtomicUsize,
    /// Whether the walk is done, for the thread that tunes the limit.
    done: Mutex<bool>,
    wake: Condvar,
}

/// How often an adaptive [`Gate`] measures the CPU time and moves its
/// limit. Long enough to list hundreds of dirs, so a measure is not noise,
/// and short enough to reach every core within a second of a large scan.
const TICK: Duration = Duration::from_millis(100);

/// The share of a tick the admitted threads spend on the CPU, under which
/// they wait on the disk, so more threads hide the waits. With 4 threads,
/// a cached tree keeps them 60-95% busy (contended kernel locks spin on the
/// CPU), and a home dir partly on the disk 30-40%.
const IDLE: f64 = 0.5;

/// The share over which the admitted threads are not waiting on the disk,
/// so fewer do the same work with less lock contention.
const BUSY: f64 = 0.75;

impl Gate {
    fn fixed(n: usize) -> Self {
        Self::new(n, None)
    }

    /// Starts at 4 rather than the performance cores: every Apple silicon
    /// Mac has at least 4, and the kernel locks, not the core kind, limit a
    /// cached walk. More than `cores` was slower: 39 s against 31 s on the
    /// home dir with 20.
    fn adaptive(cores: usize) -> Self {
        let first = cores.min(4);
        Self::new(first, Some((first, cores)))
    }

    fn new(limit: usize, range: Option<(usize, usize)>) -> Self {
        Self {
            limit: AtomicUsize::new(limit),
            range,
            pending: AtomicUsize::new(0),
            done: Mutex::new(false),
            wake: Condvar::new(),
        }
    }

    /// Whether this thread may list a dir now.
    fn admits(&self) -> bool {
        rayon::current_thread_index().is_none_or(|i| i < self.limit.load(Relaxed))
    }

    /// Waits until this thread is admitted, or the walk is done.
    fn wait(&self) {
        let i = rayon::current_thread_index().unwrap_or(0);
        let mut done = self.done.lock().unwrap();
        while i >= self.limit.load(Relaxed) && self.pending.load(Relaxed) > 0 {
            done = self.wake.wait(done).unwrap();
        }
    }

    /// One spawned dir visited.
    fn visited(&self) {
        if self.pending.fetch_sub(1, Relaxed) == 1 {
            // under the lock, so no waiter checks and then misses it
            let _done = self.done.lock().unwrap();
            self.wake.notify_all();
        }
    }

    /// Moves the limit every [`TICK`] until the walk is done, by how busy
    /// the admitted threads kept the CPU, while dirs wait for them: twice
    /// as many while under half of [`IDLE`] were, one more while under
    /// [`IDLE`]; one fewer while over [`BUSY`]. Doubling reaches every core
    /// in two ticks on a tree mostly on the disk: on an 8 GB M2 a 410k-file
    /// tree took 0.82 s adding one at a time, 0.73 s doubling, 0.65 s with
    /// 8 threads from the start.
    fn tune(&self, (min, max): (usize, usize)) {
        let mut done = self.done.lock().unwrap();
        let (mut at, mut used) = (Instant::now(), sys::cpu_time());
        loop {
            done = self.wake.wait_timeout(done, TICK).unwrap().0;
            if *done {
                return;
            }
            let (now, cpu) = (Instant::now(), sys::cpu_time());
            let limit = self.limit.load(Relaxed);
            let busy = (cpu - used).as_secs_f64() / (now - at).as_secs_f64() / limit as f64;
            (at, used) = (now, cpu);
            let queued = self.pending.load(Relaxed) > limit;
            let next = if busy < IDLE / 2.0 && queued {
                // mostly waiting on the disk: more threads hide the waits
                limit * 2
            } else if busy < IDLE && queued {
                limit + 1
            } else if busy > BUSY {
                limit - 1
            } else {
                limit
            }
            .clamp(min, max);
            if next != limit {
                self.limit.store(next, Relaxed);
                self.wake.notify_all();
            }
        }
    }
}

struct Walk {
    tree: Builder,
    /// Moved in from the tree the walk adds to, if any.
    links: Mutex<Links>,
    root_dev: u64,
    /// Of the root's filesystem, so of every dir the walk lists.
    lister: sys::Lister,
    stop: Stop,
    /// Some dir was left out for [`Walk::stop`].
    stopped: AtomicBool,
    gate: Gate,
}

/// [`Record::own`] and [`ReadTree::own_private`], summed together.
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
    /// A walk that builds `tree`, adding to `links`.
    fn new(
        tree: Builder,
        links: Links,
        root_dev: u64,
        lister: sys::Lister,
        stop: Stop,
        gate: Gate,
    ) -> Self {
        Self {
            tree,
            links: Mutex::new(links),
            root_dev,
            lister,
            stop,
            stopped: AtomicBool::new(false),
            gate,
        }
    }

    /// Runs `op` and every task it spawns on `pool`, tuning the gate
    /// meanwhile if it adapts.
    fn run<'s>(&'s self, pool: &ThreadPool, op: impl FnOnce(&rayon::Scope<'s>) + Send) {
        std::thread::scope(|t| {
            if let Some(range) = self.gate.range {
                t.spawn(move || self.gate.tune(range));
            }
            pool.scope(op);
            *self.gate.done.lock().unwrap() = true;
            self.gate.wake.notify_all();
        });
    }

    /// Spawns a visit of each of `kids`, with its name id, subdirectories
    /// of dir `parent` open as `fd`.
    fn spawn<'s>(
        &'s self,
        s: &rayon::Scope<'s>,
        fd: Arc<OwnedFd>,
        parent: u32,
        kids: Vec<(Child, u32)>,
    ) {
        self.gate.pending.fetch_add(kids.len(), Relaxed);
        for (kid, name) in kids {
            let fd = Arc::clone(&fd);
            s.spawn(move |s| self.visit(s, fd, parent, name, kid));
        }
    }

    fn finish(self, since: u64, stopped: bool) -> Tree {
        let links = self.links.into_inner().unwrap();
        let stopped = stopped || self.stopped.into_inner();
        self.tree.finish(links, since, stopped)
    }

    fn visit<'s>(
        &'s self,
        s: &rayon::Scope<'s>,
        parent_fd: Arc<OwnedFd>,
        parent: u32,
        name: u32,
        child: Child,
    ) {
        // handed back for an admitted thread, which steals it
        if !self.gate.admits() {
            s.spawn(move |s| self.visit(s, parent_fd, parent, name, child));
            self.gate.wait();
            return;
        }
        if self.stop.now() {
            self.stopped.store(true, Relaxed);
        } else {
            let opened = sys::open_child(parent_fd.as_fd(), &child.name);
            // release the parent's fd as soon as possible to bound open fds
            drop(parent_fd);
            match opened {
                Ok(fd) => self.list(s, fd, parent, name, child.own),
                // like du: a denied directory still counts its own blocks
                Err(e) => {
                    self.deny(parent, name, child.own, e);
                }
            }
        }
        self.gate.visited();
    }

    fn deny(&self, parent: u32, name: u32, own: Own, e: Errno) -> u32 {
        let record = Record {
            parent,
            name,
            flags: Record::DENIED,
            errno: e.raw_os_error() as u16,
            own: own.bytes,
        };
        self.tree.push(record, own.private)
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
            None => {
                let record = Record {
                    parent,
                    name,
                    flags: 0,
                    errno: 0,
                    own: listing.own.bytes,
                };
                self.tree.push(record, listing.own.private)
            }
        };
        self.own(id, &listing.claimed);
        self.tree.largest.offer(id, listing.files);
        if listing.denied.is_some() {
            return;
        }
        let kids = listing.kids;
        let ids = self.tree.intern(kids.iter().map(|k| k.name.to_bytes()));
        let mut visit = Vec::with_capacity(kids.len());
        for (kid, name) in kids.into_iter().zip(ids) {
            // like `du -x`: decided from the parent's listing, so a mount
            // point is never opened. The mount flag catches the macOS Data
            // volume, which shares the root's dev.
            if kid.mount || kid.dev != self.root_dev {
                let record = Record {
                    parent: id,
                    name,
                    flags: Record::OTHER_DEVICE,
                    errno: 0,
                    own: 0,
                };
                self.tree.push(record, 0);
            } else {
                visit.push((kid, name));
            }
        }
        self.spawn(s, Arc::new(fd), id, visit);
    }

    /// Scans `kids`, subdirectories of dir `d` of the tree appended to,
    /// which `names` lead to from `root`.
    fn adopt<'s>(
        &'s self,
        s: &rayon::Scope<'s>,
        root: &OwnedFd,
        names: &[CString],
        d: u32,
        kids: Vec<Child>,
    ) {
        // gone since it was listed: the change that removed it lists its
        // parent again
        let Ok(fd) = open_path(root, names) else {
            return;
        };
        let ids = self.tree.intern(kids.iter().map(|k| k.name.to_bytes()));
        self.spawn(s, Arc::new(fd), d, kids.into_iter().zip(ids).collect());
    }
}

impl Tree {
    /// Brings the tree up to date with `changes`: lists each dir the
    /// changes touched again, one bulk read each, then scans the
    /// subdirectories new in them in one walk. Returns what changed. On an
    /// error the tree is left as it was.
    pub(crate) fn apply(
        &mut self,
        changes: &Changes,
        opts: &ScanOptions,
    ) -> Result<Applied, ApplyError> {
        if changes.is_empty() {
            return Ok(Applied {
                new: self.len() as u32,
                ..Applied::default()
            });
        }
        // taken, as the records change: what reads the tree next derives
        // it again
        let index = match std::mem::take(&mut self.cache).into_inner() {
            Some(index) => index,
            None => derive(self),
        };
        let (dirty, fresh) = self.touched(&index, changes).ok_or(ApplyError::FullScan)?;
        let root_err = |e: Errno| ApplyError::Scan(ScanError::Root(e.into()));
        let root = sys::open_root(Path::new(OsStr::from_bytes(self.name(0)))).map_err(root_err)?;
        let dev = sys::dir_stat(root.as_fd()).map_err(root_err)?.dev;
        let lister = lister(&root, opts).map_err(root_err)?;
        let (pool, gate) = pool(opts).map_err(|e| ApplyError::Scan(ScanError::ThreadPool(e)))?;
        sys::raise_fd_limit();
        sys::keep_placeholders_remote();
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
        let mut new = Vec::new();
        // parents first, so a dir gone with its parent is not listed
        for &d in &dirty {
            if !self.gone(d) {
                let kids = self.relist(&root, d, index.children(d), opts);
                if !kids.is_empty() {
                    new.push((self.names_to(d), d, kids));
                }
            }
        }
        self.prune();
        if !new.is_empty() {
            self.scan_new(&root, new, dev, lister, &pool, gate, opts);
        }
        let n = index.size.len();
        let deltas = self.deltas(&index);
        let moved = self.compact();
        let new = match &moved {
            Some(id) => id[..n].iter().filter(|&&k| k != u32::MAX).count(),
            None => n,
        };
        Ok(Applied {
            deltas,
            new: new as u32,
            moved,
        })
    }

    /// Scans `new`, the subdirectories new in dirs listed again, in one
    /// walk appending to the tree.
    #[allow(clippy::too_many_arguments)]
    fn scan_new(
        &mut self,
        root: &OwnedFd,
        new: Vec<(Vec<CString>, u32, Vec<Child>)>,
        dev: u64,
        lister: sys::Lister,
        pool: &ThreadPool,
        gate: Gate,
        opts: &ScanOptions,
    ) {
        let links = std::mem::take(&mut self.links);
        let walk = Walk::new(
            Builder::append(self),
            links,
            dev,
            lister,
            opts.stop.clone(),
            gate,
        );
        // each parent is opened again from the root, rather than kept open
        // since its listing, so no more dirs are open at once than in a scan
        walk.run(pool, |s| {
            for (names, d, kids) in new {
                let walk = &walk;
                s.spawn(move |s| walk.adopt(s, root, &names, d, kids));
            }
        });
        *self = walk.finish(self.since, self.stopped);
    }

    /// What [`Tree::apply`] changed, from `index`, derived before it.
    /// Records past its end are new; removed ones are still in place.
    fn deltas(&self, index: &Derived) -> Deltas {
        let n = index.size.len();
        let gone = |id: u32| self.records[id as usize].flags & Record::REMOVED != 0;
        let own_before = |id: u32| {
            let kids = index.children(id).iter();
            index.size[id as usize] - kids.map(|&k| index.size[k as usize]).sum::<u64>()
        };
        // the sizes of the new records, children first
        let mut size = vec![0u64; self.records.len() - n];
        for i in (n..self.records.len()).rev() {
            let r = self.records[i];
            size[i - n] += r.own;
            if r.parent as usize >= n {
                size[r.parent as usize - n] += size[i - n];
            }
        }
        let mut out = Vec::new();
        for id in 0..n as u32 {
            let r = self.records[id as usize];
            if index.flags[id as usize] & Record::REMOVED != 0 {
                continue;
            }
            if gone(id) {
                if id == 0 || !gone(r.parent) {
                    out.push((self.path(id), -(index.size[id as usize] as i64)));
                }
            } else if r.own != own_before(id) {
                out.push((self.path(id), r.own as i64 - own_before(id) as i64));
            }
        }
        for i in n..self.records.len() {
            let r = self.records[i];
            if (r.parent as usize) < n {
                out.push((self.path(i as u32), size[i - n] as i64));
            }
        }
        if out.is_empty() {
            return None;
        }
        // a dir listed afresh goes and comes back, at one path
        out.sort();
        out.dedup_by(|b, a| {
            let same = a.0 == b.0;
            if same {
                a.1 += b.1;
            }
            same
        });
        out.retain(|&(_, d)| d != 0);
        Some(out)
    }

    /// The dirs `changes` touched or named, and those of them to list
    /// afresh, with everything below them. `None` if that is the root: a
    /// full scan.
    fn touched(&self, index: &Derived, changes: &Changes) -> Option<(BTreeSet<u32>, Vec<u32>)> {
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
                    kids.map(|&k| (self.name(k), k)).collect()
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

    /// Lists dir `d` again: its own blocks and files anew, and its
    /// subdirectories gone since removed. Returns the new ones, to scan.
    /// `kids` were its subdirectories, and `root` is the root's fd.
    fn relist(&mut self, root: &OwnedFd, d: u32, kids: &[u32], opts: &ScanOptions) -> Vec<Child> {
        let kids: Vec<u32> = (kids.iter().copied())
            .filter(|&k| self.record(k).flags & Record::REMOVED == 0)
            .collect();
        let opened = self
            .open(root, d)
            .and_then(|fd| Ok((sys::dir_stat(fd.as_fd())?, lister(&fd, opts)?, fd)));
        let (st, lister, fd) = match opened {
            Ok(opened) => opened,
            // listing the parent again finds that too
            Err(Errno::NOENT | Errno::NOTDIR) => {
                self.remove(d);
                return Vec::new();
            }
            // like a scan: a dir that cannot be opened counts its own blocks
            Err(e) => {
                let own = self.own_blocks(root, d, opts.reclaimable);
                self.deny(d, &kids, own, e);
                return Vec::new();
            }
        };
        let links = std::mem::take(&mut self.links);
        let gate = Gate::fixed(1);
        let walk = Walk::new(
            Builder::new(b"", false),
            links,
            st.dev,
            lister,
            Stop::default(),
            gate,
        );
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
            self.deny(d, &kids, listing.own, e);
            return Vec::new();
        }
        let r = &mut self.records[d as usize];
        (r.flags, r.errno, r.own) = (0, 0, listing.own.bytes);
        self.set_private(d, listing.own.private);

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
        new.retain(|kid| {
            if !kid.mount && kid.dev == st.dev {
                return true;
            }
            let name = self.push_name(kid.name.to_bytes());
            let record = Record {
                parent: d,
                name,
                flags: Record::OTHER_DEVICE,
                errno: 0,
                own: 0,
            };
            self.push(record, 0);
            false
        });
        new
    }

    /// Flags dir `d` [`Record::DENIED`] with `own` bytes, like a scan that
    /// could not list it, and removes its subdirectories `kids`.
    fn deny(&mut self, d: u32, kids: &[u32], own: Own, e: Errno) {
        for &k in kids {
            self.remove(k);
        }
        let errno = e.raw_os_error() as u16;
        let r = &mut self.records[d as usize];
        (r.flags, r.errno, r.own) = (Record::DENIED, errno, own.bytes);
        self.set_private(d, own.private);
    }

    /// The own blocks of dir `d`, from its parent, or 0 if that cannot be
    /// opened either.
    fn own_blocks(&self, root: &OwnedFd, d: u32, reclaimable: bool) -> Own {
        let r = self.record(d);
        let name = CString::new(self.raw_name(r.name)).expect("names hold no NUL");
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

    /// Opens dir `d` from the root's fd down: see [`open_path`].
    fn open(&self, root: &OwnedFd, d: u32) -> rustix::io::Result<OwnedFd> {
        open_path(root, &self.names_to(d))
    }

    /// The names from the root down to dir `d`.
    fn names_to(&self, d: u32) -> Vec<CString> {
        let mut names = Vec::new();
        let mut i = d;
        while i != 0 {
            let r = self.record(i);
            names.push(CString::new(self.raw_name(r.name)).expect("names hold no NUL"));
            i = r.parent;
        }
        names.reverse();
        names
    }
}

/// Opens the dir `names` lead to from `root`, one `openat` per name, so no
/// path is built and no symlink below the root is followed.
fn open_path(root: &OwnedFd, names: &[CString]) -> rustix::io::Result<OwnedFd> {
    // a fresh open of the root, which reads from the start
    let mut fd = sys::open_child(root.as_fd(), c".")?;
    for name in names {
        fd = sys::open_child(fd.as_fd(), name)?;
    }
    Ok(fd)
}

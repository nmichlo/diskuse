//! The scanned tree: one [`Record`] per directory, in a flat array where a
//! child's index is always greater than its parent's. Files are not stored;
//! their bytes are folded into their directory's `own`. Only the
//! [`LARGEST`] largest files are kept, by name.
//!
//! A live update appends the dirs new since, so the order holds, and flags
//! the ones gone [`Record::REMOVED`] rather than moving any record. Once
//! those are half the records, it drops them and numbers the rest again,
//! in order. So an id is a folder of the tree it came from: a later tree
//! may number it differently, and [`ReadTree::find`] finds it by path.

use crate::sys;
use hashbrown::HashTable;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ffi::{OsStr, OsString};
use std::hash::{BuildHasher, RandomState};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// How many of the largest files a scan keeps.
pub const LARGEST: usize = 1000;

/// One directory.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "cli", derive(rkyv::Archive, rkyv::Serialize))]
pub struct Record {
    /// Index of the parent record, [`Record::NO_PARENT`] for the root.
    pub parent: u32,
    /// Id for [`ReadTree::raw_name`].
    pub name: u32,
    /// [`Record::DENIED`], [`Record::OTHER_DEVICE`] and [`Record::REMOVED`]
    /// bits.
    pub flags: u16,
    /// The errno of a [`Record::DENIED`] directory, else 0.
    pub errno: u16,
    /// Allocated bytes of the directory itself plus all its non-directory
    /// entries. Hard links count once per scan.
    pub own: u64,
}

impl Record {
    pub const NO_PARENT: u32 = u32::MAX;
    /// Could not be opened or listed. `own` is the directory's own blocks.
    pub const DENIED: u16 = 1 << 0;
    /// On another device than the root, so not descended into (`du -x`).
    pub const OTHER_DEVICE: u16 = 1 << 1;
    /// Never in a record: some descendant is [`Record::DENIED`], see
    /// [`ReadTree::partial`].
    pub const PARTIAL: u16 = 1 << 2;
    /// Gone since the scan, like everything below it. Its `own` is 0, and
    /// [`ReadTree::children`] leaves it out. No other bit is set.
    pub const REMOVED: u16 = 1 << 3;
}

/// What [`ReadTree`]'s per-folder methods read, computed from the records
/// on first use. Sealed: only this crate's trees hold one.
pub(crate) mod cache {
    use std::sync::OnceLock;

    #[derive(Clone, Debug)]
    pub struct Derived {
        /// `own` of each record plus that of all its descendants.
        pub size: Vec<u64>,
        /// The same sum of [`super::ReadTree::own_private`].
        pub private: Vec<u64>,
        /// Each record's flags plus [`super::Record::PARTIAL`].
        pub flags: Vec<u16>,
        /// The children of record `id` are `kids[start[id]..start[id + 1]]`.
        pub start: Vec<u32>,
        pub kids: Vec<u32>,
    }

    impl Derived {
        pub fn children(&self, id: u32) -> &[u32] {
            let id = id as usize;
            &self.kids[self.start[id] as usize..self.start[id + 1] as usize]
        }
    }

    pub trait Cached {
        fn cache(&self) -> &OnceLock<Derived>;
    }
}

use cache::{Cached, Derived};

/// A file of a folder, from [`ReadTree::files`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub name: Box<[u8]>,
    /// Allocated bytes, not the apparent length.
    pub bytes: u64,
    /// Of `bytes`, those not shared with a clone, if asked for, else 0.
    pub private: u64,
}

/// One of the [`LARGEST`] largest files. Ordered by `bytes` first.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "cli", derive(rkyv::Archive, rkyv::Serialize))]
pub struct LargeFile {
    /// Allocated bytes, as counted in its directory's `own`.
    pub bytes: u64,
    /// Record id of the directory it is in.
    pub dir: u32,
    pub name: Box<[u8]>,
}

/// Names by id: append-only, so [`Progress`] reads them while a scan adds
/// more.
pub(crate) type Names = Arc<boxcar::Vec<Box<[u8]>>>;

/// The record id of the dir that counts each multiply-linked file, by
/// `(dev, ino)`, so it is counted once, and once again after that dir is
/// listed again.
pub(crate) type Links = HashMap<(u64, u64), u32>;

#[derive(Clone, Debug)]
pub struct Tree {
    pub(crate) records: Vec<Record>,
    /// Name id 0 is the root's, never shared with a directory.
    pub(crate) names: Names,
    /// In no particular order.
    pub(crate) largest: Vec<LargeFile>,
    pub(crate) links: Links,
    /// [`ReadTree::own_private`] by record id, empty unless scanned with
    /// reclaimable sizes, so other scans' records are 8 B smaller.
    pub(crate) private: Vec<u64>,
    /// The id of the OS's latest change when the scan started, so a live
    /// watch from it sees the changes made while the scan ran. 0 where the
    /// OS keeps no record of changes, and in a saved scan.
    pub(crate) since: u64,
    /// The scan was stopped before it listed every dir.
    pub(crate) stopped: bool,
    /// Reset by every change to the records.
    pub(crate) cache: OnceLock<Derived>,
}

/// Read access to a tree: an owned [`Tree`], or a saved one read in place
/// ([`crate::SavedTree`]), so `show` prints a saved scan without building a
/// [`Tree`]. Folders are record ids, the root is 0, and a parent's id is
/// always below its children's.
pub trait ReadTree: Cached {
    /// How many records there are, including [`Record::REMOVED`] ones.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn record(&self, id: u32) -> Record;

    /// Of record `id`'s `own`, the bytes not shared with a clone elsewhere,
    /// so freed by deleting. 0 unless scanned with reclaimable sizes on
    /// macOS.
    fn own_private(&self, id: u32) -> u64;

    /// The name with id `name` ([`Record::name`]). Name 0 is the root's,
    /// the scanned path as given.
    fn raw_name(&self, name: u32) -> &[u8];

    /// The [`LARGEST`] largest files, or all if fewer, in no particular
    /// order, as `(bytes, dir, name)`: see [`LargeFile`].
    fn largest(&self) -> impl Iterator<Item = (u64, u32, &[u8])>;

    /// The scan was stopped before it listed every dir, so sizes are lower
    /// bounds.
    fn stopped(&self) -> bool;

    /// The name of folder `id`; for the root, the scanned path as given.
    fn name(&self, id: u32) -> &[u8] {
        self.raw_name(self.record(id).name)
    }

    /// The path of folder `id`: the root's, then each name below it.
    fn path(&self, id: u32) -> PathBuf {
        OsString::from_vec(dir_path(self, id)).into()
    }

    /// Allocated bytes of folder `id` and everything below it.
    fn size(&self, id: u32) -> u64 {
        derived(self).size[id as usize]
    }

    /// Allocated bytes of folder `id` itself and its files.
    fn own(&self, id: u32) -> u64 {
        self.record(id).own
    }

    /// Of [`ReadTree::size`], the bytes deleting the folder frees: see
    /// [`ReadTree::own_private`].
    fn reclaimable(&self, id: u32) -> u64 {
        derived(self).private[id as usize]
    }

    /// Why folder `id` could not be read (`EACCES`, `EPERM`, `errno N`).
    fn error(&self, id: u32) -> Option<String> {
        let r = self.record(id);
        (r.flags & Record::DENIED != 0).then(|| match sys::errno_name(r.errno) {
            Some(name) => name.into(),
            None => format!("errno {}", r.errno),
        })
    }

    /// Something below folder `id` could not be read, so its size is a
    /// lower bound.
    fn partial(&self, id: u32) -> bool {
        derived(self).flags[id as usize] & Record::PARTIAL != 0
    }

    /// Folder `id` is a mount point, not scanned into (`du -x`).
    fn other_device(&self, id: u32) -> bool {
        self.record(id).flags & Record::OTHER_DEVICE != 0
    }

    /// The subfolders of `id`, in no particular order, leaving out
    /// [`Record::REMOVED`] ones.
    fn children(&self, id: u32) -> &[u32] {
        derived(self).children(id)
    }

    /// The folder at `path`: relative to the root, or absolute and below
    /// it. `.` parts are skipped.
    fn find(&self, path: &Path) -> Option<u32> {
        let root = Path::new(OsStr::from_bytes(self.raw_name(0)));
        let below = match path.is_absolute() {
            true => path.strip_prefix(root).ok()?,
            false => path,
        };
        let mut id = 0;
        for part in below.components() {
            let name = match part {
                Component::CurDir => continue,
                Component::Normal(name) => name.as_bytes(),
                _ => return None,
            };
            id = *self.children(id).iter().find(|&&k| self.name(k) == name)?;
        }
        Some(id)
    }

    /// The files of folder `id`, listed from disk now, as the tree keeps
    /// only folder totals: each one's allocated bytes, and with
    /// `reclaimable` of those the ones not shared with a clone (macOS),
    /// else 0. None for a folder on another device, which the scan did not
    /// go into.
    fn files(&self, id: u32, reclaimable: bool) -> std::io::Result<Vec<File>> {
        let mut files = Vec::new();
        if self.other_device(id) {
            return Ok(files);
        }
        let path = dir_path(self, id);
        let fd = match id {
            // a symlinked root is followed, as the scan does
            0 => sys::open_root(Path::new(OsStr::from_bytes(&path))),
            _ => sys::open_child(
                rustix::fs::CWD,
                &std::ffi::CString::new(path).expect("names hold no NUL"),
            ),
        }?;
        sys::read_dir(&fd, reclaimable, |e| {
            if e.kind != sys::Kind::Dir {
                files.push(File {
                    name: e.name.to_bytes().into(),
                    bytes: e.bytes,
                    private: e.private,
                });
            }
        })?;
        Ok(files)
    }

    /// The `n` largest files, of the [`LARGEST`] kept, as `(path, bytes)`,
    /// largest first, ties by path.
    fn largest_files(&self, n: usize) -> Vec<(PathBuf, u64)> {
        let mut files: Vec<_> = (self.largest())
            .map(|(bytes, dir, name)| (file_path(self, dir, name), bytes))
            .collect();
        files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        files.truncate(n);
        (files.into_iter())
            .map(|(path, bytes)| (OsString::from_vec(path).into(), bytes))
            .collect()
    }
}

/// The path of folder `id` as bytes, as [`ReadTree::path`].
pub(crate) fn dir_path(tree: &(impl ReadTree + ?Sized), id: u32) -> Vec<u8> {
    let mut names = Vec::new();
    let mut i = id;
    while i != 0 {
        let r = tree.record(i);
        names.push(tree.raw_name(r.name));
        i = r.parent;
    }
    let mut path = tree.raw_name(0).to_vec();
    for name in names.iter().rev() {
        join(&mut path, name);
    }
    path
}

/// The path of file `name` in folder `dir`, as [`dir_path`].
pub(crate) fn file_path(tree: &(impl ReadTree + ?Sized), dir: u32, name: &[u8]) -> Vec<u8> {
    let mut path = dir_path(tree, dir);
    join(&mut path, name);
    path
}

fn derived<T: ReadTree + ?Sized>(tree: &T) -> &Derived {
    tree.cache().get_or_init(|| derive(tree))
}

/// One backwards pass for the sums, as children come after parents, and
/// two forward ones for the children of each record.
pub(crate) fn derive(tree: &(impl ReadTree + ?Sized)) -> Derived {
    let n = tree.len();
    let (mut size, mut private, mut flags) = (vec![0; n], vec![0; n], vec![0; n]);
    for i in (0..n).rev() {
        let r = tree.record(i as u32);
        size[i] += r.own;
        private[i] += tree.own_private(i as u32);
        flags[i] |= r.flags;
        if i > 0 {
            let p = r.parent as usize;
            size[p] += size[i];
            private[p] += private[i];
            if flags[i] & (Record::DENIED | Record::PARTIAL) != 0 {
                flags[p] |= Record::PARTIAL;
            }
        }
    }
    let there = |r: &Record| r.flags & Record::REMOVED == 0;
    let mut start = vec![0u32; n + 1];
    for i in 1..n {
        let r = tree.record(i as u32);
        if there(&r) {
            start[r.parent as usize + 1] += 1;
        }
    }
    for i in 1..=n {
        start[i] += start[i - 1];
    }
    let mut next = start.clone();
    let mut kids = vec![0u32; start[n] as usize];
    for i in 1..n {
        let r = tree.record(i as u32);
        if there(&r) {
            let slot = &mut next[r.parent as usize];
            kids[*slot as usize] = i as u32;
            *slot += 1;
        }
    }
    Derived {
        size,
        private,
        flags,
        start,
        kids,
    }
}

impl Cached for Tree {
    fn cache(&self) -> &OnceLock<Derived> {
        &self.cache
    }
}

impl ReadTree for Tree {
    fn len(&self) -> usize {
        self.records.len()
    }

    fn record(&self, id: u32) -> Record {
        self.records[id as usize]
    }

    fn own_private(&self, id: u32) -> u64 {
        self.private.get(id as usize).copied().unwrap_or(0)
    }

    fn raw_name(&self, name: u32) -> &[u8] {
        &self.names[name as usize]
    }

    fn largest(&self) -> impl Iterator<Item = (u64, u32, &[u8])> {
        (self.largest.iter()).map(|f| (f.bytes, f.dir, &f.name[..]))
    }

    fn stopped(&self) -> bool {
        self.stopped
    }
}

impl Tree {
    /// How many names there are; name ids count from 0.
    pub fn name_count(&self) -> u32 {
        self.names.count() as u32
    }

    /// Whether `id` or a dir above it is [`Record::REMOVED`].
    pub(crate) fn gone(&self, id: u32) -> bool {
        let mut i = id;
        loop {
            let r = &self.records[i as usize];
            if r.flags & Record::REMOVED != 0 {
                return true;
            }
            if i == 0 {
                return false;
            }
            i = r.parent;
        }
    }

    /// Flags `id` [`Record::REMOVED`]. [`Tree::prune`] removes what is
    /// below it.
    pub(crate) fn remove(&mut self, id: u32) {
        self.records[id as usize] = Record {
            flags: Record::REMOVED,
            errno: 0,
            own: 0,
            ..self.records[id as usize]
        };
        self.set_private(id, 0);
    }

    /// Sets [`ReadTree::own_private`] of record `id`, if the tree has
    /// reclaimable sizes.
    pub(crate) fn set_private(&mut self, id: u32, bytes: u64) {
        if let Some(p) = self.private.get_mut(id as usize) {
            *p = bytes;
        }
    }

    /// Appends `record`, of `private` reclaimable bytes.
    pub(crate) fn push(&mut self, record: Record, private: u64) {
        if !self.private.is_empty() {
            self.private.push(private);
        }
        self.records.push(record);
    }

    /// Removes every record below a removed one, and the links and largest
    /// files of removed records. One forward pass, as parents come first.
    pub(crate) fn prune(&mut self) {
        for i in 1..self.records.len() {
            let parent = self.records[i].parent as usize;
            if self.records[parent].flags & Record::REMOVED != 0 {
                self.remove(i as u32);
            }
        }
        let removed = |id: u32| self.records[id as usize].flags & Record::REMOVED != 0;
        self.links.retain(|_, &mut dir| !removed(dir));
        self.largest.retain(|f| !removed(f.dir));
    }

    /// Once removed records are half the tree, drops them and the names
    /// only they used, and numbers the rest again, in order, so a tree
    /// followed for long stays the size of what is on disk. Returns each
    /// old id's new one, `u32::MAX` for those dropped, if it did.
    pub(crate) fn compact(&mut self) -> Option<Vec<u32>> {
        let removed = |r: &Record| r.flags & Record::REMOVED != 0;
        if self.records.iter().filter(|r| removed(r)).count() * 2 <= self.records.len() {
            return None;
        }
        let mut id = vec![u32::MAX; self.records.len()];
        let mut name = vec![u32::MAX; self.names.count()];
        let names = boxcar::Vec::new();
        names.push(self.names[0].clone());
        name[0] = 0;
        let mut records = Vec::new();
        let mut private = Vec::new();
        for (i, r) in self.records.iter().enumerate() {
            if removed(r) {
                continue;
            }
            id[i] = records.len() as u32;
            let n = &mut name[r.name as usize];
            if *n == u32::MAX {
                *n = names.push(self.names[r.name as usize].clone()) as u32;
            }
            records.push(Record {
                parent: match i {
                    0 => Record::NO_PARENT,
                    _ => id[r.parent as usize],
                },
                name: *n,
                ..*r
            });
            if let Some(&p) = self.private.get(i) {
                private.push(p);
            }
        }
        for f in &mut self.largest {
            f.dir = id[f.dir as usize];
        }
        for d in self.links.values_mut() {
            *d = id[*d as usize];
        }
        (self.records, self.private, self.names) = (records, private, Arc::new(names));
        self.cache = OnceLock::new();
        Some(id)
    }

    /// Appends a name, without looking for an equal one, which would need
    /// a map of every name.
    pub(crate) fn push_name(&mut self, name: &[u8]) -> u32 {
        u32::try_from(self.names.push(name.into())).expect("over 2^32 names")
    }

    /// Adds `files` to the largest files, keeping the [`LARGEST`] largest.
    pub(crate) fn offer(&mut self, files: impl IntoIterator<Item = LargeFile>) {
        self.largest.extend(files);
        if self.largest.len() > LARGEST {
            self.largest
                .select_nth_unstable_by(LARGEST, |a, b| b.cmp(a));
            self.largest.truncate(LARGEST);
        }
    }
}

/// `base`, then each `(id, bytes)` of `pairs` at its id, as a dense vector
/// of `len`. Ids at or past `len` are left out.
fn scatter(base: Vec<u64>, pairs: impl Iterator<Item = (u32, u64)>, len: usize) -> Vec<u64> {
    let mut all = base;
    all.resize(len, 0);
    for (id, bytes) in pairs {
        if let Some(p) = all.get_mut(id as usize) {
            *p = bytes;
        }
    }
    all
}

/// The path of folder `d` below the root: its names joined by `/`, empty
/// for the root.
pub(crate) fn below(tree: &impl ReadTree, d: u32) -> Vec<u8> {
    let mut names = Vec::new();
    let mut i = d;
    while i != 0 {
        names.push(tree.name(i));
        i = tree.record(i).parent;
    }
    names.reverse();
    names.join(&b'/')
}

/// Appends `name` to `path` after a `/`.
pub(crate) fn join(path: &mut Vec<u8>, name: &[u8]) {
    // `scan /` must not print `//usr`
    if path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(name);
}

/// The write side of a [`Tree`], shared by all scan threads.
pub(crate) struct Builder {
    /// The records of the tree the walk appends to, none for a scan. New
    /// records come after them, so their ids count from its length.
    base: Arc<Vec<Record>>,
    records: Arc<boxcar::Vec<Record>>,
    /// `(id, bytes)` of [`ReadTree::own_private`], with reclaimable sizes.
    private: Option<Arc<boxcar::Vec<(u32, u64)>>>,
    /// Those of the records of `base`.
    base_private: Vec<u64>,
    names: Names,
    /// The id of each name interned, found by the name's hash and the name
    /// in `names`, so no name is stored twice. Only interning takes the
    /// lock, never reading.
    ids: Mutex<HashTable<u32>>,
    hasher: RandomState,
    pub largest: Arc<Largest>,
}

/// Read access to a tree while [`crate::scan_live`] builds it, or an update
/// appends to it.
#[derive(Clone)]
pub struct Progress {
    base: Arc<Vec<Record>>,
    records: Arc<boxcar::Vec<Record>>,
    private: Option<Arc<boxcar::Vec<(u32, u64)>>>,
    names: Names,
    largest: Arc<Largest>,
}

impl Progress {
    /// The tree so far, with the largest files found so far, or `None`
    /// before the root is listed. Directories not yet listed are not in it
    /// yet, so every size is a lower bound.
    pub fn snapshot(&self) -> Option<Tree> {
        let mut records = Vec::with_capacity(self.base.len() + self.records.count());
        records.extend_from_slice(&self.base);
        // up to the first slot another thread is still writing. A parent
        // is pushed before its children, so every parent is in the prefix.
        let prefix = self.records.iter().enumerate();
        records.extend(
            prefix
                .take_while(|&(k, (i, _))| k == i)
                .map(|(_, (_, r))| *r),
        );
        // a file's dir may be past the prefix
        let largest = (self.largest.heap.lock().unwrap().iter())
            .filter(|f| (f.0.dir as usize) < records.len())
            .map(|Reverse(f)| f.clone())
            .collect();
        // only of the records in the prefix
        let private = self.private.as_ref().map_or_else(Vec::new, |p| {
            scatter(Vec::new(), p.iter().map(|(_, &pair)| pair), records.len())
        });
        (!records.is_empty()).then(|| Tree {
            records,
            names: Arc::clone(&self.names),
            largest,
            links: Links::new(),
            private,
            since: 0,
            stopped: false,
            cache: OnceLock::new(),
        })
    }
}

/// The largest files seen so far, as a min-heap of at most [`LARGEST`].
#[derive(Default)]
pub(crate) struct Largest {
    heap: Mutex<BinaryHeap<Reverse<LargeFile>>>,
    /// The smallest kept size once the heap is full, else 0. Only larger
    /// files take the lock, so most never do.
    floor: AtomicU64,
}

impl Largest {
    /// A file of at most this many bytes would not be kept. May lag behind
    /// the heap, which only costs an extra [`Largest::offer`].
    pub fn floor(&self) -> u64 {
        self.floor.load(Ordering::Relaxed)
    }

    /// Offers `(bytes, name)` files of directory `dir`, whose record id is
    /// only known after its listing.
    pub fn offer(&self, dir: u32, files: Vec<(u64, Box<[u8]>)>) {
        if files.is_empty() {
            return;
        }
        self.keep(
            files
                .into_iter()
                .map(|(bytes, name)| LargeFile { bytes, dir, name }),
        );
    }

    fn keep(&self, files: impl IntoIterator<Item = LargeFile>) {
        let mut heap = self.heap.lock().unwrap();
        for f in files {
            if heap.len() == LARGEST {
                if f.bytes <= heap.peek().unwrap().0.bytes {
                    continue;
                }
                heap.pop();
            }
            heap.push(Reverse(f));
        }
        if heap.len() == LARGEST {
            self.floor
                .store(heap.peek().unwrap().0.bytes, Ordering::Relaxed);
        }
    }
}

impl Builder {
    /// `root` becomes name id 0, the root's, outside the dedup map so no
    /// directory shares it and a saved scan can drop it. `reclaimable`
    /// keeps [`ReadTree::own_private`].
    pub fn new(root: &[u8], reclaimable: bool) -> Self {
        let names = boxcar::Vec::new();
        names.push(root.into());
        Self {
            base: Arc::default(),
            records: Arc::default(),
            private: reclaimable.then(Arc::default),
            base_private: Vec::new(),
            names: Arc::new(names),
            ids: Mutex::default(),
            hasher: RandomState::new(),
            largest: Arc::default(),
        }
    }

    /// Appends to `tree`: takes its records and largest files until
    /// [`Builder::finish`], and adds to its names. Names are not looked up
    /// among its own, which would need a map of every name.
    pub fn append(tree: &mut Tree) -> Self {
        let largest = Largest::default();
        largest.keep(std::mem::take(&mut tree.largest));
        Self {
            base: Arc::new(std::mem::take(&mut tree.records)),
            records: Arc::default(),
            private: (!tree.private.is_empty()).then(Arc::default),
            base_private: std::mem::take(&mut tree.private),
            names: Arc::clone(&tree.names),
            ids: Mutex::default(),
            hasher: RandomState::new(),
            largest: Arc::new(largest),
        }
    }

    /// Appends `record`, of `private` reclaimable bytes, and returns its
    /// id.
    pub fn push(&self, record: Record, private: u64) -> u32 {
        let id = self.base.len() + self.records.push(record);
        let id = u32::try_from(id).expect("over 2^32 directories");
        if let Some(p) = &self.private {
            p.push((id, private));
        }
        id
    }

    /// The name id of each name, under one lock.
    pub fn intern<'a>(&self, names: impl Iterator<Item = &'a [u8]>) -> Vec<u32> {
        let mut ids = self.ids.lock().unwrap();
        let named = |id: u32| &self.names[id as usize][..];
        names
            .map(|name| {
                let hash = self.hasher.hash_one(name);
                if let Some(&id) = ids.find(hash, |&id| named(id) == name) {
                    return id;
                }
                // pushed under the lock, so a name is never stored twice
                let id =
                    u32::try_from(self.names.push(name.into())).expect("over 2^32 distinct names");
                ids.insert_unique(hash, id, |&id| self.hasher.hash_one(named(id)));
                id
            })
            .collect()
    }

    pub fn progress(&self) -> Progress {
        Progress {
            base: Arc::clone(&self.base),
            records: Arc::clone(&self.records),
            private: self.private.clone(),
            names: Arc::clone(&self.names),
            largest: Arc::clone(&self.largest),
        }
    }

    pub fn finish(self, links: Links, since: u64, stopped: bool) -> Tree {
        // first, so its memory is free for the copy
        drop(self.ids);
        // copies, as a `Progress` may still share them
        let mut records = Arc::unwrap_or_clone(self.base);
        records.reserve_exact(self.records.count());
        records.extend(self.records.iter().map(|(_, r)| *r));
        let private = self.private.as_ref().map_or_else(Vec::new, |p| {
            scatter(
                self.base_private,
                p.iter().map(|(_, &pair)| pair),
                records.len(),
            )
        });
        Tree {
            private,
            records,
            names: self.names,
            // taken, as a `Progress` may still share the heap
            largest: std::mem::take(&mut *self.largest.heap.lock().unwrap())
                .into_iter()
                .map(|Reverse(f)| f)
                .collect(),
            links,
            since,
            stopped,
            cache: OnceLock::new(),
        }
    }
}

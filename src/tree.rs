//! The scanned tree: one [`Record`] per directory, in a flat array where a
//! child's index is always greater than its parent's. Files are not stored;
//! their bytes are folded into their directory's `own`. Only the
//! [`LARGEST`] largest files are kept, by name.
//!
//! An update ([`crate::update`]) appends the dirs new since, so the order
//! holds, and flags the ones gone [`Record::REMOVED`] rather than moving
//! any record. A full scan starts afresh.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How many of the largest files a scan keeps.
pub const LARGEST: usize = 1000;

/// One directory.
#[derive(Clone, Copy, Debug)]
pub struct Record {
    /// Index of the parent record, [`Record::NO_PARENT`] for the root.
    pub parent: u32,
    /// Id for [`Tree::name`].
    pub name: u32,
    /// [`Record::DENIED`], [`Record::OTHER_DEVICE`] and [`Record::REMOVED`]
    /// bits.
    pub flags: u32,
    /// The errno of a [`Record::DENIED`] directory, else 0.
    pub errno: u16,
    /// Allocated bytes of the directory itself plus all its non-directory
    /// entries. Hard links count once per scan.
    pub own: u64,
    /// Of `own`, the bytes not shared with a clone elsewhere, so freed by
    /// deleting. 0 unless scanned with `reclaimable` on macOS.
    pub own_private: u64,
}

impl Record {
    pub const NO_PARENT: u32 = u32::MAX;
    /// Could not be opened or listed. `own` is the directory's own blocks.
    pub const DENIED: u32 = 1 << 0;
    /// On another device than the root, so not descended into (`du -x`).
    pub const OTHER_DEVICE: u32 = 1 << 1;
    /// Only in [`Totals::flags`]: some descendant is [`Record::DENIED`].
    pub const PARTIAL: u32 = 1 << 2;
    /// Gone since the scan, like everything below it. Its `own` is 0, and
    /// [`Tree::child_index`] leaves it out. No other bit is set.
    pub const REMOVED: u32 = 1 << 3;
}

/// Per-record values that depend on descendants.
#[derive(Debug)]
pub struct Totals {
    /// `own` of the record plus that of all its descendants.
    pub size: Vec<u64>,
    /// The same sum of `own_private`.
    pub private: Vec<u64>,
    /// The record's flags plus [`Record::PARTIAL`].
    pub flags: Vec<u32>,
}

/// Children of every record: `kids[start[id]..start[id + 1]]`.
#[derive(Debug)]
pub struct ChildIndex {
    pub start: Vec<u32>,
    pub kids: Vec<u32>,
}

impl ChildIndex {
    pub fn children(&self, id: u32) -> &[u32] {
        let id = id as usize;
        &self.kids[self.start[id] as usize..self.start[id + 1] as usize]
    }
}

/// One of the [`LARGEST`] largest files. Ordered by `bytes` first.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

/// Where a tree stands in the OS's record of changes: it has every change
/// up to event `id` of the record `store` ([`crate::sys::event_store`]).
/// Zeros where the OS keeps no record, which no update can start from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Since {
    pub id: u64,
    pub store: u128,
}

#[derive(Debug)]
pub struct Tree {
    pub(crate) records: Vec<Record>,
    /// Name id 0 is the root's, never shared with a directory.
    pub(crate) names: Names,
    /// In no particular order.
    pub(crate) largest: Vec<LargeFile>,
    pub(crate) links: Links,
    pub(crate) since: Since,
}

impl Tree {
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn record(&self, id: u32) -> &Record {
        &self.records[id as usize]
    }

    /// Raw name bytes. The root's name is the scanned path as given.
    pub fn name(&self, name: u32) -> &[u8] {
        &self.names[name as usize]
    }

    /// The [`LARGEST`] largest files, or all if fewer, in no particular
    /// order.
    pub fn largest(&self) -> &[LargeFile] {
        &self.largest
    }

    /// The path of directory `id`: the root's name, then `/` and each name
    /// below it.
    pub fn dir_path(&self, id: u32) -> Vec<u8> {
        let mut names = Vec::new();
        let mut i = id;
        while i != 0 {
            let r = &self.records[i as usize];
            names.push(self.name(r.name));
            i = r.parent;
        }
        let mut path = self.name(0).to_vec();
        for name in names.iter().rev() {
            join(&mut path, name);
        }
        path
    }

    /// The path of `file`, like [`Tree::dir_path`].
    pub fn path(&self, file: &LargeFile) -> Vec<u8> {
        let mut path = self.dir_path(file.dir);
        join(&mut path, &file.name);
        path
    }

    pub fn totals(&self) -> Totals {
        let mut size: Vec<u64> = self.records.iter().map(|r| r.own).collect();
        let mut private: Vec<u64> = self.records.iter().map(|r| r.own_private).collect();
        let mut flags: Vec<u32> = self.records.iter().map(|r| r.flags).collect();
        // children come after parents, so one backwards pass sees every child
        // before its parent
        for i in (1..self.records.len()).rev() {
            let p = self.records[i].parent as usize;
            size[p] += size[i];
            private[p] += private[i];
            if flags[i] & (Record::DENIED | Record::PARTIAL) != 0 {
                flags[p] |= Record::PARTIAL;
            }
        }
        Totals {
            size,
            private,
            flags,
        }
    }

    /// The children of every record not [`Record::REMOVED`].
    pub fn child_index(&self) -> ChildIndex {
        let n = self.records.len();
        let there = |r: &&Record| r.flags & Record::REMOVED == 0;
        let mut start = vec![0u32; n + 1];
        for r in self.records[1..].iter().filter(there) {
            start[r.parent as usize + 1] += 1;
        }
        for i in 1..=n {
            start[i] += start[i - 1];
        }
        let mut next = start.clone();
        let mut kids = vec![0u32; start[n] as usize];
        for (i, r) in self.records.iter().enumerate().skip(1) {
            if there(&r) {
                let slot = &mut next[r.parent as usize];
                kids[*slot as usize] = i as u32;
                *slot += 1;
            }
        }
        ChildIndex { start, kids }
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
            own_private: 0,
            ..self.records[id as usize]
        };
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

    /// Appends a name, without looking for an equal one, which would need
    /// a map of every name.
    pub(crate) fn push_name(&mut self, name: &[u8]) -> u32 {
        u32::try_from(self.names.push(name.into())).expect("over 2^32 names")
    }

    /// Appends `sub`, a scan of a new subdirectory of `parent`, whose
    /// [`Tree::links`] ids already count from [`Tree::len`].
    pub(crate) fn graft(&mut self, parent: u32, sub: Tree) {
        let base = u32::try_from(self.records.len()).expect("over 2^32 directories");
        let names = u32::try_from(self.names.count()).expect("over 2^32 names");
        // every name, the root's too: it is the subdirectory's
        for (_, name) in sub.names.iter() {
            self.names.push(name.clone());
        }
        let records = sub.records.iter().enumerate().map(|(i, r)| Record {
            parent: if i == 0 { parent } else { r.parent + base },
            name: r.name + names,
            ..*r
        });
        self.records.extend(records);
        self.offer(sub.largest.into_iter().map(|f| LargeFile {
            dir: f.dir + base,
            ..f
        }));
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
    records: Arc<boxcar::Vec<Record>>,
    names: Names,
    /// Name ids by name. Only interning takes the lock, never reading.
    ids: Mutex<HashMap<Box<[u8]>, u32>>,
    pub largest: Arc<Largest>,
}

/// Read access to a tree while [`crate::scan_live`] builds it.
#[derive(Clone)]
pub struct Progress {
    records: Arc<boxcar::Vec<Record>>,
    names: Names,
    largest: Arc<Largest>,
}

impl Progress {
    /// The tree so far, with the largest files found so far, or `None`
    /// before the root is listed. Directories not yet listed are not in it
    /// yet, so every size is a lower bound.
    pub fn snapshot(&self) -> Option<Tree> {
        // up to the first slot another thread is still writing. A parent
        // is pushed before its children, so every parent is in the prefix.
        let records: Vec<Record> = self
            .records
            .iter()
            .enumerate()
            .take_while(|&(k, (i, _))| k == i)
            .map(|(_, (_, r))| *r)
            .collect();
        // a file's dir may be past the prefix
        let largest = (self.largest.heap.lock().unwrap().iter())
            .filter(|f| (f.0.dir as usize) < records.len())
            .map(|Reverse(f)| f.clone())
            .collect();
        (!records.is_empty()).then(|| Tree {
            records,
            names: Arc::clone(&self.names),
            largest,
            links: Links::new(),
            since: Since::default(),
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
        let mut heap = self.heap.lock().unwrap();
        for (bytes, name) in files {
            if heap.len() == LARGEST {
                if bytes <= heap.peek().unwrap().0.bytes {
                    continue;
                }
                heap.pop();
            }
            heap.push(Reverse(LargeFile { bytes, dir, name }));
        }
        if heap.len() == LARGEST {
            self.floor
                .store(heap.peek().unwrap().0.bytes, Ordering::Relaxed);
        }
    }
}

impl Builder {
    /// `root` becomes name id 0, the root's, outside the dedup map so no
    /// directory shares it and a saved scan can drop it.
    pub fn new(root: &[u8]) -> Self {
        let names = boxcar::Vec::new();
        names.push(root.into());
        Self {
            records: Arc::default(),
            names: Arc::new(names),
            ids: Mutex::default(),
            largest: Arc::default(),
        }
    }

    pub fn push(&self, record: Record) -> u32 {
        u32::try_from(self.records.push(record)).expect("over 2^32 directories")
    }

    /// The name id of each name, under one lock.
    pub fn intern<'a>(&self, names: impl Iterator<Item = &'a [u8]>) -> Vec<u32> {
        let mut ids = self.ids.lock().unwrap();
        names
            .map(|name| {
                if let Some(&id) = ids.get(name) {
                    return id;
                }
                // pushed under the lock, so a name is never stored twice
                let id =
                    u32::try_from(self.names.push(name.into())).expect("over 2^32 distinct names");
                ids.insert(name.into(), id);
                id
            })
            .collect()
    }

    pub fn progress(&self) -> Progress {
        Progress {
            records: Arc::clone(&self.records),
            names: Arc::clone(&self.names),
            largest: Arc::clone(&self.largest),
        }
    }

    pub fn finish(self, links: Links, since: Since) -> Tree {
        Tree {
            // a copy, as a `Progress` may still share the records
            records: self.records.iter().map(|(_, r)| *r).collect(),
            names: self.names,
            // taken, as a `Progress` may still share the heap
            largest: std::mem::take(&mut *self.largest.heap.lock().unwrap())
                .into_iter()
                .map(|Reverse(f)| f)
                .collect(),
            links,
            since,
        }
    }
}

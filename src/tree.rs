//! The scanned tree: one [`Record`] per directory, in a flat array where a
//! child's index is always greater than its parent's. Files are not stored;
//! their bytes are folded into their directory's `own`. Only the
//! [`LARGEST`] largest files are kept, by name.

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
    /// [`Record::DENIED`] and [`Record::OTHER_DEVICE`] bits.
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

#[derive(Debug)]
pub struct Tree {
    pub(crate) records: Vec<Record>,
    /// Name id 0 is the root's, never shared with a directory.
    pub(crate) names: Names,
    /// In no particular order.
    pub(crate) largest: Vec<LargeFile>,
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

    pub fn child_index(&self) -> ChildIndex {
        let n = self.records.len();
        let mut start = vec![0u32; n + 1];
        for r in &self.records[1..] {
            start[r.parent as usize + 1] += 1;
        }
        for i in 1..=n {
            start[i] += start[i - 1];
        }
        let mut next = start.clone();
        let mut kids = vec![0u32; n.saturating_sub(1)];
        for (i, r) in self.records.iter().enumerate().skip(1) {
            let slot = &mut next[r.parent as usize];
            kids[*slot as usize] = i as u32;
            *slot += 1;
        }
        ChildIndex { start, kids }
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
    pub largest: Largest,
}

/// Read access to a tree while [`crate::scan_live`] builds it.
#[derive(Clone)]
pub struct Progress {
    records: Arc<boxcar::Vec<Record>>,
    names: Names,
}

impl Progress {
    /// The tree so far, without largest files, or `None` before the root
    /// is listed. Directories not yet listed are not in it yet, so every
    /// size is a lower bound.
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
        (!records.is_empty()).then(|| Tree {
            records,
            names: Arc::clone(&self.names),
            largest: Vec::new(),
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
            largest: Largest::default(),
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
        }
    }

    pub fn finish(self) -> Tree {
        Tree {
            // a copy, as a `Progress` may still share the records
            records: self.records.iter().map(|(_, r)| *r).collect(),
            names: self.names,
            largest: self
                .largest
                .heap
                .into_inner()
                .unwrap()
                .into_iter()
                .map(|Reverse(f)| f)
                .collect(),
        }
    }
}

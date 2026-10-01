//! The scanned tree: one [`Record`] per directory, in a flat array where a
//! child's index is always greater than its parent's. Files are not stored;
//! their bytes are folded into their directory's `own`.

use std::collections::HashMap;
use std::sync::Mutex;

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

#[derive(Debug)]
pub struct Tree {
    records: Vec<Record>,
    names: Vec<Box<[u8]>>,
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

/// The write side of a [`Tree`], shared by all scan threads.
pub(crate) struct Builder {
    records: boxcar::Vec<Record>,
    pub names: Mutex<Names>,
}

#[derive(Default)]
pub(crate) struct Names {
    map: HashMap<Box<[u8]>, u32>,
    names: Vec<Box<[u8]>>,
}

impl Names {
    pub fn intern(&mut self, name: &[u8]) -> u32 {
        if let Some(&id) = self.map.get(name) {
            return id;
        }
        let id = u32::try_from(self.names.len()).expect("over 2^32 distinct names");
        self.names.push(name.into());
        self.map.insert(name.into(), id);
        id
    }
}

impl Builder {
    pub fn new() -> Self {
        Self {
            records: boxcar::Vec::new(),
            names: Mutex::default(),
        }
    }

    pub fn push(&self, record: Record) -> u32 {
        u32::try_from(self.records.push(record)).expect("over 2^32 directories")
    }

    pub fn finish(self) -> Tree {
        Tree {
            records: self.records.into_iter().collect(),
            names: self.names.into_inner().unwrap().names,
        }
    }
}

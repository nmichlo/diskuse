//! What the session's changes are counted against.

use diskuse_core::{FolderId, ReadTree, Tree};
use std::collections::HashMap;
use std::time::SystemTime;

/// The sizes of every dir when the first scan of the session was done, so
/// each dir shows how much it grew or shrank since.
pub(super) struct Baseline {
    /// Total bytes by [`FolderId::index`] in the tree shown. Dirs new
    /// since have no entry, as they come after.
    pub(super) sizes: Vec<u64>,
    pub(super) at: SystemTime,
    /// The tree `sizes` are of, while a full scan, whose ids differ, runs.
    pub(super) of: Option<Tree>,
}

impl Baseline {
    /// The total bytes of dir `id` at the start, 0 if it is new since.
    pub(super) fn size(&self, id: FolderId) -> u64 {
        self.sizes.get(id.index()).copied().unwrap_or(0)
    }

    /// Gives each folder of `new` the start size of the dir at the same
    /// path in `old`, an earlier tree of the same root, or 0 if there is
    /// none. Where `new` numbers its folders as `old` does, those of `old`
    /// keep theirs, and only the ones after them are looked up.
    pub(super) fn carry(&mut self, old: &Tree, new: &Tree) {
        let from = match new.numbered_as(old) {
            true => old.len(),
            false => 0,
        };
        let mut sizes = self.sizes.clone();
        sizes.truncate(from);
        sizes.resize(new.len(), 0);
        // the dir of `old` each folder of `new` from `from` is at
        let mut was: HashMap<FolderId, FolderId> = HashMap::new();
        // children of dirs of `old` by name, built when first needed
        let mut kids: HashMap<FolderId, HashMap<&[u8], FolderId>> = HashMap::new();
        for k in (from..new.len()).filter_map(|i| new.folder(i)) {
            let at = match new.parent(k) {
                None => Some(FolderId::ROOT),
                Some(p) => {
                    let parent = match p.index() >= from {
                        true => was.get(&p).copied(),
                        false => Some(p),
                    };
                    parent.and_then(|p| {
                        let named = kids
                            .entry(p)
                            .or_insert_with(|| old.children(p).map(|c| (old.name(c), c)).collect());
                        named.get(new.name(k)).copied()
                    })
                }
            };
            if let Some(o) = at {
                was.insert(k, o);
                sizes[k.index()] = self.size(o);
            }
        }
        self.sizes = sizes;
    }
}

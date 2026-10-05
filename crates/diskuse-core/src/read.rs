//! Reading a tree: [`FolderId`], and [`ReadTree`], the public face of
//! every tree of this crate.

use crate::tree::{File, Raw, Record};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A folder of a [`crate::Tree`]: got from one ([`FolderId::ROOT`],
/// [`ReadTree::children`], [`ReadTree::find`]), and good for it and for
/// the trees that number their folders as it does, as the ones a
/// [`crate::Live`] reports until folders gone are dropped. Any other tree
/// refuses it ([`ReadTree::contains`]), rather than read another folder.
/// Two ids are equal when they name the same folder of the same numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FolderId {
    index: u32,
    /// Of the tree it is from, or [`ANY`].
    numbering: u32,
}

/// The numbering of an id that names its folder by index alone.
const ANY: u32 = 0;

impl FolderId {
    /// The scanned folder itself: the same id in every tree.
    pub const ROOT: Self = Self::unchecked(0);

    /// The folder at `index` of whichever tree it is given to, if that has
    /// one there: for callers that keep indexes, as a binding to a
    /// language with plain integers does. It gives up the check of which
    /// tree the id is from, and but for the root is not equal to the id a
    /// tree gives for that folder.
    pub const fn unchecked(index: u32) -> Self {
        Self {
            index,
            numbering: ANY,
        }
    }

    /// Where the folder is among its tree's: below [`ReadTree::len`], and
    /// above its parent's. For side tables by folder.
    pub fn index(self) -> usize {
        self.index as usize
    }
}

/// A numbering no tree had before: of a scan, or of a tree whose folders
/// gone were dropped.
pub(crate) fn numbering() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(ANY + 1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The id of the folder at record `index` of `tree`.
fn id_at(tree: &(impl Raw + ?Sized), index: u32) -> FolderId {
    match index {
        0 => FolderId::ROOT,
        _ => FolderId {
            index,
            numbering: tree.numbering(),
        },
    }
}

/// The record of `id` in `tree`.
///
/// # Panics
///
/// If `id` is from a tree that numbers its folders differently, or names
/// a folder gone.
fn at(tree: &(impl Raw + ?Sized), id: FolderId) -> u32 {
    let ours = id.numbering == ANY || id.numbering == tree.numbering();
    assert!(ours, "folder id {} is from another tree", id.index);
    let there = id.index() < Raw::count(tree) && !gone(tree, id.index);
    assert!(there, "no folder {} in this tree", id.index);
    id.index
}

fn gone(tree: &(impl Raw + ?Sized), index: u32) -> bool {
    tree.record(index).flags & Record::REMOVED != 0
}

/// Read access to a tree: an owned [`crate::Tree`], or a saved one read in
/// place ([`crate::SavedTree`]), so `show` prints a saved scan without
/// building a tree. Folders are [`FolderId`]s, from [`FolderId::ROOT`]
/// down.
///
/// # Panics
///
/// Every method given a [`FolderId`] panics if the tree does not
/// [`contain`](ReadTree::contains) it, as indexing a slice out of range
/// does: an id from a tree that numbers its folders differently, or of a
/// folder gone. [`ReadTree::find`] carries a folder from one tree to
/// another, by path.
pub trait ReadTree: Raw {
    /// One more than the largest [`FolderId::index`]: folders gone since
    /// the scan still count, until they are dropped.
    fn len(&self) -> usize {
        Raw::count(self)
    }

    fn is_empty(&self) -> bool {
        Raw::count(self) == 0
    }

    /// Whether `id` is a folder of this tree.
    fn contains(&self, id: FolderId) -> bool {
        let ours = id.numbering == ANY || id.numbering == self.numbering();
        ours && id.index() < Raw::count(self) && !gone(self, id.index)
    }

    /// Whether this tree numbers its folders as `other` does: a
    /// [`FolderId`] of one is then good for the other, if the folder is in
    /// both, and the folders only one has come after all of the other's.
    /// So for a tree and the one it was before changes were applied,
    /// until folders gone are dropped; not for another scan.
    fn numbered_as(&self, other: &impl ReadTree) -> bool {
        self.numbering() == other.numbering()
    }

    /// The folder at `index` ([`FolderId::index`]), if there is one.
    fn folder(&self, index: usize) -> Option<FolderId> {
        let id = id_at(self, u32::try_from(index).ok()?);
        self.contains(id).then_some(id)
    }

    /// Every folder, each after the one it is in.
    fn ids(&self) -> impl Iterator<Item = FolderId> {
        (0..Raw::count(self)).filter_map(|i| self.folder(i))
    }

    /// The name of folder `id`; for the root, the scanned path as given.
    fn name(&self, id: FolderId) -> &[u8] {
        Raw::name_at(self, at(self, id))
    }

    /// The path of folder `id`: the root's, then each name below it.
    fn path(&self, id: FolderId) -> PathBuf {
        Raw::path_at(self, at(self, id))
    }

    /// The path of folder `id` below the root: empty for the root.
    fn relative(&self, id: FolderId) -> PathBuf {
        Raw::relative_at(self, at(self, id))
    }

    /// The folder `id` is in, or `None` for the root.
    fn parent(&self, id: FolderId) -> Option<FolderId> {
        let index = at(self, id);
        (index != 0).then(|| id_at(self, self.record(index).parent))
    }

    /// Allocated bytes of folder `id` and everything below it.
    fn size(&self, id: FolderId) -> u64 {
        Raw::size_at(self, at(self, id))
    }

    /// Allocated bytes of folder `id` itself and its files.
    fn own(&self, id: FolderId) -> u64 {
        Raw::own_at(self, at(self, id))
    }

    /// Of [`ReadTree::size`], the bytes deleting the folder frees: those
    /// not shared with a clone elsewhere. 0 unless scanned with
    /// reclaimable sizes on macOS.
    fn reclaimable(&self, id: FolderId) -> u64 {
        Raw::reclaimable_at(self, at(self, id))
    }

    /// Why folder `id` could not be read (`EACCES`, `EPERM`, `errno N`).
    fn error(&self, id: FolderId) -> Option<String> {
        Raw::error_at(self, at(self, id))
    }

    /// Something below folder `id` could not be read, so its size is a
    /// lower bound.
    fn partial(&self, id: FolderId) -> bool {
        Raw::partial_at(self, at(self, id))
    }

    /// Folder `id` is a mount point, not scanned into (`du -x`).
    fn other_device(&self, id: FolderId) -> bool {
        Raw::other_device_at(self, at(self, id))
    }

    /// The subfolders of `id`, in no particular order.
    fn children(&self, id: FolderId) -> impl ExactSizeIterator<Item = FolderId> + '_ {
        // never the root, so the numbering is read once
        let numbering = self.numbering();
        (Raw::children_at(self, at(self, id)).iter())
            .map(move |&index| FolderId { index, numbering })
    }

    /// The folder at `path`: relative to the root, or absolute and below
    /// it. `.` parts are skipped.
    fn find(&self, path: &Path) -> Option<FolderId> {
        Raw::find_index(self, path).map(|index| id_at(self, index))
    }

    /// The files of folder `id`, listed from disk now, as the tree keeps
    /// only folder totals: each one's allocated bytes, and with
    /// `reclaimable` of those the ones not shared with a clone (macOS),
    /// else 0. None for a folder on another device, which the scan did not
    /// go into.
    fn files(&self, id: FolderId, reclaimable: bool) -> std::io::Result<Vec<File>> {
        Raw::files_at(self, at(self, id), reclaimable)
    }

    /// The `n` largest files, of the [`crate::LARGEST`] kept, as
    /// `(path, bytes)`, largest first, ties by path.
    fn largest_files(&self, n: usize) -> Vec<(PathBuf, u64)> {
        Raw::largest_paths(self, n)
    }

    /// The scan was stopped before it listed every dir, so sizes are lower
    /// bounds.
    fn stopped(&self) -> bool {
        Raw::is_stopped(self)
    }
}

impl<T: Raw + ?Sized> ReadTree for T {}

impl FolderId {
    /// The record index of `id` in `tree`, if it is a folder of it: for
    /// this crate, which works by index.
    pub(crate) fn of(self, tree: &(impl Raw + ?Sized)) -> Option<u32> {
        ReadTree::contains(tree, self).then_some(self.index)
    }
}

//! `FolderId`: good for the tree it is from, refused by another.

#![allow(clippy::disallowed_methods)] // fixtures create files

use diskuse_core::{FolderId, ReadTree, ScanOptions, scan};
use std::path::Path;

/// ```text
/// root/
///   a/x/
///   b/
/// ```
///
/// scanned twice, on one thread so both list `a` before `b`.
fn two_scans() -> (tempfile::TempDir, diskuse_core::Tree, diskuse_core::Tree) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a/x")).unwrap();
    std::fs::create_dir(dir.path().join("b")).unwrap();
    let mut opts = ScanOptions::default();
    opts.threads = std::num::NonZeroUsize::new(1);
    let scan = || scan(dir.path(), &opts).unwrap();
    let (first, second) = (scan(), scan());
    (dir, first, second)
}

#[test]
fn an_id_is_good_for_its_own_tree_and_carried_to_another_by_path() {
    let (dir, first, second) = two_scans();
    let x = first.find(Path::new("a/x")).unwrap();
    assert_eq!(first.path(x), dir.path().join("a/x"));
    assert_eq!(
        (first.contains(x), second.contains(x)),
        (true, false),
        "another scan numbers its folders apart"
    );
    assert!(!first.numbered_as(&second));

    // by path, the folder is found in the other tree
    let there = second.find(&first.relative(x)).unwrap();
    assert_eq!(second.path(there), first.path(x));
    assert_ne!(there, x);

    // the root is one id in every tree
    assert_eq!(first.parent(first.parent(x).unwrap()), Some(FolderId::ROOT));
    assert_eq!(second.path(FolderId::ROOT), dir.path());

    // an index alone names whatever folder is there, in any tree
    let by_index = FolderId::unchecked(x.index() as u32);
    assert_eq!(
        second.path(by_index),
        second.path(second.folder(x.index()).unwrap())
    );
    assert!(!second.contains(FolderId::unchecked(second.len() as u32)));
}

#[test]
#[should_panic(expected = "is from another tree")]
fn reading_an_id_of_another_tree_panics() {
    let (_dir, first, second) = two_scans();
    second.size(first.find(Path::new("a/x")).unwrap());
}

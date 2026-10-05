//! `Tree::save` and `Tree::load`: a scan comes back as it was.

#![allow(clippy::disallowed_methods)] // fixtures create files

mod common;

use common::folders;
use diskuse_core::{LARGEST, ReadTree, ScanOptions, Tree, scan};
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;

/// ```text
/// root/
///   a/f      4096
///   a/b/g    8192
///   c/
/// ```
#[test]
fn a_saved_scan_loads_as_it_was() {
    let dir = common::tempdir();
    // a loaded tree names its root by its real path
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::create_dir(root.join("c")).unwrap();
    common::file(&root.join("a/f"), 4096);
    common::file(&root.join("a/b/g"), 8192);
    let tree = scan(&root, &ScanOptions::default()).unwrap();

    let out = tempfile::tempdir().unwrap();
    let file = out.path().join("scan");
    tree.save(&file).unwrap();
    let loaded = Tree::load(&file).unwrap();
    assert_eq!(
        (
            folders(&loaded),
            loaded.largest_files(LARGEST),
            loaded.stopped()
        ),
        (folders(&tree), tree.largest_files(LARGEST), false)
    );
    // it lists folder names, so only the owner reads it
    let mode = std::fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    std::fs::write(&file, b"not a scan").unwrap();
    let error = Tree::load(&file).map(|_| ()).map_err(|e| e.kind());
    assert_eq!(error, Err(ErrorKind::InvalidData));
}

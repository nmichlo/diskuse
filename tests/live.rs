//! `Live`: rescans keep the tree equal to a fresh scan, and its size bounded.

#![allow(clippy::disallowed_methods)] // fixtures create files

mod common;

use diskuse::{Event, LiveOptions, ReadTree, ScanOptions, Tree, live, scan};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Every folder by path, with its size and own bytes, sorted.
fn folders(t: &Tree) -> Vec<(PathBuf, u64, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![0];
    while let Some(k) = stack.pop() {
        out.push((t.path(k), t.size(k), t.own(k)));
        stack.extend_from_slice(t.children(k));
    }
    out.sort();
    out
}

fn next(live: &mut diskuse::Live) -> Event {
    live.wait(Duration::from_secs(30))
        .expect("an event")
        .expect("no error")
}

#[test]
fn rescans_keep_the_tree_right_and_its_records_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    for k in 0..10 {
        std::fs::create_dir_all(a.join(format!("{k}/x"))).unwrap();
    }
    common::file(&a.join("0/x/f"), 4096);
    let fresh = |p: &Path| folders(&scan(p, &ScanOptions::default()).unwrap());

    let opts = LiveOptions {
        interval: Duration::from_millis(50),
        ..LiveOptions::default()
    };
    let mut live = live(dir.path(), opts);
    let mut tree = loop {
        if let Event::Ready(t) = next(&mut live) {
            break t;
        }
    };
    let n = folders(&tree).len();
    for round in 0..6 {
        // each rescan replaces the 20 folders below `a`
        live.rescan(tree.find(Path::new("a")).unwrap());
        tree = loop {
            if let Event::Changed(t, _) = next(&mut live) {
                break t;
            }
        };
        assert_eq!(folders(&tree), fresh(dir.path()), "round {round}");
        assert!(
            tree.len() <= 2 * n,
            "round {round}: {} records for {n} folders",
            tree.len()
        );
    }
}

/// A file that grows in place changes no listing: the change is its
/// folder's own bytes.
#[test]
fn a_file_growing_in_place_is_a_change_of_its_folder() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    std::fs::create_dir(&a).unwrap();
    common::file(&a.join("keep"), 4096);
    let opts = LiveOptions {
        interval: Duration::from_millis(50),
        ..LiveOptions::default()
    };
    let mut live = live(dir.path(), opts);
    loop {
        if let Event::Ready(_) = next(&mut live) {
            break;
        }
    }
    // what was allocated, which may be more than was written
    let grown = common::append(&a.join("keep"), 4096) - 4096;
    let changes = loop {
        if let Event::Changed(_, changes) = next(&mut live) {
            break changes;
        }
    };
    let a = std::fs::canonicalize(&a).unwrap();
    let changes: Vec<_> = (changes.into_iter())
        .map(|(path, by)| (std::fs::canonicalize(path).unwrap(), by))
        .collect();
    assert_eq!(changes, [(a, grown as i64)]);
}

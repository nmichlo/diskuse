//! `Live`: rescans keep the tree equal to a fresh scan, and its size bounded.

#![allow(clippy::disallowed_methods)] // fixtures create files

mod common;

use common::folders;
use diskuse_core::{Event, LiveOptions, ReadTree, ScanError, ScanOptions, live, scan};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

type Events = Receiver<Result<Event, ScanError>>;

fn next(events: &Events) -> Event {
    let event = events.recv_timeout(Duration::from_secs(30));
    event.expect("an event").expect("no error")
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

    let mut opts = LiveOptions::default();
    opts.interval = Duration::from_millis(50);
    let (tx, events) = mpsc::channel();
    let live = live(dir.path(), opts, tx);
    let mut tree = loop {
        if let Event::Ready(t) = next(&events) {
            break t;
        }
    };
    let n = folders(&tree).len();
    for round in 0..6 {
        // each rescan replaces the 20 folders below `a`
        live.rescan(tree.find(Path::new("a")).unwrap());
        tree = loop {
            if let Event::Changed(t, _) = next(&events) {
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
    let mut opts = LiveOptions::default();
    opts.interval = Duration::from_millis(50);
    let (tx, events) = mpsc::channel();
    let _live = live(dir.path(), opts, tx);
    loop {
        if let Event::Ready(_) = next(&events) {
            break;
        }
    }
    // what was allocated, which may be more than was written
    let grown = common::append(&a.join("keep"), 4096) - 4096;
    let changes = loop {
        if let Event::Changed(_, changes) = next(&events) {
            break changes;
        }
    };
    let a = std::fs::canonicalize(&a).unwrap();
    let changes: Vec<_> = (changes.into_iter())
        .map(|(path, by)| (std::fs::canonicalize(path).unwrap(), by))
        .collect();
    assert_eq!(changes, [(a, grown as i64)]);
}

/// `relist` lists a folder again when asked, as after missed changes.
/// With only the folders given to `follow` followed, and none given, that
/// is the one way a change shows (Linux; macOS follows everything anyway).
#[test]
fn relist_lists_a_folder_again() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    std::fs::create_dir(&a).unwrap();
    common::file(&a.join("f"), 4096);
    let mut opts = LiveOptions::default();
    opts.interval = Duration::from_millis(50);
    opts.shown_only = true;
    let (tx, events) = mpsc::channel();
    let live = live(dir.path(), opts, tx);
    let tree = loop {
        if let Event::Ready(t) = next(&events) {
            break t;
        }
    };
    common::file(&a.join("g"), 8192);
    live.relist(&[tree.find(Path::new("a")).unwrap()]);
    let changes = loop {
        if let Event::Changed(_, changes) = next(&events) {
            break changes;
        }
    };
    let changes: Vec<_> = (changes.into_iter())
        .map(|(path, by)| (std::fs::canonicalize(path).unwrap(), by))
        .collect();
    assert_eq!(changes, [(std::fs::canonicalize(&a).unwrap(), 8192)]);
}

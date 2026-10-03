//! A scan that keeps going: snapshots while it scans, the tree once it is
//! done, then the changes on disk as they happen, with no UI. The Python
//! API's `diskuse.live` is this.
//!
//! Only changes the OS reports live are applied, as in the browser
//! ([`crate::watch`]); when it drops some, the tree is scanned again. On
//! Linux the OS keeps no such record of a whole tree, so after the scan no
//! changes are reported.

use crate::ReadTree;
use crate::scan::{ScanError, ScanOptions, Stop, scan_live};
use crate::tree::{Progress, Record, Tree};
use crate::watch::{Poll, Watch};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// What [`Live::next`] reports.
pub enum Event {
    /// The tree scanned so far: every size is a lower bound.
    Scanning(Tree),
    /// The scan is done.
    Ready(Tree),
    /// The disk changed: the tree now, and each folder whose own bytes,
    /// or whole subtree when it appeared or went, changed, by how many
    /// bytes.
    Changed(Tree, Vec<(PathBuf, i64)>),
    /// Changes were lost, for this reason, so the tree is scanned again.
    Rescanning(&'static str),
}

enum State {
    Scanning {
        thread: JoinHandle<Result<Tree, ScanError>>,
        started: mpsc::Receiver<Progress>,
        progress: Option<Progress>,
    },
    Watching {
        tree: Box<Tree>,
        watch: Option<Watch>,
    },
    Done,
}

/// A live scan of a folder. Dropping it stops the scan.
pub struct Live {
    root: PathBuf,
    threads: Option<NonZeroUsize>,
    stop: Arc<AtomicBool>,
    state: State,
}

/// How often a running scan or the watch is checked within one `next`.
const CHECK: Duration = Duration::from_millis(20);

impl Live {
    /// Starts scanning `root` with `threads` threads, or as many as help.
    pub fn start(root: &Path, threads: Option<NonZeroUsize>) -> Self {
        let mut live = Self {
            root: root.into(),
            threads,
            stop: Arc::default(),
            state: State::Done,
        };
        live.scan();
        live
    }

    fn options(&self) -> ScanOptions {
        let stop = Arc::clone(&self.stop);
        ScanOptions {
            threads: self.threads,
            stop: Stop::new(move || stop.load(Ordering::Relaxed)),
            ..ScanOptions::default()
        }
    }

    fn scan(&mut self) {
        let (tx, started) = mpsc::channel();
        let (root, opts) = (self.root.clone(), self.options());
        let thread = thread::spawn(move || {
            // the receiver only goes away with the `Live`
            scan_live(&root, &opts, |p| {
                let _ = tx.send(p);
            })
        });
        self.state = State::Scanning {
            thread,
            started,
            progress: None,
        };
    }

    /// The next event, waiting at most `interval`: a snapshot every
    /// `interval` while scanning, `Ready` as soon as the scan is done,
    /// then the changes of each `interval` merged into one `Changed`.
    /// `None` if nothing happened, or after an error.
    pub fn next(&mut self, interval: Duration) -> Result<Option<Event>, ScanError> {
        let until = Instant::now() + interval;
        let opts = self.options();
        let mut changes: HashMap<PathBuf, i64> = HashMap::new();
        loop {
            match &mut self.state {
                State::Done => return Ok(None),
                State::Scanning {
                    thread,
                    started,
                    progress,
                } => {
                    if thread.is_finished() {
                        let State::Scanning { thread, .. } =
                            std::mem::replace(&mut self.state, State::Done)
                        else {
                            unreachable!()
                        };
                        let tree = thread.join().expect("the scan thread panicked")?;
                        let watch = Watch::start(&self.root, &tree, None);
                        let ready = tree.clone();
                        let tree = Box::new(tree);
                        self.state = State::Watching { tree, watch };
                        return Ok(Some(Event::Ready(ready)));
                    }
                    if progress.is_none() {
                        *progress = started.try_recv().ok();
                    }
                    if Instant::now() >= until {
                        let tree = progress.as_ref().and_then(Progress::snapshot);
                        return Ok(tree.map(Event::Scanning));
                    }
                }
                State::Watching { tree, watch } => {
                    let Some(watch) = watch else {
                        thread::sleep(until.saturating_duration_since(Instant::now()));
                        return Ok(None);
                    };
                    match watch.poll() {
                        Poll::Wait => {}
                        Poll::Lost(why) => {
                            self.scan();
                            return Ok(Some(Event::Rescanning(why)));
                        }
                        Poll::Changes(c) if c.is_empty() => {}
                        Poll::Changes(c) => {
                            let before = tree.clone();
                            if tree.apply(&c, &opts).is_none() {
                                self.scan();
                                return Ok(Some(Event::Rescanning(
                                    "macOS asked to scan all of it again",
                                )));
                            }
                            for (path, delta) in deltas(&before, tree) {
                                *changes.entry(path).or_default() += delta;
                            }
                        }
                    }
                    if Instant::now() >= until {
                        changes.retain(|_, d| *d != 0);
                        if changes.is_empty() {
                            return Ok(None);
                        }
                        let mut changes: Vec<_> = changes.into_iter().collect();
                        changes.sort();
                        return Ok(Some(Event::Changed((**tree).clone(), changes)));
                    }
                }
            }
            thread::sleep(CHECK.min(until.saturating_duration_since(Instant::now())));
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let State::Scanning { thread, .. } = std::mem::replace(&mut self.state, State::Done) {
            let _ = thread.join();
        }
    }
}

/// What changed from `before` to `after`, the same tree with changes
/// applied: each folder whose own bytes changed, and each folder that
/// appeared or went, with its whole size.
fn deltas(before: &Tree, after: &Tree) -> Vec<(PathBuf, i64)> {
    let (old, new) = (before.totals().size, after.totals().size);
    let gone = |t: &Tree, id: u32| t.record(id).flags & Record::REMOVED != 0;
    let path = |t: &Tree, id: u32| PathBuf::from(std::ffi::OsString::from_vec(t.dir_path(id)));
    let mut out = Vec::new();
    for id in 0..after.len() as u32 {
        let r = after.record(id);
        if (id as usize) < before.len() {
            let was_gone = gone(before, id);
            let is_gone = gone(after, id);
            let parent_gone = id != 0 && gone(after, r.parent);
            if is_gone && !was_gone && !parent_gone {
                out.push((path(before, id), -(old[id as usize] as i64)));
            } else if !is_gone {
                let delta = r.own as i64 - before.record(id).own as i64;
                if delta != 0 {
                    out.push((path(after, id), delta));
                }
            }
        } else if !gone(after, id) && (r.parent as usize) < before.len() {
            out.push((path(after, id), new[id as usize] as i64));
        }
    }
    out
}

//! A scan that keeps going: snapshots while it scans, the tree once it is
//! done, then the changes on disk as they happen, with no UI. The browser
//! and the Python API's `diskuse.live` both run on it.
//!
//! Only changes the OS reports live are applied ([`crate::watch`]); when
//! it drops some, the tree is scanned again. Where the OS keeps no record
//! of a whole tree (Linux), only the folders given to [`Live::follow`] are
//! followed.

use crate::scan::{ScanError, ScanOptions, Stop, scan_live};
use crate::tree::{Progress, ReadTree, Record, Tree, below};
use crate::watch::{Changes, DirWatch, Poll, Watch};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// What a [`Live`] reports.
#[derive(Debug)]
pub enum Event {
    /// The tree scanned so far: every size is a lower bound.
    Scanning(Tree),
    /// The scan is done.
    Ready(Tree),
    /// The tree changed, on disk or by [`Live::rescan`]: the tree now, and
    /// each folder whose own bytes changed, or whole subtree appeared or
    /// went, with by how many bytes, by path. Empty if only folders of 0
    /// bytes did.
    Changed(Tree, Vec<(PathBuf, i64)>),
    /// Changes were lost, for this reason, so the tree is scanned again.
    Rescanning(&'static str),
}

/// How a [`Live`] scans and follows.
#[derive(Clone, Debug)]
pub struct LiveOptions {
    /// Its `stop` is replaced: a [`Live`] stops its own scans.
    pub scan: ScanOptions,
    /// How often a snapshot is taken while scanning, and the disk is
    /// checked for changes after.
    pub interval: Duration,
    /// Changes below this path are not followed, as where the scans are
    /// saved.
    pub ignore: Option<PathBuf>,
    /// On Linux, watch the followed folders with inotify. Off, they are
    /// listed again on a timer, as where inotify cannot watch them.
    pub inotify: bool,
}

impl Default for LiveOptions {
    fn default() -> Self {
        Self {
            scan: ScanOptions::default(),
            interval: Duration::from_millis(500),
            ignore: None,
            inotify: true,
        }
    }
}

/// A scan on its own thread: a full scan, with snapshots, or a folder
/// listed again, without.
struct Job {
    thread: JoinHandle<Result<Tree, ScanError>>,
    started: mpsc::Receiver<Progress>,
    progress: Option<Progress>,
}

enum Follow {
    /// Every change below the root.
    All(Watch),
    /// Those in the folders given to [`Live::follow`].
    Shown(DirWatch),
}

enum State {
    Scanning(Job),
    Following {
        tree: Box<Tree>,
        follow: Box<Follow>,
        /// A [`Live::rescan`] of one folder; changes wait meanwhile.
        rescan: Option<Job>,
    },
    Done,
}

/// A live scan of a folder: see [`live`]. Dropping it stops the scan.
pub struct Live {
    root: PathBuf,
    opts: LiveOptions,
    stop: Arc<AtomicBool>,
    state: State,
    /// When the next snapshot or check for changes is due.
    due: Instant,
}

/// How often a running scan is checked for being done, within
/// [`Live::wait`].
const CHECK: Duration = Duration::from_millis(20);

/// Scans `root` and keeps following it: the events come from
/// [`Live::wait`], or from iterating, which waits as long as it takes.
pub fn live(root: &Path, opts: LiveOptions) -> Live {
    let mut live = Live {
        root: root.into(),
        opts,
        stop: Arc::default(),
        state: State::Done,
        due: Instant::now(),
    };
    live.start();
    live
}

impl Live {
    fn options(&self) -> ScanOptions {
        let stop = Arc::clone(&self.stop);
        ScanOptions {
            stop: Stop::new(move || stop.load(Ordering::Relaxed)),
            ..self.opts.scan.clone()
        }
    }

    fn start(&mut self) {
        let (tx, started) = mpsc::channel();
        let (root, opts) = (self.root.clone(), self.options());
        let thread = thread::spawn(move || {
            // the receiver only goes away with the `Live`
            scan_live(&root, &opts, |p| {
                let _ = tx.send(p);
            })
        });
        self.state = State::Scanning(Job {
            thread,
            started,
            progress: None,
        });
    }

    /// The next event within `timeout`, or `None`. A snapshot comes at
    /// most every [`LiveOptions::interval`] while scanning, `Ready` as
    /// soon as the scan is done, then the changes of each interval as one
    /// `Changed`. After an error the tree is followed as before, unless
    /// the scan itself failed: then [`Live::is_done`].
    pub fn wait(&mut self, timeout: Duration) -> Option<Result<Event, ScanError>> {
        let until = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            let due = now >= self.due;
            if due {
                self.due = now + self.opts.interval;
            }
            if let Some(event) = self.step(due) {
                return Some(event);
            }
            let now = Instant::now();
            if self.is_done() || now >= until {
                return None;
            }
            let next = self.due.min(until).min(now + CHECK);
            thread::sleep(next.saturating_duration_since(now));
        }
    }

    fn step(&mut self, due: bool) -> Option<Result<Event, ScanError>> {
        match &mut self.state {
            State::Done => None,
            State::Scanning(job) => {
                if job.thread.is_finished() {
                    let State::Scanning(job) = std::mem::replace(&mut self.state, State::Done)
                    else {
                        unreachable!()
                    };
                    return Some(self.finish(job));
                }
                if job.progress.is_none() {
                    job.progress = job.started.try_recv().ok();
                }
                let snapshot = due.then(|| job.progress.as_ref()?.snapshot()).flatten();
                snapshot.map(|tree| Ok(Event::Scanning(tree)))
            }
            State::Following {
                tree,
                rescan: rescan @ Some(_),
                ..
            } => {
                if !rescan.as_ref().unwrap().thread.is_finished() {
                    return None;
                }
                let done = join(rescan.take().unwrap());
                Some(done.map(|new| {
                    let changes = deltas(tree, &new).unwrap_or_default();
                    **tree = new;
                    Event::Changed(shared(tree), changes)
                }))
            }
            State::Following { .. } if !due => None,
            State::Following {
                tree,
                follow,
                rescan: None,
            } => {
                let poll = match &mut **follow {
                    Follow::All(watch) => watch.poll(),
                    Follow::Shown(dirs) => dirs.poll(tree),
                };
                let changes = match poll {
                    Poll::Wait => return None,
                    Poll::Changes(c) if c.is_empty() => return None,
                    Poll::Changes(c) => c,
                    Poll::Lost(why) => {
                        self.start();
                        return Some(Ok(Event::Rescanning(why)));
                    }
                };
                let started = Instant::now();
                let before = shared(tree);
                let opts = self.options();
                let State::Following { tree, follow, .. } = &mut self.state else {
                    unreachable!()
                };
                if tree.apply(&changes, &opts).is_none() {
                    self.start();
                    return Some(Ok(Event::Rescanning("macOS asked to scan all of it again")));
                }
                if let Follow::Shown(dirs) = &mut **follow {
                    dirs.listed(started.elapsed());
                }
                let changes = deltas(&before, tree)?;
                Some(Ok(Event::Changed(shared(tree), changes)))
            }
        }
    }

    /// `Ready`, and the tree followed from then, or the scan's error.
    fn finish(&mut self, job: Job) -> Result<Event, ScanError> {
        let tree = join(job)?;
        let ignore = self.opts.ignore.as_deref();
        let follow = Box::new(match Watch::start(&self.root, &tree, ignore) {
            Some(watch) => Follow::All(watch),
            None => Follow::Shown(DirWatch::new(self.opts.inotify)),
        });
        let event = Event::Ready(shared(&tree));
        self.state = State::Following {
            tree: Box::new(tree),
            follow,
            rescan: None,
        };
        Ok(event)
    }

    /// Where the OS keeps no record of a whole tree, follows only the
    /// folders `dirs` of the latest tree, from now on. Ignored otherwise.
    pub fn follow(&mut self, dirs: &[u32]) {
        if let State::Following { tree, follow, .. } = &mut self.state
            && let Follow::Shown(watch) = &mut **follow
        {
            watch.show(tree, dirs);
        }
    }

    /// Scans folder `dir` of the latest tree again on another thread,
    /// then reports it as `Changed`; the root is scanned all over, with
    /// snapshots. Ignored while a scan runs.
    pub fn rescan(&mut self, dir: u32) {
        let opts = self.options();
        let State::Following {
            tree,
            rescan: rescan @ None,
            ..
        } = &mut self.state
        else {
            return;
        };
        if dir == 0 {
            return self.start();
        }
        let changes = Changes {
            rescan: vec![below(&**tree, dir).into()],
            ..Changes::default()
        };
        let mut tree = (**tree).clone();
        let (_, started) = mpsc::channel();
        let thread = thread::spawn(move || match tree.apply(&changes, &opts) {
            Some(()) => Ok(tree),
            None => Err(ScanError::Root(io::ErrorKind::NotFound.into())),
        });
        *rescan = Some(Job {
            thread,
            started,
            progress: None,
        });
    }

    /// A scan runs: the first, a [`Live::rescan`], or after lost changes.
    pub fn busy(&self) -> bool {
        match &self.state {
            State::Scanning(_) => true,
            State::Following { rescan, .. } => rescan.is_some(),
            State::Done => false,
        }
    }

    /// Every change below the root is followed, not only those in the
    /// folders given to [`Live::follow`].
    pub fn follows_all(&self) -> bool {
        match &self.state {
            State::Following { follow, .. } => matches!(**follow, Follow::All(_)),
            _ => false,
        }
    }

    /// The scan failed, so no event comes again.
    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// Stops a running scan, and returns what it found, if it was one.
    pub fn stop(mut self) -> Option<Tree> {
        self.stop.store(true, Ordering::Relaxed);
        match std::mem::replace(&mut self.state, State::Done) {
            State::Scanning(job)
            | State::Following {
                rescan: Some(job), ..
            } => join(job).ok(),
            _ => None,
        }
    }
}

impl Iterator for Live {
    type Item = Result<Event, ScanError>;

    /// The next event, however long it takes; `None` once
    /// [`Live::is_done`].
    fn next(&mut self) -> Option<Self::Item> {
        while !self.is_done() {
            if let Some(event) = self.wait(Duration::from_secs(1)) {
                return Some(event);
            }
        }
        None
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        match std::mem::replace(&mut self.state, State::Done) {
            State::Scanning(job)
            | State::Following {
                rescan: Some(job), ..
            } => drop(join(job)),
            _ => {}
        }
    }
}

fn join(job: Job) -> Result<Tree, ScanError> {
    match job.thread.join() {
        Ok(done) => done,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// A copy of `tree` for an event: its sizes and children, derived first
/// so the copy has them too, but not what only updates need.
fn shared(tree: &Tree) -> Tree {
    tree.size(0);
    let mut copy = tree.clone();
    copy.links.clear();
    copy
}

/// What changed from `before` to `after`, the same tree with changes
/// applied: each folder whose own bytes changed, and each folder that
/// appeared or went, with its whole size, by path. `None` if nothing did,
/// not even a folder of 0 bytes.
fn deltas(before: &Tree, after: &Tree) -> Option<Vec<(PathBuf, i64)>> {
    let gone = |t: &Tree, id: u32| t.record(id).flags & Record::REMOVED != 0;
    let mut out = Vec::new();
    for id in 0..after.len() as u32 {
        let r = after.record(id);
        if (id as usize) < before.len() {
            let was_gone = gone(before, id);
            let is_gone = gone(after, id);
            let parent_gone = id != 0 && gone(after, r.parent);
            if is_gone && !was_gone && !parent_gone {
                out.push((before.path(id), -(before.size(id) as i64)));
            } else if !is_gone && r.own != before.own(id) {
                out.push((after.path(id), r.own as i64 - before.own(id) as i64));
            }
        } else if !gone(after, id) && (r.parent as usize) < before.len() {
            out.push((after.path(id), after.size(id) as i64));
        }
    }
    if out.is_empty() {
        return None;
    }
    // a folder listed afresh goes and comes back, at one path
    out.sort();
    out.dedup_by(|b, a| {
        let same = a.0 == b.0;
        if same {
            a.1 += b.1;
        }
        same
    });
    out.retain(|&(_, d)| d != 0);
    Some(out)
}

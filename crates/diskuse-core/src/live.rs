//! A scan that keeps going: snapshots while it scans, the tree once it is
//! done, then the changes on disk as they happen, with no UI. The browser
//! and the Python API's `diskuse.live` both run on it.
//!
//! Only changes the OS reports live are applied ([`crate::watch`]); when
//! it drops some, the tree is scanned again. Where the OS keeps no record
//! of a whole tree (Linux), only the folders given to [`Live::follow`] are
//! followed.

use crate::scan::{Applied, ApplyError, ScanError, ScanOptions, Stop, scan_live};
use crate::tree::{Progress, ReadTree, Tree, below};
use crate::watch::{Changes, DirWatch, Poll, TreeWatch, Watch};
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
    /// Changes were lost, so the tree is scanned again.
    Rescanning(Reason),
}

/// Why a [`Live`] scans the whole tree again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The OS dropped change events.
    Dropped,
    /// macOS's change ids wrapped around.
    IdsWrapped,
    /// The scanned folder itself moved.
    RootMoved,
    /// macOS did not replay the changes made while scanning within 30 s.
    NoReplay,
    /// macOS asked for the whole tree to be scanned again.
    MustScanAll,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dropped => "the OS dropped change events",
            Self::IdsWrapped => "macOS event ids wrapped",
            Self::RootMoved => "the scanned dir moved",
            Self::NoReplay => "macOS did not replay the changes made while scanning within 30 s",
            Self::MustScanAll => "macOS asked to scan all of it again",
        })
    }
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
    /// On Linux, follow only the folders given to [`Live::follow`], as a
    /// browser showing a few at a time does, not every folder.
    pub shown_only: bool,
}

impl Default for LiveOptions {
    fn default() -> Self {
        Self {
            scan: ScanOptions::default(),
            interval: Duration::from_millis(500),
            ignore: None,
            inotify: true,
            shown_only: false,
        }
    }
}

/// A scan on its own thread: a full scan, with snapshots, or a folder
/// listed again, without.
struct Job {
    /// The tree, and for a rescan what changed.
    thread: JoinHandle<Result<(Tree, Applied), ApplyError>>,
    progress: Option<Progress>,
    /// The thread is done, so joining it does not block.
    exited: bool,
}

/// What wakes [`Live::wait`] before its deadline.
enum Wake {
    /// The running scan's progress, for snapshots.
    Started(Progress),
    /// The running scan's thread is done.
    Exited,
    /// From a [`Waker`].
    Interrupt,
}

/// Sends [`Wake::Exited`] when its scan thread is done, even by a panic.
struct Exit(mpsc::Sender<Wake>);

impl Drop for Exit {
    fn drop(&mut self) {
        // the receiver only goes away with the `Live`
        let _ = self.0.send(Wake::Exited);
    }
}

/// Wakes a [`Live::wait`] on another thread, which then returns `None`:
/// to cancel it, or to take its lock sooner. A wake while none waits makes
/// the next return at once.
#[derive(Clone)]
pub struct Waker(mpsc::Sender<Wake>);

impl Waker {
    pub fn wake(&self) {
        let _ = self.0.send(Wake::Interrupt);
    }
}

enum Follow {
    /// Every change below the root, from the OS's record of them (macOS).
    All(Watch),
    /// Every folder, each watched (Linux).
    Every(TreeWatch),
    /// Those in the folders given to [`Live::follow`].
    Shown(DirWatch),
}

impl Follow {
    /// Follows `tree` after [`Tree::apply`]: falls back to following only
    /// the folders given to [`Live::follow`] once watches run out.
    fn applied(&mut self, tree: &Tree, applied: &Applied, inotify: bool) {
        if let Follow::Every(watch) = self
            && !watch.applied(tree, applied)
        {
            *self = Follow::Shown(DirWatch::new(inotify));
        }
    }
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
    tx: mpsc::Sender<Wake>,
    rx: mpsc::Receiver<Wake>,
}

/// Scans `root` and keeps following it: the events come from
/// [`Live::wait`], or from iterating, which waits as long as it takes.
pub fn live(root: &Path, opts: LiveOptions) -> Live {
    let (tx, rx) = mpsc::channel();
    let mut live = Live {
        root: root.into(),
        opts,
        stop: Arc::default(),
        state: State::Done,
        due: Instant::now(),
        tx,
        rx,
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

    /// Runs `scan` on its own thread, which wakes [`Live::wait`] with its
    /// progress and once done.
    fn spawn(
        &self,
        scan: impl FnOnce(&dyn Fn(Progress)) -> Result<(Tree, Applied), ApplyError> + Send + 'static,
    ) -> Job {
        let tx = self.tx.clone();
        let thread = thread::spawn(move || {
            let exit = Exit(tx);
            scan(&|p| {
                let _ = exit.0.send(Wake::Started(p));
            })
        });
        Job {
            thread,
            progress: None,
            exited: false,
        }
    }

    fn start(&mut self) {
        let (root, opts) = (self.root.clone(), self.options());
        self.state = State::Scanning(self.spawn(move |progress| {
            let tree = scan_live(&root, &opts, progress).map_err(ApplyError::Scan)?;
            Ok((tree, Applied::default()))
        }));
    }

    /// Wakes a [`Live::wait`] from another thread.
    pub fn waker(&self) -> Waker {
        Waker(self.tx.clone())
    }

    /// Takes in what woke [`Live::wait`] without waiting; whether it was a
    /// [`Waker`].
    fn take(&mut self, mut wake: Option<Wake>) -> bool {
        let mut interrupted = false;
        while let Some(w) = wake.or_else(|| self.rx.try_recv().ok()) {
            let job = match &mut self.state {
                State::Scanning(job)
                | State::Following {
                    rescan: Some(job), ..
                } => Some(job),
                _ => None,
            };
            match (w, job) {
                (Wake::Interrupt, _) => interrupted = true,
                (Wake::Started(p), Some(job)) => job.progress = Some(p),
                (Wake::Exited, Some(job)) => job.exited = true,
                // each scan is joined after its last wake, before the next starts
                (_, None) => unreachable!("a wake with no scan running"),
            }
            wake = None;
        }
        interrupted
    }

    /// The next event within `timeout`, or `None`. A snapshot comes at
    /// most every [`LiveOptions::interval`] while scanning, `Ready` as
    /// soon as the scan is done, then the changes of each interval as one
    /// `Changed`. After an error the tree is followed as before, unless
    /// the scan itself failed: then [`Live::is_done`].
    /// Returns `None` early when woken by a [`Waker`].
    pub fn wait(&mut self, timeout: Duration) -> Option<Result<Event, ScanError>> {
        let until = Instant::now().checked_add(timeout);
        let mut wake = None;
        loop {
            if self.take(wake.take()) {
                return None;
            }
            let now = Instant::now();
            let due = now >= self.due;
            if due {
                self.due = now + self.opts.interval;
            }
            if let Some(event) = self.step(due) {
                return Some(event);
            }
            let now = Instant::now();
            if self.is_done() || until.is_some_and(|u| now >= u) {
                return None;
            }
            let next = until.map_or(self.due, |u| u.min(self.due));
            match self.rx.recv_timeout(next.saturating_duration_since(now)) {
                Ok(w) => wake = Some(w),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => unreachable!("`Live` holds a sender"),
            }
        }
    }

    fn step(&mut self, due: bool) -> Option<Result<Event, ScanError>> {
        match &mut self.state {
            State::Done => None,
            State::Scanning(job) => {
                if job.exited {
                    let State::Scanning(job) = std::mem::replace(&mut self.state, State::Done)
                    else {
                        unreachable!()
                    };
                    return Some(self.finish(job));
                }
                let snapshot = due.then(|| job.progress.as_ref()?.snapshot()).flatten();
                snapshot.map(|tree| Ok(Event::Scanning(tree)))
            }
            State::Following {
                tree,
                follow,
                rescan: rescan @ Some(_),
            } => {
                if !rescan.as_ref().unwrap().exited {
                    return None;
                }
                match join(rescan.take().unwrap()) {
                    Ok((new, applied)) => {
                        **tree = new;
                        follow.applied(tree, &applied, self.opts.inotify);
                        let changes = applied.deltas.unwrap_or_default();
                        Some(Ok(Event::Changed(shared(tree), changes)))
                    }
                    Err(e) => Some(self.failed(e)),
                }
            }
            State::Following { .. } if !due => None,
            State::Following {
                tree,
                follow,
                rescan: None,
            } => {
                let poll = match &mut **follow {
                    Follow::All(watch) => watch.poll(),
                    Follow::Every(watch) => watch.poll(tree),
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
                let opts = self.options();
                let State::Following { tree, follow, .. } = &mut self.state else {
                    unreachable!()
                };
                let applied = tree.apply(&changes, &opts);
                if let Follow::Shown(dirs) = &mut **follow {
                    dirs.listed(started.elapsed());
                }
                match applied {
                    Ok(applied) => {
                        follow.applied(tree, &applied, self.opts.inotify);
                        Some(Ok(Event::Changed(shared(tree), applied.deltas?)))
                    }
                    Err(e) => Some(self.failed(e)),
                }
            }
        }
    }

    /// A full scan when only that brings the tree up to date, or the error.
    fn failed(&mut self, e: ApplyError) -> Result<Event, ScanError> {
        match e {
            ApplyError::FullScan => {
                self.start();
                Ok(Event::Rescanning(Reason::MustScanAll))
            }
            ApplyError::Scan(e) => Err(e),
        }
    }

    /// `Ready`, and the tree followed from then, or the scan's error.
    fn finish(&mut self, job: Job) -> Result<Event, ScanError> {
        let tree = match join(job) {
            Ok((tree, _)) => tree,
            Err(e) => return self.failed(e),
        };
        let ignore = self.opts.ignore.as_deref();
        let every = || match self.opts.inotify && !self.opts.shown_only {
            true => TreeWatch::start(&tree),
            false => None,
        };
        let follow = Box::new(match Watch::start(&self.root, &tree, ignore) {
            Some(watch) => Follow::All(watch),
            None => match every() {
                Some(watch) => Follow::Every(watch),
                None => Follow::Shown(DirWatch::new(self.opts.inotify)),
            },
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
            tree, rescan: None, ..
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
        let job = self.spawn(move |_| {
            let deltas = tree.apply(&changes, &opts)?;
            Ok((tree, deltas))
        });
        if let State::Following { rescan, .. } = &mut self.state {
            *rescan = Some(job);
        }
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
            State::Following { follow, .. } => !matches!(**follow, Follow::Shown(_)),
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
            } => join(job).ok().map(|(tree, _)| tree),
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
            if let Some(event) = self.wait(Duration::MAX) {
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

fn join(job: Job) -> Result<(Tree, Applied), ApplyError> {
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

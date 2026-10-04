//! A scan that keeps going: snapshots while it scans, the tree once it is
//! done, then the changes on disk as they happen, with no UI. The browser
//! and the Python API's `diskuse.live` both run on it.
//!
//! One worker thread owns the tree. It hands each [`Event`] to a
//! [`Handler`], and takes [`Live`]'s commands as messages, so no caller
//! ever waits on a scan or holds a lock.
//!
//! Only changes the OS reports are applied ([`crate::watch`]). When it
//! says it missed some, that is reported ([`Event::Missed`]) and nothing
//! is scanned: whoever handles the events decides ([`Live::rescan`]).
//! Where the OS keeps no record of a whole tree and its folders cannot
//! all be watched (Linux), only those given to [`Live::follow`] are
//! followed.

use crate::scan::{ApplyError, Pool, ScanError, ScanOptions, Stop, walk_root};
use crate::tree::{Progress, ReadTree, Tree, below};
use crate::watch::{Changes, DirWatch, Poll, TreeWatch, Watch};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// Changes at or below this path were missed, so the tree may be out
    /// of date there, until a [`Live::rescan`]. Later changes still come.
    Missed(Reason, PathBuf),
}

/// Why changes were missed.
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
    /// The OS merged the changes below a folder into "scan it again".
    MustScan,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dropped => "the OS dropped change events",
            Self::IdsWrapped => "macOS event ids wrapped",
            Self::RootMoved => "the scanned dir moved",
            Self::NoReplay => "macOS did not replay the changes made while scanning within 30 s",
            Self::MustScan => "the OS asked to scan it again",
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

/// Takes the events of a [`Live`], on its worker thread: a closure, or the
/// sender of a channel. After an error the tree is followed as before,
/// unless the scan itself failed: then no event comes again.
pub trait Handler: Send + 'static {
    fn handle(&mut self, event: Result<Event, ScanError>);
}

impl<F: FnMut(Result<Event, ScanError>) + Send + 'static> Handler for F {
    fn handle(&mut self, event: Result<Event, ScanError>) {
        self(event);
    }
}

impl Handler for mpsc::Sender<Result<Event, ScanError>> {
    fn handle(&mut self, event: Result<Event, ScanError>) {
        // a receiver gone no longer wants them
        let _ = self.send(event);
    }
}

/// What the worker is told: by its [`Live`], and by the scan it runs.
enum Msg {
    Follow(Vec<u32>),
    Relist(Vec<u32>),
    Rescan(u32),
    /// Stop, and answer with what a running scan found, if asked.
    Stop(Option<mpsc::Sender<Option<Tree>>>),
    /// The running scan's progress, for snapshots.
    Started(Progress),
    /// The running scan's thread is done.
    Exited,
}

/// Sends [`Msg::Exited`] when its scan thread is done, even by a panic.
struct Exit(mpsc::Sender<Msg>);

impl Drop for Exit {
    fn drop(&mut self) {
        // the worker is gone only once it has joined this thread
        let _ = self.0.send(Msg::Exited);
    }
}

/// What a [`Live`] reads of its worker without asking it.
#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    /// The scans asked for and not yet done: the first, and each
    /// [`Live::rescan`] since.
    scans: AtomicUsize,
    follows_all: AtomicBool,
    done: AtomicBool,
}

/// A live scan of a folder: see [`live`]. Dropping it stops the scan.
pub struct Live {
    tx: mpsc::Sender<Msg>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

/// Scans `root` and keeps following it, on a thread of its own, which
/// hands every event to `handler`: a snapshot at most every
/// [`LiveOptions::interval`] while scanning, `Ready` as soon as the scan
/// is done, then the changes of each interval as one `Changed`.
pub fn live(root: &Path, opts: LiveOptions, handler: impl Handler) -> Live {
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared::default());
    // a scan runs from the start
    shared.scans.store(1, Ordering::Relaxed);
    let worker = Worker {
        root: root.into(),
        opts,
        handler: Box::new(handler),
        shared: Arc::clone(&shared),
        tx: tx.clone(),
        rx,
        pool: None,
        state: State::Done,
    };
    Live {
        tx,
        shared,
        worker: Some(thread::spawn(move || worker.run())),
    }
}

impl Live {
    fn send(&self, msg: Msg) {
        // a worker gone has nothing left to do
        let _ = self.tx.send(msg);
    }

    /// Where the OS keeps no record of a whole tree and not every folder
    /// is watched, follows only the folders `dirs` of the latest tree,
    /// from now on. Ignored otherwise.
    pub fn follow(&self, dirs: &[u32]) {
        self.send(Msg::Follow(dirs.into()));
    }

    /// Lists the folders `dirs` of the latest tree again, and reports what
    /// changed in them: for a folder looked at after changes were missed.
    /// What is below their subfolders stays as scanned.
    pub fn relist(&self, dirs: &[u32]) {
        self.send(Msg::Relist(dirs.into()));
    }

    /// Scans folder `dir` of the latest tree again, then reports it as
    /// `Changed`; the root is scanned all over, with snapshots. Ignored
    /// while a scan runs.
    pub fn rescan(&self, dir: u32) {
        self.shared.scans.fetch_add(1, Ordering::Relaxed);
        self.send(Msg::Rescan(dir));
    }

    /// A scan runs or waits its turn: the first, or a [`Live::rescan`].
    /// False only once its events were handed over.
    pub fn busy(&self) -> bool {
        self.shared.scans.load(Ordering::Relaxed) > 0
    }

    /// Every change below the root is followed, not only those in the
    /// folders given to [`Live::follow`].
    pub fn follows_all(&self) -> bool {
        self.shared.follows_all.load(Ordering::Relaxed)
    }

    /// The scan failed, so no event comes again.
    pub fn is_done(&self) -> bool {
        self.shared.done.load(Ordering::Relaxed)
    }

    /// Stops, and returns what a running scan of the whole tree found.
    pub fn stop(mut self) -> Option<Tree> {
        let (tx, found) = mpsc::channel();
        self.end(Some(tx));
        found.recv().ok().flatten()
    }

    fn end(&mut self, reply: Option<mpsc::Sender<Option<Tree>>>) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.send(Msg::Stop(reply));
        if let Some(worker) = self.worker.take() {
            // a handler that panicked took the worker with it
            let _ = worker.join();
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.end(None);
        }
    }
}

/// A scan of the whole tree on its own thread, so the worker goes on
/// taking snapshots and commands.
struct Job {
    thread: JoinHandle<Result<Tree, ScanError>>,
    progress: Option<Progress>,
    started: Instant,
}

enum Follow {
    /// Every change below the root, from the OS's record of them (macOS).
    All(Watch),
    /// Every folder, each watched (Linux).
    Every(TreeWatch),
    /// Those in the folders given to [`Live::follow`].
    Shown(DirWatch),
}

enum State {
    Scanning(Job),
    Following {
        tree: Box<Tree>,
        follow: Box<Follow>,
    },
    Done,
}

struct Worker {
    root: PathBuf,
    opts: LiveOptions,
    handler: Box<dyn Handler>,
    shared: Arc<Shared>,
    /// For the scan thread to report to `rx`.
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    /// The threads of every scan and update, once they could be started.
    pool: Option<Arc<Pool>>,
    state: State,
}

impl Worker {
    fn run(mut self) {
        match Pool::new(&self.opts.scan) {
            Ok(pool) => self.pool = Some(Arc::new(pool)),
            Err(e) => return self.fail(ScanError::ThreadPool(e)),
        }
        self.start();
        // a tick takes a snapshot, or checks for changes
        let interval = self.opts.interval.max(Duration::from_millis(1));
        let mut due = Instant::now() + interval;
        loop {
            let now = Instant::now();
            if now >= due {
                due = now + interval;
                self.tick();
            }
            match self.rx.recv_timeout(due.saturating_duration_since(now)) {
                Ok(Msg::Stop(reply)) => return self.stop(reply),
                Ok(msg) => self.on(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // the `Live` always says stop first
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn emit(&mut self, event: Result<Event, ScanError>) {
        self.handler.handle(event);
    }

    /// The scan cannot go on: says why, and that nothing comes after.
    fn fail(&mut self, e: ScanError) {
        log::warn!("live: {}: {e}", self.root.display());
        self.state = State::Done;
        self.emit(Err(e));
        self.shared.done.store(true, Ordering::Relaxed);
        self.scanned();
    }

    /// A scan asked for is done, or will not be: after its events.
    fn scanned(&self) {
        self.shared.scans.fetch_sub(1, Ordering::Relaxed);
    }

    fn options(&self) -> ScanOptions {
        let shared = Arc::clone(&self.shared);
        ScanOptions {
            stop: Stop::new(move || shared.stop.load(Ordering::Relaxed)),
            ..self.opts.scan.clone()
        }
    }

    /// Scans the whole tree on a thread of its own.
    fn start(&mut self) {
        let (root, opts) = (self.root.clone(), self.options());
        let pool = Arc::clone(self.pool.as_ref().unwrap());
        let tx = self.tx.clone();
        log::info!("live: scanning {}", root.display());
        let thread = thread::spawn(move || {
            let exit = Exit(tx);
            walk_root(&pool, &root, &opts, true, |p| {
                let _ = exit.0.send(Msg::Started(p));
            })
        });
        self.shared.follows_all.store(false, Ordering::Relaxed);
        self.state = State::Scanning(Job {
            thread,
            progress: None,
            started: Instant::now(),
        });
    }

    fn stop(self, reply: Option<mpsc::Sender<Option<Tree>>>) {
        let found = match self.state {
            State::Scanning(job) => join(job).ok(),
            _ => None,
        };
        if let Some(reply) = reply {
            let _ = reply.send(found);
        }
    }

    fn on(&mut self, msg: Msg) {
        // ids of an earlier tree may be past this one's end
        let there = |tree: &Tree, d: &u32| (*d as usize) < tree.len() && !tree.gone(*d);
        match (msg, &mut self.state) {
            (Msg::Started(progress), State::Scanning(job)) => job.progress = Some(progress),
            (Msg::Exited, State::Scanning(_)) => self.finish(),
            (Msg::Follow(mut dirs), State::Following { tree, follow }) => {
                dirs.retain(|d| there(tree, d));
                if let Follow::Shown(watch) = &mut **follow {
                    watch.show(tree, &dirs);
                }
            }
            (Msg::Relist(mut dirs), State::Following { tree, .. }) => {
                dirs.retain(|d| there(tree, d));
                let changes = Changes {
                    dirs,
                    ..Changes::default()
                };
                self.apply(&changes, false);
            }
            // its own scan, done when its thread is
            (Msg::Rescan(0), State::Following { .. }) => self.start(),
            (Msg::Rescan(dir), State::Following { tree, .. }) if there(tree, &dir) => {
                let changes = Changes {
                    rescan: vec![below(&**tree, dir).into()],
                    ..Changes::default()
                };
                log::info!("live: scanning {} again", tree.path(dir).display());
                self.apply(&changes, true);
                self.scanned();
            }
            // while a scan runs, after one failed, or of a folder gone
            (Msg::Rescan(_), _) => self.scanned(),
            // nothing to follow or list then either. A scan thread's last
            // words come before the next one starts
            _ => {}
        }
    }

    /// A snapshot of the running scan, or the changes on disk since the
    /// last one.
    fn tick(&mut self) {
        let poll = match &mut self.state {
            State::Done => return,
            State::Scanning(job) => {
                let snapshot = job.progress.as_ref().and_then(Progress::snapshot);
                if let Some(tree) = snapshot {
                    self.emit(Ok(Event::Scanning(tree)));
                }
                return;
            }
            State::Following { tree, follow } => match &mut **follow {
                Follow::All(watch) => watch.poll(),
                Follow::Every(watch) => watch.poll(tree),
                Follow::Shown(dirs) => dirs.poll(tree),
            },
        };
        match poll {
            Poll::Wait => {}
            Poll::Lost(why) => self.missed(why, self.root.clone()),
            Poll::Changes(mut changes) => {
                // not scanned again by itself: whoever listens decides
                for path in std::mem::take(&mut changes.rescan) {
                    let below = Path::new(OsStr::from_bytes(&path));
                    self.missed(Reason::MustScan, self.root.join(below));
                }
                if !changes.is_empty() {
                    self.apply(&changes, false);
                }
            }
        }
    }

    fn missed(&mut self, why: Reason, path: PathBuf) {
        log::warn!("live: missed changes at {}: {why}", path.display());
        self.emit(Ok(Event::Missed(why, path)));
    }

    /// Brings the tree up to date with `changes`, and reports what changed
    /// if anything did, or if `always`.
    fn apply(&mut self, changes: &Changes, always: bool) {
        let opts = self.options();
        let pool = Arc::clone(self.pool.as_ref().unwrap());
        let State::Following { tree, follow } = &mut self.state else {
            return;
        };
        let started = Instant::now();
        let applied = tree.apply(changes, &opts, &pool);
        if let Follow::Shown(dirs) = &mut **follow {
            dirs.listed(started.elapsed());
        }
        match applied {
            Ok(applied) => {
                // out of watches: only the folders given to `follow`, then
                if let Follow::Every(watch) = &mut **follow
                    && !watch.applied(tree, &applied)
                {
                    log::warn!("live: out of inotify watches, following the folders shown");
                    **follow = Follow::Shown(DirWatch::new(self.opts.inotify));
                    self.shared.follows_all.store(false, Ordering::Relaxed);
                }
                if always || applied.deltas.is_some() {
                    let event = Event::Changed(shared(tree), applied.deltas.unwrap_or_default());
                    self.emit(Ok(event));
                }
            }
            // the changes reach the root: reported, like every scan that
            // only a caller starts
            Err(ApplyError::FullScan) => self.missed(Reason::MustScan, self.root.clone()),
            Err(ApplyError::Scan(e)) => self.emit(Err(e)),
        }
    }

    /// The scan thread is done: `Ready`, and the tree followed from then,
    /// or its error.
    fn finish(&mut self) {
        let State::Scanning(job) = std::mem::replace(&mut self.state, State::Done) else {
            unreachable!("only a scan exits");
        };
        let took = job.started.elapsed();
        let tree = match join(job) {
            Ok(tree) => tree,
            Err(e) => return self.fail(e),
        };
        log::info!(
            "live: scanned {} in {:.1} s: {} folders, {} bytes",
            self.root.display(),
            took.as_secs_f64(),
            tree.len(),
            tree.size(0)
        );
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
        let all = !matches!(*follow, Follow::Shown(_));
        self.shared.follows_all.store(all, Ordering::Relaxed);
        let event = Event::Ready(shared(&tree));
        self.state = State::Following {
            tree: Box::new(tree),
            follow,
        };
        // told first, so whoever sees no scan running has the tree
        self.emit(Ok(event));
        self.scanned();
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

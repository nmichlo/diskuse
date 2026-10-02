//! Keeping a tree up to date from the OS's record of changes, so a saved
//! scan is brought up to date instead of scanned again, and an open browser
//! follows changes live. One mechanism serves both: a stream from the
//! tree's event id first replays the changes recorded since, then reports
//! live ones, and [`Tree::apply`] lists the dirs they touched again.
//!
//! macOS only. Elsewhere [`Watch::start`] is always `None`, so every scan
//! is a full one, unless it finishes a stopped one, and an open browser
//! follows only the dirs it shows ([`DirWatch`]).

use crate::scan::ScanOptions;
use crate::store::CacheDir;
use crate::sys::{self, Event, What};
use crate::tree::{Progress, ReadTree, Record, Tree};
use rustix::fd::AsFd;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};

/// How long the stream must stay quiet after the replay ends before the
/// replay is trusted. FSEvents sends the changes of the moments before the
/// stream started again, just after the replay ends.
const QUIET: Duration = Duration::from_millis(200);
/// The longest wait for that quiet after the replay ends, as a busy disk is
/// never quiet. Later changes are applied with the live ones.
const QUIET_MAX: Duration = Duration::from_secs(2);
/// The longest wait for the replay to end before a full scan instead.
const REPLAY_MAX: Duration = Duration::from_secs(30);
/// How often a dir shown without an inotify watch is listed again, at
/// most.
const POLL: Duration = Duration::from_secs(2);
/// Listing the dirs shown again waits this many times as long as the last
/// listing took, so it takes at most a tenth of a core.
const BACKOFF: u32 = 10;

/// Brings `tree`, an earlier scan of `root`, up to date from the changes
/// the OS recorded since, listing again only the dirs they touched, and
/// finishes it if it was stopped, scanning only what it lacks. Changes in
/// `cache` are ignored. `None` if only a full scan is right: when the OS
/// recorded the changes, but no longer has them all, or keeps no record of
/// changes and the tree is finished. So on Linux, a stopped scan is
/// finished, and the dirs it listed are left as they were.
pub fn update(
    root: &Path,
    tree: Tree,
    opts: &ScanOptions,
    cache: Option<&CacheDir>,
) -> Option<Tree> {
    update_live(root, tree, opts, cache, |_| {})
}

/// [`update`], handing `progress` a [`Progress`] of the scan of what is
/// new or missing, if any.
pub(crate) fn update_live(
    root: &Path,
    mut tree: Tree,
    opts: &ScanOptions,
    cache: Option<&CacheDir>,
    progress: impl FnOnce(Progress),
) -> Option<Tree> {
    let changes = match tree.since.store {
        0 if tree.unfinished(&tree.child_index()).next().is_some() => Changes::default(),
        0 => return None,
        _ => Watch::start(root, &tree, cache)?.replay()?,
    };
    tree.apply(&changes, opts, progress)?;
    Some(tree)
}

/// Changes below a root, as paths relative to it: names joined by `/`,
/// `""` for the root itself. Or as dirs of a tree of it.
#[derive(Debug, Default)]
pub(crate) struct Changes {
    /// Where something changed.
    pub changed: Vec<Box<[u8]>>,
    /// Where everything below is to be listed again.
    pub rescan: Vec<Box<[u8]>>,
    /// Dirs to list again, by record id in the tree they are for.
    pub dirs: Vec<u32>,
    /// The latest event id seen.
    pub id: u64,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.rescan.is_empty() && self.dirs.is_empty()
    }
}

pub(crate) enum Poll {
    /// The replay has not ended, or the stream has not gone quiet after it.
    /// Or no dir shown is due to be listed again.
    Wait,
    /// Changes were lost: only a full scan is right.
    Lost,
    Changes(Changes),
}

/// A stream of the changes below a root, from where a tree of it stands.
pub(crate) struct Watch {
    stream: sys::Stream,
    rx: Receiver<Vec<Event>>,
    /// The real root and cache dir, without a trailing `/`, so `/` is `""`.
    root: Vec<u8>,
    cache: Option<Vec<u8>>,
    pending: Changes,
    lost: bool,
    started: Instant,
    /// When the replay ended, if it has.
    replayed: Option<Instant>,
    /// When the last batch came.
    last: Instant,
    /// The replay ended and the stream went quiet once.
    settled: bool,
    /// [`sys::Stream::flush`] ran after the replay ended.
    flushed: bool,
}

impl Watch {
    /// Watches `root` from where `tree`, a scan of it, stands, ignoring
    /// changes in `cache`. `None` if the OS's record of changes does not
    /// reach back there.
    pub fn start(root: &Path, tree: &Tree, cache: Option<&CacheDir>) -> Option<Self> {
        if tree.since.id == 0 {
            return None;
        }
        // event paths are real paths
        let real = std::fs::canonicalize(root).ok()?;
        let dev = sys::dir_stat(sys::open_root(&real).ok()?.as_fd()).ok()?.dev;
        // the record was purged, erased or wrapped since
        if tree.since.store == 0 || sys::event_store(dev) != tree.since.store {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        let path = CString::new(real.as_os_str().as_bytes()).ok()?;
        let stream = sys::watch(&path, tree.since.id, tx)?;
        let trim = |path: &Path| {
            let path = path.as_os_str().as_bytes();
            path.strip_suffix(b"/").unwrap_or(path).to_vec()
        };
        let cache =
            cache.map(|c| std::fs::canonicalize(c.path()).unwrap_or_else(|_| c.path().into()));
        let now = Instant::now();
        Some(Self {
            stream,
            rx,
            root: trim(&real),
            cache: cache.as_deref().map(trim),
            pending: Changes::default(),
            lost: false,
            started: now,
            replayed: None,
            last: now,
            settled: false,
            flushed: false,
        })
    }

    /// The changes since the last poll, once the replay has ended and the
    /// stream has gone quiet after it; from then on, whatever came.
    pub fn poll(&mut self) -> Poll {
        while let Ok(batch) = self.rx.try_recv() {
            self.add(batch);
        }
        let now = Instant::now();
        if self.lost {
            return Poll::Lost;
        }
        if !self.settled && self.replayed.is_some() && !self.flushed {
            // the replay ends with what the service has seen, so changes made
            // just before may still be on their way: ask for them, then wait
            // for quiet as usual
            self.flushed = true;
            self.stream.flush();
            while let Ok(batch) = self.rx.try_recv() {
                self.add(batch);
            }
        }
        if !self.settled {
            let Some(replayed) = self.replayed else {
                return match now - self.started > REPLAY_MAX {
                    true => Poll::Lost,
                    false => Poll::Wait,
                };
            };
            if now - self.last < QUIET && now - replayed < QUIET_MAX {
                return Poll::Wait;
            }
            self.settled = true;
        }
        Poll::Changes(std::mem::take(&mut self.pending))
    }

    /// [`Watch::poll`], waiting until the replay ends and the stream goes
    /// quiet. `None` if only a full scan is right.
    pub fn replay(&mut self) -> Option<Changes> {
        loop {
            match self.poll() {
                Poll::Wait => {
                    // wakes for the next batch, or soon after the quiet
                    // time may be up
                    if let Ok(batch) = self.rx.recv_timeout(QUIET / 4) {
                        self.add(batch);
                    }
                }
                Poll::Lost => return None,
                Poll::Changes(changes) => return Some(changes),
            }
        }
    }

    fn add(&mut self, batch: Vec<Event>) {
        self.last = Instant::now();
        for e in batch {
            self.pending.id = self.pending.id.max(e.id);
            match e.what {
                What::Lost => self.lost = true,
                What::HistoryDone => self.replayed = self.replayed.or(Some(self.last)),
                What::Changed(path) => {
                    if let Some(path) = self.below(&path) {
                        self.pending.changed.push(path.into());
                    }
                }
                What::Rescan(path) => {
                    if let Some(path) = self.below(&path) {
                        self.pending.rescan.push(path.into());
                    }
                }
            }
        }
    }

    /// `path` relative to the root, unless it is outside it or in the cache
    /// dir.
    fn below<'a>(&self, path: &'a [u8]) -> Option<&'a [u8]> {
        let path = path.strip_suffix(b"/").unwrap_or(path);
        let inside = |dir: &[u8]| {
            path == dir || (path.strip_prefix(dir)).is_some_and(|rest| rest.starts_with(b"/"))
        };
        if self.cache.as_deref().is_some_and(inside) {
            return None;
        }
        if path == self.root {
            return Some(b"");
        }
        path.strip_prefix(&self.root[..])?.strip_prefix(b"/")
    }
}

/// The dirs a browser shows, followed where the OS keeps no record of
/// changes: each is watched with inotify, or listed again on a timer where
/// that cannot work, on a network or FUSE filesystem, or once the watch
/// limit is reached. Changes in other dirs are not seen until the next
/// scan. The pattern of GIO, KDirWatch and the `notify` crate.
pub(crate) struct DirWatch {
    /// When the tree was scanned, as the dirs not shown stand.
    pub scanned: SystemTime,
    inotify: Option<sys::Inotify>,
    /// The record id of each dir shown, and its watch if it has one. At
    /// most three, so searched in order.
    dirs: Vec<(u32, Option<i32>)>,
    /// Dirs found changed, to list again.
    dirty: BTreeSet<u32>,
    /// When the dirs were last listed again, and how long that took.
    listed: Instant,
    took: Duration,
}

impl DirWatch {
    /// Follows no dirs yet, of a tree scanned at `scanned`, with inotify if
    /// `inotify`.
    pub fn new(inotify: bool, scanned: SystemTime) -> Self {
        Self {
            scanned,
            inotify: inotify.then(sys::Inotify::new).flatten(),
            dirs: Vec::new(),
            dirty: BTreeSet::new(),
            listed: Instant::now(),
            took: Duration::ZERO,
        }
    }

    /// Follows dirs `shown` of `tree` from now on, and no others. Mount
    /// points and dirs that could not be read are left out, as the tree
    /// has nothing below them.
    pub fn show(&mut self, tree: &Tree, shown: &[u32]) {
        let skip = Record::OTHER_DEVICE | Record::DENIED;
        let shown: Vec<u32> = (shown.iter().copied())
            .filter(|&d| tree.record(d).flags & skip == 0)
            .collect();
        let (kept, hidden): (Vec<_>, Vec<_>) =
            self.dirs.drain(..).partition(|(d, _)| shown.contains(d));
        self.dirs = kept;
        for (_, wd) in hidden {
            // one dir seen at two paths, through a bind mount, has one watch
            if let (Some(inotify), Some(wd)) = (&self.inotify, wd)
                && !self.dirs.iter().any(|&(_, w)| w == Some(wd))
            {
                inotify.remove(wd);
            }
        }
        for d in shown {
            if self.dirs.iter().any(|&(k, _)| k == d) {
                continue;
            }
            let path = CString::new(tree.dir_path(d)).expect("names hold no NUL");
            // a symlinked root is followed, as the scan does
            let wd = (self.inotify.as_ref()).and_then(|i| i.add(&path, d == 0));
            self.dirs.push((d, wd));
        }
    }

    /// The dirs of `tree` to list again now: those inotify saw change,
    /// and every [`POLL`] those without a watch, but never sooner than
    /// [`BACKOFF`] times the last listing took. Call [`DirWatch::listed`]
    /// once listed.
    pub fn poll(&mut self, tree: &Tree) -> Poll {
        if let Some(inotify) = &self.inotify {
            let (dirs, dirty) = (&self.dirs, &mut self.dirty);
            let of = |wd: i32| dirs.iter().filter(move |&&(_, w)| w == Some(wd));
            inotify.read(|note| match note {
                sys::Note::Changed(wd) => dirty.extend(of(wd).map(|&(d, _)| d)),
                // the dir is in its parent's listing, unless it is the root
                sys::Note::Gone(wd) => {
                    let parent = |d| match d {
                        0 => 0,
                        _ => tree.record(d).parent,
                    };
                    dirty.extend(of(wd).map(|&(d, _)| parent(d)));
                }
                sys::Note::Lost => dirty.extend(dirs.iter().map(|&(d, _)| d)),
            });
        }
        let since = self.listed.elapsed();
        let rest = self.took * BACKOFF;
        if since >= rest.max(POLL) {
            let polled = self.dirs.iter().filter(|(_, wd)| wd.is_none());
            self.dirty.extend(polled.map(|&(d, _)| d));
        }
        if self.dirty.is_empty() || since < rest {
            return Poll::Wait;
        }
        Poll::Changes(Changes {
            dirs: std::mem::take(&mut self.dirty).into_iter().collect(),
            ..Changes::default()
        })
    }

    /// The dirs [`DirWatch::poll`] gave were listed again, which took
    /// `took`.
    pub fn listed(&mut self, took: Duration) {
        self.listed = Instant::now();
        self.took = took;
    }
}

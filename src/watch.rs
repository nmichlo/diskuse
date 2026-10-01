//! Keeping a tree up to date from the OS's record of changes, so a saved
//! scan is brought up to date instead of scanned again, and an open browser
//! follows changes live. One mechanism serves both: a stream from the
//! tree's event id first replays the changes recorded since, then reports
//! live ones, and [`Tree::apply`] lists the dirs they touched again.
//!
//! macOS only. Elsewhere [`Watch::start`] is always `None`, so every scan
//! is a full one.

use crate::scan::ScanOptions;
use crate::store::CacheDir;
use crate::sys::{self, Event, What};
use crate::tree::Tree;
use rustix::fd::AsFd;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How long the stream must stay quiet after the replay ends before the
/// replay is trusted. FSEvents sends the changes of the moments before the
/// stream started again, just after the replay ends.
const QUIET: Duration = Duration::from_millis(200);
/// The longest wait for that quiet after the replay ends, as a busy disk is
/// never quiet. Later changes are applied with the live ones.
const QUIET_MAX: Duration = Duration::from_secs(2);
/// The longest wait for the replay to end before a full scan instead.
const REPLAY_MAX: Duration = Duration::from_secs(30);

/// Brings `tree`, an earlier scan of `root`, up to date from the changes
/// the OS recorded since, listing again only the dirs they touched.
/// Changes in `cache` are ignored. `None` where the OS cannot say what
/// changed, so only a full scan is right: always on Linux.
pub fn update(
    root: &Path,
    mut tree: Tree,
    opts: &ScanOptions,
    cache: Option<&CacheDir>,
) -> Option<Tree> {
    let changes = Watch::start(root, &tree, cache)?.replay()?;
    tree.apply(&changes, opts)?;
    Some(tree)
}

/// Changes below a root, as paths relative to it: names joined by `/`,
/// `""` for the root itself.
#[derive(Debug, Default)]
pub(crate) struct Changes {
    /// Where something changed.
    pub changed: Vec<Box<[u8]>>,
    /// Where everything below is to be listed again.
    pub rescan: Vec<Box<[u8]>>,
    /// The latest event id seen.
    pub id: u64,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.rescan.is_empty()
    }
}

pub(crate) enum Poll {
    /// The replay has not ended, or the stream has not gone quiet after it.
    Wait,
    /// Changes were lost: only a full scan is right.
    Lost,
    Changes(Changes),
}

/// A stream of the changes below a root, from where a tree of it stands.
pub(crate) struct Watch {
    _stream: sys::Stream,
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
            _stream: stream,
            rx,
            root: trim(&real),
            cache: cache.as_deref().map(trim),
            pending: Changes::default(),
            lost: false,
            started: now,
            replayed: None,
            last: now,
            settled: false,
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

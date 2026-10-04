//! Following changes on disk while the browser is open. On macOS an
//! FSEvents stream from the event id taken when the scan started reports
//! every change below the root, the ones made while the scan ran first,
//! and [`Tree::apply`] lists the dirs they touched again.
//!
//! A saved scan is never brought up to date this way: Apple calls the
//! recorded history "advisory rather than a definitive list of all changes
//! to the volume", as another OS or another Mac can change a disk without
//! it. So every launch scans afresh, and only changes seen live, which the
//! kernel reports and flags when it drops any, are applied.
//!
//! Elsewhere [`Watch::start`] is always `None`, and an open browser follows
//! only the dirs it shows ([`DirWatch`]).

use crate::live::Reason;
use crate::sys::{self, Event};
use crate::tree::{ReadTree, Record, Tree, dir_path};
use rustix::fd::AsFd;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How long the stream must stay quiet after the replay of the changes
/// made during the scan ends before they are applied. FSEvents sends the
/// changes of the moments before the stream started again, just after the
/// replay ends.
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
    /// Changes were lost: only a full scan is right. Why, for the user.
    Lost(Reason),
    Changes(Changes),
}

/// A stream of the changes below a root, from when a scan of it started.
pub(crate) struct Watch {
    stream: sys::Stream,
    rx: Receiver<Vec<Event>>,
    /// The real root and ignored dir, without a trailing `/`, so `/` is
    /// `""`.
    root: Vec<u8>,
    ignore: Option<Vec<u8>>,
    firmlinks: Firmlinks,
    pending: Changes,
    /// Why changes were lost, if they were.
    lost: Option<Reason>,
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
    /// Watches `root` from when `tree`, a scan of it, started, ignoring
    /// changes below `ignore`. `None` where the OS keeps no record of changes
    /// ([`sys::records_changes`]).
    pub fn start(root: &Path, tree: &Tree, ignore: Option<&Path>) -> Option<Self> {
        if tree.since == 0 {
            return None;
        }
        // event paths are real paths
        let real = std::fs::canonicalize(root).ok()?;
        let dev = sys::dir_stat(sys::open_root(&real).ok()?.as_fd()).ok()?.dev;
        if !sys::records_changes(dev) {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        let path = CString::new(real.as_os_str().as_bytes()).ok()?;
        let stream = sys::watch(&path, tree.since, tx)?;
        let trim = |path: &Path| {
            let path = path.as_os_str().as_bytes();
            path.strip_suffix(b"/").unwrap_or(path).to_vec()
        };
        let ignore = ignore.map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.into()));
        let now = Instant::now();
        Some(Self {
            stream,
            rx,
            root: trim(&real),
            ignore: ignore.as_deref().map(trim),
            firmlinks: Firmlinks(sys::firmlinks()),
            pending: Changes::default(),
            lost: None,
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
        if let Some(why) = self.lost {
            return Poll::Lost(why);
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
                    true => Poll::Lost(Reason::NoReplay),
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

    fn add(&mut self, batch: Vec<Event>) {
        self.last = Instant::now();
        for e in batch {
            match e {
                Event::Lost(why) => self.lost = self.lost.or(Some(why)),
                Event::HistoryDone => self.replayed = self.replayed.or(Some(self.last)),
                Event::Changed(path) => {
                    if let Some(path) = self.below(&path) {
                        self.pending.changed.push(path);
                    }
                }
                Event::Rescan(path) => {
                    if let Some(path) = self.below(&path) {
                        self.pending.rescan.push(path);
                    }
                }
            }
        }
    }

    /// `path` relative to the root, unless it is outside it or ignored.
    /// Tried in both forms [`Firmlinks::forms`] gives, firmlink first.
    fn below(&self, path: &[u8]) -> Option<Box<[u8]>> {
        let path = path.strip_suffix(b"/").unwrap_or(path);
        let forms = self.firmlinks.forms(path);
        if let Some(ignore) = &self.ignore
            && forms.iter().any(|p| inside(p, ignore))
        {
            return None;
        }
        forms.iter().find_map(|p| match p[..] == self.root {
            true => Some(Box::default()),
            false => Some(p.strip_prefix(&self.root[..])?.strip_prefix(b"/")?.into()),
        })
    }
}

/// Whether `path` is `dir` or below it.
fn inside(path: &[u8], dir: &[u8]) -> bool {
    path == dir || (path.strip_prefix(dir)).is_some_and(|rest| rest.starts_with(b"/"))
}

/// The macOS firmlinks, `(firmlink, its target)`, such as `/Users` and
/// `/System/Volumes/Data/Users`: one dir seen at two paths. FSEvents reports
/// a change below one at the firmlink's path, even to a watch of the
/// target's, so a root below `/System/Volumes/Data` needs the target's form
/// and `/` the firmlink's.
struct Firmlinks(Vec<sys::Firmlink>);

impl Firmlinks {
    /// `path` in firmlink form, then in target form. Both are `path` if no
    /// firmlink leads to it.
    fn forms(&self, path: &[u8]) -> [Vec<u8>; 2] {
        let swap = |from: &[u8], to: &[u8]| {
            (inside(path, from)).then(|| [to, &path[from.len()..]].concat())
        };
        let link = self.0.iter().find_map(|(l, t)| swap(t, l));
        let target = self.0.iter().find_map(|(l, t)| swap(l, t));
        [
            link.unwrap_or_else(|| path.to_vec()),
            target.unwrap_or_else(|| path.to_vec()),
        ]
    }
}

/// The dirs a browser shows, followed where the OS keeps no record of
/// changes: each is watched with inotify, or listed again on a timer where
/// that cannot work, on a network or FUSE filesystem, or once the watch
/// limit is reached. Changes in other dirs are not seen until the next
/// scan. The pattern of GIO, KDirWatch and the `notify` crate.
pub(crate) struct DirWatch {
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
    /// Follows no dirs yet, with inotify if `inotify`.
    pub fn new(inotify: bool) -> Self {
        Self {
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
            let path = CString::new(dir_path(tree, d)).expect("names hold no NUL");
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

//! The full-screen browser: three columns, the parent dir, the current dir
//! and the selected child, each largest first, like OmniDiskSweeper. At the
//! root, which has no parent column, the other two move left. Each row has
//! a bar of its share of its dir. A click selects the row under it, a
//! double click goes into it, and the wheel scrolls the column under it. A scan
//! runs on its own thread meanwhile; a saved scan, if any, shows until the
//! fresh one is done. Then the tree follows changes on disk
//! ([`crate::watch`]): all of them on macOS, those in the dirs shown on
//! Linux. Quitting stops a running scan and saves what it found.
//! `d` lists the dirs the scan could not read instead, and `t` the largest
//! files. With reclaimable sizes (`-r`), each size has a second column, the
//! bytes deleting the item alone frees.
//!
//! Subdirectories and their sizes come from the tree. Files are not in the
//! tree, so a dir is listed from disk when first shown, and the listing is
//! kept until another scan's tree is shown.

use crate::access::{reason, terminal_app};
use crate::labels::{self, Label};
use crate::live::{Event, Live, LiveOptions, Waker, live};
use crate::report::{Units, largest_first};
use crate::reveal::Desktop;
use crate::scan::ScanOptions;
use crate::store::{CacheDir, Saved};
use crate::style::Styles;
use crate::sys;
use crate::tree::{File, LARGEST, ReadTree, Record, Tree, below, dir_path, join};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The browser's keys, most useful first: on a narrow screen [`fit_keys`]
/// leaves out those before `? help` until the line fits.
const HELP: &str = "hjkl move  i info  u units  space pick  p picks  r reveal  s/S rescan  \
                    / filter  t top  o open  ? help  q quit";
const PANEL_HELP: &str = "arrows/jk scroll  d close  ? help  q quit";
const INFO_HELP: &str = "i close  ? help  q quit";
const TOP_HELP: &str = "arrows/jk move  r reveal  o open  t close  ? help  q quit";
const PICKS_HELP: &str = "arrows/jk move  r reveal  o open  space unpick  p close  ? help  q quit";

/// How a valid `CACHEDIR.TAG` starts (<https://bford.info/cachedir/>).
const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// The width of the column of a dir above the parent, where there is room.
const OLDER: usize = 30;

/// A column of the browser.
#[derive(Clone, Copy, PartialEq)]
enum Col {
    /// A dir above the current one, by how many levels: 1 is its parent.
    Above(usize),
    Current,
    Preview,
}

/// Two clicks on one row within this are a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// What a browser takes from its environment, given so tests can fix it.
#[derive(Clone)]
pub struct Env {
    pub desktop: Desktop,
    /// The terminal app, named in Full Disk Access hints.
    pub terminal: String,
    /// Where a finished scan is saved, if anywhere.
    pub cache: Option<CacheDir>,
    /// On Linux, watch the dirs shown with inotify. Off, they are listed
    /// again on a timer, as where inotify cannot watch them.
    pub inotify: bool,
    /// How often a running scan is shown, and changes on disk are taken
    /// in: [`LiveOptions::interval`].
    pub interval: Duration,
    /// How sizes print, switched with `u`.
    pub units: Units,
    /// Draw in colour. Off when `NO_COLOR` is set and not empty.
    pub color: bool,
    /// The home dir, for the labels of known folders in it.
    pub home: Option<PathBuf>,
}

impl Env {
    /// The environment of this process.
    pub fn from_env() -> Self {
        Self {
            desktop: Desktop::from_env(),
            terminal: terminal_app(std::env::var("TERM_PROGRAM").ok().as_deref()),
            cache: CacheDir::from_env().ok(),
            inotify: true,
            interval: Duration::from_secs(1),
            units: Units::Binary,
            color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

/// The state of the browser. Driven by [`Browser::key`] and
/// [`Browser::poll`], drawn by [`Browser::draw`].
pub struct Browser {
    root: PathBuf,
    /// The real path of the root, for the labels of known folders.
    real: Option<PathBuf>,
    /// The real path of the home dir, as `real` is compared with it.
    real_home: Option<PathBuf>,
    /// The root as the title shows it.
    display: String,
    env: Env,
    /// Bytes in use on the volume, when the root is a volume's root.
    used: Option<u64>,
    /// Scans with reclaimable sizes and shows them (`-r`).
    reclaimable: bool,
    /// `None` until the running scan has listed the root.
    view: Option<View>,
    /// The scan, then the changes on disk, from [`Browser::scan`] on.
    live: Option<Live>,
    /// Why a scan started by itself, until it is done.
    why: Option<crate::live::Reason>,
    /// The dir scanned again, by its path below the root, while the tree
    /// shown stays.
    rescanning: Option<Vec<u8>>,
    /// When the tree shown was scanned, as the dirs not followed stand.
    scanned: SystemTime,
    /// Listings by record id, kept until another scan's tree is shown.
    files: HashMap<u32, Files>,
    /// Sorted rows by record id, for the view shown.
    rows: HashMap<u32, Vec<Row>>,
    /// Names of the dirs entered, from the root down.
    trail: Vec<Box<[u8]>>,
    /// Record ids of the root and of each dir in `trail`.
    dirs: Vec<u32>,
    /// The name of the row the user moved to in the current dir, which
    /// keeps the cursor on that item while sizes change. `None`, or not
    /// among `current`: the top row.
    selected: Option<Box<[u8]>>,
    /// The rows of the current dir that match `filter`.
    current: Vec<Row>,
    /// The selected index in `current`.
    cursor: usize,
    /// The first row of `current` drawn.
    offset: usize,
    /// The first row of the parent column drawn, once the wheel scrolled
    /// it, until the current dir changes. `None`: the current dir's row
    /// shows.
    parent_offset: Option<usize>,
    /// The first row of the preview column drawn, until the selection
    /// changes.
    preview_offset: usize,
    /// Where the columns, the denied list or the largest files were last
    /// drawn, for the page keys and the mouse.
    body: Rect,
    clicks: Clicks,
    filter: String,
    /// Keys type into `filter`.
    typing: bool,
    /// Shown in the footer until the next key.
    message: Option<String>,
    /// The first row of the denied list drawn, while it is shown.
    panel: Option<usize>,
    /// The largest files list, while it is shown.
    top: Option<Top>,
    /// Sizes when the first scan of the session was done.
    baseline: Option<Baseline>,
    /// The `i` pop-up about the row at the cursor, while it is shown.
    info: bool,
    /// The running scan's latest snapshot, `(bytes, folders)`, also while
    /// a saved scan is shown, and when it started, from the first poll.
    progress: Option<(u64, usize)>,
    started: Option<SystemTime>,
    /// Paths picked, below the root, in the order picked. Saved with the
    /// scan, so they outlive the session.
    picks: Vec<Vec<u8>>,
    /// The list of picks, while it is shown.
    picking: Option<Picks>,
    /// The sizes of the files of each dir shown since the session started,
    /// by the dir's path below the root, as when first shown.
    seen: HashMap<Vec<u8>, HashMap<Box<[u8]>, u64>>,
    /// Rows sort by how much they grew since the session started, not by
    /// size.
    by_change: bool,
}

/// The cursor and first row drawn of the list of picks.
#[derive(Default)]
struct Picks {
    cursor: usize,
    offset: usize,
}

/// The sizes of every dir when the first scan of the session was done, so
/// each dir shows how much it grew or shrank since.
struct Baseline {
    /// Total bytes by record id of the tree shown. Dirs new since have no
    /// entry, as their records come after.
    sizes: Vec<u64>,
    at: SystemTime,
    /// The tree `sizes` are of, while a full scan, whose ids differ, runs.
    of: Option<Tree>,
}

impl Baseline {
    /// The total bytes of dir `id` at the start, 0 if it is new since.
    fn size(&self, id: u32) -> u64 {
        self.sizes.get(id as usize).copied().unwrap_or(0)
    }

    /// Gives each record of `new` from id `from` on the start size of the
    /// dir at the same path in `old`, an earlier tree of the same root, or
    /// 0 if there is none: from `from` on, the ids of `new` are new or
    /// numbered again ([`moved_from`]).
    fn carry(&mut self, old: &Tree, new: &Tree, from: usize) {
        let mut sizes = self.sizes.clone();
        sizes.truncate(from);
        sizes.resize(new.len(), 0);
        // the dir of `old` each record of `new` from `from` is at
        let mut was: HashMap<u32, u32> = HashMap::new();
        // children of dirs of `old` by name, built when first needed
        let mut kids: HashMap<u32, HashMap<&[u8], u32>> = HashMap::new();
        for k in from..new.len() {
            let k = k as u32;
            let at = match k {
                0 => Some(0),
                _ => {
                    let r = new.record(k);
                    let parent = match r.parent as usize >= from {
                        true => was.get(&r.parent).copied(),
                        false => Some(r.parent),
                    };
                    parent.and_then(|p| {
                        let named = kids.entry(p).or_insert_with(|| {
                            (old.children(p).iter())
                                .map(|&c| (old.name(c), c))
                                .collect()
                        });
                        named.get(new.name(k)).copied()
                    })
                }
            };
            if let Some(o) = at {
                was.insert(k, o);
                sizes[k as usize] = self.size(o);
            }
        }
        self.sizes = sizes;
    }
}

/// The first id of `new` that no longer names the folder it named in
/// `old`, an earlier tree of the same root: where new records start, or
/// where a compaction or a full scan numbered them again.
fn moved_from(new: &Tree, old: &Tree) -> u32 {
    let same = |k: u32| {
        let (a, b) = (old.record(k), new.record(k));
        a.parent == b.parent && old.name(k) == new.name(k)
    };
    let n = old.len().min(new.len()) as u32;
    (0..n).find(|&k| !same(k)).unwrap_or(n)
}

/// A tree as shown, with what drawing it needs.
struct View {
    tree: Tree,
    /// Path and record id of every denied dir, by path.
    denied: Vec<(Vec<u8>, u32)>,
    /// `(bytes, path)` of the largest files, largest first.
    largest: Vec<(u64, Vec<u8>)>,
    status: Status,
    /// Shows reclaimable sizes.
    reclaimable: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    /// A snapshot of a running scan: sizes are lower bounds.
    Scanning,
    /// Loaded from the store, saved `at` this time, shown until the scan
    /// is done.
    Saved {
        at: SystemTime,
    },
    Done,
}

impl View {
    fn new(tree: Tree, status: Status, reclaimable: bool) -> Self {
        let ids = 0..tree.len() as u32;
        let mut denied: Vec<_> = (ids.filter(|&i| tree.record(i).flags & Record::DENIED != 0))
            .map(|i| (dir_path(&tree, i), i))
            .collect();
        denied.sort_unstable();
        Self {
            denied,
            largest: (tree.largest_files(LARGEST).into_iter())
                .map(|(path, bytes)| (bytes, path.into_os_string().into_vec()))
                .collect(),
            tree,
            status,
            reclaimable,
        }
    }
}

/// The files of a dir, `(name, allocated bytes, reclaimable bytes)`:
/// every entry but subdirectories. Reclaimable bytes are 0 unless asked
/// for.
type Files = Vec<File>;

/// One row of a column.
#[derive(Clone, Copy, PartialEq)]
struct Row {
    size: u64,
    /// Reclaimable bytes, 0 unless asked for.
    private: u64,
    item: Item,
    /// From [`labels::label`], for a dir.
    label: Option<Label>,
    /// Bytes a dir grew by since the session started, or shrank by if
    /// negative. 0 for a file.
    delta: i64,
}

#[derive(Clone, Copy, PartialEq)]
enum Item {
    /// A subdirectory, by record id.
    Dir(u32),
    /// A file, by index in its dir's [`Files`].
    File(u32),
}

/// The largest files list: rows of [`View::largest`] that are still there.
#[derive(Default)]
struct Top {
    /// The selected index among the rows still there.
    cursor: usize,
    /// The first of those rows drawn.
    offset: usize,
    /// Whether each file checked is still there, by path. Each is checked
    /// once while the list is shown, when first drawn.
    there: HashMap<Vec<u8>, bool>,
}

impl Top {
    /// The paths of `view`'s largest files not found gone.
    fn paths<'a>(&'a self, view: &'a View) -> impl Iterator<Item = &'a [u8]> {
        let paths = view.largest.iter().map(|(_, path)| &path[..]);
        paths.filter(|p| self.there.get(*p) != Some(&false))
    }
}

impl Browser {
    /// Shows `saved`, if any, until [`Browser::scan`] and
    /// [`Browser::poll`] bring a fresh scan. `display` is the root as the
    /// title shows it. `used` is the bytes in use on the volume, when
    /// `root` is a volume's root, for the not accounted for line.
    /// `reclaimable` scans with reclaimable sizes and shows them; then
    /// `saved` only shows if it has them too.
    pub fn new(
        root: &Path,
        display: &str,
        env: Env,
        saved: Option<Saved>,
        used: Option<u64>,
        reclaimable: bool,
    ) -> Self {
        let mut b = Self {
            root: root.into(),
            real: std::fs::canonicalize(root).ok(),
            real_home: (env.home.as_ref()).and_then(|h| std::fs::canonicalize(h).ok()),
            display: display.into(),
            env,
            // a filesystem that reports none in use reports nothing useful
            used: used.filter(|&u| u > 0),
            reclaimable,
            view: None,
            live: None,
            why: None,
            rescanning: None,
            scanned: SystemTime::UNIX_EPOCH,
            files: HashMap::new(),
            rows: HashMap::new(),
            trail: Vec::new(),
            dirs: vec![0],
            selected: None,
            current: Vec::new(),
            cursor: 0,
            offset: 0,
            parent_offset: None,
            preview_offset: 0,
            body: Rect::default(),
            clicks: Clicks::default(),
            filter: String::new(),
            typing: false,
            message: None,
            panel: None,
            top: None,
            baseline: None,
            info: false,
            progress: None,
            started: None,
            picks: Vec::new(),
            picking: None,
            seen: HashMap::new(),
            by_change: false,
        };
        if let Some(cache) = &b.env.cache {
            match cache.load_picks(root) {
                Ok(picks) => b.picks = picks,
                Err(e) => b.message = Some(format!("cannot read picks: {e}")),
            }
        }
        if let Some(saved) = saved.filter(|s| s.reclaimable || !reclaimable) {
            b.show(saved.tree, Status::Saved { at: saved.modified });
        }
        b
    }

    /// Scans the root on another thread, then follows the changes on disk.
    pub fn scan(&mut self) {
        self.keep_baseline();
        self.why = None;
        self.rescanning = None;
        (self.progress, self.started) = (None, None);
        let opts = LiveOptions {
            scan: ScanOptions {
                reclaimable: self.reclaimable,
                ..ScanOptions::default()
            },
            interval: self.env.interval,
            ignore: self.env.cache.as_ref().map(|c| c.path().into()),
            inotify: self.env.inotify,
            // the dirs shown are followed: a watch on every dir costs kernel
            // memory and seconds on a large tree
            shown_only: true,
        };
        self.live = Some(live(&self.root, opts));
    }

    /// What the session's changes are counted against, until the full
    /// scan starting now, whose ids differ, is done.
    fn keep_baseline(&mut self) {
        if let (Some(base), Some(view)) = (&mut self.baseline, &self.view)
            && view.status == Status::Done
            && base.of.is_none()
        {
            base.of = Some(view.tree.clone());
        }
    }

    /// A scan runs: the first, after lost changes, or of one dir.
    fn busy(&self) -> bool {
        self.live.as_ref().is_some_and(Live::busy)
    }

    /// Prints sizes in `units` from now on.
    pub fn set_units(&mut self, units: Units) {
        self.env.units = units;
    }

    /// Wakes a [`Browser::poll`] waiting on another thread. `None` if it
    /// would not wait, with no scan to wait on.
    pub fn waker(&self) -> Option<Waker> {
        self.live.as_ref().filter(|l| !l.is_done()).map(Live::waker)
    }

    /// Scans the dir at the cursor again, or at a file the current dir, on
    /// another thread, while the tree shown stays, and follows changes
    /// meanwhile. At the root, a full scan.
    fn rescan_dir(&mut self) {
        let Some(view) = self.view.as_ref().filter(|v| v.status == Status::Done) else {
            return;
        };
        let d = match self.current.get(self.cursor) {
            Some(Row {
                item: Item::Dir(k), ..
            }) => *k,
            _ => self.dir(),
        };
        if d == 0 {
            return self.rescan_all();
        }
        self.rescanning = Some(below(&view.tree, d));
        if let Some(live) = &mut self.live {
            live.rescan(d);
        }
    }

    /// Scans all of the root again, showing it as it grows.
    fn rescan_all(&mut self) {
        // first, so the session's changes are counted against the tree
        // shown
        self.scan();
        self.view = None;
        self.files.clear();
        self.rows.clear();
        self.current.clear();
    }

    /// Stops a running scan, and saves what it found, so the next run
    /// finishes it.
    pub fn quit(&mut self) -> io::Result<()> {
        let tree = self.live.take().and_then(Live::stop);
        match (&self.env.cache, tree) {
            (Some(cache), Some(tree)) => cache.save(&self.root, &tree, self.reclaimable),
            _ => Ok(()),
        }
    }

    /// Catches up with the scan: shows a snapshot of it, unless a saved
    /// scan is shown, or once it is done, its tree, and saves that. Then
    /// shows the changes on disk since the last poll. Waits up to `timeout`
    /// for one of those, or until woken ([`Browser::waker`]). Returns
    /// whether a scan still runs.
    pub fn poll(&mut self, now: SystemTime, timeout: Duration) -> bool {
        let Some(live) = &mut self.live else {
            return false;
        };
        if live.busy() && self.started.is_none() {
            self.started = Some(now);
        }
        match live.wait(timeout) {
            None => {}
            Some(Err(e)) => {
                self.rescanning = None;
                self.message = Some(format!("cannot scan: {e}"));
            }
            // a saved scan shows until the scan is done
            Some(Ok(Event::Scanning(tree))) => {
                self.progress = Some((tree.size(0), tree.len()));
                if !matches!(
                    self.view.as_ref().map(|v| v.status),
                    Some(Status::Saved { .. })
                ) {
                    self.show(tree, Status::Scanning);
                }
            }
            Some(Ok(Event::Ready(tree))) => {
                self.why = None;
                (self.progress, self.started) = (None, None);
                self.scanned = now;
                self.save(&tree);
                match &mut self.baseline {
                    Some(base) => {
                        if let Some(old) = base.of.take() {
                            base.carry(&old, &tree, moved_from(&tree, &old) as usize);
                        }
                    }
                    None => {
                        self.baseline = Some(Baseline {
                            sizes: (0..tree.len() as u32).map(|i| tree.size(i)).collect(),
                            at: now,
                            of: None,
                        });
                    }
                }
                self.show(tree, Status::Done);
            }
            Some(Ok(Event::Changed(tree, _))) => {
                // new records, or all of them numbered again
                if let (Some(base), Some(old)) = (&mut self.baseline, &self.view) {
                    base.carry(&old.tree, &tree, moved_from(&tree, &old.tree) as usize);
                }
                if self.rescanning.take().is_some() {
                    self.save(&tree);
                }
                self.show(tree, Status::Done);
            }
            Some(Ok(Event::Rescanning(why))) => {
                self.keep_baseline();
                self.why = Some(why);
            }
        }
        self.busy()
    }

    /// Handles a key. Reveal and open go through `run`, given a program and
    /// its arguments. Breaks when the user quits.
    pub fn key(
        &mut self,
        key: KeyEvent,
        run: &mut dyn FnMut(&str, &[OsString]) -> io::Result<()>,
    ) -> ControlFlow<()> {
        self.message = None;
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ControlFlow::Break(());
        }
        let page = usize::from(self.body.height);
        if let Some(offset) = &mut self.panel {
            let n = self.view.as_ref().map_or(0, |v| v.denied.len());
            // the head takes a row; clamped when drawn
            if let Some(at) = nav(key.code, *offset, n, page.saturating_sub(1)) {
                *offset = at;
            }
            match key.code {
                KeyCode::Char('d') | KeyCode::Esc => self.panel = None,
                KeyCode::Char('q') => return ControlFlow::Break(()),
                _ => {}
            }
            return ControlFlow::Continue(());
        }
        if let Some(list) = &mut self.picking {
            // the head takes a row
            if let Some(at) = nav(
                key.code,
                list.cursor,
                self.picks.len(),
                page.saturating_sub(1),
            ) {
                list.cursor = at;
            }
            let at = self.picks.get(list.cursor).cloned();
            match key.code {
                KeyCode::Char(c @ ('r' | 'o')) => {
                    let path = at.map(|p| self.below_root(&p));
                    self.reveal(path, c == 'r', run);
                }
                KeyCode::Char(' ') => {
                    if let Some(p) = at {
                        self.pick(p);
                    }
                }
                KeyCode::Char('p') | KeyCode::Esc => self.picking = None,
                KeyCode::Char('q') => return ControlFlow::Break(()),
                _ => {}
            }
            return ControlFlow::Continue(());
        }
        if self.info {
            match key.code {
                KeyCode::Char('i') | KeyCode::Esc => self.info = false,
                KeyCode::Char('q') => return ControlFlow::Break(()),
                _ => {}
            }
            return ControlFlow::Continue(());
        }
        if let Some(top) = &mut self.top {
            let n = (self.view.as_ref()).map_or(0, |v| top.paths(v).count());
            if let Some(at) = nav(key.code, top.cursor, n, page) {
                top.cursor = at;
            }
            match key.code {
                KeyCode::Char(c @ ('r' | 'o')) => {
                    let path = (self.view.as_ref()).and_then(|v| top.paths(v).nth(top.cursor));
                    let path = path.map(<[u8]>::to_vec);
                    self.reveal(path, c == 'r', run);
                }
                KeyCode::Char('t') | KeyCode::Esc => self.top = None,
                KeyCode::Char('q') => return ControlFlow::Break(()),
                _ => {}
            }
            return ControlFlow::Continue(());
        }
        match key.code {
            KeyCode::Esc if self.typing || !self.filter.is_empty() => {
                self.filter.clear();
                self.typing = false;
            }
            KeyCode::Enter if self.typing => self.typing = false,
            KeyCode::Backspace if self.typing => {
                self.filter.pop();
            }
            KeyCode::Char(c) if self.typing => self.filter.push(c),
            code if let Some(at) = nav(code, self.cursor, self.current.len(), page) => {
                self.select(at);
            }
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => self.enter(),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.back(),
            KeyCode::Char(c @ ('r' | 'o')) => self.reveal(self.cursor_path(), c == 'r', run),
            // a running scan is not restarted
            KeyCode::Char('s') if !self.busy() => self.rescan_dir(),
            KeyCode::Char('S') if !self.busy() => self.rescan_all(),
            KeyCode::Char('/') => self.typing = true,
            KeyCode::Char('d') => self.panel = Some(0),
            KeyCode::Char('t') => self.top = Some(Top::default()),
            KeyCode::Char('i') if !self.current.is_empty() => self.info = true,
            KeyCode::Char(' ') => {
                if let Some(p) = self.cursor_below() {
                    self.pick(p);
                }
            }
            KeyCode::Char('p') => self.picking = Some(Picks::default()),
            KeyCode::Char('c') => {
                self.by_change = !self.by_change;
                self.rows.clear();
            }
            KeyCode::Char('q') | KeyCode::Esc => return ControlFlow::Break(()),
            _ => {}
        }
        self.refresh();
        ControlFlow::Continue(())
    }

    /// Handles a mouse event at `now`. A click selects the row under it: in
    /// the parent column, that goes up to it, and in the preview column,
    /// into the selected dir. A second click on the same row within 400 ms
    /// goes into the row the first selected. The wheel moves the cursor of
    /// the current column, the largest files and the volume list, and
    /// scrolls the other columns and the denied list.
    pub fn mouse(&mut self, event: MouseEvent, now: SystemTime) {
        let key = match event.kind {
            MouseEventKind::Down(MouseButton::Left) => None,
            MouseEventKind::ScrollUp => Some(KeyCode::Up),
            MouseEventKind::ScrollDown => Some(KeyCode::Down),
            _ => return,
        };
        let at = Position::new(event.column, event.row);
        if self.view.is_none() || !self.body.contains(at) {
            return;
        }
        self.message = None;
        // `nav` moves by one row for these keys
        let moved = |from: usize, n: usize| key.and_then(|k| nav(k, from, n, 1)).unwrap_or(from);
        let dy = usize::from(at.y - self.body.y);
        if let Some(offset) = &mut self.panel {
            // clamped when drawn
            *offset = moved(*offset, usize::MAX);
            return;
        }
        if let Some(top) = &mut self.top {
            let n = (self.view.as_ref()).map_or(0, |v| top.paths(v).count());
            match key {
                Some(_) => top.cursor = moved(top.cursor, n),
                None if top.offset + dy < n => top.cursor = top.offset + dy,
                None => {}
            }
            return;
        }
        if let Some(list) = &mut self.picking {
            let n = self.picks.len();
            match key {
                Some(_) => list.cursor = moved(list.cursor, n),
                // the head takes the first row
                None if dy > 0 && list.offset + dy - 1 < n => list.cursor = list.offset + dy - 1,
                None => {}
            }
            return;
        }
        let columns = self.columns();
        let Some(&(col, area)) = columns.iter().find(|(_, a)| a.contains(at)) else {
            return;
        };
        let Some((d, rows, offset, _)) = self.listing(col, area.height.into()) else {
            return;
        };
        if key.is_some() {
            let n = rows.len();
            match col {
                Col::Above(1) => self.parent_offset = Some(moved(offset, n)),
                // older levels keep the dir gone into in view
                Col::Above(_) => {}
                Col::Current => self.select(moved(self.cursor, n)),
                Col::Preview => self.preview_offset = moved(offset, n),
            }
            self.refresh();
            return;
        }
        let index = offset + dy;
        let Some(row) = rows.get(index) else {
            return;
        };
        let name = name(self.view.as_ref().unwrap(), &self.files[&d], row.item).into();
        if self.clicks.double(now, (area.x, at.y)) {
            // into the row the first click selected
            self.enter();
        } else {
            match col {
                Col::Above(level) => (0..level).for_each(|_| self.back()),
                Col::Current => {}
                Col::Preview => self.enter(),
            }
            self.selected = Some(name);
            self.preview_offset = 0;
        }
        self.refresh();
    }

    /// Whether keys type into the filter.
    pub(crate) fn typing(&self) -> bool {
        self.typing
    }

    /// The root as given.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the root is shown with nothing open over it, so going up
    /// would leave the browser.
    pub(crate) fn at_root(&self) -> bool {
        self.trail.is_empty() && !self.typing && self.panel.is_none() && self.top.is_none()
    }

    /// Draws the title, the three columns, the denied list or the largest
    /// files, and the footer. `now` dates a saved scan.
    pub fn draw(&mut self, frame: &mut Frame, now: SystemTime) {
        let styles = Styles::new(self.env.color);
        let width = frame.area().width.into();
        let footer: Vec<Line> = (self.status(styles, width).into_iter())
            .chain([self.footer(styles, width)])
            .collect();
        let [top, body, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(footer.len() as u16),
        ])
        .areas(frame.area());
        self.body = body;
        let title = self.title(now, styles, width);
        let buf = frame.buffer_mut();
        buf.set_line(top.x, top.y, &title, top.width);
        for (y, line) in (bottom.y..).zip(footer) {
            buf.set_line(bottom.x, y, &line, bottom.width);
        }
        let Some(view) = &self.view else {
            return;
        };
        if let Some(offset) = &mut self.panel {
            let terminal = &self.env.terminal;
            *offset = panel(buf, body, view, terminal, *offset, styles);
            return;
        }
        if let Some(top) = &mut self.top {
            top_files(buf, body, view, top, styles, self.env.units);
            return;
        }
        if let Some(list) = &mut self.picking {
            let root = self.root.as_os_str().as_bytes();
            picks(
                buf,
                body,
                view,
                root,
                &self.picks,
                list,
                styles,
                self.env.units,
            );
            return;
        }
        let height = body.height.into();
        self.offset = scroll(self.offset, self.cursor, height);
        let columns = self.columns();
        // older levels come and go with the width
        for &(col, _) in &columns {
            if let Col::Above(level) = col {
                self.ensure(self.dirs[self.dirs.len() - 1 - level]);
            }
        }
        for &(col, area) in &columns {
            let style = match col {
                Col::Above(_) => Some(styles.parent),
                Col::Current => Some(styles.selected),
                Col::Preview => None,
            };
            if let Some((d, rows, offset, mark)) = self.listing(col, height) {
                let side = col != Col::Current;
                self.column(buf, area, d, rows, offset, mark.zip(style), side);
            }
        }
        if self.current.is_empty() && !self.filter.is_empty() {
            let (_, area) = columns.iter().find(|(c, _)| *c == Col::Current).unwrap();
            let text = format!("no matches for '{}'", self.filter);
            buf.set_stringn(area.x, area.y, text, area.width.into(), styles.dim);
        }
        if self.info {
            self.draw_info(buf, now, styles);
        }
    }

    /// The columns drawn, left to right, and where. With a dir above the
    /// current one and 100 columns or more: its parent, the current dir
    /// and the preview, 20 / 50 / 30 of the width but at most 30, 60 and
    /// 40 wide; width left over shows older levels, 30 each, nearest
    /// first. At the root or under 100 columns: current and preview, 60 /
    /// 40, at most 60 and 40 wide. Under 60: the current one alone.
    fn columns(&self) -> Vec<(Col, Rect)> {
        let w = usize::from(self.body.width);
        let above = self.dirs.len() - 1;
        let share = |of: usize, tenths: usize| (of * tenths + 5) / 10;
        let mut widths = match w {
            ..60 => vec![(Col::Current, w)],
            _ if w < 100 || above == 0 => {
                let current = share(w - 1, 6).min(60);
                vec![
                    (Col::Current, current),
                    (Col::Preview, (w - 1 - current).min(40)),
                ]
            }
            _ => {
                let parent = share(w - 2, 2).min(30);
                let current = share(w - 2, 5).min(60);
                let preview = (w - 2 - parent - current).min(40);
                vec![
                    (Col::Above(1), parent),
                    (Col::Current, current),
                    (Col::Preview, preview),
                ]
            }
        };
        // older levels where three capped columns leave room
        let mut spare = w - (widths.iter().map(|&(_, w)| w + 1).sum::<usize>() - 1);
        for level in 2..=above {
            if widths.len() < 3 || spare < OLDER + 1 {
                break;
            }
            widths.insert(0, (Col::Above(level), OLDER));
            spare -= OLDER + 1;
        }
        let mut x = self.body.x;
        (widths.into_iter())
            .map(|(col, width)| {
                let area = Rect {
                    x,
                    width: width as u16,
                    ..self.body
                };
                x += width as u16 + 1;
                (col, area)
            })
            .collect()
    }

    /// What column `col` lists, in a column `height` rows high: the dir,
    /// its rows, the first of them drawn, and the marked one, if any.
    /// Nothing for the preview of a file.
    ///
    /// The columns of dirs above and the preview show sizes and names
    /// only; the current one adds each row's share and change.
    fn listing(&self, col: Col, height: usize) -> Option<(u32, &[Row], usize, Option<usize>)> {
        let d = self.dir();
        match col {
            Col::Above(level) => {
                let at = self.dirs.len() - 1 - level;
                let (dir, child) = (self.dirs[at], self.dirs[at + 1]);
                let rows = &self.rows[&dir];
                let marked = rows
                    .iter()
                    .position(|r| r.item == Item::Dir(child))
                    .unwrap();
                let offset = match self.parent_offset.filter(|_| level == 1) {
                    Some(offset) => offset.min(rows.len().saturating_sub(height)),
                    None => scroll(0, marked, height),
                };
                Some((dir, rows, offset, Some(marked)))
            }
            Col::Current => Some((d, &self.current, self.offset, Some(self.cursor))),
            Col::Preview => {
                let Some(&Row {
                    item: Item::Dir(k), ..
                }) = self.current.get(self.cursor)
                else {
                    return None;
                };
                let rows = &self.rows[&k];
                let offset = self.preview_offset.min(rows.len().saturating_sub(height));
                Some((k, rows, offset, None))
            }
        }
    }

    /// Draws `rows[offset..]` of dir `d` that fit in `area`, with `mark`, a
    /// row index and its style, if given. A `side` column shows sizes and
    /// names only.
    #[allow(clippy::too_many_arguments)]
    fn column(
        &self,
        buf: &mut Buffer,
        area: Rect,
        d: u32,
        rows: &[Row],
        offset: usize,
        mark: Option<(usize, Style)>,
        side: bool,
    ) {
        let view = self.view.as_ref().unwrap();
        let styles = Styles::new(self.env.color);
        let total = view.tree.size(d);
        let dir = below(&view.tree, d);
        let look = Look {
            side,
            changes: !side && rows.iter().any(|r| r.delta != 0),
            percent: !side && self.body.width >= 50,
            units: self.env.units,
        };
        let lines = (area.y..area.bottom()).zip(rows.iter().enumerate().skip(offset));
        for (y, (i, row)) in lines {
            let files = &self.files[&d];
            let picked = !self.picks.is_empty() && {
                let mut path = dir.clone();
                join_below(&mut path, name(view, files, row.item));
                self.picks.contains(&path)
            };
            let width = area.width.into();
            let line = line(view, files, row, total, look, picked, width, styles);
            buf.set_line(area.x, y, &line, area.width);
            if let Some((_, style)) = mark.filter(|&(at, _)| at == i) {
                buf.set_style(
                    Rect {
                        y,
                        height: 1,
                        ..area
                    },
                    style,
                );
            }
        }
    }

    fn save(&mut self, tree: &Tree) {
        if let Some(cache) = &self.env.cache
            && let Err(e) = cache.save(&self.root, tree, self.reclaimable)
        {
            self.message = Some(format!("warning: scan not saved: {e}"));
        }
    }

    /// Shows `tree`, keeping the place in it where that still exists.
    fn show(&mut self, tree: Tree, status: Status) {
        let view = View::new(tree, status, self.reclaimable);
        // record ids, so listings, carry over between snapshots of one scan
        let snapshot = |v: &View| v.status == Status::Scanning;
        if !self.view.as_ref().is_some_and(snapshot) {
            self.files.clear();
        }
        self.rows.clear();
        self.view = Some(view);
        self.refresh();
    }

    /// The record id of the current dir.
    fn dir(&self) -> u32 {
        *self.dirs.last().unwrap()
    }

    /// Recomputes the dirs of `trail`, the rows of the three columns and
    /// the cursor, after a key or a new view.
    fn refresh(&mut self) {
        let Some(view) = &self.view else {
            return;
        };
        let mut dirs = vec![0];
        for name in &self.trail {
            let parent = *dirs.last().unwrap();
            let named = |&k: &u32| view.tree.name(k) == &name[..];
            match view.tree.children(parent).iter().copied().find(named) {
                Some(k) => dirs.push(k),
                None => break,
            }
        }
        if dirs.len() <= self.trail.len() {
            // gone from this view: stay in the deepest dir still there
            self.trail.truncate(dirs.len() - 1);
            self.selected = None;
            self.filter.clear();
        }
        self.dirs = dirs;
        let d = self.dir();
        self.ensure(d);
        if let [.., parent, _] = self.dirs[..] {
            self.ensure(parent);
        }

        let view = self.view.as_ref().unwrap();
        let files = &self.files[&d];
        let name = |r: &Row| name(view, files, r.item);
        let filter = self.filter.as_bytes();
        self.current = self.rows[&d]
            .iter()
            .filter(|r| filter.is_empty() || name(r).windows(filter.len()).any(|w| w == filter))
            .copied()
            .collect();
        self.cursor = self
            .selected
            .as_deref()
            .and_then(|s| self.current.iter().position(|r| name(r) == s))
            .unwrap_or(0);
        let mut shown = self.dirs[self.dirs.len().saturating_sub(2)..].to_vec();
        if let Some(&Row {
            item: Item::Dir(k), ..
        }) = self.current.get(self.cursor)
        {
            self.ensure(k);
            shown.push(k);
        }
        if let Some(live) = &mut self.live {
            live.follow(&shown);
        }
    }

    /// Lists and sorts dir `d`, unless done for this view.
    fn ensure(&mut self, d: u32) {
        let Some(view) = &self.view else {
            return;
        };
        if self.rows.contains_key(&d) {
            return;
        }
        let private = self.reclaimable;
        // the scan did not go into a denied dir, so neither does the browser
        let list = || match view.tree.error(d) {
            Some(_) => Vec::new(),
            None => view.tree.files(d, private).unwrap_or_default(),
        };
        let files = self.files.entry(d).or_insert_with(list);
        let path = dir_path(&view.tree, d);
        // the root's name is the whole path
        let parent = Path::new(OsStr::from_bytes(view.tree.name(d)));
        let parent = parent.file_name().unwrap_or_default().as_bytes();
        let sibling = |file: &str| files.iter().any(|f| *f.name == *file.as_bytes());
        let real = self.real.as_ref().map(|real| {
            let mut real = real.as_os_str().as_bytes().to_vec();
            join(&mut real, &below(&view.tree, d));
            real
        });
        let home = self.real_home.as_ref().map(|h| h.as_os_str().as_bytes());
        let dirs = view.tree.children(d).iter().map(|&k| {
            let name = view.tree.name(k);
            let mut at = path.clone();
            join(&mut at, name);
            let at = Path::new(OsStr::from_bytes(&at));
            let real = real.as_ref().map(|real| {
                let mut real = real.clone();
                join(&mut real, name);
                real
            });
            let dir = labels::Dir {
                name,
                parent,
                path: real.as_deref(),
                home,
            };
            let inside = |file: &str| sys::exists(&at.join(file));
            let system = || sys::restricted(at);
            let tagged = || sys::starts_with(&at.join("CACHEDIR.TAG"), CACHEDIR_SIGNATURE);
            let size = view.tree.size(k);
            // snapshots of a scan are of other ids
            let delta = match (&self.baseline, view.status) {
                (Some(base), Status::Done) => size as i64 - base.size(k) as i64,
                _ => 0,
            };
            Row {
                size,
                private: view.tree.reclaimable(k),
                item: Item::Dir(k),
                label: labels::label(&dir, sibling, inside, system, tagged),
                delta,
            }
        });
        // a file's size when its dir was first shown since the session
        // started; one new since in a dir shown before grew from nothing
        let dir = below(&view.tree, d);
        let counting = self.baseline.is_some() && view.status == Status::Done;
        let known = self.seen.get(&dir);
        let file_delta = |name: &[u8], size: u64| match (counting, known) {
            (true, Some(sizes)) => size as i64 - sizes.get(name).copied().unwrap_or(0) as i64,
            _ => 0,
        };
        let mut rows: Vec<Row> = (files.iter().enumerate())
            .map(|(i, f)| Row {
                size: f.bytes,
                private: f.private,
                item: Item::File(i as u32),
                label: None,
                delta: file_delta(&f.name, f.bytes),
            })
            .chain(dirs)
            .collect();
        // names are unique in a dir, so these orders are total
        match self.by_change {
            true => rows.sort_unstable_by(|a, b| {
                let key = |r: &Row| (r.size, name(view, files, r.item));
                b.delta.cmp(&a.delta).then(largest_first(key(a), key(b)))
            }),
            false => rows.sort_unstable_by(|a, b| {
                let key = |r: &Row| (r.size, name(view, files, r.item));
                largest_first(key(a), key(b))
            }),
        }
        if counting && known.is_none() {
            let sizes = files.iter().map(|f| (f.name.clone(), f.bytes)).collect();
            self.seen.insert(dir, sizes);
        }
        self.rows.insert(d, rows);
    }

    /// Moves the cursor to row `at` of the current column.
    fn select(&mut self, at: usize) {
        if let (Some(view), Some(row)) = (&self.view, self.current.get(at)) {
            let files = &self.files[&self.dir()];
            self.selected = Some(name(view, files, row.item).into());
            self.preview_offset = 0;
        }
    }

    /// The row at the cursor and its name.
    fn at_cursor(&self) -> Option<(Row, &[u8])> {
        let (view, row) = (self.view.as_ref()?, *self.current.get(self.cursor)?);
        Some((row, name(view, &self.files[&self.dir()], row.item)))
    }

    fn enter(&mut self) {
        if let Some((row, name)) = self.at_cursor()
            && let Item::Dir(_) = row.item
        {
            let name = name.into();
            self.trail.push(name);
            self.selected = None;
            self.leave_column();
        }
    }

    fn back(&mut self) {
        if let Some(name) = self.trail.pop() {
            self.selected = Some(name);
            self.leave_column();
        }
    }

    /// The filter and scroll belong to the column left.
    fn leave_column(&mut self) {
        self.filter.clear();
        self.typing = false;
        self.offset = 0;
        self.parent_offset = None;
        self.preview_offset = 0;
    }

    /// Picks `path`, below the root, or unpicks it if picked, and saves
    /// the picks.
    fn pick(&mut self, path: Vec<u8>) {
        match self.picks.iter().position(|p| *p == path) {
            Some(at) => {
                self.picks.remove(at);
            }
            None => self.picks.push(path),
        }
        if let Some(list) = &mut self.picking {
            list.cursor = list.cursor.min(self.picks.len().saturating_sub(1));
        }
        if let Some(cache) = &self.env.cache
            && let Err(e) = cache.save_picks(&self.root, &self.picks)
        {
            self.message = Some(format!("warning: picks not saved: {e}"));
        }
    }

    /// The path of the row at the cursor, below the root.
    fn cursor_below(&self) -> Option<Vec<u8>> {
        let (view, (_, name)) = (self.view.as_ref()?, self.at_cursor()?);
        let mut path = below(&view.tree, self.dir());
        join_below(&mut path, name);
        Some(path)
    }

    /// `path`, below the root, as a path from where diskuse runs.
    fn below_root(&self, path: &[u8]) -> Vec<u8> {
        let mut full = self.root.as_os_str().as_bytes().to_vec();
        join(&mut full, path);
        full
    }

    /// The path of the row at the cursor.
    fn cursor_path(&self) -> Option<Vec<u8>> {
        let (view, (_, name)) = (self.view.as_ref()?, self.at_cursor()?);
        let mut path = dir_path(&view.tree, self.dir());
        join(&mut path, name);
        Some(path)
    }

    /// Shows `path`, if any, in the file manager, or opens it. Without a
    /// desktop, the footer shows the path instead.
    fn reveal(
        &mut self,
        path: Option<Vec<u8>>,
        reveal: bool,
        run: &mut dyn FnMut(&str, &[OsString]) -> io::Result<()>,
    ) {
        let Some(path) = path else {
            return;
        };
        let path = PathBuf::from(OsString::from_vec(path));
        self.message = match self.env.desktop.command(&path, reveal) {
            Some((program, args)) => run(program, &args)
                .err()
                .map(|e| format!("cannot run {program}: {e}")),
            None => Some(format!("path: {}", path.display())),
        };
    }

    /// The top line: the root, its total, and what to know about it, each
    /// part after a dim `|`, the parts with nothing to say left out, and
    /// in `width` columns the last ones that do not fit.
    fn title(&self, now: SystemTime, styles: &Styles, width: usize) -> Line<'static> {
        let units = self.env.units;
        let fmt = |n| units.format(n);
        let mut parts: Vec<Span> = Vec::new();
        let status = self.view.as_ref().map(|v| v.status);
        // a snapshot, or a full scan runs while no finished tree is shown
        let scanning = self.busy() && status != Some(Status::Done);
        if scanning || status == Some(Status::Scanning) {
            let (bytes, folders) = match (&self.view, self.progress) {
                (_, Some(progress)) => progress,
                (Some(v), None) if v.status == Status::Scanning => (v.tree.size(0), v.tree.len()),
                _ => (0, 0),
            };
            // a whole volume's used bytes, else the last scan's total
            let saved = self.view.as_ref().filter(|v| v.status != Status::Scanning);
            let goal = self.used.or(saved.map(|v| v.tree.size(0)));
            match goal.filter(|&g| g > 0) {
                // a scan counts each copy of a cloned file, the used bytes
                // count it once: past them, they are no target
                Some(goal) if bytes > goal => {
                    parts.extend([
                        Span::raw(format!("scanning {}", fmt(bytes))),
                        Span::raw(format!("{} folders", thousands(folders as u64))),
                    ]);
                }
                Some(goal) => {
                    // the used bytes count more than a scan finds, so it
                    // stops short of 100%
                    let pct = (u128::from(bytes) * 100 / u128::from(goal.max(1))).min(99);
                    parts.push(Span::raw(format!(
                        "scanning {} of ~{} ({pct}%)",
                        fmt(bytes),
                        fmt(goal)
                    )));
                }
                None => {
                    let since = self.started.and_then(|t| now.duration_since(t).ok());
                    parts.extend([
                        Span::raw(format!("scanning {}", fmt(bytes))),
                        Span::raw(format!("{} folders", thousands(folders as u64))),
                        Span::raw(age(since.unwrap_or_default())),
                    ]);
                }
            }
        } else if let Some(view) = &self.view {
            let total = view.tree.size(0);
            match self.used.filter(|_| view.status == Status::Done) {
                Some(used) if total <= used => parts.push(Span::styled(
                    format!("{} of {}", fmt(total), fmt(used)),
                    styles.dir,
                )),
                Some(used) => parts.extend([
                    Span::styled(fmt(total), styles.dir),
                    Span::raw(format!("disk reports {} in use", fmt(used))),
                ]),
                None => parts.push(Span::styled(fmt(total), styles.dir)),
            }
        }
        if let Some(View {
            status: Status::Saved { at },
            tree,
            ..
        }) = &self.view
        {
            let age = age(now.duration_since(*at).unwrap_or_default());
            let incomplete = if tree.stopped() { ", incomplete" } else { "" };
            parts.push(Span::raw(format!(
                "showing the scan saved {age} ago{incomplete}"
            )));
        }
        if let Some(view) = &self.view {
            // how much the root grew or shrank since the session started
            if let Some(base) = self
                .baseline
                .as_ref()
                .filter(|_| view.status == Status::Done)
            {
                let d = view.tree.size(0) as i64 - base.size(0) as i64;
                if d != 0 {
                    let age = age(now.duration_since(base.at).unwrap_or_default());
                    parts.push(Span::raw(format!(
                        "{} since opened {age} ago",
                        signed(d, units)
                    )));
                }
            }
            if !view.denied.is_empty() {
                let n = view.denied.len() as u64;
                let folders = if n == 1 { "folder" } else { "folders" };
                parts.push(Span::raw(format!(
                    "{} {folders} unreadable (d)",
                    thousands(n)
                )));
            }
            // changes in the dirs not shown are not seen since
            if view.status == Status::Done && self.live.as_ref().is_some_and(|l| !l.follows_all()) {
                let age = age(now.duration_since(self.scanned).unwrap_or_default());
                parts.push(Span::raw(format!("scanned {age} ago")));
            }
        }
        if self.by_change {
            parts.push(Span::raw("sorted by change"));
        }
        if let Some(dir) = &self.rescanning {
            parts.push(Span::raw(format!(
                "rescanning {}/...",
                String::from_utf8_lossy(dir)
            )));
        }
        // why a scan started by itself
        if let Some(why) = self.why {
            parts.push(Span::raw(format!("scanning again: {why}")));
        }
        // on a narrow screen the last parts go, down to the first
        let room = width.saturating_sub(width_of(&self.display) + 2);
        let fits = |parts: &[Span]| {
            let text: usize = parts.iter().map(Span::width).sum();
            text + 3 * parts.len().saturating_sub(1) <= room
        };
        while !fits(&parts) && parts.len() > 1 {
            parts.pop();
        }
        let mut spans = vec![Span::raw(self.display.clone())];
        if !parts.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.extend(join_parts(parts.into_iter().map(|p| vec![p]).collect(), styles).spans);
        Line::from(spans)
    }

    /// The bottom line: the message, else the keys of what is shown, as
    /// many as fit in `width`.
    fn footer(&self, styles: &Styles, width: usize) -> Line<'static> {
        let keys = match &self.message {
            Some(message) => return Line::raw(message.clone()),
            None if self.info => INFO_HELP.into(),
            None if self.panel.is_some() => PANEL_HELP.into(),
            None if self.top.is_some() => TOP_HELP.into(),
            None if self.picking.is_some() => PICKS_HELP.into(),
            None if self.typing => format!("/{}  enter keep  esc clear", self.filter),
            None if !self.filter.is_empty() => format!("/{}  esc clear", self.filter),
            None => fit_keys(HELP, width),
        };
        styles.keys(&keys)
    }

    /// The line above the footer, about the row at the cursor: its name,
    /// sizes, what could not be read and its label, each after a dim `|`.
    /// Under 80 columns in fewer words; parts that still do not fit are
    /// left out from the end, down to the name and size.
    fn status(&self, styles: &Styles, width: usize) -> Option<Line<'static>> {
        let shown = self.panel.is_none() && self.top.is_none() && self.picking.is_none();
        let (row, name) = self.at_cursor().filter(|_| shown)?;
        let view = self.view.as_ref()?;
        let units = self.env.units;
        let narrow = width < 80;
        let (name_style, name) = row_name(&row, name, styles);
        let size = Span::styled(units.format(row.size), styles.size(row.size, units));
        let mut sizes = vec![size.clone()];
        let mut warn = None;
        if let Item::Dir(k) = row.item {
            sizes.push(Span::styled("  own ", styles.dim));
            sizes.push(Span::raw(units.format(view.tree.own(k))));
            let below = self.denied_below(k);
            warn = if let Some(error) = view.tree.error(k) {
                Some(format!("unreadable: {error}"))
            } else if view.tree.other_device(k) {
                Some(match narrow {
                    true => "other device".into(),
                    false => "on another device, not scanned".into(),
                })
            } else if below > 0 {
                let n = thousands(below as u64);
                Some(match (narrow, below) {
                    (true, _) => format!("{n} unreadable"),
                    (false, 1) => "1 folder below unreadable".into(),
                    (false, _) => format!("{n} folders below unreadable"),
                })
            } else {
                None
            };
        }
        if view.reclaimable {
            sizes.push(Span::styled("  deletes ", styles.dim));
            sizes.push(Span::raw(units.format(row.private)));
        }
        let label = row.label.map(|l| {
            let text = match narrow {
                true => l.text.to_string(),
                false => format!("{}: {}", l.text, l.short()),
            };
            vec![Span::styled(text, styles.label(l.tier))]
        });
        // least needed last
        let mut parts: Vec<Vec<Span>> = vec![vec![Span::styled(name.clone(), name_style)], sizes];
        parts.extend(warn.map(|w| vec![Span::styled(w, styles.warn)]));
        parts.extend(label);
        let fits = |parts: &[Vec<Span>]| {
            let text: usize = parts.iter().flatten().map(Span::width).sum();
            text + 3 * (parts.len() - 1) <= width
        };
        while !fits(&parts) && parts.len() > 2 {
            parts.pop();
        }
        if !fits(&parts) {
            // without `own`, then the name cut to what is left
            parts[1] = vec![size];
            let room = width.saturating_sub(parts[1][0].width() + 3);
            parts[0] = cut_middle(&name, room, name_style, styles);
        }
        Some(join_parts(parts, styles))
    }

    /// How many denied dirs are below dir `d`, not counting itself.
    fn denied_below(&self, d: u32) -> usize {
        let view = self.view.as_ref().unwrap();
        let mut path = dir_path(&view.tree, d);
        path.push(b'/');
        let start = view.denied.partition_point(|(p, _)| p[..] < path[..]);
        (view.denied[start..].iter())
            .take_while(|(p, _)| p.starts_with(&path))
            .count()
    }

    /// The `i` pop-up: everything about the row at the cursor, in a box in
    /// the middle of the columns.
    fn draw_info(&self, buf: &mut Buffer, now: SystemTime, styles: &Styles) {
        let (Some((row, name)), Some(view)) = (self.at_cursor(), &self.view) else {
            return;
        };
        let units = self.env.units;
        let mut path = self.root.as_os_str().as_bytes().to_vec();
        for name in &self.trail {
            join(&mut path, name);
        }
        join(&mut path, name);
        let mut facts = vec![("path", String::from_utf8_lossy(&path).into_owned())];
        let mut size = units.format(row.size);
        if let Item::Dir(k) = row.item {
            size += &format!(" (own {})", units.format(view.tree.own(k)));
        }
        facts.push(("size", size));
        if view.reclaimable {
            facts.push(("deletes", units.format(row.private)));
        }
        if let Item::Dir(k) = row.item {
            let mut below = 0u64;
            let mut stack = view.tree.children(k).to_vec();
            while let Some(c) = stack.pop() {
                below += 1;
                stack.extend_from_slice(view.tree.children(c));
            }
            facts.push(("folders", format!("{} below", thousands(below))));
            if let Some(error) = view.tree.error(k) {
                facts.push(("unreadable", error));
            }
            let n = self.denied_below(k);
            if n > 0 {
                facts.push((
                    "partial",
                    format!("{} folders below unreadable", thousands(n as u64)),
                ));
            }
        }
        if let Some(l) = row.label {
            facts.push(("label", format!("{}: {}", l.text, l.why)));
        }
        if let Some(base) = &self.baseline {
            let since = age(now.duration_since(base.at).unwrap_or_default());
            let change = match row.delta {
                0 => "none".to_string(),
                d => signed(d, units),
            };
            facts.push(("change", format!("{change} since opened {since} ago")));
        }
        let area = self.body;
        let width = (area.width.saturating_sub(4)).min(72);
        let height = (facts.len() as u16 + 2).min(area.height);
        let r = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        let inner = usize::from(width.saturating_sub(4));
        let edge = |left: &str, text: &str| {
            let text = format!(" {text} ");
            let fill = usize::from(width).saturating_sub(2 + width_of(&text));
            format!(
                "{left}{}{text}{}{left}",
                "-".repeat(fill / 2),
                "-".repeat(fill - fill / 2)
            )
        };
        let name = String::from_utf8_lossy(name);
        buf.set_style(r, Style::reset());
        buf.set_stringn(r.x, r.y, edge("+", &name), width.into(), Style::new());
        for (y, (key, value)) in (r.y + 1..r.bottom() - 1).zip(&facts) {
            let mut spans = vec![
                Span::raw("| "),
                Span::styled(format!("{key:<11}"), styles.dim),
            ];
            spans.extend(cut_middle(
                value,
                inner.saturating_sub(11),
                Style::new(),
                styles,
            ));
            let line = Line::from(spans);
            buf.set_stringn(r.x, y, " ".repeat(width.into()), width.into(), Style::new());
            buf.set_line(r.x, y, &line, width - 1);
            buf.set_stringn(r.right() - 1, y, "|", 1, Style::new());
        }
        let bottom = r.bottom() - 1;
        buf.set_stringn(
            r.x,
            bottom,
            edge("+", "i or esc close"),
            width.into(),
            Style::new(),
        );
    }
}

/// The name bytes of `item`, a row of a dir listed as `files`.
fn name<'a>(view: &'a View, files: &'a Files, item: Item) -> &'a [u8] {
    match item {
        Item::Dir(k) => view.tree.name(k),
        Item::File(i) => &files[i as usize].name,
    }
}

/// How a column draws its rows.
#[derive(Clone, Copy)]
struct Look {
    /// A parent or preview column: sizes and names only.
    side: bool,
    /// The change since the session started, when some row of the column
    /// changed.
    changes: bool,
    /// Each row's share of its dir, on a screen 50 columns wide or more.
    percent: bool,
    units: Units,
}

/// The name of `row` as a column shows it, `/` after a dir, and its style:
/// a dir's is bold, in its label's colour if it has one.
fn row_name(row: &Row, name: &[u8], styles: &Styles) -> (Style, String) {
    let name = String::from_utf8_lossy(name).into_owned();
    match row.item {
        Item::File(_) => (Style::new(), name),
        Item::Dir(_) => {
            let style = match row.label {
                Some(l) => styles.dir.patch(styles.label(l.tier)),
                None => styles.dir,
            };
            (style, name + "/")
        }
    }
}

/// Row `row` of a dir of `total` bytes listed as `files`, in `width`
/// columns: its sizes, unless a side column its change and share of
/// `total`, then its name, cut in the middle to fit.
#[allow(clippy::too_many_arguments)]
fn line(
    view: &View,
    files: &Files,
    row: &Row,
    total: u64,
    look: Look,
    picked: bool,
    width: usize,
    styles: &Styles,
) -> Line<'static> {
    let units = look.units;
    // something below was denied, so the sizes are lower bounds
    let plus = match row.item {
        Item::Dir(k) if view.tree.partial(k) => "+",
        _ => " ",
    };
    let size = |n: u64| Span::styled(format!("{:>10}", units.format(n)), styles.size(n, units));
    let mut spans = vec![size(row.size), Span::styled(plus, styles.dim)];
    if view.reclaimable && !look.side {
        spans.extend([size(row.private), Span::styled(plus, styles.dim)]);
    }
    if look.changes {
        spans.push(match row.delta {
            0 => Span::raw(format!("{:11}", "")),
            d @ 1.. => Span::styled(format!("{:>11}", signed(d, units)), styles.grew),
            d => Span::styled(format!("{:>11}", signed(d, units)), styles.shrank),
        });
    }
    spans.push(Span::raw(" "));
    if look.percent {
        spans.push(Span::styled(
            percent(row.size, total),
            styles.size(row.size, units),
        ));
        spans.push(Span::raw(" "));
    }
    let (style, name) = row_name(row, name(view, files, row.item), styles);
    let mut after = Vec::new();
    if let Item::Dir(k) = row.item {
        match view.tree.error(k) {
            Some(error) => after.push(Span::styled(format!(" (denied: {error})"), styles.denied)),
            None if view.tree.other_device(k) => {
                after.push(Span::styled(" (other device)", styles.dim));
            }
            None => {}
        }
    }
    if picked {
        after.push(Span::styled(" *", styles.picked));
    }
    let used: usize = spans.iter().chain(&after).map(Span::width).sum();
    spans.extend(cut_middle(&name, width.saturating_sub(used), style, styles));
    spans.extend(after);
    Line::from(spans)
}

/// `size`'s share of `total` in 5 columns, the decimal point always in
/// the same place: `99.1%`, ` 0.3%`, `<0.1%`, ` 100%`, blank for 0 B.
pub(crate) fn percent(size: u64, total: u64) -> String {
    if size == 0 || total == 0 {
        return " ".repeat(5);
    }
    // tenths of a percent, rounded
    let tenths =
        ((u128::from(size) * 2000 + u128::from(total)) / (2 * u128::from(total))).min(1000);
    match tenths {
        0 => "<0.1%".into(),
        1000 => " 100%".into(),
        t => format!("{:>2}.{}%", t / 10, t % 10),
    }
}

/// The columns `text` takes on screen.
fn width_of(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// `text` in `style`, cut in the middle to `width` columns if wider, with a
/// dim ellipsis where it was cut, so both ends show.
fn cut_middle(text: &str, width: usize, style: Style, styles: &Styles) -> Vec<Span<'static>> {
    if width_of(text) <= width {
        return vec![Span::styled(text.to_owned(), style)];
    }
    if width == 0 {
        return Vec::new();
    }
    let char_width = |c: char| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
    // the end gets half, the start the rest, after 1 for the ellipsis
    let mut tail_room = (width - 1) / 2;
    let mut head_room = width - 1 - tail_room;
    let mut head = String::new();
    for c in text.chars() {
        let w = char_width(c);
        if w > head_room {
            break;
        }
        head_room -= w;
        head.push(c);
    }
    tail_room += head_room;
    let mut tail = Vec::new();
    for c in text.chars().rev() {
        let w = char_width(c);
        if w > tail_room {
            break;
        }
        tail_room -= w;
        tail.push(c);
    }
    let tail: String = tail.into_iter().rev().collect();
    vec![
        Span::styled(head, style),
        Span::styled("\u{2026}", styles.dim),
        Span::styled(tail, style),
    ]
}

/// `parts` on one line, a dim ` | ` between them.
fn join_parts(parts: Vec<Vec<Span<'static>>>, styles: &Styles) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", styles.dim));
        }
        spans.extend(part);
    }
    Line::from(spans)
}

/// `n` with a comma every 3 digits: `48,210`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `keys`, `key what` pairs two spaces apart, leaving out those just
/// before the last two (`? help  q quit`) until it fits in `width`.
fn fit_keys(keys: &str, width: usize) -> String {
    let mut pairs: Vec<&str> = keys.split("  ").collect();
    while width_of(&pairs.join("  ")) > width && pairs.len() > 3 {
        pairs.remove(pairs.len() - 3);
    }
    pairs.join("  ")
}

/// Draws the denied dirs of `view` from row `offset`, each with why it
/// could not be read. Returns `offset`, clamped so the last row is drawn
/// as low as it can be.
fn panel(
    buf: &mut Buffer,
    area: Rect,
    view: &View,
    terminal: &str,
    offset: usize,
    styles: &Styles,
) -> usize {
    let head = match view.denied.len() {
        0 => "every directory could be read".into(),
        1 => "1 directory could not be read, so its contents are not counted:".into(),
        n => format!("{n} directories could not be read, so their contents are not counted:"),
    };
    buf.set_stringn(area.x, area.y, head, area.width.into(), Style::new());
    let height = usize::from(area.height.saturating_sub(1));
    let offset = offset.min(view.denied.len().saturating_sub(height));
    let lines = (area.y + 1..area.bottom()).zip(&view.denied[offset..]);
    for (y, (path, id)) in lines {
        let why = reason(&view.tree, *id, terminal);
        let line = Line::from_iter([
            Span::raw(format!("{}  ", String::from_utf8_lossy(path))),
            Span::styled(why, styles.denied),
        ]);
        buf.set_line(area.x, y, &line, area.width);
    }
    offset
}

/// Draws the largest files of `view` still there, as paths below the root,
/// from `top`'s offset, the one at its cursor selected. A file is checked
/// with one `lstat` when first drawn, and left out if gone.
fn top_files(
    buf: &mut Buffer,
    area: Rect,
    view: &View,
    top: &mut Top,
    styles: &Styles,
    units: Units,
) {
    if view.largest.is_empty() && view.status == Status::Scanning {
        buf.set_stringn(
            area.x,
            area.y,
            "scanning...",
            area.width.into(),
            Style::new(),
        );
        return;
    }
    let height = usize::from(area.height);
    top.offset = scroll(top.offset, top.cursor, height);
    // every row down to the last drawn is checked
    let mut shown = Vec::new();
    for (size, path) in &view.largest {
        if shown.len() == top.offset + height {
            break;
        }
        let there = match top.there.get(path) {
            Some(&there) => there,
            None => {
                let there = sys::exists(Path::new(OsStr::from_bytes(path)));
                top.there.insert(path.clone(), there);
                there
            }
        };
        if there {
            shown.push((*size, &path[..]));
        }
    }
    // the list may have ended above the cursor
    top.cursor = top.cursor.min(shown.len().saturating_sub(1));
    top.offset = scroll(top.offset, top.cursor, height);
    let root = view.tree.name(0).len();
    let lines = (area.y..area.bottom()).zip(shown.iter().enumerate().skip(top.offset));
    for (y, (i, &(size, path))) in lines {
        let below = path[root..].strip_prefix(b"/").unwrap_or(&path[root..]);
        let line = Line::from_iter([
            Span::styled(
                format!("{:>10}", units.format(size)),
                styles.size(size, units),
            ),
            Span::raw(format!("  {}", String::from_utf8_lossy(below))),
        ]);
        buf.set_line(area.x, y, &line, area.width);
        if i == top.cursor {
            buf.set_style(
                Rect {
                    y,
                    height: 1,
                    ..area
                },
                styles.selected,
            );
        }
    }
}

/// Where `key` moves a cursor from row `at` of `len`, `page` of them shown,
/// if it is a key that moves it: up or down a row or a page, or to the
/// first or the last.
pub(crate) fn nav(key: KeyCode, at: usize, len: usize, page: usize) -> Option<usize> {
    let page = page.max(1);
    let to = match key {
        KeyCode::Up | KeyCode::Char('k') => at.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => at.saturating_add(1),
        KeyCode::PageUp => at.saturating_sub(page),
        KeyCode::PageDown => at.saturating_add(page),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => usize::MAX,
        _ => return None,
    };
    Some(to.min(len.saturating_sub(1)))
}

/// The last click, to tell a double click.
#[derive(Default)]
pub(crate) struct Clicks(Option<(SystemTime, (u16, u16))>);

impl Clicks {
    /// Whether a click at `now` on `row`, the x of its column and its y,
    /// is the second of a double click. A third click starts a new one.
    pub fn double(&mut self, now: SystemTime, row: (u16, u16)) -> bool {
        let double = self.0.is_some_and(|(then, was)| {
            let soon = now.duration_since(then).is_ok_and(|d| d <= DOUBLE_CLICK);
            was == row && soon
        });
        self.0 = (!double).then_some((now, row));
        double
    }
}

/// The first row to draw, moved from `offset` as little as possible so
/// that row `at` is among the `height` drawn.
fn scroll(offset: usize, at: usize, height: usize) -> usize {
    offset.min(at).max((at + 1).saturating_sub(height))
}

/// Appends `name` to `path`, a path below the root, which may be empty.
fn join_below(path: &mut Vec<u8>, name: &[u8]) {
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
}

/// Draws the picks, below the root `root`, from `list`'s offset, the one
/// at its cursor selected, after a head with their count and total. A
/// dir's size is from the tree, a file's from one `lstat`, and either is
/// `gone` if not found.
#[allow(clippy::too_many_arguments)]
fn picks(
    buf: &mut Buffer,
    area: Rect,
    view: &View,
    root: &[u8],
    picks: &[Vec<u8>],
    list: &mut Picks,
    styles: &Styles,
    units: Units,
) {
    let size = |path: &[u8]| match view.tree.find(Path::new(OsStr::from_bytes(path))) {
        Some(d) => Some(view.tree.size(d)),
        None => {
            let mut full = root.to_vec();
            join(&mut full, path);
            sys::allocated(Path::new(OsStr::from_bytes(&full)))
        }
    };
    let sizes: Vec<Option<u64>> = picks.iter().map(|p| size(p)).collect();
    let total: u64 = sizes.iter().flatten().sum();
    let head = match picks.len() {
        0 => "nothing picked: space picks the item at the cursor".into(),
        n => format!("{n} picked, {} in all:", units.format(total)),
    };
    buf.set_stringn(area.x, area.y, head, area.width.into(), Style::new());
    let height = usize::from(area.height.saturating_sub(1));
    list.offset = scroll(list.offset, list.cursor, height);
    let rows = picks.iter().zip(&sizes).enumerate().skip(list.offset);
    for (y, (i, (path, size))) in (area.y + 1..area.bottom()).zip(rows) {
        let size = match size {
            Some(n) => Span::styled(format!("{:>10}", units.format(*n)), styles.size(*n, units)),
            None => Span::styled(format!("{:>10}", "gone"), styles.dim),
        };
        let line = Line::from_iter([
            size,
            Span::raw(format!("  {}", String::from_utf8_lossy(path))),
        ]);
        buf.set_line(area.x, y, &line, area.width);
        if i == list.cursor {
            let row = Rect {
                y,
                height: 1,
                ..area
            };
            buf.set_style(row, styles.selected);
        }
    }
}

/// `+1.5 GiB` or `-4.0 KiB`.
fn signed(bytes: i64, units: Units) -> String {
    let sign = if bytes < 0 { '-' } else { '+' };
    format!("{sign}{}", units.format(bytes.unsigned_abs()))
}

/// `5 min`: whole seconds, minutes, hours or days.
fn age(d: Duration) -> String {
    match d.as_secs() {
        s @ ..60 => format!("{s} s"),
        s @ ..3600 => format!("{} min", s / 60),
        s @ ..86400 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Builder;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};

    /// 100x20: `title`, empty columns and the keys.
    fn screen(title: &str) -> Buffer {
        let mut lines = vec![format!("{title:<100}")];
        lines.extend((0..18).map(|_| format!("{:100}", "")));
        lines.push(String::new());
        let mut buf = Buffer::with_lines(lines);
        buf.set_line(0, 19, &Styles::new(true).keys(&fit_keys(HELP, 100)), 100);
        buf
    }

    /// A dir's row of `size` KiB, green: with its share `percent` in the
    /// current column, without in the preview.
    fn dir_row(size: &str, percent: Option<&str>, name: &str) -> Line<'static> {
        let green = Style::new().fg(Color::Green);
        let mut spans = vec![
            Span::styled(format!("{size:>10}"), green),
            Span::styled(" ", Style::new().fg(Color::DarkGray)),
            Span::raw(" "),
        ];
        if let Some(percent) = percent {
            spans.extend([Span::styled(percent.to_string(), green), Span::raw(" ")]);
        }
        spans.push(Span::styled(
            format!("{name}/"),
            Style::new().add_modifier(Modifier::BOLD),
        ));
        Line::from(spans)
    }

    fn draw(b: &mut Browser) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| b.draw(f, SystemTime::now())).unwrap();
        terminal.backend().buffer().clone()
    }

    /// A scan that has listed the root, its child `a/` and `a/x/`, but not
    /// yet the root's other children. The root's dir on disk is empty.
    #[test]
    fn a_running_scan_shows_lower_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let tree = Builder::new(dir.path().as_os_str().as_bytes(), false);
        let env = Env {
            desktop: Desktop::None,
            terminal: "Terminal".into(),
            cache: None,
            inotify: true,
            interval: Duration::ZERO,
            units: Units::Binary,
            color: true,
            home: None,
        };
        let mut b = Browser::new(dir.path(), "/scan", env, None, None, false);
        assert!(tree.progress().snapshot().is_none());
        assert_eq!(draw(&mut b), screen("/scan"));

        let ids = tree.intern([&b"a"[..], b"x"].into_iter());
        let dir = |parent, name, own| Record {
            parent,
            name,
            flags: 0,
            errno: 0,
            own,
        };
        tree.push(dir(Record::NO_PARENT, 0, 0), 0);
        tree.push(dir(0, ids[0], 4096), 0);
        tree.push(dir(1, ids[1], 8192), 0);
        b.show(tree.progress().snapshot().unwrap(), Status::Scanning);
        // at the root, the current column is at x 0, the preview at 60
        let mut expected = screen("");
        let title = Line::from_iter([
            Span::raw("/scan  scanning 12.0 KiB"),
            Span::styled(" | ", Style::new().fg(Color::DarkGray)),
            Span::raw("3 folders"),
            Span::styled(" | ", Style::new().fg(Color::DarkGray)),
            Span::raw("0 s"),
        ]);
        expected.set_line(0, 0, &title, 100);
        expected.set_line(0, 1, &dir_row("12.0 KiB", Some(" 100%"), "a"), 59);
        let selected = Style::new().fg(Color::Indexed(231)).bg(Color::Indexed(25));
        expected.set_style(
            Rect::new(0, 1, 59, 1),
            selected.add_modifier(Modifier::BOLD),
        );
        expected.set_line(60, 1, &dir_row("8.0 KiB", None, "x"), 40);
        let dim = Style::new().fg(Color::DarkGray);
        let status = Line::from_iter([
            Span::styled("a/", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(" | ", dim),
            Span::styled("12.0 KiB", Style::new().fg(Color::Green)),
            Span::styled("  own ", dim),
            Span::raw("4.0 KiB"),
        ]);
        expected.set_line(0, 18, &status, 100);
        assert_eq!(draw(&mut b), expected);
    }

    /// The top line of a 100-column screen, without the spaces after it.
    fn title_of(b: &mut Browser) -> String {
        let drawn = draw(b);
        let line: String = (0..100).map(|x| drawn[(x, 0)].symbol()).collect();
        line.trim_end().into()
    }

    /// A volume reports fewer bytes in use than a scan finds, as it counts
    /// a cloned file once: then they are no target, and no total.
    #[test]
    fn a_scan_past_the_used_bytes_drops_them_as_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let tree = Builder::new(dir.path().as_os_str().as_bytes(), false);
        let env = Env {
            desktop: Desktop::None,
            terminal: "Terminal".into(),
            cache: None,
            inotify: true,
            interval: Duration::ZERO,
            units: Units::Binary,
            color: true,
            home: None,
        };
        let record = |parent, own| Record {
            parent,
            name: 0,
            flags: 0,
            errno: 0,
            own,
        };
        tree.push(record(Record::NO_PARENT, 4096), 0);
        let mut b = Browser::new(dir.path(), "/vol", env, None, Some(16384), false);
        b.show(tree.progress().snapshot().unwrap(), Status::Scanning);
        assert_eq!(
            title_of(&mut b),
            "/vol  scanning 4.0 KiB of ~16.0 KiB (25%)"
        );
        tree.push(record(0, 28672), 0);
        b.show(tree.progress().snapshot().unwrap(), Status::Scanning);
        assert_eq!(title_of(&mut b), "/vol  scanning 32.0 KiB | 2 folders");
        b.show(tree.progress().snapshot().unwrap(), Status::Done);
        assert_eq!(
            title_of(&mut b),
            "/vol  32.0 KiB | disk reports 16.0 KiB in use"
        );
    }
}

//! The full-screen browser: three columns, the parent dir, the current dir
//! and the selected child, each largest first, like OmniDiskSweeper. At the
//! root, which has no parent column, the other two move left. Each row has
//! its share of its dir. A click selects the row under it, a
//! double click goes into it, and the wheel scrolls the column under it. A scan
//! runs on its own thread meanwhile; a saved scan, if any, shows until the
//! fresh one is done. Then the tree follows changes on disk
//! ([`diskuse_core::Live`]): all of them on macOS, those in the dirs shown on
//! Linux. Quitting stops a running scan and saves what it found.
//! `d` lists the dirs the scan could not read instead, and `t` the largest
//! files. With reclaimable sizes (`-r`), each size has a second column, the
//! bytes deleting the item alone frees.
//!
//! Subdirectories and their sizes come from the tree. Files are not in the
//! tree, so a dir is listed from disk when first shown, and the listing is
//! kept until another scan's tree is shown.
//!
//! The code is in three parts. State: [`Browser`] and what it holds, here.
//! Update, all that changes it: `update.rs`, by the scan's events, the
//! keys and the mouse. View, which draws it: `view.rs`, and a file for
//! each part of the screen, `title.rs`, `columns.rs`, `info.rs` and
//! `lists.rs`.

mod baseline;
mod columns;
mod info;
mod lists;
mod text;
mod title;
mod update;
mod view;

pub(crate) use text::percent;
pub(crate) use update::{Clicks, nav};

use crate::guide::terminal_app;
use crate::reveal::Desktop;
use baseline::Baseline;
use diskuse_core::{
    CacheDir, Event, File, FolderId, LARGEST, Label, Labels, Live, ReadTree, Saved, ScanError,
    Tree, Units,
};
use lists::{Picks, Top};
use ratatui::layout::Rect;
use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime};

/// The path of folder `id` of `tree`, as bytes.
fn dir_path(tree: &Tree, id: FolderId) -> Vec<u8> {
    tree.path(id).into_os_string().into_vec()
}

/// The path of folder `id` below the root of `tree`, as bytes: empty for
/// the root.
fn below(tree: &Tree, id: FolderId) -> Vec<u8> {
    tree.relative(id).into_os_string().into_vec()
}

/// Appends `name` to the absolute `path` after a `/`.
fn join(path: &mut Vec<u8>, name: &[u8]) {
    // below `/` there is one already
    if path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(name);
}

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
    /// Called, from another thread, when a scan has something new for
    /// [`Browser::poll`]: the terminal loop waits on it and on the keys.
    pub wake: Option<Wake>,
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
            wake: None,
            color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

/// What [`Env::wake`] calls.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// What a scan sends the browser.
type Events = mpsc::Receiver<Result<Event, ScanError>>;

/// Changes were missed, so the tree may be out of date: said in the title
/// until all of it is scanned again, and each dir is listed again when
/// first shown since.
struct Stale {
    since: SystemTime,
    /// The scanned dir itself moved: no scan of its old path finds it.
    moved: bool,
    /// The dirs listed again since, by path below the root.
    listed: HashSet<Vec<u8>>,
}

/// The state of the browser. Driven by [`Browser::key`] and
/// [`Browser::poll`], drawn by [`Browser::draw`].
pub struct Browser {
    root: PathBuf,
    /// The labels of the dirs shown.
    labels: Labels,
    /// The root as the title shows it.
    display: String,
    env: Env,
    /// Bytes in use on the volume, when the root is a volume's root.
    used: Option<u64>,
    /// Scans with reclaimable sizes and shows them (`-r`).
    reclaimable: bool,
    /// `None` until the running scan has listed the root.
    view: Option<View>,
    /// The scan, then the changes on disk, from [`Browser::scan`] on, and
    /// what it reports, taken by [`Browser::poll`].
    live: Option<Live>,
    events: Option<Events>,
    /// Changes the OS missed, until everything is scanned again.
    stale: Option<Stale>,
    /// A scan of all of the root was asked for, and has not finished or
    /// failed. Changed by events alone, so the screen follows what was
    /// polled, not where the scan's thread is.
    scanning: bool,
    /// The dir scanned again, by its path below the root, while the tree
    /// shown stays.
    rescanning: Option<Vec<u8>>,
    /// When the tree shown was scanned, as the dirs not followed stand.
    scanned: SystemTime,
    /// Listings by record id, kept until another scan's tree is shown.
    files: HashMap<FolderId, Files>,
    /// Sorted rows by record id, for the view shown.
    rows: HashMap<FolderId, Vec<Row>>,
    /// Names of the dirs entered, from the root down.
    trail: Vec<Box<[u8]>>,
    /// Record ids of the root and of each dir in `trail`.
    dirs: Vec<FolderId>,
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

/// A tree as shown, with what drawing it needs.
struct View {
    tree: Tree,
    /// Path and id of every denied dir, by path.
    denied: Vec<(Vec<u8>, FolderId)>,
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
        let mut denied: Vec<_> = (tree.ids().filter(|&i| tree.error(i).is_some()))
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
    /// A subdirectory.
    Dir(FolderId),
    /// A file, by index in its dir's [`Files`].
    File(u32),
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
            labels: Labels::new(root, env.home.as_deref()),
            display: display.into(),
            env,
            // a filesystem that reports none in use reports nothing useful
            used: used.filter(|&u| u > 0),
            reclaimable,
            view: None,
            live: None,
            events: None,
            stale: None,
            scanning: false,
            rescanning: None,
            scanned: SystemTime::UNIX_EPOCH,
            files: HashMap::new(),
            rows: HashMap::new(),
            trail: Vec::new(),
            dirs: vec![FolderId::ROOT],
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

    /// A scan runs: the first, or one asked for with `s` or `S`.
    fn busy(&self) -> bool {
        self.live.as_ref().is_some_and(Live::busy)
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

    /// The current dir.
    fn dir(&self) -> FolderId {
        *self.dirs.last().unwrap()
    }

    /// The row at the cursor and its name.
    fn at_cursor(&self) -> Option<(Row, &[u8])> {
        let (view, row) = (self.view.as_ref()?, *self.current.get(self.cursor)?);
        Some((row, name(view, &self.files[&self.dir()], row.item)))
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
}

/// The name bytes of `item`, a row of a dir listed as `files`.
fn name<'a>(view: &'a View, files: &'a Files, item: Item) -> &'a [u8] {
    match item {
        Item::Dir(k) => view.tree.name(k),
        Item::File(i) => &files[i as usize].name,
    }
}

/// Appends `name` to `path`, a path below the root, which may be empty.
fn join_below(path: &mut Vec<u8>, name: &[u8]) {
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
}

#[cfg(test)]
mod tests;

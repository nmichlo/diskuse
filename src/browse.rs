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
use crate::labels;
use crate::report::{format_size, largest_files, largest_first, suffix};
use crate::reveal::Desktop;
use crate::scan::{ScanError, ScanOptions, Stop, scan_live};
use crate::store::{CacheDir, Saved};
use crate::style::Styles;
use crate::sys::{self, Kind};
use crate::tree::{ChildIndex, LARGEST, Progress, ReadTree, Record, Totals, Tree, join};
use crate::watch::{DirWatch, Poll, Watch};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

const HELP: &str = "arrows/hjkl move  r reveal  o open  R rescan  / filter  d denied  t top files  \
                    ? help  q quit";
const PANEL_HELP: &str = "arrows/jk scroll  d close  ? help  q quit";
const TOP_HELP: &str = "arrows/jk move  r reveal  o open  t close  ? help  q quit";

/// The width of the bar of a row's share of its dir.
const BAR: u64 = 10;

/// Columns narrower than this have no bars, so names keep the room.
const BAR_COLUMN: u16 = 40;

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
    /// Draw in colour. Off when `NO_COLOR` is set and not empty.
    pub color: bool,
}

impl Env {
    /// The environment of this process.
    pub fn from_env() -> Self {
        Self {
            desktop: Desktop::from_env(),
            terminal: terminal_app(std::env::var("TERM_PROGRAM").ok().as_deref()),
            cache: CacheDir::from_env().ok(),
            inotify: true,
            color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        }
    }
}

/// The state of the browser. Driven by [`Browser::key`] and
/// [`Browser::poll`], drawn by [`Browser::draw`].
pub struct Browser {
    root: PathBuf,
    /// The root as the title shows it.
    display: String,
    env: Env,
    /// Bytes in use on the volume, when the root is a volume's root.
    used: Option<u64>,
    /// Scans with reclaimable sizes and shows them (`-r`).
    reclaimable: bool,
    /// `None` until the running scan has listed the root.
    view: Option<View>,
    scan: Option<Scan>,
    /// Set on quitting, to stop the running scan.
    stop: Arc<AtomicBool>,
    /// Changes on disk below the root, since the tree shown.
    watch: Option<Watch>,
    /// Without `watch`, changes in the dirs shown.
    dir_watch: Option<DirWatch>,
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

    /// `sizes` moved to the ids of `tree`, a fresh scan of the tree they
    /// were of, matching dirs by name from the root down.
    fn rebase(&mut self, tree: &Tree, index: &ChildIndex) {
        let Some(old) = self.of.take() else {
            return;
        };
        let old_index = old.child_index();
        let mut sizes = vec![0; tree.len()];
        sizes[0] = self.size(0);
        let mut stack = vec![(0, 0)];
        while let Some((o, n)) = stack.pop() {
            let kids: HashMap<&[u8], u32> = (old_index.children(o).iter())
                .map(|&k| (old.name(old.record(k).name), k))
                .collect();
            for &k in index.children(n) {
                if let Some(&was) = kids.get(tree.name(tree.record(k).name)) {
                    sizes[k as usize] = self.size(was);
                    stack.push((was, k));
                }
            }
        }
        self.sizes = sizes;
    }
}

/// A tree as shown, with what drawing it needs.
struct View {
    tree: Tree,
    totals: Totals,
    index: ChildIndex,
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
            .map(|i| (tree.dir_path(i), i))
            .collect();
        denied.sort_unstable();
        let index = tree.child_index();
        Self {
            totals: tree.totals(),
            index,
            denied,
            largest: largest_files(&tree, LARGEST),
            tree,
            status,
            reclaimable,
        }
    }
}

/// A scan running on its own thread.
struct Scan {
    started: mpsc::Receiver<Progress>,
    progress: Option<Progress>,
    thread: JoinHandle<Result<Tree, ScanError>>,
}

/// The files of a dir, `(name, allocated bytes, reclaimable bytes)`:
/// every entry but subdirectories. Reclaimable bytes are 0 unless asked
/// for.
type Files = Vec<(Box<[u8]>, u64, u64)>;

/// One row of a column.
#[derive(Clone, Copy, PartialEq)]
struct Row {
    size: u64,
    /// Reclaimable bytes, 0 unless asked for.
    private: u64,
    item: Item,
    /// From [`labels::label`], for a dir.
    label: Option<&'static str>,
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
            display: display.into(),
            env,
            used,
            reclaimable,
            view: None,
            scan: None,
            stop: Arc::default(),
            watch: None,
            dir_watch: None,
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
        };
        if let Some(saved) = saved.filter(|s| s.reclaimable || !reclaimable) {
            b.show(saved.tree, Status::Saved { at: saved.modified });
        }
        b
    }

    /// Scans the root on another thread.
    pub fn scan(&mut self) {
        // what the session's changes are counted against, until the scan
        // is done
        if let (Some(base), Some(view)) = (&mut self.baseline, &self.view)
            && view.status == Status::Done
            && base.of.is_none()
        {
            base.of = Some(view.tree.clone());
        }
        self.watch = None;
        self.dir_watch = None;
        let (tx, started) = mpsc::channel();
        let root = self.root.clone();
        let opts = self.options();
        let thread = thread::spawn(move || {
            // the receiver only goes away with the browser
            scan_live(&root, &opts, |p| {
                let _ = tx.send(p);
            })
        });
        self.scan = Some(Scan {
            started,
            progress: None,
            thread,
        });
    }

    /// Stops a running scan, and saves what it found, so the next run
    /// finishes it.
    pub fn quit(&mut self) -> io::Result<()> {
        let Some(scan) = self.scan.take() else {
            return Ok(());
        };
        self.stop.store(true, Ordering::Relaxed);
        let tree = match scan.thread.join() {
            Ok(Ok(tree)) => tree,
            Ok(Err(_)) => return Ok(()),
            Err(panic) => std::panic::resume_unwind(panic),
        };
        match &self.env.cache {
            Some(cache) => cache.save(&self.root, &tree, self.reclaimable),
            None => Ok(()),
        }
    }

    /// Catches up with the running scan: shows a snapshot of it, unless a
    /// saved scan is shown, or once it is done, its tree, and saves that.
    /// Without a scan, applies the changes on disk since the last poll.
    /// Returns whether a scan, or the update of a saved scan, still runs.
    pub fn poll(&mut self, now: SystemTime) -> bool {
        let Some(scan) = &mut self.scan else {
            return self.follow();
        };
        if scan.thread.is_finished() {
            let scan = self.scan.take().unwrap();
            let done = match scan.thread.join() {
                Ok(done) => done,
                Err(panic) => std::panic::resume_unwind(panic),
            };
            match done {
                Ok(tree) => {
                    self.save(&tree);
                    // from where the scan started, so it sees the changes
                    // made while it ran
                    self.watch = Watch::start(&self.root, &tree, self.env.cache.as_ref());
                    self.dir_watch =
                        (self.watch.is_none()).then(|| DirWatch::new(self.env.inotify, now));
                    let index = tree.child_index();
                    match &mut self.baseline {
                        Some(base) => base.rebase(&tree, &index),
                        None => {
                            self.baseline = Some(Baseline {
                                sizes: tree.totals().size,
                                at: now,
                                of: None,
                            });
                        }
                    }
                    self.show_done(tree, Instant::now());
                }
                Err(e) => self.message = Some(format!("cannot scan: {e}")),
            }
            return false;
        }
        if scan.progress.is_none() {
            scan.progress = scan.started.try_recv().ok();
        }
        // a saved scan shows until the scan is done
        if let Some(View {
            status: Status::Saved { .. },
            ..
        }) = self.view
        {
            return true;
        }
        if let Some(tree) = scan.progress.as_ref().and_then(Progress::snapshot) {
            self.show(tree, Status::Scanning);
        }
        true
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
            KeyCode::Char('R') if self.scan.is_none() => {
                self.view = None;
                self.files.clear();
                self.rows.clear();
                self.current.clear();
                self.scan();
            }
            KeyCode::Char('/') => self.typing = true,
            KeyCode::Char('d') => self.panel = Some(0),
            KeyCode::Char('t') => self.top = Some(Top::default()),
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
        let mut columns = self.columns().into_iter().enumerate();
        let Some((i, area)) = columns.find_map(|(i, a)| Some((i, a.filter(|a| a.contains(at))?)))
        else {
            return;
        };
        let Some((d, rows, offset, _)) = self.listing(i, area.height.into()) else {
            return;
        };
        if key.is_some() {
            let n = rows.len();
            match i {
                0 => self.parent_offset = Some(moved(offset, n)),
                1 => self.select(moved(self.cursor, n)),
                _ => self.preview_offset = moved(offset, n),
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
            match i {
                0 => self.back(),
                1 => {}
                _ => self.enter(),
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
        let footer: Vec<String> = self
            .unaccounted()
            .into_iter()
            .chain([self.footer()])
            .collect();
        let [top, body, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(footer.len() as u16),
        ])
        .areas(frame.area());
        self.body = body;
        let styles = Styles::new(self.env.color);
        let title = self.title(now);
        let buf = frame.buffer_mut();
        buf.set_stringn(top.x, top.y, title, top.width.into(), Style::new());
        for (y, line) in (bottom.y..).zip(footer) {
            buf.set_stringn(bottom.x, y, line, bottom.width.into(), Style::new());
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
            top_files(buf, body, view, top, styles);
            return;
        }
        let height = body.height.into();
        self.offset = scroll(self.offset, self.cursor, height);
        let marks = [Some(styles.parent), Some(styles.selected), None];
        for (i, area) in self.columns().into_iter().enumerate() {
            let listing = self.listing(i, height);
            if let (Some(area), Some((d, rows, offset, mark))) = (area, listing) {
                self.column(buf, area, d, rows, offset, mark.zip(marks[i]));
            }
        }
        if self.current.is_empty() && !self.filter.is_empty() {
            let area = self.columns()[1].unwrap();
            let text = format!("no matches for '{}'", self.filter);
            buf.set_stringn(area.x, area.y, text, area.width.into(), styles.dim);
        }
    }

    /// Where the parent, current and preview columns are drawn. At the
    /// root, there is no parent column, and the other two move left.
    fn columns(&self) -> [Option<Rect>; 3] {
        let [left, mid, right] = Layout::horizontal([Constraint::Fill(1); 3])
            .spacing(1)
            .areas(self.body);
        match self.dirs.len() {
            1 => [None, Some(left), Some(mid)],
            _ => [Some(left), Some(mid), Some(right)],
        }
    }

    /// What column `i` of [`Browser::columns`] lists, in a column `height`
    /// rows high: the dir, its rows, the first of them drawn, and the
    /// marked one, if any. Nothing for the parent column at the root, or
    /// the preview of a file.
    fn listing(&self, i: usize, height: usize) -> Option<(u32, &[Row], usize, Option<usize>)> {
        let d = self.dir();
        match i {
            0 => {
                let [.., parent, _] = self.dirs[..] else {
                    return None;
                };
                let rows = &self.rows[&parent];
                let at = rows.iter().position(|r| r.item == Item::Dir(d)).unwrap();
                let offset = match self.parent_offset {
                    Some(offset) => offset.min(rows.len().saturating_sub(height)),
                    None => scroll(0, at, height),
                };
                Some((parent, rows, offset, Some(at)))
            }
            1 => Some((d, &self.current, self.offset, Some(self.cursor))),
            _ => {
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
    /// row index and its style, if given.
    fn column(
        &self,
        buf: &mut Buffer,
        area: Rect,
        d: u32,
        rows: &[Row],
        offset: usize,
        mark: Option<(usize, Style)>,
    ) {
        let view = self.view.as_ref().unwrap();
        let styles = Styles::new(self.env.color);
        let total = view.totals.size[d as usize];
        let lines = (area.y..area.bottom()).zip(rows.iter().enumerate().skip(offset));
        for (y, (i, row)) in lines {
            let bar = area.width >= BAR_COLUMN;
            let line = line(view, &self.files[&d], row, total, bar, styles);
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

    /// Applies the changes on disk since the last poll to the tree shown,
    /// once the watch has replayed those from before it started. Returns
    /// whether a saved scan is still being brought up to date.
    fn follow(&mut self) -> bool {
        let poll = match (&mut self.watch, &mut self.dir_watch, &self.view) {
            (Some(watch), _, _) => watch.poll(),
            (None, Some(dirs), Some(view)) => dirs.poll(&view.tree),
            _ => return false,
        };
        let changes = match poll {
            Poll::Wait => return false,
            Poll::Lost => {
                self.scan();
                return true;
            }
            Poll::Changes(changes) => changes,
        };
        if changes.is_empty() {
            return false;
        }
        let started = Instant::now();
        let View {
            mut tree, status, ..
        } = self.view.take().expect("a watch follows a view");
        if tree.apply(&changes, &self.options()).is_none() {
            self.show(tree, status);
            self.scan();
            return true;
        }
        self.show_done(tree, started);
        false
    }

    /// Shows `tree`, done since `started`, and tells the dir watch how long
    /// that took, with the listings of the dirs shown, which `show` redoes.
    fn show_done(&mut self, tree: Tree, started: Instant) {
        self.show(tree, Status::Done);
        if let Some(dirs) = &mut self.dir_watch {
            dirs.listed(started.elapsed());
        }
    }

    fn options(&self) -> ScanOptions {
        let stop = Arc::clone(&self.stop);
        ScanOptions {
            reclaimable: self.reclaimable,
            stop: Stop::new(move || stop.load(Ordering::Relaxed)),
            ..ScanOptions::default()
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
            let named = |&k: &u32| view.tree.name(view.tree.record(k).name) == &name[..];
            match view.index.children(parent).iter().copied().find(named) {
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
        if let (Some(dirs), Some(view)) = (&mut self.dir_watch, &self.view) {
            dirs.show(&view.tree, &shown);
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
        let files = (self.files.entry(d)).or_insert_with(|| list(&view.tree, d, private));
        let path = view.tree.dir_path(d);
        // the root's name is the whole path
        let parent = Path::new(OsStr::from_bytes(view.tree.name(view.tree.record(d).name)));
        let parent = parent.file_name().unwrap_or_default().as_bytes();
        let sibling = |file: &str| files.iter().any(|f| *f.0 == *file.as_bytes());
        let dirs = view.index.children(d).iter().map(|&k| {
            let name = view.tree.name(view.tree.record(k).name);
            let inside = |file: &str| {
                let mut path = path.clone();
                join(&mut path, name);
                join(&mut path, file.as_bytes());
                sys::exists(Path::new(OsStr::from_bytes(&path)))
            };
            let size = view.totals.size[k as usize];
            // snapshots of a scan are of other ids
            let delta = match (&self.baseline, view.status) {
                (Some(base), Status::Done) => size as i64 - base.size(k) as i64,
                _ => 0,
            };
            Row {
                size,
                private: view.totals.private[k as usize],
                item: Item::Dir(k),
                label: labels::label(name, parent, sibling, inside),
                delta,
            }
        });
        let mut rows: Vec<Row> = (files.iter().enumerate())
            .map(|(i, &(_, size, private))| Row {
                size,
                private,
                item: Item::File(i as u32),
                label: None,
                delta: 0,
            })
            .chain(dirs)
            .collect();
        // names are unique in a dir, so this order is total
        rows.sort_unstable_by(|a, b| {
            let key = |r: &Row| (r.size, name(view, files, r.item));
            largest_first(key(a), key(b))
        });
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

    /// The path of the row at the cursor.
    fn cursor_path(&self) -> Option<Vec<u8>> {
        let (view, (_, name)) = (self.view.as_ref()?, self.at_cursor()?);
        let mut path = view.tree.dir_path(self.dir());
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

    fn title(&self, now: SystemTime) -> String {
        let root = &self.display;
        let Some(view) = &self.view else {
            return format!("{root}  scanning...");
        };
        let total = format_size(view.totals.size[0]);
        let partial = match view.denied.len() {
            0 => String::new(),
            n => format!("  (partial: {n} denied)"),
        };
        // how much the root grew or shrank since the session started
        let change = match &self.baseline {
            Some(base) if view.status == Status::Done => {
                match view.totals.size[0] as i64 - base.size(0) as i64 {
                    0 => String::new(),
                    d => {
                        let age = age(now.duration_since(base.at).unwrap_or_default());
                        format!("  ({} in {age})", signed(d))
                    }
                }
            }
            _ => String::new(),
        };
        match view.status {
            Status::Scanning => format!("{root}  scanning... at least {total}"),
            Status::Done => match &self.dir_watch {
                // changes in the dirs not shown are not seen since
                Some(dirs) => {
                    let age = age(now.duration_since(dirs.scanned).unwrap_or_default());
                    format!("{root}  {total}{change}{partial}  scanned {age} ago")
                }
                None => format!("{root}  {total}{change}{partial}"),
            },
            Status::Saved { at } => {
                let age = age(now.duration_since(at).unwrap_or_default());
                let stopped = match view.tree.stopped() {
                    true => " (incomplete)",
                    false => "",
                };
                let scanning = match self.scan {
                    Some(_) => ", rescanning...",
                    None => "",
                };
                format!("{root}  {total}{partial}  saved {age} ago{stopped}{scanning}")
            }
        }
    }

    fn footer(&self) -> String {
        match &self.message {
            Some(message) => message.clone(),
            None if self.panel.is_some() => PANEL_HELP.into(),
            None if self.top.is_some() => TOP_HELP.into(),
            None if self.typing => format!("/{}  enter keep  esc clear", self.filter),
            None if !self.filter.is_empty() => format!("/{}  esc clear", self.filter),
            None => HELP.into(),
        }
    }

    /// The bytes the volume uses beyond those a finished scan of its root
    /// found: snapshots, purgeable space, denied dirs. None if the scan
    /// found more, which clones that share blocks can cause.
    fn unaccounted(&self) -> Option<String> {
        let view = self.view.as_ref().filter(|v| v.status == Status::Done)?;
        let lost = self
            .used?
            .checked_sub(view.totals.size[0])
            .filter(|&n| n > 0)?;
        Some(format!("not accounted for: {}", format_size(lost)))
    }
}

/// The name bytes of `item`, a row of a dir listed as `files`.
fn name<'a>(view: &'a View, files: &'a Files, item: Item) -> &'a [u8] {
    match item {
        Item::Dir(k) => view.tree.name(view.tree.record(k).name),
        Item::File(i) => &files[i as usize].0,
    }
}

/// Lists the files of dir `d` from disk, with reclaimable bytes if
/// `private`. Empty if it cannot be read, or the scan did not go into it,
/// so neither does the browser.
fn list(tree: &Tree, d: u32, private: bool) -> Files {
    let mut files = Vec::new();
    if tree.record(d).flags & (Record::DENIED | Record::OTHER_DEVICE) != 0 {
        return files;
    }
    let path = tree.dir_path(d);
    let fd = match d {
        // a symlinked root is followed, as the scan does
        0 => sys::open_root(Path::new(OsStr::from_bytes(&path))),
        _ => sys::open_child(
            rustix::fs::CWD,
            &CString::new(path).expect("names hold no NUL"),
        ),
    };
    if let Ok(fd) = fd {
        // a listing cut short keeps what it got
        let _ = sys::read_dir(&fd, private, |e| {
            if e.kind != Kind::Dir {
                files.push((e.name.to_bytes().into(), e.bytes, e.private));
            }
        });
    }
    files
}

/// Row `row` of a dir of `total` bytes listed as `files`: its sizes, a bar
/// of its share of `total` if `bar`, and its name.
fn line(
    view: &View,
    files: &Files,
    row: &Row,
    total: u64,
    bar: bool,
    styles: &Styles,
) -> Line<'static> {
    let label = String::from_utf8_lossy(name(view, files, row.item)).into_owned();
    let flags = match row.item {
        Item::Dir(k) => view.totals.flags[k as usize],
        Item::File(_) => 0,
    };
    // something below was denied, so the sizes are lower bounds
    let plus = match flags & Record::PARTIAL {
        0 => " ",
        _ => "+",
    };
    let size = |n: u64| Span::styled(format!("{:>10}", format_size(n)), styles.size(n));
    let mut spans = vec![size(row.size), Span::styled(plus, styles.dim)];
    if view.reclaimable {
        spans.extend([size(row.private), Span::styled(plus, styles.dim)]);
    }
    spans.push(Span::raw(" "));
    if bar {
        spans.extend(self::bar(row.size, total, styles.size(row.size), styles));
        spans.push(Span::raw(" "));
    }
    match row.item {
        Item::Dir(k) => {
            spans.push(Span::styled(format!("{label}/"), styles.dir));
            let marker = suffix(&view.tree.record(k), flags & !Record::PARTIAL);
            spans.push(match flags & Record::DENIED {
                0 => Span::styled(marker, styles.dim),
                _ => Span::styled(marker, styles.denied),
            });
            if let Some(l) = row.label {
                spans.push(Span::styled(format!("  [{l}]"), styles.dim));
            }
            match row.delta {
                0 => {}
                d @ 1.. => spans.push(Span::styled(format!("  {}", signed(d)), styles.grew)),
                d => spans.push(Span::styled(format!("  {}", signed(d)), styles.shrank)),
            }
        }
        Item::File(_) => spans.push(Span::raw(label)),
    }
    Line::from(spans)
}

/// A bar of `size`'s share of `total`: a `#`, in `style`, for each tenth,
/// rounded to the nearest, at least one from 1%, then a dim `.` for each
/// tenth left.
pub(crate) fn bar(size: u64, total: u64, style: Style, styles: &Styles) -> [Span<'static>; 2] {
    let (size, total) = (u128::from(size), u128::from(total));
    let n = match total {
        0 => 0,
        _ if size * 100 < total => 0,
        _ => ((size * 20 + total) / (2 * total)).clamp(1, BAR.into()),
    };
    let n = n as usize;
    [
        Span::styled("#".repeat(n), style),
        Span::styled(".".repeat(BAR as usize - n), styles.dim),
    ]
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
        let why = reason(&view.tree.record(*id), terminal);
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
fn top_files(buf: &mut Buffer, area: Rect, view: &View, top: &mut Top, styles: &Styles) {
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
            Span::styled(format!("{:>10}", format_size(size)), styles.size(size)),
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

/// `+1.5 GiB` or `-4.0 KiB`.
fn signed(bytes: i64) -> String {
    let sign = if bytes < 0 { '-' } else { '+' };
    format!("{sign}{}", format_size(bytes.unsigned_abs()))
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

    /// 100x20: `title`, empty columns and the help.
    fn screen(title: &str) -> Buffer {
        let mut lines = vec![format!("{title:<100}")];
        lines.extend((0..18).map(|_| format!("{:100}", "")));
        lines.push(format!("{HELP:<100}"));
        Buffer::with_lines(lines)
    }

    /// A dir's row of `size` KiB, green, with no bar: the columns are
    /// narrower than 40.
    fn dir_row(size: &str, name: &str) -> Line<'static> {
        Line::from_iter([
            Span::styled(format!("{size:>10}"), Style::new().fg(Color::Green)),
            Span::styled(" ", Style::new().fg(Color::DarkGray)),
            Span::raw(" "),
            Span::styled(
                format!("{name}/"),
                Style::new().add_modifier(Modifier::BOLD),
            ),
        ])
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
        let tree = Builder::new(dir.path().as_os_str().as_bytes());
        let env = Env {
            desktop: Desktop::None,
            terminal: "Terminal".into(),
            cache: None,
            inotify: true,
            color: true,
        };
        let mut b = Browser::new(dir.path(), "/scan", env, None, None, false);
        assert!(tree.progress().snapshot().is_none());
        assert_eq!(draw(&mut b), screen("/scan  scanning..."));

        let ids = tree.intern([&b"a"[..], b"x"].into_iter());
        let dir = |parent, name, own| Record {
            parent,
            name,
            flags: 0,
            errno: 0,
            own,
            own_private: 0,
        };
        tree.push(dir(Record::NO_PARENT, 0, 0));
        tree.push(dir(0, ids[0], 4096));
        tree.push(dir(1, ids[1], 8192));
        b.show(tree.progress().snapshot().unwrap(), Status::Scanning);
        // the root's column starts at x 0, the preview at 34
        let mut expected = screen("/scan  scanning... at least 12.0 KiB");
        expected.set_line(0, 1, &dir_row("12.0 KiB", "a"), 33);
        let selected = Style::new().fg(Color::White).bg(Color::Blue);
        expected.set_style(
            Rect::new(0, 1, 33, 1),
            selected.add_modifier(Modifier::BOLD),
        );
        expected.set_line(34, 1, &dir_row("8.0 KiB", "x"), 32);
        assert_eq!(draw(&mut b), expected);
    }
}

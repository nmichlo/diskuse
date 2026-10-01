//! The full-screen browser: three columns, the parent dir, the current dir
//! and the selected child, each largest first, like OmniDiskSweeper. A scan
//! runs on its own thread meanwhile; a saved scan, if any, shows until the
//! fresh one is done.
//!
//! Subdirectories and their sizes come from the tree. Files are not in the
//! tree, so a dir is listed from disk when first shown, and the listing is
//! kept until another scan's tree is shown.

use crate::report::{count_denied, format_size, largest_first, suffix};
use crate::reveal::{self, Desktop};
use crate::scan::{ScanError, ScanOptions, scan_live};
use crate::store::{CacheDir, Saved};
use crate::sys::{self, Kind};
use crate::tree::{ChildIndex, Progress, Record, Totals, Tree, join};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

/// How often the view catches up with a running scan.
const TICK: Duration = Duration::from_secs(1);

const HELP: &str = "arrows/hjkl move  r reveal  o open  R rescan  / filter  q quit";

/// Browses `root` full screen until the user quits. Shows the saved scan
/// of `root` at once, if any, while a fresh scan runs, then saves that.
pub fn browse(root: &Path) -> io::Result<()> {
    // fail before taking over the terminal
    drop(sys::open_root(root).map_err(io::Error::from)?);
    let cache = CacheDir::from_env();
    let saved = cache
        .as_ref()
        .ok()
        .and_then(|c| c.load(root).ok().flatten());
    let display = root.display().to_string();
    let mut b = Browser::new(root, &display, Desktop::from_env(), saved, cache.ok());
    b.scan();
    // restores the terminal on panic too
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut b);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, b: &mut Browser) -> io::Result<()> {
    let mut tick = Instant::now();
    loop {
        terminal.draw(|f| b.draw(f, SystemTime::now()))?;
        if event::poll(TICK.saturating_sub(tick.elapsed()))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && b.key(key, &mut reveal::spawn).is_break()
        {
            return Ok(());
        }
        if tick.elapsed() >= TICK {
            b.poll();
            tick = Instant::now();
        }
    }
}

/// The state of the browser. Driven by [`Browser::key`] and
/// [`Browser::poll`], drawn by [`Browser::draw`].
pub struct Browser {
    root: PathBuf,
    /// The root as the title shows it.
    display: String,
    desktop: Desktop,
    /// Where a finished scan is saved, if anywhere.
    cache: Option<CacheDir>,
    /// `None` until the running scan has listed the root.
    view: Option<View>,
    scan: Option<Scan>,
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
    filter: String,
    /// Keys type into `filter`.
    typing: bool,
    /// Shown in the footer until the next key.
    message: Option<String>,
}

/// A tree as shown, with what drawing it needs.
struct View {
    tree: Tree,
    totals: Totals,
    index: ChildIndex,
    denied: usize,
    status: Status,
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    /// A snapshot of a running scan: sizes are lower bounds.
    Scanning,
    /// Loaded from the store, saved at this time.
    Saved(SystemTime),
    Done,
}

impl View {
    fn new(tree: Tree, status: Status) -> Self {
        Self {
            totals: tree.totals(),
            index: tree.child_index(),
            denied: count_denied(&tree),
            tree,
            status,
        }
    }
}

/// A scan running on its own thread.
struct Scan {
    started: mpsc::Receiver<Progress>,
    progress: Option<Progress>,
    thread: JoinHandle<Result<Tree, ScanError>>,
}

/// The files of a dir, `(name, allocated bytes)`: every entry but
/// subdirectories.
type Files = Vec<(Box<[u8]>, u64)>;

/// One row of a column.
#[derive(Clone, Copy, PartialEq)]
struct Row {
    size: u64,
    item: Item,
}

#[derive(Clone, Copy, PartialEq)]
enum Item {
    /// A subdirectory, by record id.
    Dir(u32),
    /// A file, by index in its dir's [`Files`].
    File(u32),
}

impl Browser {
    /// Shows `saved`, if any, until [`Browser::scan`] and
    /// [`Browser::poll`] bring a fresh scan. `display` is the root as the
    /// title shows it.
    pub fn new(
        root: &Path,
        display: &str,
        desktop: Desktop,
        saved: Option<Saved>,
        cache: Option<CacheDir>,
    ) -> Self {
        let mut b = Self {
            root: root.into(),
            display: display.into(),
            desktop,
            cache,
            view: None,
            scan: None,
            files: HashMap::new(),
            rows: HashMap::new(),
            trail: Vec::new(),
            dirs: vec![0],
            selected: None,
            current: Vec::new(),
            cursor: 0,
            offset: 0,
            filter: String::new(),
            typing: false,
            message: None,
        };
        if let Some(saved) = saved {
            b.show(View::new(saved.tree, Status::Saved(saved.modified)));
        }
        b
    }

    /// Starts scanning the root on another thread.
    pub fn scan(&mut self) {
        let (tx, started) = mpsc::channel();
        let root = self.root.clone();
        let thread = thread::spawn(move || {
            scan_live(&root, &ScanOptions::default(), |p| {
                // the receiver only goes away with the browser
                let _ = tx.send(p);
            })
        });
        self.scan = Some(Scan {
            started,
            progress: None,
            thread,
        });
    }

    /// Catches up with the running scan: shows a snapshot of it, unless a
    /// saved scan is shown, or once it is done, its tree, and saves that.
    /// Returns whether a scan still runs.
    pub fn poll(&mut self) -> bool {
        let Some(scan) = &mut self.scan else {
            return false;
        };
        if scan.thread.is_finished() {
            let scan = self.scan.take().unwrap();
            let done = match scan.thread.join() {
                Ok(done) => done,
                Err(panic) => std::panic::resume_unwind(panic),
            };
            match done {
                Ok(tree) => {
                    if let Some(cache) = &self.cache
                        && let Err(e) = cache.save(&self.root, &tree, false)
                    {
                        self.message = Some(format!("warning: scan not saved: {e}"));
                    }
                    self.show(View::new(tree, Status::Done));
                }
                Err(e) => self.message = Some(format!("cannot scan: {e}")),
            }
            return false;
        }
        if scan.progress.is_none() {
            scan.progress = scan.started.try_recv().ok();
        }
        let saved = matches!(&self.view, Some(v) if matches!(v.status, Status::Saved(_)));
        if !saved && let Some(tree) = scan.progress.as_ref().and_then(Progress::snapshot) {
            self.show(View::new(tree, Status::Scanning));
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
        match key.code {
            KeyCode::Up => self.step(false),
            KeyCode::Down => self.step(true),
            KeyCode::Esc if self.typing || !self.filter.is_empty() => {
                self.filter.clear();
                self.typing = false;
            }
            KeyCode::Enter if self.typing => self.typing = false,
            KeyCode::Backspace if self.typing => {
                self.filter.pop();
            }
            KeyCode::Char(c) if self.typing => self.filter.push(c),
            KeyCode::Char('k') => self.step(false),
            KeyCode::Char('j') => self.step(true),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => self.enter(),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.back(),
            KeyCode::Char('r') => self.reveal(true, run),
            KeyCode::Char('o') => self.reveal(false, run),
            // a running scan is not restarted
            KeyCode::Char('R') if self.scan.is_none() => {
                self.view = None;
                self.files.clear();
                self.rows.clear();
                self.current.clear();
                self.scan();
            }
            KeyCode::Char('/') => self.typing = true,
            KeyCode::Char('q') | KeyCode::Esc => return ControlFlow::Break(()),
            _ => {}
        }
        self.refresh();
        ControlFlow::Continue(())
    }

    /// Draws the title, the three columns and the footer. `now` dates a
    /// saved scan.
    pub fn draw(&mut self, frame: &mut Frame, now: SystemTime) {
        let [top, body, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let [left, mid, right] = Layout::horizontal([Constraint::Fill(1); 3])
            .spacing(1)
            .areas(body);
        let (title, footer, d) = (self.title(now), self.footer(), self.dir());
        let buf = frame.buffer_mut();
        buf.set_stringn(top.x, top.y, title, top.width.into(), Style::new());
        buf.set_stringn(
            bottom.x,
            bottom.y,
            footer,
            bottom.width.into(),
            Style::new(),
        );
        let Some(view) = &self.view else {
            return;
        };
        if let [.., parent, _] = self.dirs[..] {
            let rows = &self.rows[&parent];
            let at = rows.iter().position(|r| r.item == Item::Dir(d)).unwrap();
            let offset = scroll(0, at, left.height.into());
            let mark = (at, Style::new().add_modifier(Modifier::BOLD));
            column(
                buf,
                left,
                view,
                &self.files[&parent],
                rows,
                offset,
                Some(mark),
            );
        }
        self.offset = scroll(self.offset, self.cursor, mid.height.into());
        let mark = (self.cursor, Style::new().add_modifier(Modifier::REVERSED));
        let files = &self.files[&d];
        column(
            buf,
            mid,
            view,
            files,
            &self.current,
            self.offset,
            Some(mark),
        );
        if let Some(&Row {
            item: Item::Dir(k), ..
        }) = self.current.get(self.cursor)
        {
            column(buf, right, view, &self.files[&k], &self.rows[&k], 0, None);
        }
    }

    /// Shows `view`, keeping the place in the tree where it still exists.
    fn show(&mut self, view: View) {
        // record ids, so listings, carry over between snapshots of one scan
        if self
            .view
            .as_ref()
            .is_none_or(|v| v.status != Status::Scanning)
        {
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
        if let Some(&Row {
            item: Item::Dir(k), ..
        }) = self.current.get(self.cursor)
        {
            self.ensure(k);
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
        let files = self.files.entry(d).or_insert_with(|| list(&view.tree, d));
        let dirs = view.index.children(d).iter().map(|&k| Row {
            size: view.totals.size[k as usize],
            item: Item::Dir(k),
        });
        let mut rows: Vec<Row> = (files.iter().enumerate())
            .map(|(i, &(_, size))| Row {
                size,
                item: Item::File(i as u32),
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

    fn step(&mut self, down: bool) {
        let at = match down {
            true => (self.cursor + 1).min(self.current.len().saturating_sub(1)),
            false => self.cursor.saturating_sub(1),
        };
        if let (Some(view), Some(row)) = (&self.view, self.current.get(at)) {
            let files = &self.files[&self.dir()];
            self.selected = Some(name(view, files, row.item).into());
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
    }

    /// Shows the selected item in the file manager, or opens it. Without a
    /// desktop, the footer shows its path instead.
    fn reveal(&mut self, reveal: bool, run: &mut dyn FnMut(&str, &[OsString]) -> io::Result<()>) {
        let (Some(view), Some((_, name))) = (&self.view, self.at_cursor()) else {
            return;
        };
        let mut path = view.tree.dir_path(self.dir());
        join(&mut path, name);
        let path = PathBuf::from(OsString::from_vec(path));
        self.message = match self.desktop.command(&path, reveal) {
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
        let partial = match view.denied {
            0 => String::new(),
            n => format!("  (partial: {n} denied)"),
        };
        match view.status {
            Status::Scanning => format!("{root}  scanning... at least {total}"),
            Status::Done => format!("{root}  {total}{partial}"),
            Status::Saved(at) => {
                let age = age(now.duration_since(at).unwrap_or_default());
                format!("{root}  {total}{partial}  saved {age} ago")
            }
        }
    }

    fn footer(&self) -> String {
        match &self.message {
            Some(message) => message.clone(),
            None if self.typing || !self.filter.is_empty() => format!("/{}", self.filter),
            None => HELP.into(),
        }
    }
}

/// The name bytes of `item`, a row of a dir listed as `files`.
fn name<'a>(view: &'a View, files: &'a Files, item: Item) -> &'a [u8] {
    match item {
        Item::Dir(k) => view.tree.name(view.tree.record(k).name),
        Item::File(i) => &files[i as usize].0,
    }
}

/// Lists the files of dir `d` from disk. Empty if it cannot be read, or the
/// scan did not go into it, so neither does the browser.
fn list(tree: &Tree, d: u32) -> Files {
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
        let _ = sys::read_dir(&fd, false, |e| {
            if e.kind != Kind::Dir {
                files.push((e.name.to_bytes().into(), e.bytes));
            }
        });
    }
    files
}

/// Draws `rows[offset..]` that fit in `area`, with `mark`, a row index and
/// its style, if given.
fn column(
    buf: &mut Buffer,
    area: Rect,
    view: &View,
    files: &Files,
    rows: &[Row],
    offset: usize,
    mark: Option<(usize, Style)>,
) {
    let lines = (area.y..area.bottom()).zip(rows.iter().enumerate().skip(offset));
    for (y, (i, row)) in lines {
        let size = format_size(row.size);
        let label = String::from_utf8_lossy(name(view, files, row.item));
        let text = match row.item {
            Item::Dir(k) => {
                let flags = view.totals.flags[k as usize];
                let marker = suffix(view.tree.record(k), flags);
                format!("{size:>10}  {label}/{marker}")
            }
            Item::File(_) => format!("{size:>10}  {label}"),
        };
        let style = match mark {
            Some((at, style)) if at == i => style,
            _ => Style::new(),
        };
        buf.set_style(
            Rect {
                y,
                height: 1,
                ..area
            },
            style,
        );
        buf.set_stringn(area.x, y, text, area.width.into(), style);
    }
}

/// The first row to draw, moved from `offset` as little as possible so
/// that row `at` is among the `height` drawn.
fn scroll(offset: usize, at: usize, height: usize) -> usize {
    offset.min(at).max((at + 1).saturating_sub(height))
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

    /// 100x20: `title`, then `body` lines, the first row of the current
    /// column reversed if there is one, and the help.
    fn screen(title: &str, body: &[&str]) -> Buffer {
        let mut lines = vec![format!("{title:<100}")];
        lines.extend((0..18).map(|y| format!("{:<100}", body.get(y).unwrap_or(&""))));
        lines.push(format!("{HELP:<100}"));
        let mut buf = Buffer::with_lines(lines);
        if !body.is_empty() {
            let reversed = Style::new().add_modifier(Modifier::REVERSED);
            buf.set_style(Rect::new(34, 1, 32, 1), reversed);
        }
        buf
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
        let mut b = Browser::new(dir.path(), "/scan", Desktop::None, None, None);
        assert!(tree.progress().snapshot().is_none());
        assert_eq!(draw(&mut b), screen("/scan  scanning...", &[]));

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
        b.show(View::new(
            tree.progress().snapshot().unwrap(),
            Status::Scanning,
        ));
        // the columns start at x 34 and 67
        let a = format!("{:34}{:<33}{}", "", "  12.0 KiB  a/", "   8.0 KiB  x/");
        let expected = screen("/scan  scanning... at least 12.0 KiB", &[&a]);
        assert_eq!(draw(&mut b), expected);
    }
}

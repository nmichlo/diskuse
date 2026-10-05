//! What changes a browser: its scan, the keys, the mouse, and moving
//! between dirs.

use super::baseline::Baseline;
use super::columns::Col;
use super::lists::{Picks, Top, scroll};
use super::{Browser, Events, Item, Row, Stale, Status, View, below, name};
use diskuse_core::{
    Event, FolderId, Live, LiveOptions, ReadTree, Reason, ScanError, Tree, Units, largest_first,
    live,
};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

/// Two clicks on one row within this are a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

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

impl Browser {
    /// Scans the root on another thread, then follows the changes on disk.
    pub fn scan(&mut self) {
        self.keep_baseline();
        self.stale = None;
        self.scanning = true;
        self.rescanning = None;
        (self.progress, self.started) = (None, None);
        let mut opts = LiveOptions::default();
        opts.scan.reclaimable = self.reclaimable;
        opts.interval = self.env.interval;
        opts.ignore = self.env.cache.as_ref().map(|c| c.path().into());
        opts.inotify = self.env.inotify;
        // the dirs shown are followed: a watch on every dir costs kernel
        // memory and seconds on a large tree
        opts.shown_only = true;
        // the scan before stops first, and what it sent goes with it
        self.live = None;
        let (tx, events) = mpsc::channel();
        let wake = self.env.wake.clone();
        let handler = move |event| {
            if tx.send(event).is_ok()
                && let Some(wake) = &wake
            {
                wake();
            }
        };
        self.live = Some(live(&self.root, opts, handler));
        self.events = Some(events);
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

    /// Prints sizes in `units` from now on.
    pub fn set_units(&mut self, units: Units) {
        self.env.units = units;
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
        if d == FolderId::ROOT {
            return self.rescan_all();
        }
        self.rescanning = Some(below(&view.tree, d));
        if let Some(live) = &self.live {
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

    /// Catches up with the scan, taking everything it has sent: shows a
    /// snapshot of it, unless a saved scan is shown, or once it is done,
    /// its tree, and saves that. Then shows the changes on disk since.
    /// Never waits: [`Env::wake`] says when there is more. Returns whether
    /// a scan still runs.
    pub fn poll(&mut self, now: SystemTime) -> bool {
        if self.busy() && self.started.is_none() {
            self.started = Some(now);
        }
        // read before the events, so a scan seen as done has sent its tree
        let busy = self.busy();
        let events: Vec<_> = self.events.iter().flat_map(Events::try_iter).collect();
        for event in events {
            self.on_event(event, now);
        }
        busy
    }

    pub(super) fn on_event(&mut self, event: Result<Event, ScanError>, now: SystemTime) {
        match event {
            Err(e) => {
                self.scanning = false;
                self.rescanning = None;
                self.message = Some(format!("cannot scan: {e}"));
            }
            // a saved scan shows until the scan is done
            Ok(Event::Scanning(tree)) => {
                self.progress = Some((tree.size(FolderId::ROOT), tree.len()));
                if !matches!(
                    self.view.as_ref().map(|v| v.status),
                    Some(Status::Saved { .. })
                ) {
                    self.show(tree, Status::Scanning);
                }
            }
            Ok(Event::Ready(tree)) => {
                self.scanning = false;
                (self.progress, self.started) = (None, None);
                self.scanned = now;
                self.save(&tree);
                match &mut self.baseline {
                    Some(base) => {
                        if let Some(old) = base.of.take() {
                            base.carry(&old, &tree);
                        }
                    }
                    None => {
                        self.baseline = Some(Baseline {
                            sizes: (0..tree.len())
                                .map(|i| tree.folder(i).map_or(0, |id| tree.size(id)))
                                .collect(),
                            at: now,
                            of: None,
                        });
                    }
                }
                self.show(tree, Status::Done);
            }
            Ok(Event::Changed(tree, _)) => {
                // new records, or all of them numbered again
                if let (Some(base), Some(old)) = (&mut self.baseline, &self.view) {
                    base.carry(&old.tree, &tree);
                }
                if self.rescanning.take().is_some() {
                    self.save(&tree);
                }
                self.show(tree, Status::Done);
            }
            // nothing is scanned again unless asked: the title says so, and
            // the dirs shown are listed again
            Ok(Event::Missed(why, _)) => {
                let stale = self.stale.get_or_insert_with(|| Stale {
                    since: now,
                    moved: false,
                    listed: HashSet::new(),
                });
                stale.moved |= why == Reason::RootMoved;
                self.refresh();
            }
            // an event the library has since this was written: nothing
            // here shows it
            Ok(_) => {}
        }
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

    fn save(&mut self, tree: &Tree) {
        if let Some(cache) = &self.env.cache
            && let Err(e) = cache.save(&self.root, tree, self.reclaimable)
        {
            self.message = Some(format!("warning: scan not saved: {e}"));
        }
    }

    /// Shows `tree`, keeping the place in it where that still exists.
    pub(super) fn show(&mut self, tree: Tree, status: Status) {
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

    /// Recomputes the dirs of `trail`, the rows of the three columns and
    /// the cursor, after a key or a new view.
    fn refresh(&mut self) {
        let Some(view) = &self.view else {
            return;
        };
        let mut dirs = vec![FolderId::ROOT];
        for name in &self.trail {
            let parent = *dirs.last().unwrap();
            let named = |&k: &FolderId| view.tree.name(k) == &name[..];
            match view.tree.children(parent).find(named) {
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
        if let Some(live) = &self.live {
            live.follow(&shown);
            // after missed changes, each dir is listed again when first shown
            if let Some(stale) = &mut self.stale {
                let view = self.view.as_ref().unwrap();
                let fresh = |d: &FolderId| stale.listed.insert(below(&view.tree, *d));
                let again: Vec<FolderId> = shown.iter().copied().filter(fresh).collect();
                if !again.is_empty() {
                    live.relist(&again);
                }
            }
        }
    }

    /// Takes in `body`, the area of the columns: the mouse is told by it,
    /// the current column scrolls so the cursor shows, and the older
    /// levels, which come and go with the width, are listed.
    pub(super) fn fit(&mut self, body: Rect) {
        self.body = body;
        let lists = self.panel.is_some() || self.top.is_some() || self.picking.is_some();
        if self.view.is_none() || lists {
            return;
        }
        self.offset = scroll(self.offset, self.cursor, body.height.into());
        for (col, _) in self.columns() {
            if let Col::Above(level) = col {
                self.ensure(self.dirs[self.dirs.len() - 1 - level]);
            }
        }
    }

    /// Lists and sorts dir `d`, unless done for this view.
    fn ensure(&mut self, d: FolderId) {
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
        let dirs = view.tree.children(d).map(|k| {
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
                label: self.labels.label(&view.tree, k),
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
}

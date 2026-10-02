//! The screens `disksweep` moves between, the volume list, the Full Disk
//! Access guide and the browser, the help over them, and the terminal loop
//! that drives them.

use crate::access::{self, FullDiskAccess, draw_guide, settings_command};
use crate::browse::{Browser, Clicks, Env, nav};
use crate::reveal;
use crate::style::Styles;
use crate::sys;
use crate::volumes::{self, Mount};
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::ffi::OsString;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// How often the view catches up with a running scan or with changes on
/// disk.
const TICK: Duration = Duration::from_secs(1);

/// Every key, and what it does, as the help lists them.
const KEYS: [(&str, &str); 16] = [
    ("Up Down k j", "move"),
    ("PageUp PageDown", "move by a screen"),
    ("Home End g G", "go to the first or the last row"),
    (
        "Right Enter l",
        "go into the directory; on the volume list, scan the volume",
    ),
    ("Left Backspace h", "go to the parent directory"),
    ("r", "reveal in Finder or the file manager"),
    ("o", "open"),
    ("R", "rescan"),
    (
        "/",
        "filter the column by text; Enter keeps the filter, Esc clears it",
    ),
    ("d", "list the directories that could not be read, and why"),
    ("t", "list the largest files"),
    ("?", "show or hide this help"),
    (
        "q Esc",
        "quit; Esc closes a filter or list first, or goes back to the volume list",
    ),
    ("click", "select; in the parent or preview column, go there"),
    ("double click", "go into the directory, or scan the volume"),
    ("wheel", "scroll the column under the pointer"),
];
const KEYS_HELP: &str = "? or esc close";

/// Lists the folders macOS asks about and probes for Full Disk Access:
/// [`access::preflight`], or a stand-in in tests.
pub type Preflight = Box<dyn FnOnce() -> FullDiskAccess>;

/// Browses `path` full screen until the user quits, or without a path,
/// starts on the volume list. `reclaimable` adds reclaimable sizes.
pub fn browse(path: Option<&Path>, reclaimable: bool) -> io::Result<()> {
    let mounts = match path {
        Some(path) => {
            // fail before taking over the terminal
            drop(sys::open_root(path).map_err(io::Error::from)?);
            // only needed if `path` is a volume's root
            volumes::mounts().unwrap_or_default()
        }
        None => volumes::mounts()?,
    };
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    // only macOS has the privacy layer, and only scans that reach home
    // need its folders
    let preflight = home
        .map(PathBuf::from)
        .filter(|home| cfg!(target_os = "macos") && path.is_none_or(|p| on_home_path(p, home)))
        .map(|home| Box::new(move || access::preflight(&home)) as Preflight);
    // the preflight runs here, before the terminal is taken over
    let mut app = App::new(path, mounts, Env::from_env(), preflight, reclaimable);
    // gives the mouse back on panic too: `ratatui::init` restores the rest,
    // then calls this hook
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    let mut terminal = ratatui::init();
    let result =
        execute!(io::stdout(), EnableMouseCapture).and_then(|()| run(&mut terminal, &mut app));
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Whether `path` is `home`, inside it or above it.
fn on_home_path(path: &Path, home: &Path) -> bool {
    std::fs::canonicalize(path).is_ok_and(|p| p.starts_with(home) || home.starts_with(&p))
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    let mut tick = Instant::now();
    loop {
        terminal.draw(|f| app.draw(f, SystemTime::now()))?;
        if event::poll(TICK.saturating_sub(tick.elapsed()))? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && app.key(key, &mut reveal::spawn).is_break() =>
                {
                    return Ok(());
                }
                Event::Mouse(m) => app.mouse(m, SystemTime::now()),
                _ => {}
            }
        }
        if tick.elapsed() >= TICK {
            app.poll(SystemTime::now());
            tick = Instant::now();
        }
    }
}

/// The screen shown and what moving between screens needs. Driven by
/// [`App::key`], [`App::mouse`] and [`App::poll`], drawn by [`App::draw`].
pub struct App {
    env: Env,
    /// The volumes worth listing, by mount point.
    volumes: Vec<Mount>,
    /// Started on the volume list, so leaving the browser goes back there.
    listed: bool,
    /// The selected index in `volumes`.
    cursor: usize,
    /// Taken by the first scan.
    preflight: Option<Preflight>,
    screen: Screen,
    /// The browser left for the volume list, kept so going back into its
    /// volume does not scan it again.
    parked: Option<Box<Browser>>,
    /// Browsers scan with reclaimable sizes and show them.
    reclaimable: bool,
    /// The help is shown over the screen.
    help: bool,
    /// The area last drawn, for the page keys and the mouse.
    area: Rect,
    /// Clicks on the volume list.
    clicks: Clicks,
}

enum Screen {
    Volumes,
    /// Full Disk Access is missing: the guide, with a message, then the
    /// browser of the target.
    Guide(Target, Option<String>),
    Browse(Box<Browser>),
}

/// A root to browse.
#[derive(Clone)]
struct Target {
    root: PathBuf,
    /// Bytes in use on the volume, when `root` is a volume's root.
    used: Option<u64>,
}

impl App {
    /// Browses `path`, or without one, lists the volumes among `mounts`.
    /// `preflight`, if given, runs before the first scan, so here if there
    /// is a `path`. `reclaimable` is for [`Browser::new`].
    pub fn new(
        path: Option<&Path>,
        mounts: Vec<Mount>,
        env: Env,
        preflight: Option<Preflight>,
        reclaimable: bool,
    ) -> Self {
        let mut app = Self {
            env,
            volumes: volumes::volumes(mounts),
            listed: path.is_none(),
            cursor: 0,
            preflight,
            screen: Screen::Volumes,
            parked: None,
            reclaimable,
            help: false,
            area: Rect::default(),
            clicks: Clicks::default(),
        };
        if let Some(path) = path {
            let real = std::fs::canonicalize(path).ok();
            let volume = app.volumes.iter().find(|v| Some(&v.point) == real.as_ref());
            let used = volume.map(|v| v.used);
            app.open(Target {
                root: path.into(),
                used,
            });
        }
        app
    }

    /// Catches up with a running scan, or with changes on disk. Returns
    /// whether a scan still runs.
    pub fn poll(&mut self, now: SystemTime) -> bool {
        // a parked browser keeps up too, so changes do not pile up
        if let Some(b) = &mut self.parked {
            b.poll(now);
        }
        match &mut self.screen {
            Screen::Browse(b) => b.poll(now),
            _ => false,
        }
    }

    /// Handles a key. Reveal, open and the settings pane go through `run`,
    /// given a program and its arguments. Breaks when the user quits.
    pub fn key(
        &mut self,
        key: KeyEvent,
        run: &mut dyn FnMut(&str, &[OsString]) -> io::Result<()>,
    ) -> ControlFlow<()> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ControlFlow::Break(());
        }
        if self.help {
            self.help = !matches!(key.code, KeyCode::Char('?') | KeyCode::Esc);
            return ControlFlow::Continue(());
        }
        // the guide lists its keys, and the filter takes a typed `?`
        let typing = matches!(&self.screen, Screen::Browse(b) if b.typing());
        if key.code == KeyCode::Char('?') && !typing && !matches!(self.screen, Screen::Guide(..)) {
            self.help = true;
            return ControlFlow::Continue(());
        }
        let page = volumes::height(self.area);
        match &mut self.screen {
            Screen::Volumes => match key.code {
                code if let Some(at) = nav(code, self.cursor, self.volumes.len(), page) => {
                    self.cursor = at;
                }
                KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => self.scan_selected(),
                KeyCode::Char('q') | KeyCode::Esc => return ControlFlow::Break(()),
                _ => {}
            },
            Screen::Guide(target, message) => match key.code {
                KeyCode::Char('o') => {
                    let (program, args) = settings_command();
                    *message = run(program, &args)
                        .err()
                        .map(|e| format!("cannot run {program}: {e}"));
                }
                KeyCode::Char('c') => {
                    let target = target.clone();
                    self.screen = Screen::Browse(self.browser(target));
                }
                KeyCode::Esc if self.listed => self.screen = Screen::Volumes,
                KeyCode::Char('q') | KeyCode::Esc => return ControlFlow::Break(()),
                _ => {}
            },
            Screen::Browse(b) => {
                let up = self.listed && key.code == KeyCode::Backspace && b.at_root();
                if !up && b.key(key, run).is_continue() {
                    return ControlFlow::Continue(());
                }
                // going up from the root or Esc goes back to the list
                if !(self.listed && (up || key.code == KeyCode::Esc)) {
                    return ControlFlow::Break(());
                }
                if let Screen::Browse(b) = std::mem::replace(&mut self.screen, Screen::Volumes) {
                    self.parked = Some(b);
                }
            }
        }
        ControlFlow::Continue(())
    }

    /// Handles a mouse event at `now`. On the volume list, a click selects
    /// the row under it, a second click on it within 400 ms scans it, and
    /// the wheel moves the cursor. See [`Browser::mouse`] for the browser.
    pub fn mouse(&mut self, event: MouseEvent, now: SystemTime) {
        if self.help {
            return;
        }
        match &mut self.screen {
            Screen::Volumes => {
                let n = self.volumes.len();
                let key = match event.kind {
                    MouseEventKind::ScrollUp => KeyCode::Up,
                    MouseEventKind::ScrollDown => KeyCode::Down,
                    MouseEventKind::Down(MouseButton::Left) => {
                        let row = volumes::row_at(self.area, self.cursor, event.row);
                        if let Some(at) = row.filter(|&at| at < n) {
                            match self.clicks.double(now, (0, event.row)) {
                                true => self.scan_selected(),
                                false => self.cursor = at,
                            }
                        }
                        return;
                    }
                    _ => return,
                };
                self.cursor = nav(key, self.cursor, n, 1).unwrap();
            }
            Screen::Browse(b) => b.mouse(event, now),
            Screen::Guide(..) => {}
        }
    }

    /// Draws the screen shown, or the help over it. `now` dates a saved
    /// scan.
    pub fn draw(&mut self, frame: &mut Frame, now: SystemTime) {
        self.area = frame.area();
        if self.help {
            draw_help(frame);
            return;
        }
        let styles = Styles::new(self.env.color);
        match &mut self.screen {
            Screen::Volumes => volumes::draw(frame, &self.volumes, self.cursor, styles),
            Screen::Guide(_, message) => {
                draw_guide(frame, &self.env.terminal, message.as_deref());
            }
            Screen::Browse(b) => b.draw(frame, now),
        }
    }

    /// Scans the volume at the cursor, if any.
    fn scan_selected(&mut self) {
        if let Some(v) = self.volumes.get(self.cursor) {
            let target = Target {
                root: v.point.clone(),
                used: Some(v.used),
            };
            self.open(target);
        }
    }

    /// Runs the preflight if it has not run, then shows the guide if Full
    /// Disk Access is missing, else browses `target`.
    fn open(&mut self, target: Target) {
        let missing = (self.preflight.take()).is_some_and(|p| p() == FullDiskAccess::Missing);
        self.screen = match missing {
            true => Screen::Guide(target, None),
            false => Screen::Browse(self.browser(target)),
        };
    }

    /// A browser of `target`, scanning it: the parked one if of `target`.
    fn browser(&mut self, target: Target) -> Box<Browser> {
        if let Some(b) = self.parked.take()
            && b.root() == target.root
        {
            return b;
        }
        let cache = self.env.cache.as_ref();
        let saved = cache.and_then(|c| c.load(&target.root).ok().flatten());
        let display = target.root.display().to_string();
        let env = self.env.clone();
        let reclaimable = self.reclaimable;
        let mut b = Browser::new(&target.root, &display, env, saved, target.used, reclaimable);
        b.scan();
        Box::new(b)
    }
}

/// Draws [`KEYS`] over the whole screen.
fn draw_help(frame: &mut Frame) {
    let area = frame.area();
    let width = area.width.into();
    let buf = frame.buffer_mut();
    buf.set_stringn(0, 0, "keys", width, Style::new());
    for (y, (keys, action)) in (1..area.height.saturating_sub(1)).zip(KEYS) {
        buf.set_stringn(0, y, format!("{keys:<18}{action}"), width, Style::new());
    }
    let bottom = area.height.saturating_sub(1);
    buf.set_stringn(0, bottom, KEYS_HELP, width, Style::new());
}

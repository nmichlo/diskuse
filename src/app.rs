//! The screens `disksweep` moves between, the volume list, the Full Disk
//! Access guide and the browser, and the terminal loop that drives them.

use crate::access::{self, FullDiskAccess, draw_guide, settings_command};
use crate::browse::{Browser, Env};
use crate::reveal;
use crate::sys;
use crate::volumes::{self, Mount};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::ffi::OsString;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// How often the view catches up with a running scan or with changes on
/// disk.
const TICK: Duration = Duration::from_secs(1);

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
    // restores the terminal on panic too
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
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
        if event::poll(TICK.saturating_sub(tick.elapsed()))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && app.key(key, &mut reveal::spawn).is_break()
        {
            return Ok(());
        }
        if tick.elapsed() >= TICK {
            app.poll(SystemTime::now());
            tick = Instant::now();
        }
    }
}

/// The screen shown and what moving between screens needs. Driven by
/// [`App::key`] and [`App::poll`], drawn by [`App::draw`].
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
        match &mut self.screen {
            Screen::Volumes => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.cursor = (self.cursor + 1).min(self.volumes.len().saturating_sub(1));
                }
                KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => {
                    if let Some(v) = self.volumes.get(self.cursor) {
                        let target = Target {
                            root: v.point.clone(),
                            used: Some(v.used),
                        };
                        self.open(target);
                    }
                }
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

    /// Draws the screen shown. `now` dates a saved scan.
    pub fn draw(&mut self, frame: &mut Frame, now: SystemTime) {
        match &mut self.screen {
            Screen::Volumes => volumes::draw(frame, &self.volumes, self.cursor),
            Screen::Guide(_, message) => {
                draw_guide(frame, &self.env.terminal, message.as_deref());
            }
            Screen::Browse(b) => b.draw(frame, now),
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

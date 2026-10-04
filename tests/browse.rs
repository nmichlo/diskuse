//! The screens, drawn into a 100x20 test terminal and compared cell by cell.

#![allow(clippy::disallowed_methods)] // fixtures create and delete files

mod common;

use common::{Fixture, file, fixture, kib};
use diskuse::reveal::Desktop;
use diskuse::{App, Browser, CacheDir, Env, FullDiskAccess, Mount, ReadTree, Saved, ScanOptions};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{Frame, Terminal};
use std::cmp::Reverse;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const WIDTH: usize = 100;
const HEIGHT: usize = 20;
/// `(x, width)` of the parent, current and preview columns. At the root,
/// the current and preview columns take the first two.
const COLUMNS: [(usize, usize); 3] = [(0, 33), (34, 32), (67, 33)];
/// A wider terminal, for rows too long for 100 columns.
const WIDE: usize = 130;
const WIDE_COLUMNS: [(usize, usize); 3] = [(0, 43), (44, 42), (87, 43)];
const HELP: &str =
    "hjkl move  r reveal  o open  space pick  p picks  s/S rescan  / filter  t top  ? help  q quit";
const PICKS_HELP: &str = "arrows/jk move  r reveal  o open  space unpick  p close  ? help  q quit";
const PANEL_HELP: &str = "arrows/jk scroll  d close  ? help  q quit";
const TOP_HELP: &str = "arrows/jk move  r reveal  o open  t close  ? help  q quit";
const VOLUMES_HELP: &str = "arrows/jk move  enter scan  ? help  q quit";
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// After the total of a finished scan: its age, on Linux, where only the
/// dirs shown follow changes.
const SCANNED: &str = match cfg!(target_os = "macos") {
    true => "",
    false => "  scanned 0 s ago",
};

/// The time of day the tests' clock always gives, so a scan is dated, and
/// drawn, then.
fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
}

/// The styles the screens are drawn with, with colours or without.
#[derive(Clone, Copy)]
struct Look {
    color: bool,
}

const COLOR: Look = Look { color: true };
const NO_COLOR: Look = Look { color: false };

impl Look {
    fn fg(self, color: Color) -> Style {
        match self.color {
            true => Style::new().fg(color),
            false => Style::new(),
        }
    }

    /// A size, by its unit: like OmniDiskSweeper, red from 1 GiB, yellow
    /// from 1 MiB, green from 1 KiB, dim below.
    fn size(self, text: &str) -> Style {
        match text.rsplit(' ').next().unwrap() {
            "GiB" => self.fg(Color::Red),
            "MiB" => self.fg(Color::Yellow),
            "KiB" => self.fg(Color::Green),
            _ => self.fg(Color::DarkGray),
        }
    }

    fn dim(self) -> Style {
        self.fg(Color::DarkGray)
    }

    /// The row at the cursor.
    fn selected(self) -> Style {
        match self.color {
            true => Style::new()
                .fg(Color::Indexed(231))
                .bg(Color::Indexed(25))
                .add_modifier(Modifier::BOLD),
            false => Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// The current dir's row in the parent column.
    fn parent(self) -> Style {
        match self.color {
            true => Style::new().fg(Color::White).bg(Color::DarkGray),
            false => Style::new().add_modifier(Modifier::UNDERLINED),
        }
    }
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// `bytes` as the browser prints them, for the sizes the tests use.
fn size(bytes: u64) -> String {
    match bytes {
        ..MIB => kib(bytes),
        _ => format!("{:.1} MiB", bytes as f64 / MIB as f64),
    }
}

/// `+4.0 KiB` or `-4.0 KiB`.
fn signed(bytes: i64) -> String {
    let sign = if bytes < 0 { '-' } else { '+' };
    format!("{sign}{}", size(bytes.unsigned_abs()))
}

/// The `#`s and `.`s of a bar of `size`'s share of `of`: tenths, to the
/// nearest, at least one from 1%.
fn bar(size: u64, of: u64) -> (String, String) {
    let n = match of {
        0 => 0,
        _ if size * 100 < of => 0,
        _ => ((size as f64 * 10.0 / of as f64).round() as usize).max(1),
    };
    ("#".repeat(n), ".".repeat(10 - n))
}

/// What a row shows after its sizes and bar.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    File,
    /// `+` after the size if `partial`: something below was denied.
    Dir {
        partial: bool,
    },
    Denied,
    /// A dir with a cache label.
    Label(&'static str),
    /// A dir with the label of a known big folder.
    Known(&'static str),
}

/// An expected row of a column: `size` bytes of a dir of `of` bytes.
#[derive(Clone, PartialEq)]
struct Row {
    size: u64,
    /// Reclaimable bytes, shown with `-r`.
    private: Option<u64>,
    of: u64,
    name: String,
    kind: Kind,
    /// Bytes a dir grew by since the session started, shrank by if
    /// negative.
    delta: i64,
    /// Marked `*` as picked.
    picked: bool,
}

impl Row {
    fn new(size: u64, of: u64, name: &str, kind: Kind) -> Self {
        Row {
            size,
            private: None,
            of,
            name: name.into(),
            kind,
            delta: 0,
            picked: false,
        }
    }

    fn picked(self) -> Self {
        Row {
            picked: true,
            ..self
        }
    }

    fn changed(self, delta: i64) -> Self {
        Row { delta, ..self }
    }

    fn file(size: u64, of: u64, name: &str) -> Self {
        Self::new(size, of, name, Kind::File)
    }

    fn dir(size: u64, of: u64, name: &str) -> Self {
        Self::new(size, of, name, Kind::Dir { partial: false })
    }

    /// `size  ##........ name`, the size coloured, and the `#`s the same,
    /// the bar only if `bar`.
    /// `changes`: the column shows each row's change, as one of its rows
    /// changed.
    fn line(&self, look: Look, bar: bool, changes: bool) -> Line<'static> {
        let plus = match self.kind {
            Kind::Dir { partial: true } => "+",
            _ => " ",
        };
        let size = |n: u64| {
            let text = size(n);
            Span::styled(format!("{text:>10}"), look.size(&text))
        };
        let mut spans = vec![size(self.size), Span::styled(plus, look.dim())];
        if let Some(private) = self.private {
            spans.extend([size(private), Span::styled(plus, look.dim())]);
        }
        if changes {
            spans.push(match self.delta {
                0 => Span::raw(" ".repeat(11)),
                d => {
                    let color = if d > 0 { Color::Red } else { Color::Green };
                    Span::styled(format!("{:>11}", signed(d)), look.fg(color))
                }
            });
        }
        spans.push(Span::raw(" "));
        if bar {
            let (filled, empty) = self::bar(self.size, self.of);
            spans.extend([
                Span::styled(filled, look.size(&size(self.size).content)),
                Span::styled(empty, look.dim()),
                Span::raw(" "),
            ]);
        }
        let name = self.name.clone();
        match self.kind {
            Kind::File => spans.push(Span::raw(name)),
            Kind::Dir { .. } => spans.push(Span::styled(name + "/", bold())),
            Kind::Denied => spans.extend([
                Span::styled(name + "/", bold()),
                Span::styled(" (denied: EACCES)", look.fg(Color::Red)),
            ]),
            Kind::Label(label) => spans.extend([
                Span::styled(name + "/", bold()),
                Span::styled(format!("  [{label}]"), look.fg(Color::Green)),
            ]),
            Kind::Known(label) => spans.extend([
                Span::styled(name + "/", bold()),
                Span::styled(format!("  [{label}]"), look.fg(Color::Yellow)),
            ]),
        }
        if self.picked {
            let style = match look.color {
                true => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                false => bold(),
            };
            spans.push(Span::styled(" *", style));
        }
        Line::from(spans)
    }
}

/// An expected screen: `title`, the rows of the parent, current and
/// preview columns, and `footer`. `parent` is the marked row of the
/// parent column, `None` at the root, and `current` the selected row of
/// the current column.
fn screen(
    title: &str,
    columns: [&[Row]; 3],
    parent: Option<usize>,
    current: usize,
    footer: &str,
) -> Buffer {
    screen_in(
        COLOR, WIDTH, COLUMNS, title, columns, parent, current, footer,
    )
}

/// [`screen`] drawn with `look`, in a terminal `width` wide, with columns
/// at `layout`.
#[allow(clippy::too_many_arguments)]
fn screen_in(
    look: Look,
    width: usize,
    layout: [(usize, usize); 3],
    title: &str,
    columns: [&[Row]; 3],
    parent: Option<usize>,
    current: usize,
    footer: &str,
) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, width as u16, HEIGHT as u16));
    buf.set_stringn(0, 0, title, width, Style::new());
    buf.set_line(0, HEIGHT as u16 - 1, &bottom(look, footer), width as u16);
    // at the root, the columns move left
    let places = match parent {
        None => [None, Some(layout[0]), Some(layout[1])],
        Some(_) => layout.map(Some),
    };
    let marks = [
        parent.map(|i| (i, look.parent())),
        Some((current, look.selected())),
        None,
    ];
    for ((rows, place), mark) in columns.iter().zip(places).zip(marks) {
        let Some((x, width)) = place else {
            assert!(rows.is_empty(), "no parent column at the root");
            continue;
        };
        let (x, width) = (x as u16, width as u16);
        let changes = rows.iter().any(|r| r.delta != 0);
        for (y, row) in (1..HEIGHT as u16 - 1).zip(rows.iter()) {
            // narrower columns have no bars
            buf.set_line(x, y, &row.line(look, width >= 40, changes), width);
        }
        if let Some((i, style)) = mark.filter(|&(i, _)| i < rows.len()) {
            buf.set_style(Rect::new(x, 1 + i as u16, width, 1), style);
        }
    }
    buf
}

/// An expected screen of whole-width `lines`, `selected` the index of the
/// selected one.
fn plain(lines: &[Line<'static>], selected: Option<usize>) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, WIDTH as u16, HEIGHT as u16));
    for (y, line) in (0..HEIGHT as u16).zip(lines) {
        buf.set_line(0, y, line, WIDTH as u16);
    }
    if let Some(y) = selected {
        buf.set_style(Rect::new(0, y as u16, WIDTH as u16, 1), COLOR.selected());
    }
    buf
}

/// `lines` as unstyled lines, the last at the bottom, the others from the
/// top.
fn text(lines: &[&str], bottom: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = lines.iter().map(|l| Line::raw(l.to_string())).collect();
    lines.resize(HEIGHT - 1, Line::default());
    lines.push(self::bottom(COLOR, bottom));
    lines
}

/// The bottom line: a line of keys, each highlighted, or a message.
fn bottom(look: Look, footer: &str) -> Line<'static> {
    let help = [HELP, TOP_HELP, PICKS_HELP, PANEL_HELP, VOLUMES_HELP];
    match help.contains(&footer) || footer.starts_with('/') {
        true => key_line(look, footer),
        false => Line::raw(footer.to_string()),
    }
}

/// `key what` pairs, two spaces apart, each key in bold cyan.
fn key_line(look: Look, text: &str) -> Line<'static> {
    let key = match look.color {
        true => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        false => bold(),
    };
    let mut spans = Vec::new();
    for (i, pair) in text.split("  ").enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let (k, what) = pair.split_once(' ').unwrap_or((pair, ""));
        spans.push(Span::styled(k.to_string(), key));
        if !what.is_empty() {
            spans.push(Span::raw(format!(" {what}")));
        }
    }
    Line::from(spans)
}

/// An environment with no cache, so tests never touch the user's.
fn env(desktop: Desktop) -> Env {
    Env {
        desktop,
        terminal: "iTerm".into(),
        cache: None,
        inotify: true,
        color: true,
        home: None,
    }
}

/// What the browser shows of the fixture, scanned as root or not.
struct Expected {
    title: String,
    root: Vec<Row>,
    a: Vec<Row>,
    b: Vec<Row>,
    /// The rows of `big/` and `a/b/c/`.
    big: Vec<Row>,
    c: Vec<Row>,
}

fn expected(f: &Fixture) -> Expected {
    let d = f.dir_bytes;
    let as_root = rustix::process::geteuid().is_root();
    // the same total `scan` prints
    let total = f.expected[..10].trim();
    let locked = match as_root {
        true => 0,
        false => d,
    };
    // a dir above a denied one
    let dir = |size, of, name| Row::new(size, of, name, Kind::Dir { partial: !as_root });
    // each dir's bytes: its own, its files' and its subdirs'
    let c_bytes = 12288 + d;
    let b_bytes = c_bytes + 8192 + d + locked;
    let a_bytes = b_bytes + 4096 + 8192 + d;
    let big_bytes = 1572864 + d;
    let root_bytes = big_bytes + a_bytes + 8192 + f.sym_bytes + 2 * d + locked;
    let of = root_bytes;
    let denied = Row::new(d, of, "locked", Kind::Denied);
    // whether these tie depends on the filesystem
    let mut small = vec![
        Row::dir(d, of, "empty"),
        denied.clone(),
        Row::file(f.sym_bytes, of, "sym"),
    ];
    small.sort_by_key(|r| (Reverse(r.size), r.name.clone()));
    small.retain(|r| !(as_root && r.kind == Kind::Denied));
    let mut root = vec![
        Row::dir(big_bytes, of, "big"),
        dir(a_bytes, of, "a"),
        Row::file(8192, of, "top"),
    ];
    root.extend(small);
    let a = vec![
        dir(b_bytes, a_bytes, "b"),
        Row::file(8192, a_bytes, "h1"),
        Row::file(8192, a_bytes, "h2"),
        Row::file(4096, a_bytes, "f1"),
    ];
    let mut b = vec![
        Row::dir(c_bytes, b_bytes, "c"),
        Row::file(8192, b_bytes, "f2"),
    ];
    if !as_root {
        b.push(Row::new(d, b_bytes, "locked", Kind::Denied));
    }
    let title = match as_root {
        true => format!("/fixture  {total}{SCANNED}"),
        false => format!("/fixture  {total}  (partial: 2 denied){SCANNED}"),
    };
    Expected {
        title,
        root,
        a,
        b,
        big: vec![Row::file(1572864, big_bytes, "f")],
        c: vec![Row::file(12288, c_bytes, "f3")],
    }
}

/// The first view: the root, its largest child selected and previewed.
fn first_view(e: &Expected, title: &str, footer: &str) -> Buffer {
    screen(title, [&[], &e.root, &e.big], None, 0, footer)
}

fn render(draw: impl FnOnce(&mut Frame)) -> Buffer {
    render_in(WIDTH, draw)
}

fn render_in(width: usize, draw: impl FnOnce(&mut Frame)) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width as u16, HEIGHT as u16)).unwrap();
    terminal.draw(draw).unwrap();
    terminal.backend().buffer().clone()
}

fn draw(b: &mut Browser, now: SystemTime) -> Buffer {
    render(|frame| b.draw(frame, now))
}

fn draw_app(app: &mut App) -> Buffer {
    render(|frame| app.draw(frame, now()))
}

/// The program and arguments of each command run.
type Runs = Vec<(String, Vec<OsString>)>;

type Run<'a> = dyn FnMut(&str, &[OsString]) -> io::Result<()> + 'a;

/// Presses `keys` through `handle`, recording commands instead of running
/// them. No key may quit.
fn keys(keys: &[KeyCode], mut handle: impl FnMut(KeyEvent, &mut Run) -> ControlFlow<()>) -> Runs {
    let mut runs = Vec::new();
    for &key in keys {
        let mut run = |program: &str, args: &[OsString]| -> io::Result<()> {
            runs.push((program.to_string(), args.to_vec()));
            Ok(())
        };
        assert!(handle(KeyEvent::from(key), &mut run).is_continue());
    }
    runs
}

fn press(b: &mut Browser, k: &[KeyCode]) -> Runs {
    keys(k, |key, run| b.key(key, run))
}

fn press_app(app: &mut App, k: &[KeyCode]) -> Runs {
    keys(k, |key, run| app.key(key, run))
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

fn click(x: u16, y: u16) -> MouseEvent {
    mouse(MouseEventKind::Down(MouseButton::Left), x, y)
}

/// `ms` milliseconds after [`now`].
fn after(ms: u64) -> SystemTime {
    now() + Duration::from_millis(ms)
}

/// A browser of the fixture after a real scan of it has finished.
fn scanned(root: &Path, desktop: Desktop) -> Browser {
    scanned_as(root, "/fixture", desktop, false)
}

/// A browser of `root`, titled `display`, after a real scan of it, with
/// reclaimable sizes if `reclaimable`.
fn scanned_as(root: &Path, display: &str, desktop: Desktop, reclaimable: bool) -> Browser {
    let mut b = Browser::new(root, display, env(desktop), None, None, reclaimable);
    b.scan();
    while b.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    b
}

/// Columns of 40 cells or more have a bar of each row's share of its dir.
#[test]
fn shows_the_finished_scan() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    let columns = [&[][..], &e.root, &e.big];
    let expected = screen_in(COLOR, WIDE, WIDE_COLUMNS, &e.title, columns, None, 0, HELP);
    assert_eq!(render_in(WIDE, |f| b.draw(f, now())), expected);
}

/// Columns under 40 cells, as in 100 wide, have no bars: names need the
/// room.
#[test]
fn hides_the_bars_in_narrow_columns() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    let drawn = draw(&mut b, now());
    let first: String = (0..33).map(|x| drawn[(x, 1)].symbol()).collect();
    assert_eq!(first, format!("{:<33}", "   1.5 MiB  big/"));
    assert_eq!(drawn, first_view(&e, &e.title, HELP));
}

#[test]
fn enters_two_levels() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Down, KeyCode::Right, KeyCode::Char('l')]);
    let expected = screen(&e.title, [&e.a, &e.b, &e.c], Some(0), 0, HELP);
    assert_eq!(draw(&mut b, now()), expected);

    // back up to the root, with `a/` selected
    press(
        &mut b,
        &[KeyCode::Left, KeyCode::Char('h'), KeyCode::Char('h')],
    );
    let expected = screen(&e.title, [&[], &e.root, &e.a], None, 1, HELP);
    assert_eq!(draw(&mut b, now()), expected);
}

#[test]
fn filters_the_current_column() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Char('/'), KeyCode::Char('p')]);
    let named = |name: &str| e.root.iter().find(|r| r.name == name).unwrap().clone();
    let matches = [named("top"), named("empty")];
    let typing = "/p  enter keep  esc clear";
    let expected = screen(&e.title, [&[], &matches, &[]], None, 0, typing);
    assert_eq!(draw(&mut b, now()), expected);

    // clearing the filter keeps a selection made while filtered
    press(&mut b, &[KeyCode::Down, KeyCode::Esc]);
    let empty = e.root.iter().position(|r| *r == matches[1]).unwrap();
    let expected = screen(&e.title, [&[], &e.root, &[]], None, empty, HELP);
    assert_eq!(draw(&mut b, now()), expected);
}

#[test]
fn shows_a_saved_scan_with_its_age() {
    let f = fixture();
    let e = expected(&f);
    let saved = Saved {
        tree: diskuse::scan(f.dir.path(), &ScanOptions::default()).unwrap(),
        reclaimable: false,
        modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
    };
    let mut b = Browser::new(
        f.dir.path(),
        "/fixture",
        env(Desktop::None),
        Some(saved),
        None,
        false,
    );
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + 5 * 60);
    let title = e.title.strip_suffix(SCANNED).unwrap();
    let title = format!("{title}  saved 5 min ago");
    assert_eq!(draw(&mut b, now), first_view(&e, &title, HELP));
}

#[test]
fn reveals_and_opens_with_the_desktop() {
    let f = fixture();
    let root = f.dir.path();
    let big = root.join("big").into_os_string();
    let r = KeyCode::Char('r');
    let o = KeyCode::Char('o');
    let cases = [
        (
            Desktop::Mac,
            r,
            ("/usr/bin/open", vec!["-R".into(), big.clone()]),
        ),
        (Desktop::Mac, o, ("/usr/bin/open", vec![big.clone()])),
        (Desktop::Linux, r, ("xdg-open", vec![root.into()])),
        (Desktop::Linux, o, ("xdg-open", vec![big.clone()])),
    ];
    for (desktop, key, (program, args)) in cases {
        let mut b = scanned(root, desktop);
        assert_eq!(press(&mut b, &[key]), [(program.to_string(), args)]);
    }
    // a file's path is its dir's plus its name
    let mut b = scanned(root, Desktop::Mac);
    let top = root.join("top").into_os_string();
    let runs = press(&mut b, &[KeyCode::Char('j'), KeyCode::Char('j'), o]);
    assert_eq!(runs, [("/usr/bin/open".to_string(), vec![top])]);
}

#[test]
fn shows_the_path_instead_over_ssh_or_without_a_display() {
    let detect = |macos, vars: &[&str]| Desktop::detect(macos, |k| vars.contains(&k));
    let ssh = "SSH_CONNECTION";
    assert_eq!(
        [
            detect(true, &[]),
            detect(true, &[ssh]),
            detect(false, &["DISPLAY"]),
            detect(false, &["WAYLAND_DISPLAY"]),
            detect(false, &["DISPLAY", ssh]),
            detect(false, &[]),
        ],
        [
            Desktop::Mac,
            Desktop::None,
            Desktop::Linux,
            Desktop::Linux,
            Desktop::None,
            Desktop::None,
        ]
    );

    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), detect(true, &[ssh]));
    assert_eq!(press(&mut b, &[KeyCode::Char('r')]), []);
    let footer = format!("path: {}", f.dir.path().join("big").display());
    assert_eq!(draw(&mut b, now()), first_view(&e, &e.title, &footer));
}

#[test]
fn lists_the_denied_dirs_with_why() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Char('d')]);
    let root = f.dir.path().display();
    let (a, top) = (format!("{root}/a/b/locked"), format!("{root}/locked"));
    let why = |path: String| {
        let red = Style::new().fg(Color::Red);
        Line::from_iter([
            Span::raw(path + "  "),
            Span::styled("permission denied (try sudo)", red),
        ])
    };
    let help = "arrows/jk scroll  d close  ? help  q quit";
    let lines = match rustix::process::geteuid().is_root() {
        true => text(&[&e.title, "every directory could be read"], help),
        false => {
            let head = "2 directories could not be read, so their contents are not counted:";
            let mut lines = text(&[&e.title, head], help);
            lines[2] = why(a);
            lines[3] = why(top);
            lines
        }
    };
    assert_eq!(draw(&mut b, now()), plain(&lines, None));

    press(&mut b, &[KeyCode::Char('d')]);
    assert_eq!(draw(&mut b, now()), first_view(&e, &e.title, HELP));
}

/// ```text
/// root/
///   locked/    mode 000, hides a 4096 file
/// ```
#[test]
fn says_one_directory_could_not_be_read() {
    // root can read anything
    if rustix::process::geteuid().is_root() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    file(&locked.join("hidden"), 4096);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let d = common::own_bytes(dir.path());
    let mut b = scanned_as(dir.path(), "/one", Desktop::None, false);
    press(&mut b, &[KeyCode::Char('d')]);
    let drawn = draw(&mut b, now());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let title = format!("/one  {}  (partial: 1 denied){SCANNED}", kib(2 * d));
    let head = "1 directory could not be read, so its contents are not counted:";
    let mut lines = text(&[&title, head], PANEL_HELP);
    lines[2] = Line::from_iter([
        Span::raw(format!("{}  ", locked.display())),
        Span::styled("permission denied (try sudo)", Style::new().fg(Color::Red)),
    ]);
    assert_eq!(drawn, plain(&lines, None));
}

/// A row of the volume list: `used`, coloured `color`, of `total`, `free`,
/// then a bar of `tenths` `#`s, coloured the same, and `point`.
fn volume(
    used: &str,
    [total, free]: [&str; 2],
    tenths: usize,
    color: Color,
    point: &str,
) -> Line<'static> {
    let color = Style::new().fg(color);
    Line::from_iter([
        Span::styled(format!("{used:>10}"), color),
        Span::raw(format!(" used of {total:>10}  {free:>10} free  ")),
        Span::styled("#".repeat(tenths), color),
        Span::styled(".".repeat(10 - tenths), COLOR.dim()),
        Span::raw(format!("  {point}")),
    ])
}

fn mount(point: &Path, fs: &str, hidden: bool, total: u64, used: u64, free: u64) -> Mount {
    Mount {
        point: point.into(),
        fs: fs.into(),
        hidden,
        total,
        used,
        free,
    }
}

#[test]
fn lists_the_volumes() {
    let mounts = vec![
        mount(
            "/Volumes/USB".as_ref(),
            "exfat",
            false,
            64 * GIB,
            10 * GIB,
            54 * GIB,
        ),
        mount("/".as_ref(), "apfs", false, 926 * GIB, 800 * GIB, 126 * GIB),
        mount(
            "/System/Volumes/Data".as_ref(),
            "apfs",
            true,
            926 * GIB,
            800 * GIB,
            126 * GIB,
        ),
        mount("/dev".as_ref(), "devfs", true, 1 << 20, 1 << 20, 0),
        mount("/proc".as_ref(), "proc", false, 0, 0, 0),
        mount("/run".as_ref(), "tmpfs", false, 2 * GIB, 1 << 20, 2 * GIB),
    ];
    let mut app = App::new(None, mounts, env(Desktop::Mac), None, false);
    let mut screen = text(&["volumes"], VOLUMES_HELP);
    // the bars are used of total, to the nearest tenth: 8.6 and 1.6
    screen[1] = volume("800.0 GiB", ["926.0 GiB", "126.0 GiB"], 9, Color::Red, "/");
    screen[2] = volume(
        "10.0 GiB",
        ["64.0 GiB", "54.0 GiB"],
        2,
        Color::Red,
        "/Volumes/USB",
    );
    assert_eq!(draw_app(&mut app), plain(&screen, Some(1)));
    press_app(&mut app, &[KeyCode::Down, KeyCode::Down]);
    assert_eq!(draw_app(&mut app), plain(&screen, Some(2)));
}

#[test]
fn shows_every_key_on_question_mark() {
    let usb = mount(
        "/Volumes/USB".as_ref(),
        "exfat",
        false,
        64 * GIB,
        10 * GIB,
        54 * GIB,
    );
    let mut app = App::new(None, vec![usb], env(Desktop::Mac), None, false);
    let mut list = text(&["volumes"], VOLUMES_HELP);
    list[1] = volume(
        "10.0 GiB",
        ["64.0 GiB", "54.0 GiB"],
        2,
        Color::Red,
        "/Volumes/USB",
    );
    let list = plain(&list, Some(1));
    let keys = [
        "keys",
        "Up Down k j       move",
        "PageUp PageDown   move by a screen",
        "Home End g G      go to the first or the last row",
        "Right Enter l     go into the directory; on the volume list, scan the volume",
        "Left Backspace h  go to the parent directory",
        "r                 reveal in Finder or the file manager",
        "o                 open",
        "space             pick or unpick the selected item, to delete by hand later",
        "p                 list the picks, with their sizes now; space unpicks",
        "c                 sort by how much each item changed this session; again for size",
        "s                 rescan the selected directory",
        "S                 rescan everything",
        "/                 filter the column by text; Enter keeps the filter, Esc clears it",
        "d                 list the directories that could not be read, and why",
        "t                 list the largest files",
        "?                 show or hide this help",
        "q Esc             quit; Esc closes a filter or list first, or goes back to the volume list",
        "click             select; in the parent or preview column, go there",
        "double click      go into the directory, or scan the volume",
        "wheel             scroll the column under the pointer",
    ];
    let mut lines = vec![Line::raw("keys")];
    lines.extend(keys[1..].iter().map(|k| {
        let (key, what) = k.split_at(18);
        Line::from_iter([
            Span::styled(
                key.to_string(),
                COLOR.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(what.to_string()),
        ])
    }));
    lines.resize(HEIGHT - 1, Line::default());
    lines.push(key_line(COLOR, "? or esc close"));
    let help = plain(&lines, None);
    // other keys do nothing while it is shown
    press_app(&mut app, &[KeyCode::Char('?'), KeyCode::Char('q')]);
    assert_eq!(draw_app(&mut app), help);
    press_app(&mut app, &[KeyCode::Char('?')]);
    assert_eq!(draw_app(&mut app), list);
    press_app(&mut app, &[KeyCode::Char('?'), KeyCode::Esc]);
    assert_eq!(draw_app(&mut app), list);

    // typed into a filter instead
    let f = fixture();
    let e = expected(&f);
    let mut app = App::new(Some(f.dir.path()), vec![], env(Desktop::Mac), None, false);
    finish(&mut app);
    press_app(&mut app, &[KeyCode::Char('/'), KeyCode::Char('?')]);
    let title = e
        .title
        .replacen("/fixture", &f.dir.path().display().to_string(), 1);
    let footer = "/?  enter keep  esc clear";
    let mut expected = screen(&title, [&[], &[], &[]], None, 0, footer);
    expected.set_stringn(0, 1, "no matches for '?'", 33, COLOR.dim());
    assert_eq!(draw_app(&mut app), expected);
}

/// A click selects a volume, a double click scans it.
#[test]
fn clicks_select_and_scan_a_volume() {
    let f = fixture();
    let e = expected(&f);
    let usb = mount(
        "/Volumes/USB".as_ref(),
        "exfat",
        false,
        64 * GIB,
        10 * GIB,
        54 * GIB,
    );
    let volume_ = mount(f.dir.path(), "apfs", false, 100 * GIB, 0, 50 * GIB);
    let mut app = App::new(None, vec![usb, volume_], env(Desktop::Mac), None, false);
    let root = f.dir.path().display().to_string();
    let mut list = text(&["volumes"], VOLUMES_HELP);
    // `/Volumes` sorts before the temp dir
    list[1] = volume(
        "10.0 GiB",
        ["64.0 GiB", "54.0 GiB"],
        2,
        Color::Red,
        "/Volumes/USB",
    );
    list[2] = volume("0 B", ["100.0 GiB", "50.0 GiB"], 0, Color::DarkGray, &root);
    draw_app(&mut app);
    app.mouse(click(5, 2), after(0));
    assert_eq!(draw_app(&mut app), plain(&list, Some(2)));
    app.mouse(click(5, 2), after(200));
    finish(&mut app);
    let title = e.title.replacen("/fixture", &root, 1);
    assert_eq!(draw_app(&mut app), first_view(&e, &title, HELP));
}

/// The app on a volume list of the fixture alone, using `used` bytes,
/// after `Enter`, with the preflight finding `access`.
fn enter_volume(f: &Fixture, used: u64, access: FullDiskAccess) -> App {
    let volume = mount(f.dir.path(), "apfs", false, 100 * GIB, used, 50 * GIB);
    let preflight = Box::new(move || access);
    let mut app = App::new(
        None,
        vec![volume],
        env(Desktop::Mac),
        Some(preflight),
        false,
    );
    press_app(&mut app, &[KeyCode::Enter]);
    app
}

fn finish(app: &mut App) {
    while app.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn guides_to_full_disk_access_once_before_the_first_scan() {
    let f = fixture();
    let e = expected(&f);
    let total = diskuse::scan(f.dir.path(), &ScanOptions::default()).unwrap();
    let used = total.size(0) + (1 << 20);
    let mut app = enter_volume(&f, used, FullDiskAccess::Missing);
    let guide: Vec<Line> = [
        "Full Disk Access is off for iTerm",
        "",
        "macOS keeps some folders from every app without Full Disk Access: Mail, Messages, Safari, Time",
        "Machine and other apps' data. The scan lists them as denied, and their sizes are missing. To grant",
        "it, turn on iTerm in System Settings > Privacy & Security > Full Disk Access, then quit and reopen",
        "iTerm. diskuse stays read-only either way.",
        "",
        "o  open the Full Disk Access settings",
        "c  continue without",
        "q  quit",
    ]
    .map(Line::raw)
    .into();
    assert_eq!(draw_app(&mut app), plain(&guide, None));

    let settings = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
    let runs = press_app(&mut app, &[KeyCode::Char('o')]);
    assert_eq!(runs, [("/usr/bin/open".to_string(), vec![settings.into()])]);

    // the browser of the volume, with what its scan did not find
    press_app(&mut app, &[KeyCode::Char('c')]);
    finish(&mut app);
    let root = f.dir.path().display().to_string();
    let title = e.title.replacen("/fixture", &root, 1);
    let mut browsing = first_view(&e, &title, HELP);
    browsing.set_string(
        0,
        HEIGHT as u16 - 2,
        "not accounted for: 1.0 MiB",
        Style::new(),
    );
    assert_eq!(draw_app(&mut app), browsing);

    // back on the list and in again: the same browser, no guide
    press_app(&mut app, &[KeyCode::Esc]);
    let used = size(used);
    let of = ["100.0 GiB", "50.0 GiB"];
    let mut list = text(&["volumes"], VOLUMES_HELP);
    list[1] = volume(&used, of, 0, Color::Yellow, &root);
    assert_eq!(draw_app(&mut app), plain(&list, Some(1)));
    press_app(&mut app, &[KeyCode::Enter]);
    assert_eq!(draw_app(&mut app), browsing);
}

#[test]
fn browses_at_once_with_full_disk_access() {
    let f = fixture();
    let e = expected(&f);
    let mut app = enter_volume(&f, 0, FullDiskAccess::Granted);
    finish(&mut app);
    let title = e
        .title
        .replacen("/fixture", &f.dir.path().display().to_string(), 1);
    assert_eq!(draw_app(&mut app), first_view(&e, &title, HELP));
}

/// `-r` adds the reclaimable bytes after each size.
#[cfg(target_os = "macos")]
#[test]
fn shows_reclaimable_sizes() {
    let dir = common::clones();
    let mut b = scanned_as(dir.path(), "/clones", Desktop::None, true);
    // as `scan -r` prints them; APFS dirs allocate 0 B
    let reclaimable = |row: Row, private| Row {
        private: Some(private),
        ..row
    };
    let root = [
        reclaimable(Row::dir(MIB, 3 * MIB, "clone"), 0),
        reclaimable(Row::dir(MIB, 3 * MIB, "orig"), 0),
        reclaimable(Row::dir(MIB, 3 * MIB, "solo"), MIB),
    ];
    let clone = [reclaimable(Row::file(MIB, MIB, "f"), 0)];
    let expected = screen("/clones  3.0 MiB", [&[], &root, &clone], None, 0, HELP);
    assert_eq!(draw(&mut b, now()), expected);
}

/// ```text
/// root/
///   rust/Cargo.toml          4096
///   rust/target/x           16384   [cache: cargo], beside Cargo.toml
///   node_modules/m          12288   [cache: npm]
///   target/t                 8192   no label, no Cargo.toml beside it
///   __pycache__/p            4096   [cache: python]
///   tagged/CACHEDIR.TAG             [cache], from the tag
///   .venv/pyvenv.cfg            0   [cache: venv]
///   Downloads/                      [downloads], the root being home
///   venv/                           no label, no pyvenv.cfg
/// ```
///
/// The footer explains the label of the row at the cursor.
#[test]
fn labels_dirs_by_how_safe_deleting_them_is() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for d in [
        "rust/target",
        "node_modules",
        "target",
        "__pycache__",
        "tagged",
        ".venv",
        "Downloads",
        "venv",
    ] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    let files = [
        ("rust/Cargo.toml", 4096),
        ("rust/target/x", 16384),
        ("node_modules/m", 12288),
        ("target/t", 8192),
        ("__pycache__/p", 4096),
        (".venv/pyvenv.cfg", 0),
    ];
    for (path, len) in files {
        file(&root.join(path), len);
    }
    let tag = root.join("tagged/CACHEDIR.TAG");
    #[allow(clippy::disallowed_methods)]
    fs::write(&tag, "Signature: 8a477f597d28d172789f06886806bc55\n").unwrap();
    let tag = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&tag).unwrap().blocks() * 512
    };
    let d = common::own_bytes(root);
    // not the real path on macOS, where temp dirs are below a symlink
    let env = Env {
        home: Some(root.into()),
        ..env(Desktop::None)
    };
    let mut b = Browser::new(root, "/labels", env, None, None, false);
    b.scan();
    while b.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let of = 45056 + tag + 10 * d;
    let label = |size, name, label| Row::new(size, of, name, Kind::Label(label));
    let rows = [
        Row::dir(20480 + 2 * d, of, "rust"),
        label(12288 + d, "node_modules", "cache: npm"),
        Row::dir(8192 + d, of, "target"),
        label(4096 + d, "__pycache__", "cache: python"),
        label(tag + d, "tagged", "cache"),
        label(d, ".venv", "cache: venv"),
        Row::new(d, of, "Downloads", Kind::Known("downloads")),
        Row::dir(d, of, "venv"),
    ];
    let rust = [
        Row::new(
            16384 + d,
            20480 + 2 * d,
            "target",
            Kind::Label("cache: cargo"),
        ),
        Row::file(4096, 20480 + 2 * d, "Cargo.toml"),
    ];
    let title = format!("/labels  {}{SCANNED}", kib(of));
    let screen = |at: usize, preview: &[Row], footer: &str| {
        screen_in(
            COLOR,
            WIDE,
            WIDE_COLUMNS,
            &title,
            [&[], &rows, preview],
            None,
            at,
            footer,
        )
    };
    assert_eq!(
        render_in(WIDE, |f| b.draw(f, now())),
        screen(0, &rust, HELP)
    );
    // the row above the keys explains the label at the cursor
    let explained = |at, preview: &[Row], tag: &str, color, why: &str, keys: &str| {
        let mut buf = screen(at, preview, HELP);
        let y = HEIGHT as u16 - 2;
        buf.set_string(0, y, " ".repeat(WIDE), Style::new());
        let mut line = Line::from_iter([
            Span::styled(format!("[{tag}]"), COLOR.fg(color)),
            Span::raw(format!(" {why}.  ")),
        ]);
        line.spans.extend(key_line(COLOR, keys).spans);
        buf.set_line(0, y, &line, WIDE as u16);
        buf
    };
    press(&mut b, &[KeyCode::Down]);
    let npm = [Row::file(12288, 12288 + d, "m")];
    let why = "npm install rebuilds it";
    assert_eq!(
        render_in(WIDE, |f| b.draw(f, now())),
        explained(
            1,
            &npm,
            "cache: npm",
            Color::Green,
            why,
            "r reveal to delete  space pick"
        )
    );
    press(&mut b, &[KeyCode::End, KeyCode::Up]);
    let why = "downloaded files, often old installers and archives";
    assert_eq!(
        render_in(WIDE, |f| b.draw(f, now())),
        explained(
            6,
            &[],
            "downloads",
            Color::Yellow,
            why,
            "r reveal  space pick"
        )
    );
}

/// `NO_COLOR`: no colours, but the selection is reversed, the parent
/// column's row underlined and dirs bold.
#[test]
fn draws_without_colours_for_no_color() {
    let f = fixture();
    let e = expected(&f);
    let env = Env {
        color: false,
        ..env(Desktop::None)
    };
    let mut b = Browser::new(f.dir.path(), "/fixture", env, None, None, false);
    b.scan();
    while b.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    press(&mut b, &[KeyCode::Down, KeyCode::Right, KeyCode::Right]);
    let columns = [&e.a[..], &e.b, &e.c];
    let expected = screen_in(
        NO_COLOR,
        WIDTH,
        COLUMNS,
        &e.title,
        columns,
        Some(0),
        0,
        HELP,
    );
    assert_eq!(draw(&mut b, now()), expected);
}

#[test]
fn says_when_a_filter_matches_nothing() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(
        &mut b,
        &[KeyCode::Char('/'), KeyCode::Char('z'), KeyCode::Char('z')],
    );
    let footer = "/zz  enter keep  esc clear";
    let mut expected = screen(&e.title, [&[], &[], &[]], None, 0, footer);
    expected.set_stringn(0, 1, "no matches for 'zz'", 33, COLOR.dim());
    assert_eq!(draw(&mut b, now()), expected);
}

#[test]
fn clicks_select_go_up_and_into_dirs() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    draw(&mut b, now());
    // `a/`, in the root's column
    b.mouse(click(5, 2), after(0));
    let expected = screen(&e.title, [&[], &e.root, &e.a], None, 1, HELP);
    assert_eq!(draw(&mut b, now()), expected);
    // `b/`, in the preview of `a/`: into `a/`, `b/` selected
    b.mouse(click(40, 1), after(1000));
    let in_a = screen(&e.title, [&e.root, &e.a, &e.b], Some(1), 0, HELP);
    assert_eq!(draw(&mut b, now()), in_a);
    // `big/`, in the parent column: up to the root, `big/` selected
    b.mouse(click(5, 1), after(2000));
    assert_eq!(draw(&mut b, now()), first_view(&e, &e.title, HELP));

    // two clicks on `a/` within 400 ms go into it
    b.mouse(click(5, 2), after(3000));
    b.mouse(click(5, 2), after(3300));
    assert_eq!(draw(&mut b, now()), in_a);
    // 500 ms apart, they only select `h1`
    b.mouse(click(40, 2), after(4000));
    b.mouse(click(40, 2), after(4500));
    let h1 = screen(&e.title, [&e.root, &e.a, &[]], Some(1), 1, HELP);
    assert_eq!(draw(&mut b, now()), h1);

    // the wheel over the current column moves its cursor
    b.mouse(mouse(MouseEventKind::ScrollDown, 40, 9), after(5000));
    let h2 = screen(&e.title, [&e.root, &e.a, &[]], Some(1), 2, HELP);
    assert_eq!(draw(&mut b, now()), h2);
}

/// ```text
/// root/
///   many/f01 .. f25    4096 .. 102400: more than a column has rows for
/// ```
#[test]
fn pages_and_scrolls_long_columns() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("many")).unwrap();
    for i in 1..=25 {
        file(&root.join(format!("many/f{i:02}")), i * 4096);
    }
    let d = common::own_bytes(root);
    let many = 325 * 4096 + d;
    let files: Vec<Row> = (1..=25)
        .rev()
        .map(|i| Row::file(i * 4096, many, &format!("f{i:02}")))
        .collect();
    let mut b = scanned_as(root, "/many", Desktop::None, false);
    let title = format!("/many  {}{SCANNED}", size(many + d));
    let parent = [Row::dir(many, many + d, "many")];
    // the wheel over the preview scrolls it, by a row
    draw(&mut b, now());
    b.mouse(mouse(MouseEventKind::ScrollDown, 40, 5), now());
    let expected = screen(&title, [&[], &parent, &files[1..]], None, 0, HELP);
    assert_eq!(draw(&mut b, now()), expected);

    // a page is the 18 rows shown
    press(&mut b, &[KeyCode::Right, KeyCode::PageDown]);
    let expected = screen(&title, [&parent, &files[1..], &[]], Some(0), 17, HELP);
    assert_eq!(draw(&mut b, now()), expected);
    press(&mut b, &[KeyCode::End]);
    let expected = screen(&title, [&parent, &files[7..], &[]], Some(0), 17, HELP);
    assert_eq!(draw(&mut b, now()), expected);
    press(&mut b, &[KeyCode::Char('g')]);
    let expected = screen(&title, [&parent, &files, &[]], Some(0), 0, HELP);
    assert_eq!(draw(&mut b, now()), expected);
}

/// The fixture with one of the hard links `a/h1` and `a/h2` removed, as
/// which of the two a scan keeps among the largest depends on the order
/// the directory lists them in.
fn one_link() -> Fixture {
    let f = fixture();
    fs::remove_file(f.dir.path().join("a/h2")).unwrap();
    f
}

/// The `t` screen of [`one_link`] from the file at `from`, which is
/// selected.
fn top_files(f: &Fixture, from: usize) -> Buffer {
    let mut files = vec![
        (1572864, "big/f"),
        (12288, "a/b/c/f3"),
        (8192, "a/b/f2"),
        (8192, "a/h1"),
        (8192, "top"),
        (4096, "a/f1"),
    ];
    if f.sym_bytes > 0 {
        files.push((f.sym_bytes, "sym"));
    }
    files.sort_by_key(|&(size, path)| (Reverse(size), path));
    let mut lines = text(&[&expected(f).title], TOP_HELP);
    // no bars: the files are in different dirs
    for (line, &(bytes, path)) in lines[1..].iter_mut().zip(&files[from..]) {
        let size = size(bytes);
        *line = Line::from_iter([
            Span::styled(format!("{size:>10}"), COLOR.size(&size)),
            Span::raw(format!("  {path}")),
        ]);
    }
    plain(&lines, Some(1))
}

#[test]
fn lists_the_largest_files() {
    let f = one_link();
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Char('t')]);
    assert_eq!(draw(&mut b, now()), top_files(&f, 0));

    press(&mut b, &[KeyCode::Char('t')]);
    let e = expected(&f);
    assert_eq!(draw(&mut b, now()), first_view(&e, &e.title, HELP));
}

#[test]
fn drops_a_largest_file_deleted_since_the_scan() {
    let f = one_link();
    let mut b = scanned(f.dir.path(), Desktop::None);
    fs::remove_file(f.dir.path().join("big/f")).unwrap();
    press(&mut b, &[KeyCode::Char('t')]);
    assert_eq!(draw(&mut b, now()), top_files(&f, 1));
}

#[test]
fn reveals_and_opens_a_largest_file() {
    let f = one_link();
    let root = f.dir.path();
    let mut b = scanned(root, Desktop::Mac);
    let (big, f3) = (root.join("big/f"), root.join("a/b/c/f3"));
    let runs = press(&mut b, &[KeyCode::Char('t'), KeyCode::Char('r')]);
    let open = "/usr/bin/open".to_string();
    assert_eq!(runs, [(open.clone(), vec!["-R".into(), big.into()])]);
    let runs = press(&mut b, &[KeyCode::Char('j'), KeyCode::Char('o')]);
    assert_eq!(runs, [(open, vec![f3.into()])]);
}

/// Polls `b` until it draws `expected`, for at most `within`.
fn shows_within(b: &mut Browser, expected: &Buffer, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        b.poll(now());
        let drawn = draw(b, now());
        if drawn == *expected {
            return;
        }
        if Instant::now() > deadline {
            assert_eq!(drawn, *expected, "not shown within {within:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `root/a/` with `keep` (4096) and `gone` (8192), browsed over a saved
/// scan, so scanned again, then changed while shown. Each change shows
/// `within`. `inotify` is for [`Env::inotify`]. `at` gives the path the
/// browser is given for the dir.
fn follows_changes(inotify: bool, within: Duration, at: fn(&Path) -> PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("a")).unwrap();
    file(&root.join("a/keep"), 4096);
    file(&root.join("a/gone"), 8192);
    let d = common::own_bytes(root);
    // the root lists only `a/`, previewed, with how much it changed since
    // the scan, `age` ago
    let shown_at = |age: &str, files: &[(u64, &str)]| {
        let a = d + files.iter().map(|&(size, _)| size).sum::<u64>();
        let delta = a as i64 - (d + 8192 + 4096) as i64;
        let change = match delta {
            0 => String::new(),
            _ => format!("  ({} in {age})", signed(delta)),
        };
        let scanned = SCANNED.replace("0 s", age);
        let title = format!("/live  {}{change}{scanned}", kib(a + d));
        // each file's change since it was first shown, at the scan
        let was = |name| match name {
            "keep" => 4096,
            "gone" => 8192,
            _ => 0,
        };
        let files: Vec<Row> = files
            .iter()
            .map(|&(size, name)| Row::file(size, a, name).changed(size as i64 - was(name)))
            .collect();
        let root = [Row::dir(a, a + d, "a").changed(delta)];
        screen(&title, [&[], &root, &files], None, 0, HELP)
    };
    let shown = |files: &[(u64, &str)]| shown_at("0 s", files);
    let saved = Saved {
        tree: diskuse::scan(root, &ScanOptions::default()).unwrap(),
        reclaimable: false,
        modified: now(),
    };
    let env = Env {
        inotify,
        ..env(Desktop::None)
    };
    let mut b = Browser::new(&at(root), "/live", env, Some(saved), None, false);
    b.scan();
    while b.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        draw(&mut b, now()),
        shown(&[(8192, "gone"), (4096, "keep")])
    );

    file(&root.join("a/new"), 4096);
    let expected = shown(&[(8192, "gone"), (4096, "keep"), (4096, "new")]);
    shows_within(&mut b, &expected, within);
    fs::remove_file(root.join("a/gone")).unwrap();
    shows_within(&mut b, &shown(&[(4096, "keep"), (4096, "new")]), within);
    // in place: no entry of `a/` changes
    let keep = common::append(&root.join("a/keep"), 4096);
    let files = [(keep, "keep"), (4096, "new")];
    shows_within(&mut b, &shown(&files), within);
    // the title says since when, and on Linux how old the scan is
    let later = now() + Duration::from_secs(4 * 60);
    assert_eq!(draw(&mut b, later), shown_at("4 min", &files));
}

/// FSEvents on macOS, inotify on Linux. The deadline is generous so a
/// loaded machine cannot fail it; inotify itself shows changes well inside
/// a second.
#[test]
fn follows_changes_on_disk() {
    follows_changes(true, Duration::from_secs(3), Path::to_path_buf);
}

/// Below `/System/Volumes/Data`, where FSEvents reports changes at their
/// firmlinked path, such as `/private/var/...` for a temp dir.
#[cfg(target_os = "macos")]
#[test]
fn follows_changes_below_the_data_volume() {
    let data = |root: &Path| {
        let real = fs::canonicalize(root).unwrap();
        Path::new("/System/Volumes/Data").join(real.strip_prefix("/").unwrap())
    };
    follows_changes(true, Duration::from_secs(3), data);
}

/// Without inotify, as on NFS, the dirs shown are listed again every 2 s.
#[cfg(target_os = "linux")]
#[test]
fn follows_changes_on_a_timer_without_inotify() {
    follows_changes(false, Duration::from_secs(3), Path::to_path_buf);
}

/// The dirs below `root`, of `dirs`, that an inotify watch of this process
/// is on, from `/proc`. The watches of other tests are on other dirs.
#[cfg(target_os = "linux")]
fn watched<'a>(root: &Path, dirs: &[&'a str]) -> Vec<&'a str> {
    use std::os::unix::fs::MetadataExt;
    let ino = |d: &str| fs::metadata(root.join(d)).unwrap().ino();
    let mut watched = Vec::new();
    for fd in fs::read_dir("/proc/self/fdinfo").unwrap() {
        // closed since it was listed
        let Ok(info) = fs::read_to_string(fd.unwrap().path()) else {
            continue;
        };
        // `inotify wd:1 ino:2a sdev:... mask:...`, hex
        for line in info.lines().filter_map(|l| l.strip_prefix("inotify wd:")) {
            let hex = line.split(' ').find_map(|f| f.strip_prefix("ino:"));
            let n = u64::from_str_radix(hex.unwrap(), 16).unwrap();
            watched.extend(dirs.iter().filter(|d| ino(d) == n));
        }
    }
    watched.sort_unstable();
    watched
}

/// ```text
/// root/
///   a/x/f    8192
///   b/
/// ```
#[cfg(target_os = "linux")]
#[test]
fn watches_only_the_dirs_shown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("a/x")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    file(&root.join("a/x/f"), 8192);
    let dirs = ["", "a", "a/x", "b"];
    let mut b = scanned(root, Desktop::None);
    // the root, and `a/` previewed
    assert_eq!(watched(root, &dirs), ["", "a"]);
    press(&mut b, &[KeyCode::Right]);
    assert_eq!(watched(root, &dirs), ["", "a", "a/x"]);
    press(&mut b, &[KeyCode::Left, KeyCode::Down]);
    assert_eq!(watched(root, &dirs), ["", "b"]);
    drop(b);
    assert_eq!(watched(root, &dirs), [""; 0]);
}

/// ```text
/// root/
///   x0/f .. x4/f    4096 each
/// ```
///
/// scanned with a stop at once, so the saved scan has the root alone. The
/// browser shows it, saying it is incomplete and being scanned again, then
/// the fresh scan.
#[test]
fn rescans_over_a_stopped_saved_scan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let names = ["x0", "x1", "x2", "x3", "x4"];
    for name in names {
        fs::create_dir(root.join(name)).unwrap();
        file(&root.join(name).join("f"), 4096);
    }
    let opts = ScanOptions {
        stop: diskuse::Stop::new(|| true),
        ..ScanOptions::default()
    };
    let saved = Saved {
        tree: diskuse::scan(root, &opts).unwrap(),
        reclaimable: false,
        modified: now(),
    };
    let mut b = Browser::new(
        root,
        "/stopped",
        env(Desktop::None),
        Some(saved),
        None,
        false,
    );
    b.scan();
    let d = common::own_bytes(root);
    let title = format!(
        "/stopped  {}  saved 0 s ago (incomplete), rescanning...",
        kib(d)
    );
    assert_eq!(
        draw(&mut b, now()),
        screen(&title, [&[], &[], &[]], None, 0, HELP)
    );

    while b.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let x = 4096 + d;
    let total = 5 * x + d;
    let rows: Vec<Row> = names.iter().map(|name| Row::dir(x, total, name)).collect();
    let title = format!("/stopped  {}{SCANNED}", kib(total));
    let f = [Row::file(4096, x, "f")];
    let expected = screen(&title, [&[], &rows, &f], None, 0, HELP);
    assert_eq!(draw(&mut b, now()), expected);
}

/// ```text
/// root/
///   a/x/f    4096
///   b/g      4096
/// ```
///
/// then `a/x/new` (8192) created. `s` on `a/` scans it again while the
/// tree shown stays, and `S` scans all again, keeping what the session's
/// changes are counted against.
#[test]
fn rescans_the_selected_dir_or_all() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("a/x")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    file(&root.join("a/x/f"), 4096);
    file(&root.join("b/g"), 4096);
    let mut b = scanned(root, Desktop::None);
    let d = common::own_bytes(root);
    let shown = |new: u64, title_end: &str| {
        let (x, bb) = (4096 + d + new, 4096 + d);
        let (a, total) = (x + d, x + bb + 2 * d);
        let change = match new {
            0 => String::new(),
            _ => format!("  (+{} in 0 s)", kib(new)),
        };
        let title = format!("/fixture  {}{change}{SCANNED}{title_end}", kib(total));
        let root = [
            Row::dir(a, total, "a").changed(new as i64),
            Row::dir(bb, total, "b"),
        ];
        let preview = [Row::dir(x, a, "x").changed(new as i64)];
        screen(&title, [&[], &root, &preview], None, 0, HELP)
    };
    file(&root.join("a/x/new"), 8192);

    press(&mut b, &[KeyCode::Char('s')]);
    assert_eq!(draw(&mut b, now()), shown(0, "  rescanning a/..."));
    while b.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(draw(&mut b, now()), shown(8192, ""));

    press(&mut b, &[KeyCode::Char('S')]);
    assert_eq!(
        draw(&mut b, now()),
        screen("/fixture  scanning...", [&[], &[], &[]], None, 0, HELP)
    );
    while b.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(draw(&mut b, now()), shown(8192, ""));
}

/// ```text
/// root/
///   a/f    8192
///   g      4096
/// ```
///
/// `space` picks the item at the cursor, marked `*`, and `p` lists the
/// picks with their sizes now. They are saved, so a later browser has
/// them, and one deleted since shows as gone.
#[test]
fn picks_items_and_lists_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("a")).unwrap();
    file(&root.join("a/f"), 8192);
    file(&root.join("g"), 4096);
    let cache = tempfile::tempdir().unwrap();
    let browse = || {
        let env = Env {
            cache: Some(CacheDir::at(cache.path().into())),
            ..env(Desktop::None)
        };
        let mut b = Browser::new(root, "/picks", env, None, None, false);
        b.scan();
        while b.poll(now()) {
            std::thread::sleep(Duration::from_millis(1));
        }
        b
    };
    let d = common::own_bytes(root);
    let (a, total) = (8192 + d, 8192 + 4096 + 2 * d);
    let title = format!("/picks  {}{SCANNED}", kib(total));
    let mut b = browse();
    press(
        &mut b,
        &[KeyCode::Char(' '), KeyCode::Down, KeyCode::Char(' ')],
    );
    let rows = [
        Row::dir(a, total, "a").picked(),
        Row::file(4096, total, "g").picked(),
    ];
    let expected = screen(&title, [&[], &rows, &[]], None, 1, HELP);
    assert_eq!(draw(&mut b, now()), expected);

    let listed = |title: &str, head: &str, picks: &[(Option<u64>, &str)], selected: usize| {
        let mut lines = text(&[title, head], PICKS_HELP);
        for (line, &(bytes, path)) in lines[2..].iter_mut().zip(picks) {
            let size = match bytes {
                Some(bytes) => {
                    let size = size(bytes);
                    Span::styled(format!("{size:>10}"), COLOR.size(&size))
                }
                None => Span::styled(format!("{:>10}", "gone"), COLOR.dim()),
            };
            *line = Line::from_iter([size, Span::raw(format!("  {path}"))]);
        }
        plain(&lines, Some(2 + selected))
    };
    press(&mut b, &[KeyCode::Char('p')]);
    let head = format!("2 picked, {} in all:", kib(a + 4096));
    let both = [(Some(a), "a"), (Some(4096), "g")];
    assert_eq!(draw(&mut b, now()), listed(&title, &head, &both, 0));
    drop(b);

    fs::remove_file(root.join("g")).unwrap();
    let mut b = browse();
    press(&mut b, &[KeyCode::Char('p'), KeyCode::Down]);
    let head = format!("2 picked, {} in all:", kib(a));
    let gone = [(Some(a), "a"), (None, "g")];
    let title = format!("/picks  {}{SCANNED}", kib(a + d));
    assert_eq!(draw(&mut b, now()), listed(&title, &head, &gone, 1));
    press(&mut b, &[KeyCode::Char(' ')]);
    let head = format!("1 picked, {} in all:", kib(a));
    assert_eq!(draw(&mut b, now()), listed(&title, &head, &gone[..1], 0));
}

/// ```text
/// root/
///   big/f      16384
///   small/f     4096
/// ```
///
/// then `small/new` (8192) created and `small/` rescanned. `c` sorts by
/// change, so `small/` goes above the larger `big/`, and again by size.
#[test]
fn sorts_by_change_on_c() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("big")).unwrap();
    fs::create_dir(root.join("small")).unwrap();
    file(&root.join("big/f"), 16384);
    file(&root.join("small/f"), 4096);
    let mut b = scanned(root, Desktop::None);
    file(&root.join("small/new"), 8192);
    press(&mut b, &[KeyCode::Down, KeyCode::Char('s')]);
    while b.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let d = common::own_bytes(root);
    let (big, small) = (16384 + d, 12288 + d);
    let total = big + small + d;
    let title = format!("/fixture  {}  (+8.0 KiB in 0 s){SCANNED}", kib(total));
    let bigs = Row::dir(big, total, "big");
    let smalls = Row::dir(small, total, "small").changed(8192);
    // `small/` was not shown before it changed, so its files' sizes then
    // are not known; its own change is
    let files = [Row::file(8192, small, "new"), Row::file(4096, small, "f")];
    // the cursor stays on `small/`
    let by_size = screen(
        &title,
        [&[], &[bigs.clone(), smalls.clone()], &files],
        None,
        1,
        HELP,
    );
    assert_eq!(draw(&mut b, now()), by_size);
    press(&mut b, &[KeyCode::Char('c')]);
    let title = format!("{title}  [by change]");
    let by_change = screen(&title, [&[], &[smalls, bigs], &files], None, 0, HELP);
    assert_eq!(draw(&mut b, now()), by_change);
    press(&mut b, &[KeyCode::Char('c')]);
    assert_eq!(draw(&mut b, now()), by_size);
}

/// Below the home dir, the title shows the root from `~`.
#[test]
fn titles_a_path_below_home_with_a_tilde() {
    let f = fixture();
    let e = expected(&f);
    let path = f.dir.path();
    let env = Env {
        home: Some(path.parent().unwrap().into()),
        ..env(Desktop::Mac)
    };
    let mut app = App::new(Some(path), vec![], env, None, false);
    finish(&mut app);
    let name = path.file_name().unwrap().to_str().unwrap();
    let title = e.title.replacen("/fixture", &format!("~/{name}"), 1);
    assert_eq!(draw_app(&mut app), first_view(&e, &title, HELP));
}

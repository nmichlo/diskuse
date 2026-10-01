//! The screens, drawn into a 100x20 test terminal and compared cell by cell.

#![allow(clippy::disallowed_methods)] // fixtures create and delete files

mod common;

use common::{Fixture, file, fixture, kib};
use disksweep::reveal::Desktop;
use disksweep::{App, Browser, Env, FullDiskAccess, Mount, Saved, ScanOptions};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::{Frame, Terminal};
use std::cmp::Reverse;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

const WIDTH: usize = 100;
const HEIGHT: usize = 20;
/// `(x, width)` of the parent, current and preview columns.
const COLUMNS: [(usize, usize); 3] = [(0, 33), (34, 32), (67, 33)];
/// A wider terminal, for rows too long for 100 columns.
const WIDE: usize = 130;
const WIDE_COLUMNS: [(usize, usize); 3] = [(0, 43), (44, 42), (87, 43)];
const HELP: &str =
    "arrows/hjkl move  r reveal  o open  R rescan  / filter  d denied  t top files  q quit";
const TOP_HELP: &str = "arrows/jk move  r reveal  o open  t close  q quit";
const GIB: u64 = 1 << 30;
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

/// An expected screen: `title`, the rows of each column, and `footer`.
/// `parent` is the bold row of the parent column, `current` the reversed
/// row of the current one.
fn screen(
    title: &str,
    columns: [&[String]; 3],
    parent: Option<usize>,
    current: usize,
    footer: &str,
) -> Buffer {
    screen_in(WIDTH, COLUMNS, title, columns, parent, current, footer)
}

/// [`screen`] in a terminal `width` wide, with columns at `layout`.
fn screen_in(
    width: usize,
    layout: [(usize, usize); 3],
    title: &str,
    columns: [&[String]; 3],
    parent: Option<usize>,
    current: usize,
    footer: &str,
) -> Buffer {
    let mut lines = vec![vec![' '; width]; HEIGHT];
    let mut put = |y: usize, x: usize, width: usize, text: &str| {
        for (i, c) in text.chars().take(width).enumerate() {
            lines[y][x + i] = c;
        }
    };
    put(0, 0, width, title);
    for (rows, (x, width)) in columns.iter().zip(layout) {
        for (y, row) in rows.iter().take(HEIGHT - 2).enumerate() {
            put(y + 1, x, width, row);
        }
    }
    put(HEIGHT - 1, 0, width, footer);
    let lines: Vec<String> = lines.into_iter().map(String::from_iter).collect();
    let mut buf = Buffer::with_lines(lines);
    let row =
        |i: usize, (x, width): (usize, usize)| Rect::new(x as u16, 1 + i as u16, width as u16, 1);
    if let Some(i) = parent {
        buf.set_style(row(i, layout[0]), Style::new().add_modifier(Modifier::BOLD));
    }
    if current < columns[1].len() {
        buf.set_style(
            row(current, layout[1]),
            Style::new().add_modifier(Modifier::REVERSED),
        );
    }
    buf
}

fn row(size: &str, name: &str) -> String {
    format!("{size:>10}  {name}")
}

/// A row with a reclaimable size too, as `-r` shows it.
#[cfg(target_os = "macos")]
fn row_r(size: &str, reclaimable: &str, name: &str) -> String {
    format!("{size:>10} {reclaimable:>10}  {name}")
}

/// A dir with something denied below it.
fn partial(size: &str, name: &str) -> String {
    format!("{size:>10}+ {name}")
}

/// An expected screen of whole-width `lines`, `reversed` the index of a
/// reversed one.
fn plain(lines: &[&str], reversed: Option<usize>) -> Buffer {
    let line = |y: usize| lines.get(y).unwrap_or(&"").chars().take(WIDTH);
    let rows = (0..HEIGHT).map(|y| format!("{:<WIDTH$}", String::from_iter(line(y))));
    let mut buf = Buffer::with_lines(rows);
    if let Some(y) = reversed {
        let style = Style::new().add_modifier(Modifier::REVERSED);
        buf.set_style(Rect::new(0, y as u16, WIDTH as u16, 1), style);
    }
    buf
}

/// An environment with no cache, so tests never touch the user's.
fn env(desktop: Desktop) -> Env {
    Env {
        desktop,
        terminal: "iTerm".into(),
        cache: None,
        inotify: true,
    }
}

/// What the browser shows of the fixture, scanned as root or not.
struct Expected {
    title: String,
    root: Vec<String>,
    a: Vec<String>,
    b: Vec<String>,
}

fn expected(f: &Fixture) -> Expected {
    let d = f.dir_bytes;
    let as_root = rustix::process::geteuid().is_root();
    // the same total `scan` prints
    let total = f.expected[..10].trim();
    let (dir, locked): (fn(&str, &str) -> String, u64) = match as_root {
        true => (row, 0),
        false => (partial, d),
    };
    let denied = "locked/ (denied: EACCES)";
    // whether these tie depends on the filesystem
    let mut small = vec![(d, "empty/"), (d, denied), (f.sym_bytes, "sym")];
    small.sort_by_key(|&(size, name)| (Reverse(size), name));
    small.retain(|&(_, name)| !(as_root && name == denied));
    let mut root = vec![
        row("1.5 MiB", "big/"),
        dir(&kib(32768 + 3 * d + locked), "a/"),
        row("8.0 KiB", "top"),
    ];
    root.extend(small.iter().map(|&(size, name)| row(&kib(size), name)));
    let a = vec![
        dir(&kib(20480 + 2 * d + locked), "b/"),
        row("8.0 KiB", "h1"),
        row("8.0 KiB", "h2"),
        row("4.0 KiB", "f1"),
    ];
    let mut b = vec![row(&kib(12288 + d), "c/"), row("8.0 KiB", "f2")];
    if !as_root {
        b.push(row(&kib(d), denied));
    }
    let title = match as_root {
        true => format!("/fixture  {total}{SCANNED}"),
        false => format!("/fixture  {total}  (partial: 2 denied){SCANNED}"),
    };
    Expected { title, root, a, b }
}

/// The first view: the root, its largest child selected and previewed.
fn first_view(e: &Expected, title: &str, footer: &str) -> Buffer {
    let big = [row("1.5 MiB", "f")];
    screen(title, [&[], &e.root, &big], None, 0, footer)
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

#[test]
fn shows_the_finished_scan() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    assert_eq!(draw(&mut b, now()), first_view(&e, &e.title, HELP));
}

#[test]
fn enters_two_levels() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Down, KeyCode::Right, KeyCode::Char('l')]);
    let c = [row("12.0 KiB", "f3")];
    let expected = screen(&e.title, [&e.a, &e.b, &c], Some(0), 0, HELP);
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
    let matches = [row("8.0 KiB", "top"), row(&kib(f.dir_bytes), "empty/")];
    let expected = screen(&e.title, [&[], &matches, &[]], None, 0, "/p");
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
        tree: disksweep::scan(f.dir.path(), &ScanOptions::default()).unwrap(),
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
    let why = "permission denied (try sudo)";
    let lines = match rustix::process::geteuid().is_root() {
        true => vec![e.title.clone(), "every directory could be read".into()],
        false => vec![
            e.title.clone(),
            "2 directories could not be read, so their contents are not counted:".into(),
            format!("{a}  {why}"),
            format!("{top}  {why}"),
        ],
    };
    let mut lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    lines.resize(HEIGHT - 1, "");
    lines.push("arrows/jk scroll  d close  q quit");
    assert_eq!(draw(&mut b, now()), plain(&lines, None));

    press(&mut b, &[KeyCode::Char('d')]);
    assert_eq!(draw(&mut b, now()), first_view(&e, &e.title, HELP));
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
    let lines = [
        "volumes",
        " 800.0 GiB used of  926.0 GiB   126.0 GiB free  /",
        "  10.0 GiB used of   64.0 GiB    54.0 GiB free  /Volumes/USB",
    ];
    let mut screen = lines.to_vec();
    screen.resize(HEIGHT - 1, "");
    screen.push("arrows/jk move  enter scan  q quit");
    assert_eq!(draw_app(&mut app), plain(&screen, Some(1)));
    press_app(&mut app, &[KeyCode::Down, KeyCode::Down]);
    assert_eq!(draw_app(&mut app), plain(&screen, Some(2)));
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
    let total = disksweep::scan(f.dir.path(), &ScanOptions::default()).unwrap();
    let used = total.totals().size[0] + (1 << 20);
    let mut app = enter_volume(&f, used, FullDiskAccess::Missing);
    let guide = [
        "Full Disk Access is off for iTerm",
        "",
        "macOS keeps some folders from every app without Full Disk Access: Mail, Messages, Safari, Time",
        "Machine and other apps' data. The scan lists them as denied, and their sizes are missing. To grant",
        "it, turn on iTerm in System Settings > Privacy & Security > Full Disk Access, then quit and reopen",
        "iTerm. disksweep stays read-only either way.",
        "",
        "o  open the Full Disk Access settings",
        "c  continue without",
        "q  quit",
    ];
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
    let used = format!("{:.1} MiB", used as f64 / (1 << 20) as f64);
    let line = format!("{used:>10} used of  100.0 GiB    50.0 GiB free  {root}");
    let mut list = vec!["volumes", &line];
    list.resize(HEIGHT - 1, "");
    list.push("arrows/jk move  enter scan  q quit");
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
    let root = [
        row_r("1.0 MiB", "0 B", "clone/"),
        row_r("1.0 MiB", "0 B", "orig/"),
        row_r("1.0 MiB", "1.0 MiB", "solo/"),
    ];
    let clone = [row_r("1.0 MiB", "0 B", "f")];
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
///   .venv/pyvenv.cfg            0   [cache: venv]
///   venv/                           no label, no pyvenv.cfg
/// ```
#[test]
fn labels_dirs_tools_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for d in [
        "rust/target",
        "node_modules",
        "target",
        "__pycache__",
        ".venv",
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
    let d = common::own_bytes(root);
    let mut b = scanned_as(root, "/labels", Desktop::None, false);
    let rows = [
        row(&kib(20480 + 2 * d), "rust/"),
        row(&kib(12288 + d), "node_modules/  [cache: npm]"),
        row(&kib(8192 + d), "target/"),
        row(&kib(4096 + d), "__pycache__/  [cache: python]"),
        row(&kib(d), ".venv/  [cache: venv]"),
        row(&kib(d), "venv/"),
    ];
    let rust = [
        row(&kib(16384 + d), "target/  [cache: cargo]"),
        row("4.0 KiB", "Cargo.toml"),
    ];
    let title = format!("/labels  {}{SCANNED}", kib(45056 + 8 * d));
    let expected = screen_in(
        WIDE,
        WIDE_COLUMNS,
        &title,
        [&[], &rows, &rust],
        None,
        0,
        HELP,
    );
    assert_eq!(render_in(WIDE, |f| b.draw(f, now())), expected);
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
    let rows: Vec<String> = files[from..]
        .iter()
        .map(|&(size, path)| match size {
            1572864 => row("1.5 MiB", path),
            _ => row(&kib(size), path),
        })
        .collect();
    let mut lines = vec![expected(f).title];
    lines.extend(rows);
    let mut lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    lines.resize(HEIGHT - 1, "");
    lines.push(TOP_HELP);
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

/// `root/a/` with `keep` (4096) and `gone` (8192), browsed from a saved
/// scan brought up to date, or on Linux scanned again, then changed while
/// shown. Each change shows `within`. `inotify` is for [`Env::inotify`].
fn follows_changes(inotify: bool, within: Duration) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("a")).unwrap();
    file(&root.join("a/keep"), 4096);
    file(&root.join("a/gone"), 8192);
    let d = common::own_bytes(root);
    // the root lists only `a/`, previewed
    let shown_at = |scanned: &str, files: &[(u64, &str)]| {
        let a = d + files.iter().map(|&(size, _)| size).sum::<u64>();
        let title = format!("/live  {}{scanned}", kib(a + d));
        let files: Vec<String> = files
            .iter()
            .map(|&(size, name)| row(&kib(size), name))
            .collect();
        screen(&title, [&[], &[row(&kib(a), "a/")], &files], None, 0, HELP)
    };
    let shown = |files: &[(u64, &str)]| shown_at(SCANNED, files);
    let saved = Saved {
        tree: disksweep::scan(root, &ScanOptions::default()).unwrap(),
        reclaimable: false,
        modified: now(),
    };
    let env = Env {
        inotify,
        ..env(Desktop::None)
    };
    let mut b = Browser::new(root, "/live", env, Some(saved), None, false);
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
    common::append(&root.join("a/keep"), 4096);
    let files = [(8192, "keep"), (4096, "new")];
    shows_within(&mut b, &shown(&files), within);
    // on Linux the title says how old the scan is
    let later = now() + Duration::from_secs(4 * 60);
    let scanned = SCANNED.replace("0 s", "4 min");
    assert_eq!(draw(&mut b, later), shown_at(&scanned, &files));
}

/// FSEvents on macOS, inotify on Linux.
#[test]
fn follows_changes_on_disk() {
    let within = match cfg!(target_os = "macos") {
        true => 3,
        false => 1,
    };
    follows_changes(true, Duration::from_secs(within));
}

/// Without inotify, as on NFS, the dirs shown are listed again every 2 s.
#[cfg(target_os = "linux")]
#[test]
fn follows_changes_on_a_timer_without_inotify() {
    follows_changes(false, Duration::from_secs(3));
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

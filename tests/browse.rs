//! The screens, drawn into a 100x20 test terminal and compared cell by cell.

mod common;

use common::{Fixture, fixture, kib};
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
use std::io;
use std::ops::ControlFlow;
use std::path::Path;
use std::time::{Duration, SystemTime};

const WIDTH: usize = 100;
const HEIGHT: usize = 20;
/// `(x, width)` of the parent, current and preview columns.
const COLUMNS: [(usize, usize); 3] = [(0, 33), (34, 32), (67, 33)];
const HELP: &str = "arrows/hjkl move  r reveal  o open  R rescan  / filter  d denied  q quit";
const GIB: u64 = 1 << 30;

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
    let mut lines = vec![vec![' '; WIDTH]; HEIGHT];
    let mut put = |y: usize, x: usize, width: usize, text: &str| {
        for (i, c) in text.chars().take(width).enumerate() {
            lines[y][x + i] = c;
        }
    };
    put(0, 0, WIDTH, title);
    for (rows, (x, width)) in columns.iter().zip(COLUMNS) {
        for (y, row) in rows.iter().take(HEIGHT - 2).enumerate() {
            put(y + 1, x, width, row);
        }
    }
    put(HEIGHT - 1, 0, WIDTH, footer);
    let lines: Vec<String> = lines.into_iter().map(String::from_iter).collect();
    let mut buf = Buffer::with_lines(lines);
    let row =
        |i: usize, (x, width): (usize, usize)| Rect::new(x as u16, 1 + i as u16, width as u16, 1);
    if let Some(i) = parent {
        buf.set_style(
            row(i, COLUMNS[0]),
            Style::new().add_modifier(Modifier::BOLD),
        );
    }
    if current < columns[1].len() {
        buf.set_style(
            row(current, COLUMNS[1]),
            Style::new().add_modifier(Modifier::REVERSED),
        );
    }
    buf
}

fn row(size: &str, name: &str) -> String {
    format!("{size:>10}  {name}")
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
        true => format!("/fixture  {total}"),
        false => format!("/fixture  {total}  (partial: 2 denied)"),
    };
    Expected { title, root, a, b }
}

/// The first view: the root, its largest child selected and previewed.
fn first_view(e: &Expected, title: &str, footer: &str) -> Buffer {
    let big = [row("1.5 MiB", "f")];
    screen(title, [&[], &e.root, &big], None, 0, footer)
}

fn render(draw: impl FnOnce(&mut Frame)) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH as u16, HEIGHT as u16)).unwrap();
    terminal.draw(draw).unwrap();
    terminal.backend().buffer().clone()
}

fn draw(b: &mut Browser, now: SystemTime) -> Buffer {
    render(|frame| b.draw(frame, now))
}

fn draw_app(app: &mut App) -> Buffer {
    render(|frame| app.draw(frame, SystemTime::now()))
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
    let mut b = Browser::new(root, "/fixture", env(desktop), None, None);
    b.scan();
    while b.poll() {
        std::thread::sleep(Duration::from_millis(1));
    }
    b
}

#[test]
fn shows_the_finished_scan() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    assert_eq!(
        draw(&mut b, SystemTime::now()),
        first_view(&e, &e.title, HELP)
    );
}

#[test]
fn enters_two_levels() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Down, KeyCode::Right, KeyCode::Char('l')]);
    let c = [row("12.0 KiB", "f3")];
    let expected = screen(&e.title, [&e.a, &e.b, &c], Some(0), 0, HELP);
    assert_eq!(draw(&mut b, SystemTime::now()), expected);

    // back up to the root, with `a/` selected
    press(
        &mut b,
        &[KeyCode::Left, KeyCode::Char('h'), KeyCode::Char('h')],
    );
    let expected = screen(&e.title, [&[], &e.root, &e.a], None, 1, HELP);
    assert_eq!(draw(&mut b, SystemTime::now()), expected);
}

#[test]
fn filters_the_current_column() {
    let f = fixture();
    let e = expected(&f);
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Char('/'), KeyCode::Char('p')]);
    let matches = [row("8.0 KiB", "top"), row(&kib(f.dir_bytes), "empty/")];
    let expected = screen(&e.title, [&[], &matches, &[]], None, 0, "/p");
    assert_eq!(draw(&mut b, SystemTime::now()), expected);

    // clearing the filter keeps a selection made while filtered
    press(&mut b, &[KeyCode::Down, KeyCode::Esc]);
    let empty = e.root.iter().position(|r| *r == matches[1]).unwrap();
    let expected = screen(&e.title, [&[], &e.root, &[]], None, empty, HELP);
    assert_eq!(draw(&mut b, SystemTime::now()), expected);
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
    );
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + 5 * 60);
    let title = format!("{}  saved 5 min ago", e.title);
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
    assert_eq!(
        draw(&mut b, SystemTime::now()),
        first_view(&e, &e.title, &footer)
    );
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
    assert_eq!(draw(&mut b, SystemTime::now()), plain(&lines, None));

    press(&mut b, &[KeyCode::Char('d')]);
    assert_eq!(
        draw(&mut b, SystemTime::now()),
        first_view(&e, &e.title, HELP)
    );
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
    let mut app = App::new(None, mounts, env(Desktop::Mac), None);
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
    let mut app = App::new(None, vec![volume], env(Desktop::Mac), Some(preflight));
    press_app(&mut app, &[KeyCode::Enter]);
    app
}

fn finish(app: &mut App) {
    while app.poll() {
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

//! The browser, drawn into a 100x20 test terminal and compared cell by cell.

mod common;

use common::{Fixture, fixture, kib};
use disksweep::reveal::Desktop;
use disksweep::{Browser, Saved, ScanOptions};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::cmp::Reverse;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

const WIDTH: usize = 100;
const HEIGHT: usize = 20;
/// `(x, width)` of the parent, current and preview columns.
const COLUMNS: [(usize, usize); 3] = [(0, 33), (34, 32), (67, 33)];
const HELP: &str = "arrows/hjkl move  r reveal  o open  R rescan  / filter  q quit";

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
    let (partial, locked) = match as_root {
        true => ("", 0),
        false => (" (partial)", d),
    };
    let denied = "locked/ (denied: EACCES)";
    // whether these tie depends on the filesystem
    let mut small = vec![(d, "empty/"), (d, denied), (f.sym_bytes, "sym")];
    small.sort_by_key(|&(size, name)| (Reverse(size), name));
    small.retain(|&(_, name)| !(as_root && name == denied));
    let mut root = vec![
        row("1.5 MiB", "big/"),
        row(&kib(32768 + 3 * d + locked), &format!("a/{partial}")),
        row("8.0 KiB", "top"),
    ];
    root.extend(small.iter().map(|&(size, name)| row(&kib(size), name)));
    let a = vec![
        row(&kib(20480 + 2 * d + locked), &format!("b/{partial}")),
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

fn draw(b: &mut Browser, now: SystemTime) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH as u16, HEIGHT as u16)).unwrap();
    terminal.draw(|frame| b.draw(frame, now)).unwrap();
    terminal.backend().buffer().clone()
}

/// The program and arguments of each command run.
type Runs = Vec<(String, Vec<OsString>)>;

/// Presses `keys`, recording commands instead of running them.
fn press(b: &mut Browser, keys: &[KeyCode]) -> Runs {
    let mut runs = Vec::new();
    for &key in keys {
        let mut run = |program: &str, args: &[OsString]| -> io::Result<()> {
            runs.push((program.to_string(), args.to_vec()));
            Ok(())
        };
        assert!(b.key(KeyEvent::from(key), &mut run).is_continue());
    }
    runs
}

/// A browser of the fixture after a real scan of it has finished.
fn scanned(root: &Path, desktop: Desktop) -> Browser {
    let mut b = Browser::new(root, "/fixture", desktop, None, None);
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
    let mut b = Browser::new(f.dir.path(), "/fixture", Desktop::None, Some(saved), None);
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

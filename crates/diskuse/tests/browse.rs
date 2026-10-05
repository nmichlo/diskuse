//! The screens, drawn into a test terminal and compared with the stored
//! ones in `tests/snapshots`: the text of every row, then every run of
//! cells that is not in the default style.
//!
//! Sizes depend on the filesystem (a dir takes 0 B on APFS and tmpfs,
//! 4 KiB on ext4) and the title on the OS, so each screen is stored once
//! per platform: `name@macos-d0-s0.snap`, `name@linux-d4096-s0.snap`.
//! `cargo insta review` shows what changed and accepts it.

#![allow(clippy::disallowed_methods)] // fixtures create and delete files

#[path = "../../diskuse-core/tests/common/mod.rs"]
mod common;

use common::{Fixture, file, fixture, tempdir};
use diskuse::reveal::Desktop;
use diskuse::{App, Browser, Env};
use diskuse_core::{
    CacheDir, FolderId, FullDiskAccess, Mount, ReadTree, Saved, ScanOptions, Units,
};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::{Color, Style};
use ratatui::{Frame, Terminal};
use std::ffi::OsString;
use std::fmt::Write;
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

const WIDTH: usize = 100;
const HEIGHT: usize = 20;
/// A wider terminal, where every column is as wide as it gets.
const WIDE: usize = 130;
const GIB: u64 = 1 << 30;

/// The time of day the tests' clock always gives, so a scan is dated, and
/// drawn, then.
fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
}

/// `ms` milliseconds after [`now`].
fn after(ms: u64) -> SystemTime {
    now() + Duration::from_millis(ms)
}

/// What the stored screens depend on: the OS, and the bytes a dir and a
/// symlink take on the filesystem of `/tmp`.
fn platform() -> &'static str {
    static PLATFORM: OnceLock<String> = OnceLock::new();
    PLATFORM.get_or_init(|| {
        let dir = tempdir();
        symlink("x", dir.path().join("sym")).unwrap();
        let d = common::own_bytes(dir.path());
        let sym = common::own_bytes(&dir.path().join("sym"));
        format!("{}-d{d}-s{sym}", std::env::consts::OS)
    })
}

/// `buf` as text: each row without the spaces after it, then each run of
/// cells of one style that is not the default, as `row x..x style`.
fn shot(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        let row: String = (0..area.width).map(|x| buf[(x, y)].symbol()).collect();
        writeln!(out, "{}", row.trim_end()).unwrap();
    }
    writeln!(out, "--- styles ---").unwrap();
    for y in 0..area.height {
        let mut x = 0;
        while x < area.width {
            let style = buf[(x, y)].style();
            let start = x;
            while x < area.width && buf[(x, y)].style() == style {
                x += 1;
            }
            let plain = Style::new().fg(Color::Reset).bg(Color::Reset);
            if style.fg != plain.fg || style.bg != plain.bg || !style.add_modifier.is_empty() {
                let color = |c: Option<Color>| c.filter(|&c| c != Color::Reset);
                let mut what = Vec::new();
                what.extend(color(style.fg).map(|c| format!("{c:?}")));
                what.extend(color(style.bg).map(|c| format!("on {c:?}")));
                if !style.add_modifier.is_empty() {
                    what.push(format!("{:?}", style.add_modifier));
                }
                writeln!(out, "{y:>2} {start:>3}..{x:<3} {}", what.join(" ")).unwrap();
            }
        }
    }
    out
}

/// Compares `buf` with the stored screen `name` of this platform. Temp
/// dirs show as `du-XXXXXX`. Root reads every dir, so sees other screens:
/// they are not stored, and not compared.
macro_rules! assert_screen {
    ($name:expr, $buf:expr) => {{
        let buf: Buffer = $buf;
        if !rustix::process::geteuid().is_root() {
            insta::with_settings!({
                snapshot_suffix => platform(),
                filters => vec![(r"du-[A-Za-z0-9]{6}", "du-XXXXXX")],
                prepend_module_to_snapshot => false,
                omit_expression => true,
            }, {
                insta::assert_snapshot!($name, shot(&buf));
            });
        }
    }};
}

/// An environment with no cache, so tests never touch the user's.
fn env(desktop: Desktop) -> Env {
    Env {
        desktop,
        terminal: "iTerm".into(),
        cache: None,
        inotify: true,
        // often, so the tests are fast
        interval: Duration::from_millis(5),
        units: Units::Binary,
        wake: None,
        color: true,
        home: None,
    }
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

/// Polls `b` until no scan runs.
fn finish_scan(b: &mut Browser) {
    while b.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn finish(app: &mut App) {
    while app.poll(now()) {
        std::thread::sleep(Duration::from_millis(1));
    }
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
    finish_scan(&mut b);
    b
}

/// The first view: the root, its largest child selected and previewed, 130
/// columns wide, where every key fits.
#[test]
fn shows_the_finished_scan() {
    let f = fixture();
    let mut b = scanned(f.dir.path(), Desktop::None);
    assert_screen!("first_view_wide", render_in(WIDE, |f| b.draw(f, now())));
}

/// Under 100 columns the parent column goes, under 60 the preview too,
/// and the keys and words that do not fit.
#[test]
fn drops_columns_on_narrow_screens() {
    let f = fixture();
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Down, KeyCode::Right]);
    assert_screen!("in_a_80_wide", render_in(80, |f| b.draw(f, now())));
    assert_screen!("in_a_59_wide", render_in(59, |f| b.draw(f, now())));
    // no share of the dir under 50
    assert_screen!("in_a_49_wide", render_in(49, |f| b.draw(f, now())));
}

/// 200 columns: the parent, current and preview columns are 30, 60 and 40
/// wide, and the room left shows the two levels above the parent.
#[test]
fn shows_older_levels_on_a_wide_screen() {
    let f = fixture();
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(
        &mut b,
        &[
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Right,
            KeyCode::Right,
        ],
    );
    assert_screen!("in_c_200_wide", render_in(200, |f| b.draw(f, now())));
    // a click on `big/` in the root's column goes up to the root
    b.mouse(click(5, 1), after(0));
    assert_screen!("first_view_200_wide", render_in(200, |f| b.draw(f, now())));
}

#[test]
fn enters_two_levels() {
    let f = fixture();
    let mut b = scanned(f.dir.path(), Desktop::None);
    assert_screen!("first_view", draw(&mut b, now()));
    press(&mut b, &[KeyCode::Down, KeyCode::Right, KeyCode::Char('l')]);
    assert_screen!("in_b", draw(&mut b, now()));

    // back up to the root, with `a/` selected
    press(
        &mut b,
        &[KeyCode::Left, KeyCode::Char('h'), KeyCode::Char('h')],
    );
    assert_screen!("root_a_selected", draw(&mut b, now()));
}

#[test]
fn filters_the_current_column() {
    let f = fixture();
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Char('/'), KeyCode::Char('p')]);
    // `top` and `empty/`
    assert_screen!("filter_p", draw(&mut b, now()));

    // clearing the filter keeps a selection made while filtered
    press(&mut b, &[KeyCode::Down, KeyCode::Esc]);
    assert_screen!("root_empty_selected", draw(&mut b, now()));
}

#[test]
fn shows_a_saved_scan_with_its_age() {
    let f = fixture();
    let saved = Saved {
        tree: diskuse_core::scan(f.dir.path(), &ScanOptions::default()).unwrap(),
        reclaimable: false,
        modified: now(),
    };
    let mut b = Browser::new(
        f.dir.path(),
        "/fixture",
        env(Desktop::None),
        Some(saved),
        None,
        false,
    );
    let later = now() + Duration::from_secs(5 * 60);
    assert_screen!("saved_5_min_ago", draw(&mut b, later));
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
    let mut b = scanned(f.dir.path(), detect(true, &[ssh]));
    assert_eq!(press(&mut b, &[KeyCode::Char('r')]), []);
    assert_screen!("path_in_the_footer", draw(&mut b, now()));
}

#[test]
fn lists_the_denied_dirs_with_why() {
    let f = fixture();
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Char('d')]);
    assert_screen!("denied_two", draw(&mut b, now()));

    press(&mut b, &[KeyCode::Char('d')]);
    assert_screen!("first_view", draw(&mut b, now()));
}

/// ```text
/// root/
///   locked/    mode 000, hides a 4096 file
/// ```
#[test]
fn says_one_directory_could_not_be_read() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    file(&locked.join("hidden"), 4096);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let mut b = scanned_as(dir.path(), "/one", Desktop::None, false);
    press(&mut b, &[KeyCode::Char('d')]);
    let drawn = draw(&mut b, now());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_screen!("denied_one", drawn);
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

fn usb() -> Mount {
    mount(
        "/Volumes/USB".as_ref(),
        "exfat",
        false,
        64 * GIB,
        10 * GIB,
        54 * GIB,
    )
}

/// Hidden mounts, those with no size and memory filesystems are left out.
#[test]
fn lists_the_volumes() {
    let mounts = vec![
        usb(),
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
    assert_screen!("volumes", draw_app(&mut app));
    press_app(&mut app, &[KeyCode::Down, KeyCode::Down]);
    assert_screen!("volumes_second_selected", draw_app(&mut app));
}

#[test]
fn shows_every_key_on_question_mark() {
    let mut app = App::new(None, vec![usb()], env(Desktop::Mac), None, false);
    // other keys do nothing while it is shown
    press_app(&mut app, &[KeyCode::Char('?'), KeyCode::Char('q')]);
    assert_screen!("keys", draw_app(&mut app));
    press_app(&mut app, &[KeyCode::Char('?')]);
    assert_screen!("volume_usb", draw_app(&mut app));
    press_app(&mut app, &[KeyCode::Char('?'), KeyCode::Esc]);
    assert_screen!("volume_usb", draw_app(&mut app));

    // typed into a filter instead
    let f = fixture();
    let mut app = App::new(Some(f.dir.path()), vec![], env(Desktop::Mac), None, false);
    finish(&mut app);
    press_app(&mut app, &[KeyCode::Char('/'), KeyCode::Char('?')]);
    assert_screen!("filter_question_mark", draw_app(&mut app));
}

/// `u` switches every size between GiB (1024) and GB (1000), and back.
#[test]
fn switches_units_on_u() {
    let mut app = App::new(None, vec![usb()], env(Desktop::Mac), None, false);
    press_app(&mut app, &[KeyCode::Char('u')]);
    assert_screen!("volume_usb_in_gb", draw_app(&mut app));
    press_app(&mut app, &[KeyCode::Char('u')]);
    assert_screen!("volume_usb", draw_app(&mut app));
}

/// A click selects a volume, a double click scans it.
#[test]
fn clicks_select_and_scan_a_volume() {
    let f = fixture();
    let volume = mount(f.dir.path(), "apfs", false, 100 * GIB, 0, 50 * GIB);
    let mut app = App::new(None, vec![usb(), volume], env(Desktop::Mac), None, false);
    draw_app(&mut app);
    // `/Volumes` sorts before `/tmp`
    app.mouse(click(5, 2), after(0));
    assert_screen!("volumes_clicked", draw_app(&mut app));
    app.mouse(click(5, 2), after(200));
    finish(&mut app);
    assert_screen!("first_view_of_a_path", draw_app(&mut app));
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

#[test]
fn guides_to_full_disk_access_once_before_the_first_scan() {
    let f = fixture();
    let total = diskuse_core::scan(f.dir.path(), &ScanOptions::default()).unwrap();
    let used = total.size(FolderId::ROOT) + (1 << 20);
    let mut app = enter_volume(&f, used, FullDiskAccess::Missing);
    assert_screen!("full_disk_access_guide", draw_app(&mut app));

    let settings = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
    let runs = press_app(&mut app, &[KeyCode::Char('o')]);
    assert_eq!(runs, [("/usr/bin/open".to_string(), vec![settings.into()])]);

    // the browser of the volume: what its scan found of the bytes in use
    press_app(&mut app, &[KeyCode::Char('c')]);
    finish(&mut app);
    assert_screen!("first_view_of_a_volume", draw_app(&mut app));

    // back on the list and in again: the same browser, no guide
    press_app(&mut app, &[KeyCode::Esc]);
    assert_screen!("volume_of_the_fixture", draw_app(&mut app));
    press_app(&mut app, &[KeyCode::Enter]);
    assert_screen!("first_view_of_a_volume", draw_app(&mut app));
}

#[test]
fn browses_at_once_with_full_disk_access() {
    let f = fixture();
    let mut app = enter_volume(&f, 0, FullDiskAccess::Granted);
    finish(&mut app);
    assert_screen!("first_view_of_a_path", draw_app(&mut app));
}

/// `-r` adds the reclaimable bytes after each size.
#[cfg(target_os = "macos")]
#[test]
fn shows_reclaimable_sizes() {
    let dir = common::clones();
    let mut b = scanned_as(dir.path(), "/clones", Desktop::None, true);
    assert_screen!("reclaimable", draw(&mut b, now()));
}

/// ```text
/// root/
///   rust/Cargo.toml          4096
///   rust/target/x           16384   cache: cargo, beside Cargo.toml
///   node_modules/m          12288   cache: npm
///   target/t                 8192   no label, no Cargo.toml beside it
///   __pycache__/p            4096   cache: python
///   tagged/CACHEDIR.TAG             cache, from the tag
///   .venv/pyvenv.cfg            0   cache: venv
///   Downloads/                      downloads, the root being home
///   venv/                           no label, no pyvenv.cfg
/// ```
///
/// Each label colours its name, and the line above the keys says what it
/// means for the row at the cursor.
#[test]
fn labels_dirs_by_how_safe_deleting_them_is() {
    let dir = tempdir();
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
    fs::write(&tag, "Signature: 8a477f597d28d172789f06886806bc55\n").unwrap();
    // not the real path on macOS, where `/tmp` is a symlink
    let env = Env {
        home: Some(root.into()),
        ..env(Desktop::None)
    };
    let mut b = Browser::new(root, "/labels", env, None, None, false);
    b.scan();
    finish_scan(&mut b);
    assert_screen!("labels", render_in(WIDE, |f| b.draw(f, now())));
    press(&mut b, &[KeyCode::Down]);
    assert_screen!("labels_on_a_cache", render_in(WIDE, |f| b.draw(f, now())));
    press(&mut b, &[KeyCode::End, KeyCode::Up]);
    assert_screen!(
        "labels_on_a_known_folder",
        render_in(WIDE, |f| b.draw(f, now()))
    );
}

/// `i` shows everything about the row at the cursor in a box over the
/// columns; `i` again closes it.
#[test]
fn shows_everything_about_a_row_on_i() {
    let f = fixture();
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Char('i')]);
    assert_screen!("info_box", draw(&mut b, now()));
    press(&mut b, &[KeyCode::Char('i')]);
    assert_screen!("first_view", draw(&mut b, now()));
}

/// `NO_COLOR`: no colours, but the selection is reversed, the parent
/// column's row underlined and dirs bold.
#[test]
fn draws_without_colours_for_no_color() {
    let f = fixture();
    let env = Env {
        color: false,
        ..env(Desktop::None)
    };
    let mut b = Browser::new(f.dir.path(), "/fixture", env, None, None, false);
    b.scan();
    finish_scan(&mut b);
    press(&mut b, &[KeyCode::Down, KeyCode::Right, KeyCode::Right]);
    assert_screen!("in_b_without_colours", draw(&mut b, now()));
}

#[test]
fn says_when_a_filter_matches_nothing() {
    let f = fixture();
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(
        &mut b,
        &[KeyCode::Char('/'), KeyCode::Char('z'), KeyCode::Char('z')],
    );
    assert_screen!("filter_matches_nothing", draw(&mut b, now()));
}

#[test]
fn clicks_select_go_up_and_into_dirs() {
    let f = fixture();
    let mut b = scanned(f.dir.path(), Desktop::None);
    draw(&mut b, now());
    // `a/`, in the root's column
    b.mouse(click(5, 2), after(0));
    assert_screen!("root_a_selected", draw(&mut b, now()));
    // `b/`, in the preview of `a/`: into `a/`, `b/` selected
    b.mouse(click(65, 1), after(1000));
    assert_screen!("in_a", draw(&mut b, now()));
    // `big/`, in the parent column: up to the root, `big/` selected
    b.mouse(click(5, 1), after(2000));
    assert_screen!("first_view", draw(&mut b, now()));

    // two clicks on `a/` within 400 ms go into it
    b.mouse(click(5, 2), after(3000));
    b.mouse(click(5, 2), after(3300));
    assert_screen!("in_a", draw(&mut b, now()));
    // 500 ms apart, they only select `h1`
    b.mouse(click(40, 2), after(4000));
    b.mouse(click(40, 2), after(4500));
    assert_screen!("in_a_h1_selected", draw(&mut b, now()));

    // the wheel over the current column moves its cursor
    b.mouse(mouse(MouseEventKind::ScrollDown, 40, 9), after(5000));
    assert_screen!("in_a_h2_selected", draw(&mut b, now()));
}

/// ```text
/// root/
///   many/f01 .. f25    4096 .. 102400: more than a column has rows for
/// ```
#[test]
fn pages_and_scrolls_long_columns() {
    let dir = tempdir();
    let root = dir.path();
    fs::create_dir(root.join("many")).unwrap();
    for i in 1..=25 {
        file(&root.join(format!("many/f{i:02}")), i * 4096);
    }
    let mut b = scanned_as(root, "/many", Desktop::None, false);
    // the wheel over the preview scrolls it, by a row
    draw(&mut b, now());
    b.mouse(mouse(MouseEventKind::ScrollDown, 65, 5), now());
    assert_screen!("many_preview_scrolled", draw(&mut b, now()));

    // a page is the 17 rows shown
    press(&mut b, &[KeyCode::Right, KeyCode::PageDown]);
    assert_screen!("many_a_page_down", draw(&mut b, now()));
    press(&mut b, &[KeyCode::End]);
    assert_screen!("many_at_the_end", draw(&mut b, now()));
    press(&mut b, &[KeyCode::Char('g')]);
    assert_screen!("many_at_the_start", draw(&mut b, now()));
}

/// The fixture with one of the hard links `a/h1` and `a/h2` removed, as
/// which of the two a scan keeps among the largest depends on the order
/// the directory lists them in.
fn one_link() -> Fixture {
    let f = fixture();
    fs::remove_file(f.dir.path().join("a/h2")).unwrap();
    f
}

#[test]
fn lists_the_largest_files() {
    let f = one_link();
    let mut b = scanned(f.dir.path(), Desktop::None);
    press(&mut b, &[KeyCode::Char('t')]);
    assert_screen!("largest_files", draw(&mut b, now()));

    press(&mut b, &[KeyCode::Char('t')]);
    assert_screen!("first_view", draw(&mut b, now()));
}

#[test]
fn drops_a_largest_file_deleted_since_the_scan() {
    let f = one_link();
    let mut b = scanned(f.dir.path(), Desktop::None);
    fs::remove_file(f.dir.path().join("big/f")).unwrap();
    press(&mut b, &[KeyCode::Char('t')]);
    assert_screen!("largest_files_without_the_deleted", draw(&mut b, now()));
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

/// Polls `b` until the text of its screen is `ready`, for at most
/// `within`, and returns that screen.
fn shown_within(b: &mut Browser, within: Duration, ready: impl Fn(&str) -> bool) -> Buffer {
    let deadline = Instant::now() + within;
    loop {
        b.poll(now());
        let drawn = draw(b, now());
        let text = shot(&drawn);
        if ready(&text) {
            return drawn;
        }
        assert!(Instant::now() < deadline, "not within {within:?}:\n{text}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `root/a/` with `keep` (4096) and `gone` (8192), browsed over a saved
/// scan, so scanned again, then changed while shown. Each change shows
/// `within`. `inotify` is for [`Env::inotify`]. `at` gives the path the
/// browser is given for the dir.
fn follows_changes(inotify: bool, within: Duration, at: fn(&Path) -> PathBuf) {
    let dir = tempdir();
    let root = dir.path();
    fs::create_dir(root.join("a")).unwrap();
    file(&root.join("a/keep"), 4096);
    file(&root.join("a/gone"), 8192);
    let saved = Saved {
        tree: diskuse_core::scan(root, &ScanOptions::default()).unwrap(),
        reclaimable: false,
        modified: now(),
    };
    let env = Env {
        inotify,
        ..env(Desktop::None)
    };
    let mut b = Browser::new(&at(root), "/live", env, Some(saved), None, false);
    b.scan();
    finish_scan(&mut b);
    assert_screen!("live_scanned", draw(&mut b, now()));

    // each file's change is since it was first shown, at the scan
    file(&root.join("a/new"), 4096);
    let drawn = shown_within(&mut b, within, |text| text.contains(" new"));
    assert_screen!("live_a_file_created", drawn);
    fs::remove_file(root.join("a/gone")).unwrap();
    let drawn = shown_within(&mut b, within, |text| !text.contains(" gone"));
    assert_screen!("live_a_file_deleted", drawn);
    // the title says since when, and on Linux how old the scan is
    let later = now() + Duration::from_secs(4 * 60);
    assert_screen!("live_4_min_later", draw(&mut b, later));
}

/// FSEvents on macOS, inotify on Linux. The deadline is generous so a
/// loaded machine cannot fail it; inotify itself shows changes well inside
/// a second.
#[test]
fn follows_changes_on_disk() {
    follows_changes(true, Duration::from_secs(3), Path::to_path_buf);
}

/// Below `/System/Volumes/Data`, where FSEvents reports changes at their
/// firmlinked path, such as `/private/tmp/...` for a temp dir.
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

/// Waits until `watched` gives `want`: the scan's thread sets the watches,
/// after the key that asked for them.
#[cfg(target_os = "linux")]
fn assert_watched(root: &Path, dirs: &[&str], want: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while watched(root, dirs) != want && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(watched(root, dirs), want);
}

/// ```text
/// root/
///   a/x/f    8192
///   b/
/// ```
#[cfg(target_os = "linux")]
#[test]
fn watches_only_the_dirs_shown() {
    let dir = tempdir();
    let root = dir.path();
    fs::create_dir_all(root.join("a/x")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    file(&root.join("a/x/f"), 8192);
    let dirs = ["", "a", "a/x", "b"];
    let mut b = scanned(root, Desktop::None);
    // the root, and `a/` previewed
    assert_watched(root, &dirs, &["", "a"]);
    press(&mut b, &[KeyCode::Right]);
    assert_watched(root, &dirs, &["", "a", "a/x"]);
    press(&mut b, &[KeyCode::Left, KeyCode::Down]);
    assert_watched(root, &dirs, &["", "b"]);
    drop(b);
    assert_watched(root, &dirs, &[]);
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
    let dir = tempdir();
    let root = dir.path();
    for name in ["x0", "x1", "x2", "x3", "x4"] {
        fs::create_dir(root.join(name)).unwrap();
        file(&root.join(name).join("f"), 4096);
    }
    let mut opts = ScanOptions::default();
    opts.stop = diskuse_core::Stop::new(|| true);
    let saved = Saved {
        tree: diskuse_core::scan(root, &opts).unwrap(),
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
    assert_screen!("stopped_scan_shown_while_scanning", draw(&mut b, now()));

    finish_scan(&mut b);
    assert_screen!("stopped_scan_scanned_again", draw(&mut b, now()));
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
    let dir = tempdir();
    let root = dir.path();
    fs::create_dir_all(root.join("a/x")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    file(&root.join("a/x/f"), 4096);
    file(&root.join("b/g"), 4096);
    let mut b = scanned(root, Desktop::None);
    file(&root.join("a/x/new"), 8192);

    press(&mut b, &[KeyCode::Char('s')]);
    assert_screen!("rescanning_a_dir", draw(&mut b, now()));
    finish_scan(&mut b);
    assert_screen!("rescanned", draw(&mut b, now()));

    press(&mut b, &[KeyCode::Char('S')]);
    assert_screen!("rescanning_everything", draw(&mut b, now()));
    finish_scan(&mut b);
    assert_screen!("rescanned", draw(&mut b, now()));
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
    let dir = tempdir();
    let root = dir.path();
    fs::create_dir(root.join("a")).unwrap();
    file(&root.join("a/f"), 8192);
    file(&root.join("g"), 4096);
    let cache = tempdir();
    let browse = || {
        let env = Env {
            cache: Some(CacheDir::at(cache.path().into())),
            ..env(Desktop::None)
        };
        let mut b = Browser::new(root, "/picks", env, None, None, false);
        b.scan();
        finish_scan(&mut b);
        b
    };
    let mut b = browse();
    press(
        &mut b,
        &[KeyCode::Char(' '), KeyCode::Down, KeyCode::Char(' ')],
    );
    assert_screen!("picks_marked", draw(&mut b, now()));
    press(&mut b, &[KeyCode::Char('p')]);
    assert_screen!("picks_listed", draw(&mut b, now()));
    drop(b);

    fs::remove_file(root.join("g")).unwrap();
    let mut b = browse();
    press(&mut b, &[KeyCode::Char('p'), KeyCode::Down]);
    assert_screen!("picks_with_one_gone", draw(&mut b, now()));
    press(&mut b, &[KeyCode::Char(' ')]);
    assert_screen!("picks_after_unpicking", draw(&mut b, now()));
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
    let dir = tempdir();
    let root = dir.path();
    fs::create_dir(root.join("big")).unwrap();
    fs::create_dir(root.join("small")).unwrap();
    file(&root.join("big/f"), 16384);
    file(&root.join("small/f"), 4096);
    let mut b = scanned(root, Desktop::None);
    file(&root.join("small/new"), 8192);
    press(&mut b, &[KeyCode::Down, KeyCode::Char('s')]);
    finish_scan(&mut b);
    // the cursor stays on `small/`
    assert_screen!("sorted_by_size", draw(&mut b, now()));
    press(&mut b, &[KeyCode::Char('c')]);
    assert_screen!("sorted_by_change", draw(&mut b, now()));
    press(&mut b, &[KeyCode::Char('c')]);
    assert_screen!("sorted_by_size", draw(&mut b, now()));
}

/// Below the home dir, the title shows the root from `~`.
#[test]
fn titles_a_path_below_home_with_a_tilde() {
    let f = fixture();
    let path = f.dir.path();
    let env = Env {
        home: Some(path.parent().unwrap().into()),
        ..env(Desktop::Mac)
    };
    let mut app = App::new(Some(path), vec![], env, None, false);
    finish(&mut app);
    assert_screen!("first_view_below_home", draw_app(&mut app));
}

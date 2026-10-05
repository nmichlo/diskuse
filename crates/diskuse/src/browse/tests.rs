//! The title and the screen, against real scans.

#![allow(clippy::disallowed_methods)] // the tests make a tree of files

use super::text::fit_keys;
use super::view::HELP;
use super::*;
use crate::style::Styles;
use diskuse_core::{Reason, ScanOptions};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::Style;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use std::os::unix::fs::MetadataExt;

fn env() -> Env {
    Env {
        desktop: Desktop::None,
        terminal: "Terminal".into(),
        cache: None,
        inotify: true,
        interval: Duration::ZERO,
        units: Units::Binary,
        wake: None,
        color: true,
        home: None,
    }
}

/// 100x20: `title`, empty columns and the keys.
fn screen(title: &str) -> Buffer {
    let mut lines = vec![format!("{title:<100}")];
    lines.extend((0..18).map(|_| format!("{:100}", "")));
    lines.push(String::new());
    let mut buf = Buffer::with_lines(lines);
    buf.set_line(0, 19, &Styles::new(true).keys(&fit_keys(HELP, 100)), 100);
    buf
}

/// A dir's row of `size` bytes, in the colour of its size: with its
/// share of `of` in the current column, without in the preview.
fn dir_row(size: u64, of: Option<u64>, name: &str) -> Line<'static> {
    let styles = Styles::new(true);
    let style = styles.size(size, Units::Binary);
    let mut spans = vec![
        Span::styled(format!("{:>10}", Units::Binary.format(size)), style),
        Span::styled(" ", styles.dim),
        Span::raw(" "),
    ];
    if let Some(of) = of {
        spans.extend([Span::styled(percent(size, of), style), Span::raw(" ")]);
    }
    spans.push(Span::styled(format!("{name}/"), styles.dir));
    Line::from(spans)
}

fn draw(b: &mut Browser) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal.draw(|f| b.draw(f, SystemTime::now())).unwrap();
    terminal.backend().buffer().clone()
}

/// The top line of a 100-column screen, without the spaces after it.
fn title_of(b: &mut Browser) -> String {
    let drawn = draw(b);
    let line: String = (0..100).map(|x| drawn[(x, 0)].symbol()).collect();
    line.trim_end().into()
}

/// ```text
/// root/
///   a/f      4096
///   a/x/g    8192
/// ```
///
/// shown as a snapshot of a running scan: the title counts, with no
/// total to head for.
#[test]
fn a_running_scan_shows_what_it_has_so_far() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("a/x")).unwrap();
    std::fs::write(root.join("a/f"), vec![0; 4096]).unwrap();
    std::fs::write(root.join("a/x/g"), vec![0; 8192]).unwrap();
    // what a dir itself takes: 0 B on APFS, 4 KiB on ext4
    let d = std::fs::metadata(root).unwrap().blocks() * 512;
    let (x, a) = (8192 + d, 12288 + 2 * d);
    let tree = diskuse_core::scan(root, &ScanOptions::default()).unwrap();
    let mut b = Browser::new(root, "/scan", env(), None, None, false);
    assert_eq!(draw(&mut b), screen("/scan"));

    b.show(tree, Status::Scanning);
    // at the root, the current column is at x 0, the preview at 60
    let mut expected = screen("");
    let dim = Style::new().fg(Color::DarkGray);
    let size = |n: u64| {
        Span::styled(
            Units::Binary.format(n),
            Styles::new(true).size(n, Units::Binary),
        )
    };
    let title = Line::from_iter([
        Span::raw(format!("/scan  scanning {}", Units::Binary.format(a + d))),
        Span::styled(" | ", dim),
        Span::raw("3 folders"),
        Span::styled(" | ", dim),
        Span::raw("0 s"),
    ]);
    expected.set_line(0, 0, &title, 100);
    expected.set_line(0, 1, &dir_row(a, Some(a + d), "a"), 59);
    let selected = Style::new().fg(Color::Indexed(231)).bg(Color::Indexed(25));
    expected.set_style(
        Rect::new(0, 1, 59, 1),
        selected.add_modifier(Modifier::BOLD),
    );
    expected.set_line(60, 1, &dir_row(x, None, "x"), 40);
    // the smaller of the two unless a dir takes 4 KiB itself
    let f = Line::from_iter([
        Span::styled(format!("{:>10}", "4.0 KiB"), Style::new().fg(Color::Green)),
        Span::styled(" ", dim),
        Span::raw(" f"),
    ]);
    expected.set_line(60, 2, &f, 40);
    let status = Line::from_iter([
        Span::styled("a/", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(" | ", dim),
        size(a),
        Span::styled("  own ", dim),
        Span::raw(Units::Binary.format(4096 + d)),
    ]);
    expected.set_line(0, 18, &status, 100);
    assert_eq!(draw(&mut b), expected);
}

/// A volume reports fewer bytes in use than a scan finds, as it counts
/// a cloned file once: then they are no target, and no total.
#[test]
fn a_scan_past_the_used_bytes_drops_them_as_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f"), vec![0; 8192]).unwrap();
    let tree = diskuse_core::scan(root, &ScanOptions::default()).unwrap();
    let total = tree.size(FolderId::ROOT);
    let fmt = |n| Units::Binary.format(n);

    let mut b = Browser::new(root, "/vol", env(), None, Some(total * 4), false);
    b.show(tree.clone(), Status::Scanning);
    let title = format!("/vol  scanning {} of ~{} (25%)", fmt(total), fmt(total * 4));
    assert_eq!(title_of(&mut b), title);

    let mut b = Browser::new(root, "/vol", env(), None, Some(total / 2), false);
    b.show(tree.clone(), Status::Scanning);
    let title = format!("/vol  scanning {} | 1 folders", fmt(total));
    assert_eq!(title_of(&mut b), title);
    b.show(tree, Status::Done);
    let title = format!(
        "/vol  {} | disk reports {} in use",
        fmt(total),
        fmt(total / 2)
    );
    assert_eq!(title_of(&mut b), title);
}

/// The OS says it missed changes: nothing is scanned again, the title
/// says so until `S` scans everything, and the dir shown is listed
/// again, so a file made since shows.
#[test]
fn missed_changes_mark_the_tree_stale_until_a_full_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f"), vec![0; 4096]).unwrap();
    let env = Env {
        interval: Duration::from_millis(5),
        ..env()
    };
    let mut b = Browser::new(root, "/s", env, None, None, false);
    let now = SystemTime::now();
    let finish = |b: &mut Browser| {
        while b.poll(now) {
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    b.scan();
    finish(&mut b);
    let fmt = |n| Units::Binary.format(n);
    let total = b.view.as_ref().unwrap().tree.size(FolderId::ROOT);
    // where only the dirs shown are followed (Linux), the scan's age
    let scanned = match cfg!(target_os = "macos") {
        true => "",
        false => " | scanned 0 s ago",
    };
    assert_eq!(title_of(&mut b), format!("/s  {}{scanned}", fmt(total)));

    // made while the OS was not telling
    std::fs::write(root.join("g"), vec![0; 8192]).unwrap();
    b.on_event(Ok(Event::Missed(Reason::Dropped, root.into())), now);
    assert!(!b.busy(), "nothing is scanned again by itself");
    let stale = "changes missed 0 s ago (S rescans)";
    // the root, shown, is listed again (and followed as before)
    let listed = |b: &Browser| b.view.as_ref().unwrap().tree.size(FolderId::ROOT) == total + 8192;
    while !listed(&b) {
        b.poll(now);
        std::thread::sleep(Duration::from_millis(1));
    }
    let grown = format!(
        "/s  {} | +8.0 KiB since opened 0 s ago{scanned}",
        fmt(total + 8192)
    );
    assert_eq!(
        title_of(&mut b),
        grown.replacen(" | ", &format!(" | {stale} | "), 1)
    );

    assert!(
        b.key(KeyEvent::from(KeyCode::Char('S')), &mut |_, _| Ok(()))
            .is_continue()
    );
    finish(&mut b);
    assert_eq!(title_of(&mut b), grown);
}

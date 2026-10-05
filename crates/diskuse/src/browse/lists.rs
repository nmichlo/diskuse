//! The screens that are one list: the dirs not read, the largest files,
//! the picks.

use super::{Status, View, join};
use crate::guide::reason;
use crate::style::Styles;
use diskuse_core::{FolderId, ReadTree, Units};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The cursor and first row drawn of the list of picks.
#[derive(Default)]
pub(super) struct Picks {
    pub(super) cursor: usize,
    pub(super) offset: usize,
}

/// The largest files list: rows of [`View::largest`] that are still there.
#[derive(Default)]
pub(super) struct Top {
    /// The selected index among the rows still there.
    pub(super) cursor: usize,
    /// The first of those rows drawn.
    pub(super) offset: usize,
    /// Whether each file checked is still there, by path. Each is checked
    /// once while the list is shown, when first drawn.
    there: HashMap<Vec<u8>, bool>,
}

impl Top {
    /// The paths of `view`'s largest files not found gone.
    pub(super) fn paths<'a>(&'a self, view: &'a View) -> impl Iterator<Item = &'a [u8]> {
        let paths = view.largest.iter().map(|(_, path)| &path[..]);
        paths.filter(|p| self.there.get(*p) != Some(&false))
    }
}

/// Draws the denied dirs of `view` from row `offset`, each with why it
/// could not be read. Returns `offset`, clamped so the last row is drawn
/// as low as it can be.
pub(super) fn panel(
    buf: &mut Buffer,
    area: Rect,
    view: &View,
    terminal: &str,
    offset: usize,
    styles: &Styles,
) -> usize {
    let head = match view.denied.len() {
        0 => "every directory could be read".into(),
        1 => "1 directory could not be read, so its contents are not counted:".into(),
        n => format!("{n} directories could not be read, so their contents are not counted:"),
    };
    buf.set_stringn(area.x, area.y, head, area.width.into(), Style::new());
    let height = usize::from(area.height.saturating_sub(1));
    let offset = offset.min(view.denied.len().saturating_sub(height));
    let lines = (area.y + 1..area.bottom()).zip(&view.denied[offset..]);
    for (y, (path, id)) in lines {
        let why = reason(&view.tree, *id, terminal);
        let line = Line::from_iter([
            Span::raw(format!("{}  ", String::from_utf8_lossy(path))),
            Span::styled(why, styles.denied),
        ]);
        buf.set_line(area.x, y, &line, area.width);
    }
    offset
}

/// Draws the largest files of `view` still there, as paths below the root,
/// from `top`'s offset, the one at its cursor selected. A file is checked
/// with one `lstat` when first drawn, and left out if gone.
pub(super) fn top_files(
    buf: &mut Buffer,
    area: Rect,
    view: &View,
    top: &mut Top,
    styles: &Styles,
    units: Units,
) {
    if view.largest.is_empty() && view.status == Status::Scanning {
        buf.set_stringn(
            area.x,
            area.y,
            "scanning...",
            area.width.into(),
            Style::new(),
        );
        return;
    }
    let height = usize::from(area.height);
    top.offset = scroll(top.offset, top.cursor, height);
    // every row down to the last drawn is checked
    let mut shown = Vec::new();
    for (size, path) in &view.largest {
        if shown.len() == top.offset + height {
            break;
        }
        let there = match top.there.get(path) {
            Some(&there) => there,
            None => {
                let there = diskuse_core::exists(Path::new(OsStr::from_bytes(path)));
                top.there.insert(path.clone(), there);
                there
            }
        };
        if there {
            shown.push((*size, &path[..]));
        }
    }
    // the list may have ended above the cursor
    top.cursor = top.cursor.min(shown.len().saturating_sub(1));
    top.offset = scroll(top.offset, top.cursor, height);
    let root = view.tree.name(FolderId::ROOT).len();
    let lines = (area.y..area.bottom()).zip(shown.iter().enumerate().skip(top.offset));
    for (y, (i, &(size, path))) in lines {
        let below = path[root..].strip_prefix(b"/").unwrap_or(&path[root..]);
        let line = Line::from_iter([
            Span::styled(
                format!("{:>10}", units.format(size)),
                styles.size(size, units),
            ),
            Span::raw(format!("  {}", String::from_utf8_lossy(below))),
        ]);
        buf.set_line(area.x, y, &line, area.width);
        if i == top.cursor {
            buf.set_style(
                Rect {
                    y,
                    height: 1,
                    ..area
                },
                styles.selected,
            );
        }
    }
}

/// The first row to draw, moved from `offset` as little as possible so
/// that row `at` is among the `height` drawn.
pub(super) fn scroll(offset: usize, at: usize, height: usize) -> usize {
    offset.min(at).max((at + 1).saturating_sub(height))
}

/// Draws the picks, below the root `root`, from `list`'s offset, the one
/// at its cursor selected, after a head with their count and total. A
/// dir's size is from the tree, a file's from one `lstat`, and either is
/// `gone` if not found.
#[allow(clippy::too_many_arguments)]
pub(super) fn picks(
    buf: &mut Buffer,
    area: Rect,
    view: &View,
    root: &[u8],
    picks: &[Vec<u8>],
    list: &mut Picks,
    styles: &Styles,
    units: Units,
) {
    let size = |path: &[u8]| match view.tree.find(Path::new(OsStr::from_bytes(path))) {
        Some(d) => Some(view.tree.size(d)),
        None => {
            let mut full = root.to_vec();
            join(&mut full, path);
            diskuse_core::allocated(Path::new(OsStr::from_bytes(&full)))
        }
    };
    let sizes: Vec<Option<u64>> = picks.iter().map(|p| size(p)).collect();
    let total: u64 = sizes.iter().flatten().sum();
    let head = match picks.len() {
        0 => "nothing picked: space picks the item at the cursor".into(),
        n => format!("{n} picked, {} in all:", units.format(total)),
    };
    buf.set_stringn(area.x, area.y, head, area.width.into(), Style::new());
    let height = usize::from(area.height.saturating_sub(1));
    list.offset = scroll(list.offset, list.cursor, height);
    let rows = picks.iter().zip(&sizes).enumerate().skip(list.offset);
    for (y, (i, (path, size))) in (area.y + 1..area.bottom()).zip(rows) {
        let size = match size {
            Some(n) => Span::styled(format!("{:>10}", units.format(*n)), styles.size(*n, units)),
            None => Span::styled(format!("{:>10}", "gone"), styles.dim),
        };
        let line = Line::from_iter([
            size,
            Span::raw(format!("  {}", String::from_utf8_lossy(path))),
        ]);
        buf.set_line(area.x, y, &line, area.width);
        if i == list.cursor {
            let row = Rect {
                y,
                height: 1,
                ..area
            };
            buf.set_style(row, styles.selected);
        }
    }
}

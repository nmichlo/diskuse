//! The columns of dirs: which are drawn where, what each lists, and its rows.

use super::lists::scroll;
use super::text::{cut_middle, percent, signed};
use super::{Browser, Files, Item, Row, View, below, join_below, name};
use crate::style::Styles;
use diskuse_core::{FolderId, ReadTree, Units};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// The width of the column of a dir above the parent, where there is room.
const OLDER: usize = 30;

/// A column of the browser.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Col {
    /// A dir above the current one, by how many levels: 1 is its parent.
    Above(usize),
    Current,
    Preview,
}

/// How a column draws its rows.
#[derive(Clone, Copy)]
struct Look {
    /// A parent or preview column: sizes and names only.
    side: bool,
    /// The change since the session started, when some row of the column
    /// changed.
    changes: bool,
    /// Each row's share of its dir, on a screen 50 columns wide or more.
    percent: bool,
    units: Units,
}

/// The name of `row` as a column shows it, `/` after a dir, and its style:
/// a dir's is bold, in its label's colour if it has one.
pub(super) fn row_name(row: &Row, name: &[u8], styles: &Styles) -> (Style, String) {
    let name = String::from_utf8_lossy(name).into_owned();
    match row.item {
        Item::File(_) => (Style::new(), name),
        Item::Dir(_) => {
            let style = match row.label {
                Some(l) => styles.dir.patch(styles.label(l.tier)),
                None => styles.dir,
            };
            (style, name + "/")
        }
    }
}

/// Row `row` of a dir of `total` bytes listed as `files`, in `width`
/// columns: its sizes, unless a side column its change and share of
/// `total`, then its name, cut in the middle to fit.
#[allow(clippy::too_many_arguments)]
fn line(
    view: &View,
    files: &Files,
    row: &Row,
    total: u64,
    look: Look,
    picked: bool,
    width: usize,
    styles: &Styles,
) -> Line<'static> {
    let units = look.units;
    // something below was denied, so the sizes are lower bounds
    let plus = match row.item {
        Item::Dir(k) if view.tree.partial(k) => "+",
        _ => " ",
    };
    let size = |n: u64| Span::styled(format!("{:>10}", units.format(n)), styles.size(n, units));
    let mut spans = vec![size(row.size), Span::styled(plus, styles.dim)];
    if view.reclaimable && !look.side {
        spans.extend([size(row.private), Span::styled(plus, styles.dim)]);
    }
    if look.changes {
        spans.push(match row.delta {
            0 => Span::raw(format!("{:11}", "")),
            d @ 1.. => Span::styled(format!("{:>11}", signed(d, units)), styles.grew),
            d => Span::styled(format!("{:>11}", signed(d, units)), styles.shrank),
        });
    }
    spans.push(Span::raw(" "));
    if look.percent {
        spans.push(Span::styled(
            percent(row.size, total),
            styles.size(row.size, units),
        ));
        spans.push(Span::raw(" "));
    }
    let (style, name) = row_name(row, name(view, files, row.item), styles);
    let mut after = Vec::new();
    if let Item::Dir(k) = row.item {
        match view.tree.error(k) {
            Some(error) => after.push(Span::styled(format!(" (denied: {error})"), styles.denied)),
            None if view.tree.other_device(k) => {
                after.push(Span::styled(" (other device)", styles.dim));
            }
            None => {}
        }
    }
    if picked {
        after.push(Span::styled(" *", styles.picked));
    }
    let used: usize = spans.iter().chain(&after).map(Span::width).sum();
    spans.extend(cut_middle(&name, width.saturating_sub(used), style, styles));
    spans.extend(after);
    Line::from(spans)
}

impl Browser {
    /// The columns drawn, left to right, and where. With a dir above the
    /// current one and 100 columns or more: its parent, the current dir
    /// and the preview, 20 / 50 / 30 of the width but at most 30, 60 and
    /// 40 wide; width left over shows older levels, 30 each, nearest
    /// first. At the root or under 100 columns: current and preview, 60 /
    /// 40, at most 60 and 40 wide. Under 60: the current one alone.
    pub(super) fn columns(&self) -> Vec<(Col, Rect)> {
        let w = usize::from(self.body.width);
        let above = self.dirs.len() - 1;
        let share = |of: usize, tenths: usize| (of * tenths + 5) / 10;
        let mut widths = match w {
            ..60 => vec![(Col::Current, w)],
            _ if w < 100 || above == 0 => {
                let current = share(w - 1, 6).min(60);
                vec![
                    (Col::Current, current),
                    (Col::Preview, (w - 1 - current).min(40)),
                ]
            }
            _ => {
                let parent = share(w - 2, 2).min(30);
                let current = share(w - 2, 5).min(60);
                let preview = (w - 2 - parent - current).min(40);
                vec![
                    (Col::Above(1), parent),
                    (Col::Current, current),
                    (Col::Preview, preview),
                ]
            }
        };
        // older levels where three capped columns leave room
        let mut spare = w - (widths.iter().map(|&(_, w)| w + 1).sum::<usize>() - 1);
        for level in 2..=above {
            if widths.len() < 3 || spare < OLDER + 1 {
                break;
            }
            widths.insert(0, (Col::Above(level), OLDER));
            spare -= OLDER + 1;
        }
        let mut x = self.body.x;
        (widths.into_iter())
            .map(|(col, width)| {
                let area = Rect {
                    x,
                    width: width as u16,
                    ..self.body
                };
                x += width as u16 + 1;
                (col, area)
            })
            .collect()
    }

    /// Draws every column, and under a filter nothing matches, that it
    /// does not.
    pub(super) fn draw_columns(&self, buf: &mut Buffer, styles: &Styles) {
        let height = self.body.height.into();
        let columns = self.columns();
        for &(col, area) in &columns {
            let style = match col {
                Col::Above(_) => Some(styles.parent),
                Col::Current => Some(styles.selected),
                Col::Preview => None,
            };
            if let Some((d, rows, offset, mark)) = self.listing(col, height) {
                let side = col != Col::Current;
                self.column(buf, area, d, rows, offset, mark.zip(style), side);
            }
        }
        if self.current.is_empty() && !self.filter.is_empty() {
            let (_, area) = columns.iter().find(|(c, _)| *c == Col::Current).unwrap();
            let text = format!("no matches for '{}'", self.filter);
            buf.set_stringn(area.x, area.y, text, area.width.into(), styles.dim);
        }
    }

    /// What column `col` lists, in a column `height` rows high: the dir,
    /// its rows, the first of them drawn, and the marked one, if any.
    /// Nothing for the preview of a file.
    ///
    /// The columns of dirs above and the preview show sizes and names
    /// only; the current one adds each row's share and change.
    pub(super) fn listing(
        &self,
        col: Col,
        height: usize,
    ) -> Option<(FolderId, &[Row], usize, Option<usize>)> {
        let d = self.dir();
        match col {
            Col::Above(level) => {
                let at = self.dirs.len() - 1 - level;
                let (dir, child) = (self.dirs[at], self.dirs[at + 1]);
                let rows = &self.rows[&dir];
                let marked = rows
                    .iter()
                    .position(|r| r.item == Item::Dir(child))
                    .unwrap();
                let offset = match self.parent_offset.filter(|_| level == 1) {
                    Some(offset) => offset.min(rows.len().saturating_sub(height)),
                    None => scroll(0, marked, height),
                };
                Some((dir, rows, offset, Some(marked)))
            }
            Col::Current => Some((d, &self.current, self.offset, Some(self.cursor))),
            Col::Preview => {
                let Some(&Row {
                    item: Item::Dir(k), ..
                }) = self.current.get(self.cursor)
                else {
                    return None;
                };
                let rows = &self.rows[&k];
                let offset = self.preview_offset.min(rows.len().saturating_sub(height));
                Some((k, rows, offset, None))
            }
        }
    }

    /// Draws `rows[offset..]` of dir `d` that fit in `area`, with `mark`, a
    /// row index and its style, if given. A `side` column shows sizes and
    /// names only.
    #[allow(clippy::too_many_arguments)]
    fn column(
        &self,
        buf: &mut Buffer,
        area: Rect,
        d: FolderId,
        rows: &[Row],
        offset: usize,
        mark: Option<(usize, Style)>,
        side: bool,
    ) {
        let view = self.view.as_ref().unwrap();
        let styles = Styles::new(self.env.color);
        let total = view.tree.size(d);
        let dir = below(&view.tree, d);
        let look = Look {
            side,
            changes: !side && rows.iter().any(|r| r.delta != 0),
            percent: !side && self.body.width >= 50,
            units: self.env.units,
        };
        let lines = (area.y..area.bottom()).zip(rows.iter().enumerate().skip(offset));
        for (y, (i, row)) in lines {
            let files = &self.files[&d];
            let picked = !self.picks.is_empty() && {
                let mut path = dir.clone();
                join_below(&mut path, name(view, files, row.item));
                self.picks.contains(&path)
            };
            let width = area.width.into();
            let line = line(view, files, row, total, look, picked, width, styles);
            buf.set_line(area.x, y, &line, area.width);
            if let Some((_, style)) = mark.filter(|&(at, _)| at == i) {
                buf.set_style(
                    Rect {
                        y,
                        height: 1,
                        ..area
                    },
                    style,
                );
            }
        }
    }
}

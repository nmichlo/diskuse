//! Everything about the row at the cursor: the line above the keys, and
//! the `i` pop-up.

use super::columns::row_name;
use super::text::{age, cut_middle, join_parts, signed, thousands, width_of};
use super::{Browser, Item, dir_path, join};
use crate::style::Styles;
use diskuse_core::{FolderId, ReadTree};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::os::unix::ffi::OsStrExt;
use std::time::SystemTime;

impl Browser {
    /// The line above the footer, about the row at the cursor: its name,
    /// sizes, what could not be read and its label, each after a dim `|`.
    /// Under 80 columns in fewer words; parts that still do not fit are
    /// left out from the end, down to the name and size.
    pub(super) fn status(&self, styles: &Styles, width: usize) -> Option<Line<'static>> {
        let shown = self.panel.is_none() && self.top.is_none() && self.picking.is_none();
        let (row, name) = self.at_cursor().filter(|_| shown)?;
        let view = self.view.as_ref()?;
        let units = self.env.units;
        let narrow = width < 80;
        let (name_style, name) = row_name(&row, name, styles);
        let size = Span::styled(units.format(row.size), styles.size(row.size, units));
        let mut sizes = vec![size.clone()];
        let mut warn = None;
        if let Item::Dir(k) = row.item {
            sizes.push(Span::styled("  own ", styles.dim));
            sizes.push(Span::raw(units.format(view.tree.own(k))));
            let below = self.denied_below(k);
            warn = if let Some(error) = view.tree.error(k) {
                Some(format!("unreadable: {error}"))
            } else if view.tree.other_device(k) {
                Some(match narrow {
                    true => "other device".into(),
                    false => "on another device, not scanned".into(),
                })
            } else if below > 0 {
                let n = thousands(below as u64);
                Some(match (narrow, below) {
                    (true, _) => format!("{n} unreadable"),
                    (false, 1) => "1 folder below unreadable".into(),
                    (false, _) => format!("{n} folders below unreadable"),
                })
            } else {
                None
            };
        }
        if view.reclaimable {
            sizes.push(Span::styled("  deletes ", styles.dim));
            sizes.push(Span::raw(units.format(row.private)));
        }
        let label = row.label.map(|l| {
            let text = match narrow {
                true => l.text.to_string(),
                false => format!("{}: {}", l.text, l.short()),
            };
            vec![Span::styled(text, styles.label(l.tier))]
        });
        // least needed last
        let mut parts: Vec<Vec<Span>> = vec![vec![Span::styled(name.clone(), name_style)], sizes];
        parts.extend(warn.map(|w| vec![Span::styled(w, styles.warn)]));
        parts.extend(label);
        let fits = |parts: &[Vec<Span>]| {
            let text: usize = parts.iter().flatten().map(Span::width).sum();
            text + 3 * (parts.len() - 1) <= width
        };
        while !fits(&parts) && parts.len() > 2 {
            parts.pop();
        }
        if !fits(&parts) {
            // without `own`, then the name cut to what is left
            parts[1] = vec![size];
            let room = width.saturating_sub(parts[1][0].width() + 3);
            parts[0] = cut_middle(&name, room, name_style, styles);
        }
        Some(join_parts(parts, styles))
    }

    /// How many denied dirs are below dir `d`, not counting itself.
    fn denied_below(&self, d: FolderId) -> usize {
        let view = self.view.as_ref().unwrap();
        let mut path = dir_path(&view.tree, d);
        path.push(b'/');
        let start = view.denied.partition_point(|(p, _)| p[..] < path[..]);
        (view.denied[start..].iter())
            .take_while(|(p, _)| p.starts_with(&path))
            .count()
    }

    /// The `i` pop-up: everything about the row at the cursor, in a box in
    /// the middle of the columns.
    pub(super) fn draw_info(&self, buf: &mut Buffer, now: SystemTime, styles: &Styles) {
        let (Some((row, name)), Some(view)) = (self.at_cursor(), &self.view) else {
            return;
        };
        let units = self.env.units;
        let mut path = self.root.as_os_str().as_bytes().to_vec();
        for name in &self.trail {
            join(&mut path, name);
        }
        join(&mut path, name);
        let mut facts = vec![("path", String::from_utf8_lossy(&path).into_owned())];
        let mut size = units.format(row.size);
        if let Item::Dir(k) = row.item {
            size += &format!(" (own {})", units.format(view.tree.own(k)));
        }
        facts.push(("size", size));
        if view.reclaimable {
            facts.push(("deletes", units.format(row.private)));
        }
        if let Item::Dir(k) = row.item {
            let mut below = 0u64;
            let mut stack: Vec<FolderId> = view.tree.children(k).collect();
            while let Some(c) = stack.pop() {
                below += 1;
                stack.extend(view.tree.children(c));
            }
            facts.push(("folders", format!("{} below", thousands(below))));
            if let Some(error) = view.tree.error(k) {
                facts.push(("unreadable", error));
            }
            let n = self.denied_below(k);
            if n > 0 {
                facts.push((
                    "partial",
                    format!("{} folders below unreadable", thousands(n as u64)),
                ));
            }
        }
        if let Some(l) = row.label {
            facts.push(("label", format!("{}: {}", l.text, l.why)));
        }
        if let Some(base) = &self.baseline {
            let since = age(now.duration_since(base.at).unwrap_or_default());
            let change = match row.delta {
                0 => "none".to_string(),
                d => signed(d, units),
            };
            facts.push(("change", format!("{change} since opened {since} ago")));
        }
        let area = self.body;
        let width = (area.width.saturating_sub(4)).min(72);
        let height = (facts.len() as u16 + 2).min(area.height);
        let r = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        let inner = usize::from(width.saturating_sub(4));
        let edge = |left: &str, text: &str| {
            let text = format!(" {text} ");
            let fill = usize::from(width).saturating_sub(2 + width_of(&text));
            format!(
                "{left}{}{text}{}{left}",
                "-".repeat(fill / 2),
                "-".repeat(fill - fill / 2)
            )
        };
        let name = String::from_utf8_lossy(name);
        buf.set_style(r, Style::reset());
        buf.set_stringn(r.x, r.y, edge("+", &name), width.into(), Style::new());
        for (y, (key, value)) in (r.y + 1..r.bottom() - 1).zip(&facts) {
            let mut spans = vec![
                Span::raw("| "),
                Span::styled(format!("{key:<11}"), styles.dim),
            ];
            spans.extend(cut_middle(
                value,
                inner.saturating_sub(11),
                Style::new(),
                styles,
            ));
            let line = Line::from(spans);
            buf.set_stringn(r.x, y, " ".repeat(width.into()), width.into(), Style::new());
            buf.set_line(r.x, y, &line, width - 1);
            buf.set_stringn(r.right() - 1, y, "|", 1, Style::new());
        }
        let bottom = r.bottom() - 1;
        buf.set_stringn(
            r.x,
            bottom,
            edge("+", "i or esc close"),
            width.into(),
            Style::new(),
        );
    }
}

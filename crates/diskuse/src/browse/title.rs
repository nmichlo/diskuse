//! The title line: the root, its size or the scan's progress, and what
//! the tree shown is.

use super::text::{age, join_parts, signed, thousands, width_of};
use super::{Browser, Status, View};
use crate::style::Styles;
use diskuse_core::{FolderId, ReadTree};
use ratatui::text::{Line, Span};
use std::time::SystemTime;

impl Browser {
    /// The top line: the root, its total, and what to know about it, each
    /// part after a dim `|`, the parts with nothing to say left out, and
    /// in `width` columns the last ones that do not fit.
    pub(super) fn title(&self, now: SystemTime, styles: &Styles, width: usize) -> Line<'static> {
        let units = self.env.units;
        let fmt = |n| units.format(n);
        let mut parts: Vec<Span> = Vec::new();
        let status = self.view.as_ref().map(|v| v.status);
        // a snapshot, or a full scan runs while no finished tree is shown
        if self.scanning || status == Some(Status::Scanning) {
            let (bytes, folders) = match (&self.view, self.progress) {
                (_, Some(progress)) => progress,
                (Some(v), None) if v.status == Status::Scanning => {
                    (v.tree.size(FolderId::ROOT), v.tree.len())
                }
                _ => (0, 0),
            };
            // a whole volume's used bytes, else the last scan's total
            let saved = self.view.as_ref().filter(|v| v.status != Status::Scanning);
            let goal = self.used.or(saved.map(|v| v.tree.size(FolderId::ROOT)));
            match goal.filter(|&g| g > 0) {
                // a scan counts each copy of a cloned file, the used bytes
                // count it once: past them, they are no target
                Some(goal) if bytes > goal => {
                    parts.extend([
                        Span::raw(format!("scanning {}", fmt(bytes))),
                        Span::raw(format!("{} folders", thousands(folders as u64))),
                    ]);
                }
                Some(goal) => {
                    // the used bytes count more than a scan finds, so it
                    // stops short of 100%
                    let pct = (u128::from(bytes) * 100 / u128::from(goal.max(1))).min(99);
                    parts.push(Span::raw(format!(
                        "scanning {} of ~{} ({pct}%)",
                        fmt(bytes),
                        fmt(goal)
                    )));
                }
                None => {
                    let since = self.started.and_then(|t| now.duration_since(t).ok());
                    parts.extend([
                        Span::raw(format!("scanning {}", fmt(bytes))),
                        Span::raw(format!("{} folders", thousands(folders as u64))),
                        Span::raw(age(since.unwrap_or_default())),
                    ]);
                }
            }
        } else if let Some(view) = &self.view {
            let total = view.tree.size(FolderId::ROOT);
            match self.used.filter(|_| view.status == Status::Done) {
                Some(used) if total <= used => parts.push(Span::styled(
                    format!("{} of {}", fmt(total), fmt(used)),
                    styles.dir,
                )),
                Some(used) => parts.extend([
                    Span::styled(fmt(total), styles.dir),
                    Span::raw(format!("disk reports {} in use", fmt(used))),
                ]),
                None => parts.push(Span::styled(fmt(total), styles.dir)),
            }
        }
        if let Some(View {
            status: Status::Saved { at },
            tree,
            ..
        }) = &self.view
        {
            let age = age(now.duration_since(*at).unwrap_or_default());
            let incomplete = if tree.stopped() { ", incomplete" } else { "" };
            parts.push(Span::raw(format!(
                "showing the scan saved {age} ago{incomplete}"
            )));
        }
        // first after the total: what all the sizes are worth
        if let Some(stale) = &self.stale {
            let age = age(now.duration_since(stale.since).unwrap_or_default());
            parts.push(Span::styled(
                match stale.moved {
                    true => format!("the scanned folder moved {age} ago"),
                    false => format!("changes missed {age} ago (S rescans)"),
                },
                styles.warn,
            ));
        }
        if let Some(view) = &self.view {
            // how much the root grew or shrank since the session started
            if let Some(base) = self
                .baseline
                .as_ref()
                .filter(|_| view.status == Status::Done)
            {
                let d = view.tree.size(FolderId::ROOT) as i64 - base.size(FolderId::ROOT) as i64;
                if d != 0 {
                    let age = age(now.duration_since(base.at).unwrap_or_default());
                    parts.push(Span::raw(format!(
                        "{} since opened {age} ago",
                        signed(d, units)
                    )));
                }
            }
            if !view.denied.is_empty() {
                let n = view.denied.len() as u64;
                let folders = if n == 1 { "folder" } else { "folders" };
                parts.push(Span::raw(format!(
                    "{} {folders} unreadable (d)",
                    thousands(n)
                )));
            }
            // changes in the dirs not shown are not seen since
            if view.status == Status::Done && self.live.as_ref().is_some_and(|l| !l.follows_all()) {
                let age = age(now.duration_since(self.scanned).unwrap_or_default());
                parts.push(Span::raw(format!("scanned {age} ago")));
            }
        }
        if self.by_change {
            parts.push(Span::raw("sorted by change"));
        }
        if let Some(dir) = &self.rescanning {
            parts.push(Span::raw(format!(
                "rescanning {}/...",
                String::from_utf8_lossy(dir)
            )));
        }
        // on a narrow screen the last parts go, down to the first
        let room = width.saturating_sub(width_of(&self.display) + 2);
        let fits = |parts: &[Span]| {
            let text: usize = parts.iter().map(Span::width).sum();
            text + 3 * parts.len().saturating_sub(1) <= room
        };
        while !fits(&parts) && parts.len() > 1 {
            parts.pop();
        }
        let mut spans = vec![Span::raw(self.display.clone())];
        if !parts.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.extend(join_parts(parts.into_iter().map(|p| vec![p]).collect(), styles).spans);
        Line::from(spans)
    }
}

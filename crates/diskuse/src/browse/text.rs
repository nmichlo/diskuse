//! Text for the screen: numbers, ages, and fitting a line to a width.

use crate::style::Styles;
use diskuse_core::Units;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::time::Duration;

/// `size`'s share of `total` in 5 columns, the decimal point always in
/// the same place: `99.1%`, ` 0.3%`, `<0.1%`, ` 100%`, blank for 0 B.
pub(crate) fn percent(size: u64, total: u64) -> String {
    if size == 0 || total == 0 {
        return " ".repeat(5);
    }
    // tenths of a percent, rounded
    let tenths =
        ((u128::from(size) * 2000 + u128::from(total)) / (2 * u128::from(total))).min(1000);
    match tenths {
        0 => "<0.1%".into(),
        1000 => " 100%".into(),
        t => format!("{:>2}.{}%", t / 10, t % 10),
    }
}

/// The columns `text` takes on screen.
pub(super) fn width_of(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// `text` in `style`, cut in the middle to `width` columns if wider, with a
/// dim ellipsis where it was cut, so both ends show.
pub(super) fn cut_middle(
    text: &str,
    width: usize,
    style: Style,
    styles: &Styles,
) -> Vec<Span<'static>> {
    if width_of(text) <= width {
        return vec![Span::styled(text.to_owned(), style)];
    }
    if width == 0 {
        return Vec::new();
    }
    let char_width = |c: char| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
    // the end gets half, the start the rest, after 1 for the ellipsis
    let mut tail_room = (width - 1) / 2;
    let mut head_room = width - 1 - tail_room;
    let mut head = String::new();
    for c in text.chars() {
        let w = char_width(c);
        if w > head_room {
            break;
        }
        head_room -= w;
        head.push(c);
    }
    tail_room += head_room;
    let mut tail = Vec::new();
    for c in text.chars().rev() {
        let w = char_width(c);
        if w > tail_room {
            break;
        }
        tail_room -= w;
        tail.push(c);
    }
    let tail: String = tail.into_iter().rev().collect();
    vec![
        Span::styled(head, style),
        Span::styled("\u{2026}", styles.dim),
        Span::styled(tail, style),
    ]
}

/// `parts` on one line, a dim ` | ` between them.
pub(super) fn join_parts(parts: Vec<Vec<Span<'static>>>, styles: &Styles) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", styles.dim));
        }
        spans.extend(part);
    }
    Line::from(spans)
}

/// `n` with a comma every 3 digits: `48,210`.
pub(super) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `keys`, `key what` pairs two spaces apart, leaving out those just
/// before the last two (`? help  q quit`) until it fits in `width`.
pub(super) fn fit_keys(keys: &str, width: usize) -> String {
    let mut pairs: Vec<&str> = keys.split("  ").collect();
    while width_of(&pairs.join("  ")) > width && pairs.len() > 3 {
        pairs.remove(pairs.len() - 3);
    }
    pairs.join("  ")
}

/// `+1.5 GiB` or `-4.0 KiB`.
pub(super) fn signed(bytes: i64, units: Units) -> String {
    let sign = if bytes < 0 { '-' } else { '+' };
    format!("{sign}{}", units.format(bytes.unsigned_abs()))
}

/// `5 min`: whole seconds, minutes, hours or days.
pub(super) fn age(d: Duration) -> String {
    match d.as_secs() {
        s @ ..60 => format!("{s} s"),
        s @ ..3600 => format!("{} min", s / 60),
        s @ ..86400 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86400),
    }
}

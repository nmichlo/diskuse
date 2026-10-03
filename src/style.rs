//! The styles of every screen, in one table. With `NO_COLOR` set, the
//! table without colours is used: it keeps the modifiers, so the selection
//! still shows.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// How the parts of a screen are drawn.
pub(crate) struct Styles {
    /// Sizes of at least 1 GiB, 1 MiB and 1 KiB, and smaller ones, like
    /// OmniDiskUseer.
    sizes: [Style; 4],
    /// The name of a directory.
    pub dir: Style,
    /// `(denied: EACCES)` and why a dir could not be read.
    pub denied: Style,
    /// The `+` of a lower bound, `(other device)` and hints.
    pub dim: Style,
    /// The row at the cursor.
    pub selected: Style,
    /// The current dir's row in the parent column.
    pub parent: Style,
    /// The labels of dirs macOS protects, caches, and known big folders.
    pub system: Style,
    pub cache: Style,
    pub known: Style,
    /// A key in a line of keys and what they do.
    pub key: Style,
    /// The `*` after a picked item's name.
    pub picked: Style,
    /// How much a dir grew or shrank since the session started.
    pub grew: Style,
    pub shrank: Style,
}

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

const COLOR: Styles = Styles {
    sizes: [
        Style::new().fg(Color::Red),
        Style::new().fg(Color::Yellow),
        Style::new().fg(Color::Green),
        Style::new().fg(Color::DarkGray),
    ],
    dir: Style::new().add_modifier(Modifier::BOLD),
    denied: Style::new().fg(Color::Red),
    dim: Style::new().fg(Color::DarkGray),
    selected: Style::new()
        .fg(Color::White)
        .bg(Color::Blue)
        .add_modifier(Modifier::BOLD),
    parent: Style::new().fg(Color::White).bg(Color::DarkGray),
    key: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    system: Style::new().fg(Color::Red),
    cache: Style::new().fg(Color::Green),
    known: Style::new().fg(Color::Yellow),
    picked: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    grew: Style::new().fg(Color::Red),
    shrank: Style::new().fg(Color::Green),
};

const PLAIN: Styles = Styles {
    sizes: [Style::new(); 4],
    dir: Style::new().add_modifier(Modifier::BOLD),
    denied: Style::new(),
    dim: Style::new(),
    selected: Style::new().add_modifier(Modifier::REVERSED),
    // dirs are bold already
    parent: Style::new().add_modifier(Modifier::UNDERLINED),
    key: Style::new().add_modifier(Modifier::BOLD),
    system: Style::new(),
    cache: Style::new(),
    known: Style::new(),
    picked: Style::new().add_modifier(Modifier::BOLD),
    grew: Style::new(),
    shrank: Style::new(),
};

impl Styles {
    /// The table with colours, or without.
    pub fn new(color: bool) -> &'static Self {
        match color {
            true => &COLOR,
            false => &PLAIN,
        }
    }

    /// The style of a size of `bytes`.
    pub fn size(&self, bytes: u64) -> Style {
        match bytes {
            GIB.. => self.sizes[0],
            MIB.. => self.sizes[1],
            KIB.. => self.sizes[2],
            _ => self.sizes[3],
        }
    }

    /// A line of `key what` pairs, two spaces apart, each key highlighted.
    pub fn keys(&self, keys: &str) -> Line<'static> {
        let mut spans = Vec::new();
        for (i, pair) in keys.split("  ").enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            let (key, what) = pair.split_once(' ').unwrap_or((pair, ""));
            spans.push(Span::styled(key.to_owned(), self.key));
            if !what.is_empty() {
                spans.push(Span::raw(format!(" {what}")));
            }
        }
        Line::from(spans)
    }
}

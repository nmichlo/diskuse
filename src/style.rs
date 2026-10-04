//! The styles of every screen, in one table. With `NO_COLOR` set, the
//! table without colours is used: it keeps the modifiers, so the selection
//! still shows.

use crate::labels::Tier;
use crate::report::Units;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// How the parts of a screen are drawn.
pub(crate) struct Styles {
    /// Sizes of at least 1 GiB, 1 MiB and 1 KiB, and smaller ones, like
    /// OmniDiskSweeper: by the unit they print in ([`Units::tier`]).
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
    /// What to know about the row at the cursor: unreadable folders below.
    pub warn: Style,
}

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
    // fixed palette entries, not the theme's white and blue, which some
    // themes make too close to read
    selected: Style::new()
        .fg(Color::Indexed(231))
        .bg(Color::Indexed(25))
        .add_modifier(Modifier::BOLD),
    parent: Style::new().fg(Color::White).bg(Color::DarkGray),
    key: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    system: Style::new().fg(Color::Red),
    cache: Style::new().fg(Color::Green),
    known: Style::new().fg(Color::Yellow),
    picked: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    grew: Style::new().fg(Color::Red),
    shrank: Style::new().fg(Color::Green),
    warn: Style::new().fg(Color::Yellow),
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
    warn: Style::new(),
};

impl Styles {
    /// The table with colours, or without.
    pub fn new(color: bool) -> &'static Self {
        match color {
            true => &COLOR,
            false => &PLAIN,
        }
    }

    /// The style of a size of `bytes`, printed in `units`.
    pub fn size(&self, bytes: u64, units: Units) -> Style {
        self.sizes[units.tier(bytes)]
    }

    /// The colour of a label of `tier`, and of the name it is on.
    pub fn label(&self, tier: Tier) -> Style {
        match tier {
            Tier::System => self.system,
            Tier::Cache => self.cache,
            Tier::Known => self.known,
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

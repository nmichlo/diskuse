//! Drawing a browser: the title, the screen shown, and the keys.

use super::Browser;
use super::lists::{panel, picks, top_files};
use super::text::fit_keys;
use crate::style::Styles;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use std::os::unix::ffi::OsStrExt;
use std::time::SystemTime;

/// The browser's keys, most useful first: on a narrow screen [`fit_keys`]
/// leaves out those before `? help` until the line fits.
pub(super) const HELP: &str = "hjkl move  i info  u units  space pick  p picks  r reveal  s/S rescan  \
                    / filter  t top  o open  ? help  q quit";

const PANEL_HELP: &str = "arrows/jk scroll  d close  ? help  q quit";

const INFO_HELP: &str = "i close  ? help  q quit";

const TOP_HELP: &str = "arrows/jk move  r reveal  o open  t close  ? help  q quit";

const PICKS_HELP: &str = "arrows/jk move  r reveal  o open  space unpick  p close  ? help  q quit";

impl Browser {
    /// Draws the title, the columns or the list shown instead, and the
    /// footer. `now` dates a saved scan. Of the browser it changes only
    /// what follows from the size of the screen ([`Browser::fit`]), and
    /// where a list has scrolled to, as a ratatui stateful widget does.
    pub fn draw(&mut self, frame: &mut Frame, now: SystemTime) {
        let styles = Styles::new(self.env.color);
        let width = frame.area().width.into();
        let footer: Vec<Line> = (self.status(styles, width).into_iter())
            .chain([self.footer(styles, width)])
            .collect();
        let [top, body, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(footer.len() as u16),
        ])
        .areas(frame.area());
        self.fit(body);
        let title = self.title(now, styles, width);
        let buf = frame.buffer_mut();
        buf.set_line(top.x, top.y, &title, top.width);
        for (y, line) in (bottom.y..).zip(footer) {
            buf.set_line(bottom.x, y, &line, bottom.width);
        }
        let Some(view) = &self.view else {
            return;
        };
        if let Some(offset) = &mut self.panel {
            let terminal = &self.env.terminal;
            *offset = panel(buf, body, view, terminal, *offset, styles);
            return;
        }
        if let Some(top) = &mut self.top {
            top_files(buf, body, view, top, styles, self.env.units);
            return;
        }
        if let Some(list) = &mut self.picking {
            let root = self.root.as_os_str().as_bytes();
            picks(
                buf,
                body,
                view,
                root,
                &self.picks,
                list,
                styles,
                self.env.units,
            );
            return;
        }
        self.draw_columns(buf, styles);
        if self.info {
            self.draw_info(buf, now, styles);
        }
    }

    /// The bottom line: the message, else the keys of what is shown, as
    /// many as fit in `width`.
    fn footer(&self, styles: &Styles, width: usize) -> Line<'static> {
        let keys = match &self.message {
            Some(message) => return Line::raw(message.clone()),
            None if self.info => INFO_HELP.into(),
            None if self.panel.is_some() => PANEL_HELP.into(),
            None if self.top.is_some() => TOP_HELP.into(),
            None if self.picking.is_some() => PICKS_HELP.into(),
            None if self.typing => format!("/{}  enter keep  esc clear", self.filter),
            None if !self.filter.is_empty() => format!("/{}  esc clear", self.filter),
            None => fit_keys(HELP, width),
        };
        styles.keys(&keys)
    }
}

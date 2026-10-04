//! What the browser says about folders a scan could not read: why, and
//! the guide to Full Disk Access ([`diskuse_core::preflight`]).

use diskuse_core::ReadTree;
use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};
use std::ffi::OsString;

/// The macOS settings pane that grants Full Disk Access.
const SETTINGS: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// The name of the terminal app, from `$TERM_PROGRAM`, as System Settings
/// lists it, or `your terminal`.
pub(crate) fn terminal_app(term_program: Option<&str>) -> String {
    match term_program.unwrap_or_default() {
        // a multiplexer runs inside the app that needs access
        "" | "tmux" | "screen" => "your terminal",
        "Apple_Terminal" => "Terminal",
        "iTerm.app" => "iTerm",
        "vscode" => "Visual Studio Code",
        "ghostty" => "Ghostty",
        other => other,
    }
    .into()
}

/// Why the denied dir `id` could not be read, and what would let it be.
/// `terminal` is from [`terminal_app`].
pub(crate) fn reason(tree: &impl ReadTree, id: u32, terminal: &str) -> String {
    match tree.error(id).unwrap_or_default().as_str() {
        "EACCES" => "permission denied (try sudo)".into(),
        "EPERM" if cfg!(target_os = "macos") => format!("needs Full Disk Access for {terminal}"),
        other => other.into(),
    }
}

/// The program and arguments that open the Full Disk Access settings.
pub(crate) fn settings_command() -> (&'static str, Vec<OsString>) {
    ("/usr/bin/open", vec![SETTINGS.into()])
}

/// Draws the guide shown when Full Disk Access is missing, and `message`,
/// if any, at the bottom.
pub(crate) fn draw_guide(frame: &mut Frame, terminal: &str, message: Option<&str>) {
    let text = Text::from_iter([
        format!("Full Disk Access is off for {terminal}"),
        String::new(),
        format!(
            "macOS keeps some folders from every app without Full Disk Access: \
             Mail, Messages, Safari, Time Machine and other apps' data. The scan \
             lists them as denied, and their sizes are missing. To grant it, turn \
             on {terminal} in System Settings > Privacy & Security > Full Disk \
             Access, then quit and reopen {terminal}. diskuse stays read-only \
             either way."
        ),
        String::new(),
        "o  open the Full Disk Access settings".into(),
        "c  continue without".into(),
        "q  quit".into(),
    ]);
    let area = frame.area();
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
    if let Some(message) = message {
        let bottom = area.height.saturating_sub(1);
        let buf = frame.buffer_mut();
        buf.set_stringn(0, bottom, message, area.width.into(), Style::new());
    }
}

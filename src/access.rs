//! What a scan could not read, and why. On macOS a privacy layer (TCC)
//! guards some folders on top of Unix permissions: it denies with `EPERM`
//! where Unix permissions deny with `EACCES`. Full Disk Access, given to
//! the terminal app, lifts it.

use crate::sys;
use crate::tree::{ReadTree, Tree};
use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};
use rustix::io::Errno;
use std::ffi::OsString;
use std::path::Path;

/// Folders whose first read makes macOS ask the user, one popup each.
const PROMPTED: [&str; 3] = ["Desktop", "Documents", "Downloads"];

/// Folders only Full Disk Access can read. Each may be missing.
const PROTECTED: [&str; 3] = [
    "Library/Application Support/com.apple.TCC",
    "Library/Safari",
    "Library/Mail",
];

/// The macOS settings pane that grants Full Disk Access.
const SETTINGS: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// Whether the terminal has Full Disk Access, as far as a probe can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullDiskAccess {
    Granted,
    Missing,
    /// No protected folder exists to probe, or Linux.
    Unknown,
}

/// Lists `~/Desktop`, `~/Documents` and `~/Downloads`, so any macOS
/// popups asking for them appear now, together, not one by one during the
/// scan. Then probes the [`PROTECTED`] folders for Full Disk Access.
pub(crate) fn preflight(home: &Path) -> FullDiskAccess {
    let list = |dir: &str| {
        let fd = sys::open_root(&home.join(dir))?;
        sys::read_dir(&fd, false, |_| {})
    };
    for dir in PROMPTED {
        // only asking matters, not the answer
        let _ = list(dir);
    }
    for dir in PROTECTED {
        match list(dir) {
            Ok(()) => return FullDiskAccess::Granted,
            Err(Errno::PERM) => return FullDiskAccess::Missing,
            Err(_) => {}
        }
    }
    FullDiskAccess::Unknown
}

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
pub(crate) fn reason(tree: &Tree, id: u32, terminal: &str) -> String {
    let errno = i32::from(tree.record(id).errno);
    if errno == Errno::ACCESS.raw_os_error() {
        "permission denied (try sudo)".into()
    } else if cfg!(target_os = "macos") && errno == Errno::PERM.raw_os_error() {
        format!("needs Full Disk Access for {terminal}")
    } else {
        tree.error(id).unwrap_or_default()
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

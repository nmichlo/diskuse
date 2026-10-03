//! The volume list `disksweep` starts on without a path: one row per real
//! filesystem, with its size, used and free bytes, and a bar of its used
//! share.

use crate::browse::bar;
use crate::report::format_size;
use crate::style::Styles;
use crate::sys;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::io;
use std::path::{Path, PathBuf};

const HELP: &str = "arrows/jk move  enter scan  ? help  q quit";

/// Filesystem types never listed, though they have a size: memory, the
/// layers of containers, and snap packages. Most other pseudo filesystems,
/// like `proc`, `sysfs` and `cgroup2`, have no size and are dropped for
/// that.
const PSEUDO: [&str; 5] = ["devtmpfs", "tmpfs", "overlay", "squashfs", "efivarfs"];

/// One mounted filesystem, as the OS lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub point: PathBuf,
    /// The filesystem type: `apfs`, `ext4`, `proc`...
    pub fs: String,
    /// macOS `nobrowse`: hidden from Finder, like the VM and Preboot
    /// volumes, `devfs` and `autofs`. Never set on Linux.
    pub hidden: bool,
    /// Bytes in all.
    pub total: u64,
    /// Bytes in use: `f_blocks - f_bfree`. On APFS that is the whole
    /// container, all its volumes, so on macOS `/` counts the hidden Data
    /// volume, which a scan of `/` reaches through firmlinks.
    pub used: u64,
    /// Bytes free for users other than root.
    pub free: u64,
}

/// Every mounted filesystem. On macOS the sizes may be a few seconds old,
/// so a hung network mount cannot block the list.
pub fn mounts() -> io::Result<Vec<Mount>> {
    sys::mounts()
}

/// The mounts worth listing, by mount point: `/` always, even as a
/// container's `overlay`, then every mount that is not hidden, has a size
/// and is not of a [`PSEUDO`] type.
pub(crate) fn volumes(mounts: Vec<Mount>) -> Vec<Mount> {
    let mut kept: Vec<Mount> = mounts
        .into_iter()
        .filter(|m| {
            let pseudo = m.hidden || m.total == 0 || PSEUDO.contains(&&*m.fs);
            m.point == Path::new("/") || !pseudo
        })
        .collect();
    kept.sort_by(|a, b| a.point.cmp(&b.point));
    kept
}

/// The rows of volumes a screen of `area` has room for.
pub(crate) fn height(area: Rect) -> usize {
    area.height.saturating_sub(2).into()
}

/// The index of the volume drawn at row `y` of `area`, the cursor at
/// `cursor`, if a volume could be drawn there.
pub(crate) fn row_at(area: Rect, cursor: usize, y: u16) -> Option<usize> {
    let y = usize::from(y.checked_sub(1)?);
    (y < height(area)).then(|| offset(area, cursor) + y)
}

/// The first volume drawn.
fn offset(area: Rect, cursor: usize) -> usize {
    (cursor + 1).saturating_sub(height(area))
}

/// Draws `volumes`, each with a bar of its used share, the row at `cursor`
/// selected and on screen.
pub(crate) fn draw(frame: &mut Frame, volumes: &[Mount], cursor: usize, styles: &Styles) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    let width = area.width.into();
    buf.set_stringn(0, 0, "volumes", width, Style::new());
    let rows = (1..).zip(volumes.iter().enumerate().skip(offset(area, cursor)));
    for (y, (i, v)) in rows.take(height(area)) {
        let used = styles.size(v.used);
        let mut line = Line::from_iter([
            Span::styled(format!("{:>10}", format_size(v.used)), used),
            Span::raw(format!(
                " used of {:>10}  {:>10} free  ",
                format_size(v.total),
                format_size(v.free)
            )),
        ]);
        line.extend(bar(v.used, v.total, used, styles));
        line.push_span(Span::raw(format!("  {}", v.point.display())));
        buf.set_line(0, y, &line, area.width);
        if i == cursor {
            buf.set_style(Rect::new(0, y, area.width, 1), styles.selected);
        }
    }
    let bottom = area.height.saturating_sub(1);
    buf.set_line(0, bottom, &styles.keys(HELP), area.width);
}

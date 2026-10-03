//! The `diskuse scan` text output.

use crate::sys;
use crate::tree::{ReadTree, Record};
use std::cmp::Ordering;
use std::fmt::Write;

/// The root's total, then its direct children and its own files, largest
/// first, ties by name bytes. `reclaimable` adds a second size column, the
/// bytes not shared with a clone. `top` appends that many of the largest
/// files. The first line says so if the tree is partial or stopped.
pub fn report(tree: &impl ReadTree, reclaimable: bool, top: Option<usize>) -> String {
    let totals = tree.totals();
    let index = tree.child_index();
    let denied = count_denied(tree);
    let sizes = |size: u64, private: u64| match reclaimable {
        true => format!("{:>10}  {:>10}", format_size(size), format_size(private)),
        false => format!("{:>10}", format_size(size)),
    };

    let mut out = format!(
        "{}  {}",
        sizes(totals.size[0], totals.private[0]),
        String::from_utf8_lossy(tree.name(tree.record(0).name))
    );
    if denied > 0 {
        write!(out, "  (partial: {denied} denied)").unwrap();
    }
    if tree.stopped() {
        out.push_str(INCOMPLETE);
    }
    out.push('\n');

    // (size, private, name bytes for the tie-break, label)
    let mut rows: Vec<(u64, u64, &[u8], String)> = index
        .children(0)
        .iter()
        .map(|&i| {
            let r = tree.record(i);
            let name = tree.name(r.name);
            let label = format!(
                "{}/{}",
                String::from_utf8_lossy(name),
                suffix(&r, totals.flags[i as usize])
            );
            let i = i as usize;
            (totals.size[i], totals.private[i], name, label)
        })
        .collect();
    let root = tree.record(0);
    if root.own > 0 {
        rows.push((root.own, tree.own_private(0), b"[files]", "[files]".into()));
    }
    rows.sort_by(|a, b| largest_first((a.0, a.2), (b.0, b.2)));
    for (size, private, _, label) in rows {
        writeln!(out, "{}  {label}", sizes(size, private)).unwrap();
    }
    if let Some(n) = top {
        out.push_str("\nlargest files:\n");
        for (size, path) in largest_files(tree, n) {
            let path = String::from_utf8_lossy(&path);
            writeln!(out, "{:>10}  {path}", format_size(size)).unwrap();
        }
    }
    out
}

/// After the root's line of a stopped scan.
const INCOMPLETE: &str = "  (incomplete: scan stopped)";

/// The output order of `(size, name)` pairs: largest first, ties by name
/// bytes, so output never depends on scan order.
pub(crate) fn largest_first(a: (u64, &[u8]), b: (u64, &[u8])) -> Ordering {
    b.0.cmp(&a.0).then(a.1.cmp(b.1))
}

/// The `n` largest files as `(bytes, path)`, ties by path bytes.
pub(crate) fn largest_files(tree: &impl ReadTree, n: usize) -> Vec<(u64, Vec<u8>)> {
    let mut files: Vec<_> = (tree.largest())
        .map(|(bytes, dir, name)| (bytes, tree.path(dir, name)))
        .collect();
    files.sort_by(|a, b| largest_first((a.0, &a.1), (b.0, &b.1)));
    files.truncate(n);
    files
}

/// How many directories could not be read.
fn count_denied(tree: &impl ReadTree) -> usize {
    (0..tree.len() as u32)
        .filter(|&i| tree.record(i).flags & Record::DENIED != 0)
        .count()
}

/// The marker after a directory's name: ` (denied: EACCES)`,
/// ` (other device)`, ` (partial)` or nothing. `flags` is from
/// [`crate::Totals::flags`].
pub(crate) fn suffix(r: &Record, flags: u16) -> String {
    if flags & Record::DENIED != 0 {
        format!(" (denied: {})", denied(r))
    } else if flags & Record::OTHER_DEVICE != 0 {
        " (other device)".into()
    } else if flags & Record::PARTIAL != 0 {
        " (partial)".into()
    } else {
        String::new()
    }
}

/// Why a [`Record::DENIED`] dir could not be read: `EACCES`, `EPERM` or
/// `errno N`.
pub(crate) fn denied(r: &Record) -> String {
    match sys::errno_name(r.errno) {
        Some(name) => name.into(),
        None => format!("errno {}", r.errno),
    }
}

/// `n B` below 1 KiB, else one decimal in binary units.
pub(crate) fn format_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

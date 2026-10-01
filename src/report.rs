//! The `disksweep scan` text output.

use crate::sys;
use crate::tree::{Record, Tree};
use std::fmt::Write;

/// The root's total, then its direct children and its own files, largest
/// first, ties by name bytes. `reclaimable` adds a second size column, the
/// bytes not shared with a clone.
pub fn report(tree: &Tree, reclaimable: bool) -> String {
    let totals = tree.totals();
    let index = tree.child_index();
    let denied = (0..tree.len() as u32)
        .filter(|&i| tree.record(i).flags & Record::DENIED != 0)
        .count();
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
                suffix(r, totals.flags[i as usize])
            );
            let i = i as usize;
            (totals.size[i], totals.private[i], name, label)
        })
        .collect();
    let root = tree.record(0);
    if root.own > 0 {
        rows.push((root.own, root.own_private, b"[files]", "[files]".into()));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(b.2)));
    for (size, private, _, label) in rows {
        writeln!(out, "{}  {label}", sizes(size, private)).unwrap();
    }
    out
}

fn suffix(r: &Record, flags: u32) -> String {
    if flags & Record::DENIED != 0 {
        match sys::errno_name(r.errno) {
            Some(name) => format!(" (denied: {name})"),
            None => format!(" (denied: errno {})", r.errno),
        }
    } else if flags & Record::OTHER_DEVICE != 0 {
        " (other device)".into()
    } else if flags & Record::PARTIAL != 0 {
        " (partial)".into()
    } else {
        String::new()
    }
}

/// `n B` below 1 KiB, else one decimal in binary units.
fn format_size(n: u64) -> String {
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

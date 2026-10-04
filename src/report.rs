//! The `diskuse scan` text output.

use crate::tree::{ReadTree, Record};
use std::cmp::Ordering;
use std::fmt::Write;

/// The root's total, then its direct children and its own files, largest
/// first, ties by name bytes. `reclaimable` adds a second size column, the
/// bytes not shared with a clone. `top` appends that many of the largest
/// files. The first line says so if the tree is partial or stopped. Sizes
/// print in `units`.
pub fn report(tree: &impl ReadTree, reclaimable: bool, top: Option<usize>, units: Units) -> String {
    let format_size = |n| units.format(n);
    let denied = count_denied(tree);
    let sizes = |size: u64, private: u64| match reclaimable {
        true => format!("{:>10}  {:>10}", format_size(size), format_size(private)),
        false => format!("{:>10}", format_size(size)),
    };

    let mut out = format!(
        "{}  {}",
        sizes(tree.size(0), tree.reclaimable(0)),
        String::from_utf8_lossy(tree.name(0))
    );
    if denied > 0 {
        write!(out, "  (partial: {denied} denied)").unwrap();
    }
    if tree.stopped() {
        out.push_str(INCOMPLETE);
    }
    out.push('\n');

    // (size, private, name bytes for the tie-break, label)
    let mut rows: Vec<(u64, u64, &[u8], String)> = (tree.children(0).iter())
        .map(|&i| {
            let name = tree.name(i);
            let label = format!("{}/{}", String::from_utf8_lossy(name), suffix(tree, i));
            (tree.size(i), tree.reclaimable(i), name, label)
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
        for (path, size) in tree.largest_files(n) {
            writeln!(out, "{:>10}  {}", format_size(size), path.display()).unwrap();
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

/// How many directories could not be read.
fn count_denied(tree: &impl ReadTree) -> usize {
    (0..tree.len() as u32)
        .filter(|&i| tree.record(i).flags & Record::DENIED != 0)
        .count()
}

/// The marker after a directory's name: ` (denied: EACCES)`,
/// ` (other device)`, ` (partial)` or nothing.
pub(crate) fn suffix(tree: &impl ReadTree, id: u32) -> String {
    if let Some(error) = tree.error(id) {
        format!(" (denied: {error})")
    } else if tree.other_device(id) {
        " (other device)".into()
    } else if tree.partial(id) {
        " (partial)".into()
    } else {
        String::new()
    }
}

/// How sizes print: in powers of 1024 (KiB, MiB, GiB), as `du` and ncdu,
/// or of 1000 (kB, MB, GB), as Finder and disk makers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Units {
    #[default]
    Binary,
    Decimal,
}

impl Units {
    fn base(self) -> u64 {
        match self {
            Self::Binary => 1024,
            Self::Decimal => 1000,
        }
    }

    /// The other units.
    pub fn toggled(self) -> Self {
        match self {
            Self::Binary => Self::Decimal,
            Self::Decimal => Self::Binary,
        }
    }

    /// `n B` below 1 KiB (1 kB), else one decimal.
    pub fn format(self, n: u64) -> String {
        let units = match self {
            Self::Binary => ["KiB", "MiB", "GiB", "TiB", "PiB"],
            Self::Decimal => ["kB", "MB", "GB", "TB", "PB"],
        };
        let base = self.base();
        if n < base {
            return format!("{n} B");
        }
        let mut v = n as f64 / base as f64;
        let mut unit = 0;
        while v >= base as f64 && unit < units.len() - 1 {
            v /= base as f64;
            unit += 1;
        }
        format!("{v:.1} {}", units[unit])
    }

    /// Which unit `n` prints in: 0 from 1 GiB (GB) on, 1 from 1 MiB, 2
    /// from 1 KiB, 3 below.
    pub fn tier(self, n: u64) -> usize {
        let base = self.base();
        match n {
            _ if n >= base.pow(3) => 0,
            _ if n >= base.pow(2) => 1,
            _ if n >= base => 2,
            _ => 3,
        }
    }
}

//! The `--json` output: one object, the root dir, with its subdirectories
//! nested in `children`. Written by hand, as the schema is small.

use crate::read::{FolderId, ReadTree};
use crate::report::largest_first;
use std::fmt::Write;
use std::os::unix::ffi::OsStrExt;

/// The root and `depth` levels of subdirectories below it, each level
/// ordered like [`crate::report`]. `reclaimable` adds `reclaimable` sizes,
/// and `top` a `largest_files` list to the root. The root of a tree a
/// stopped scan has `incomplete`.
pub fn json(tree: &impl ReadTree, reclaimable: bool, depth: usize, top: Option<usize>) -> String {
    let kids = |id: FolderId| {
        let mut kids: Vec<FolderId> = tree.children(id).collect();
        kids.sort_by(|&a, &b| {
            let key = |i: FolderId| (tree.size(i), tree.name(i));
            largest_first(key(a), key(b))
        });
        kids.into_iter()
    };
    // every key but `children`, and no closing brace
    let node = |out: &mut String, id: FolderId| {
        out.push_str("{\"name\":");
        let lossy = string(out, tree.name(id));
        write!(out, ",\"size\":{}", tree.size(id)).unwrap();
        if reclaimable {
            write!(out, ",\"reclaimable\":{}", tree.reclaimable(id)).unwrap();
        }
        write!(out, ",\"own\":{}", tree.own(id)).unwrap();
        if let Some(error) = tree.error(id) {
            out.push_str(",\"denied\":");
            string(out, error.as_bytes());
        }
        if tree.other_device(id) {
            out.push_str(",\"other_device\":true");
        }
        if tree.partial(id) {
            out.push_str(",\"partial\":true");
        }
        if lossy {
            out.push_str(",\"name_lossy\":true");
        }
    };

    let mut out = String::new();
    node(&mut out, FolderId::ROOT);
    if tree.stopped() {
        out.push_str(",\"incomplete\":true");
    }
    // one iterator of unvisited children per open level: a loop, not
    // recursion, so a deep tree cannot overflow the stack
    let mut stack = Vec::new();
    if depth > 0 {
        out.push_str(",\"children\":[");
        stack.push(kids(FolderId::ROOT));
    }
    while let Some(level) = stack.last_mut() {
        match level.next() {
            Some(id) => {
                if !out.ends_with('[') {
                    out.push(',');
                }
                node(&mut out, id);
                if stack.len() < depth {
                    out.push_str(",\"children\":[");
                    stack.push(kids(id));
                } else {
                    out.push('}');
                }
            }
            None => {
                stack.pop();
                out.push(']');
                // the root's brace closes after `largest_files`
                if !stack.is_empty() {
                    out.push('}');
                }
            }
        }
    }
    if let Some(n) = top {
        out.push_str(",\"largest_files\":[");
        for (k, (path, size)) in tree.largest_files(n).into_iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            out.push_str("{\"path\":");
            string(&mut out, path.as_os_str().as_bytes());
            write!(out, ",\"size\":{size}}}").unwrap();
        }
        out.push(']');
    }
    out.push_str("}\n");
    out
}

/// Writes `bytes` as a JSON string, escaped per RFC 8259. Bytes that are not
/// UTF-8 become U+FFFD; returns whether any did.
fn string(out: &mut String, bytes: &[u8]) -> bool {
    let s = String::from_utf8_lossy(bytes);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out.push('"');
    matches!(s, std::borrow::Cow::Owned(_))
}

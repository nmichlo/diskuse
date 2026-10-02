//! The `--json` output: one object, the root dir, with its subdirectories
//! nested in `children`. Written by hand, as the schema is small.

use crate::report::{denied, largest_files, largest_first};
use crate::tree::{ReadTree, Record};
use std::fmt::Write;

/// The root and `depth` levels of subdirectories below it, each level
/// ordered like [`crate::report`]. `reclaimable` adds `reclaimable` sizes,
/// and `top` a `largest_files` list to the root. The root of a tree a
/// stopped scan left unfinished has `incomplete`.
pub fn json(tree: &impl ReadTree, reclaimable: bool, depth: usize, top: Option<usize>) -> String {
    let totals = tree.totals();
    let index = tree.child_index();
    let kids = |id: u32| {
        let mut kids = index.children(id).to_vec();
        kids.sort_by(|&a, &b| {
            let key = |i: u32| (totals.size[i as usize], tree.name(tree.record(i).name));
            largest_first(key(a), key(b))
        });
        kids.into_iter()
    };
    // every key but `children`, and no closing brace
    let node = |out: &mut String, id: u32| {
        let r = tree.record(id);
        let i = id as usize;
        out.push_str("{\"name\":");
        let lossy = string(out, tree.name(r.name));
        write!(out, ",\"size\":{}", totals.size[i]).unwrap();
        if reclaimable {
            write!(out, ",\"reclaimable\":{}", totals.private[i]).unwrap();
        }
        write!(out, ",\"own\":{}", r.own).unwrap();
        let flags = totals.flags[i];
        if flags & Record::DENIED != 0 {
            out.push_str(",\"denied\":");
            string(out, denied(&r).as_bytes());
        }
        if flags & Record::OTHER_DEVICE != 0 {
            out.push_str(",\"other_device\":true");
        }
        if flags & Record::PARTIAL != 0 {
            out.push_str(",\"partial\":true");
        }
        if lossy {
            out.push_str(",\"name_lossy\":true");
        }
    };

    let mut out = String::new();
    node(&mut out, 0);
    if tree.unfinished(&index).next().is_some() {
        out.push_str(",\"incomplete\":true");
    }
    // one iterator of unvisited children per open level: a loop, not
    // recursion, so a deep tree cannot overflow the stack
    let mut stack = Vec::new();
    if depth > 0 {
        out.push_str(",\"children\":[");
        stack.push(kids(0));
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
        for (k, (size, path)) in largest_files(tree, n).into_iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            out.push_str("{\"path\":");
            string(&mut out, &path);
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

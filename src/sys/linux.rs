//! The mount table of Linux and other non-macOS unixes.

use crate::volumes::Mount;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

/// Every mount in `/proc/self/mountinfo`, with sizes from `statvfs`. A
/// mount `statvfs` fails on gets size 0.
pub fn mounts() -> io::Result<Vec<Mount>> {
    let table = std::fs::read("/proc/self/mountinfo")?;
    let mounts = table
        .split(|&b| b == b'\n')
        .filter_map(parse)
        .map(|(point, fs)| {
            let (total, used, free) = match rustix::fs::statvfs(&point) {
                Ok(s) => (
                    s.f_blocks * s.f_frsize,
                    s.f_blocks.saturating_sub(s.f_bfree) * s.f_frsize,
                    s.f_bavail * s.f_frsize,
                ),
                Err(_) => (0, 0, 0),
            };
            Mount {
                point,
                fs,
                hidden: false,
                total,
                used,
                free,
            }
        });
    Ok(mounts.collect())
}

/// The mount point and filesystem type of one mountinfo line: the 5th
/// field, and the one after the `-` that ends the optional fields.
fn parse(line: &[u8]) -> Option<(PathBuf, String)> {
    let mut fields = line.split(|&b| b == b' ');
    let point = unescape(fields.nth(4)?);
    let fs = fields.skip_while(|&f| f != b"-").nth(1)?;
    let fs = String::from_utf8_lossy(fs).into();
    Some((OsString::from_vec(point).into(), fs))
}

/// Undoes the kernel's `\ooo` octal escapes, which it uses for space, tab,
/// newline and backslash.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut rest = field;
    while let Some((&first, tail)) = rest.split_first() {
        let octal = (tail.get(..3))
            .filter(|_| first == b'\\')
            .and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok());
        rest = match octal {
            Some(byte) => {
                out.push(byte);
                &tail[3..]
            }
            _ => {
                out.push(first);
                tail
            }
        };
    }
    out
}

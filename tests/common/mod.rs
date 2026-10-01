//! The fixture tree and file helpers the test crates share.

// each test crate uses only part of this
#![allow(dead_code, clippy::disallowed_methods)] // fixtures create files

use std::fs::{self, File, Permissions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Writes a file of `len` bytes and requires the filesystem to allocate
/// exactly `len` bytes for it, which the expected sizes below rely on.
pub fn file(path: &Path, len: usize) {
    File::create(path)
        .unwrap()
        .write_all(&vec![0xab; len])
        .unwrap();
    let allocated = fs::metadata(path).unwrap().blocks() * 512;
    assert_eq!(
        allocated, len as u64,
        "this filesystem allocates {allocated} bytes for a {len} byte file; \
         the tests need allocated == apparent size for multiples of 4096"
    );
}

/// Allocated bytes of a directory or symlink itself (APFS: 0, ext4: 4096).
pub fn own_bytes(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

/// The output format for 0 B and whole KiB below 1 MiB.
pub fn kib(bytes: u64) -> String {
    assert!(bytes.is_multiple_of(1024) && bytes < 1024 * 1024);
    match bytes {
        0 => "0 B".to_string(),
        _ => format!("{}.0 KiB", bytes / 1024),
    }
}

/// Makes the locked dirs readable again before the temp dir is removed.
pub struct Unlock(Vec<PathBuf>);

impl Drop for Unlock {
    fn drop(&mut self) {
        for path in &self.0 {
            fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
        }
    }
}

/// Field order matters: `Unlock` drops before the temp dir.
pub struct Fixture {
    _unlock: Unlock,
    pub dir: TempDir,
    /// The `scan` text output.
    pub expected: String,
    /// Allocated bytes of each directory itself.
    pub dir_bytes: u64,
    /// Allocated bytes of `sym`.
    pub sym_bytes: u64,
}

/// ```text
/// root/
///   top             8192
///   sym -> a        symlink, its own allocation only
///   a/
///     f1            4096
///     h1, h2        8192, one file hard-linked twice: counted once
///     b/
///       f2          8192
///       locked/     mode 000, hides a 4096 file
///       c/
///         f3       12288
///   big/
///     f          1572864 (1.5 MiB)
///   empty/
///   locked/         mode 000, hides a 4096 file
/// ```
///
/// Both `locked/` dirs are left out when running as root, which can open
/// anything.
pub fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let as_root = rustix::process::geteuid().is_root();
    let locked = match as_root {
        true => vec![],
        false => vec![root.join("locked"), root.join("a/b/locked")],
    };
    for d in ["a/b/c", "big", "empty"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    file(&root.join("top"), 8192);
    symlink("a", root.join("sym")).unwrap();
    file(&root.join("a/f1"), 4096);
    file(&root.join("a/h1"), 8192);
    fs::hard_link(root.join("a/h1"), root.join("a/h2")).unwrap();
    file(&root.join("a/b/f2"), 8192);
    file(&root.join("a/b/c/f3"), 12288);
    file(&root.join("big/f"), 1572864);
    for l in &locked {
        fs::create_dir(l).unwrap();
        file(&l.join("hidden"), 4096);
    }

    let d = own_bytes(root);
    for sub in ["a", "a/b", "a/b/c", "big", "empty"] {
        assert_eq!(own_bytes(&root.join(sub)), d, "dir {sub} differs from root");
    }
    for l in &locked {
        assert_eq!(own_bytes(l), d, "dir {} differs from root", l.display());
    }
    let sym = own_bytes(&root.join("sym"));
    // the dir size only moves the total across a rounding boundary
    let total = match d {
        0 => "1.5 MiB",
        4096 => "1.6 MiB",
        _ => panic!("unexpected dir size {d}"),
    };
    let unlock = Unlock(locked);
    for l in &unlock.0 {
        fs::set_permissions(l, Permissions::from_mode(0o000)).unwrap();
    }
    let a = 4096 + 8192 + 8192 + 12288 + 3 * d;
    let expected = if as_root {
        format!(
            "{total:>10}  {}\n\
             {:>10}  big/\n\
             {:>10}  a/\n\
             {:>10}  [files]\n\
             {:>10}  empty/\n",
            root.display(),
            "1.5 MiB",
            kib(a),
            kib(8192 + sym + d),
            kib(d),
        )
    } else {
        // a locked dir counts only its own blocks
        format!(
            "{total:>10}  {}  (partial: 2 denied)\n\
             {:>10}  big/\n\
             {:>10}  a/ (partial)\n\
             {:>10}  [files]\n\
             {:>10}  empty/\n\
             {:>10}  locked/ (denied: EACCES)\n",
            root.display(),
            "1.5 MiB",
            kib(a + d),
            kib(8192 + sym + d),
            kib(d),
            kib(d),
        )
    };
    Fixture {
        _unlock: unlock,
        dir,
        expected,
        dir_bytes: d,
        sym_bytes: sym,
    }
}

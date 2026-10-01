#![allow(clippy::disallowed_methods)] // fixtures create files and run the binary

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat};
use std::fs::{self, File, Permissions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Runs `disksweep scan <path> <args>` and returns stdout, requiring success
/// and an empty stderr.
fn scan(path: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_disksweep"))
        .arg("scan")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
    assert!(out.status.success(), "exit status {}", out.status);
    String::from_utf8(out.stdout).unwrap()
}

/// Writes a file of `len` bytes and requires the filesystem to allocate
/// exactly `len` bytes for it, which the expected sizes below rely on.
fn file(path: &Path, len: usize) {
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
fn own_bytes(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

/// The output format for 0 B and whole KiB below 1 MiB.
fn kib(bytes: u64) -> String {
    assert!(bytes.is_multiple_of(1024) && bytes < 1024 * 1024);
    match bytes {
        0 => "0 B".to_string(),
        _ => format!("{}.0 KiB", bytes / 1024),
    }
}

/// Makes the locked dirs readable again before the temp dir is removed.
struct Unlock(Vec<PathBuf>);

impl Drop for Unlock {
    fn drop(&mut self) {
        for path in &self.0 {
            fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
        }
    }
}

/// Field order matters: `Unlock` drops before the temp dir.
struct Fixture {
    _unlock: Unlock,
    dir: TempDir,
    expected: String,
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
fn fixture() -> Fixture {
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
    }
}

#[test]
fn scan_reports_exact_sizes() {
    let f = fixture();
    assert_eq!(scan(f.dir.path(), &[]), f.expected);
}

#[test]
fn scan_output_is_independent_of_thread_count() {
    let f = fixture();
    assert_eq!(scan(f.dir.path(), &["--threads", "1"]), f.expected);
    assert_eq!(scan(f.dir.path(), &["--threads", "8"]), f.expected);
}

#[test]
fn scan_follows_a_symlinked_root() {
    let f = fixture();
    let link = tempfile::tempdir().unwrap();
    let root = link.path().join("root");
    symlink(f.dir.path(), &root).unwrap();
    let expected = f.expected.replacen(
        &f.dir.path().display().to_string(),
        &root.display().to_string(),
        1,
    );
    assert_eq!(scan(&root, &[]), expected);
}

/// Creates `root/d/d/.../d/f` with `depth` dirs and a 4096 byte `f`.
///
/// Every call stays at depth 1, wrapping the chain built so far in a new
/// top dir, because on macOS each mkdir or rmdir at depth N costs O(N).
fn make_chain(root: &OwnedFd, depth: usize) {
    mkdirat(root, "d", Mode::from_raw_mode(0o755)).unwrap();
    let d = openat(root, "d", OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
    let f = openat(
        &d,
        "f",
        OFlags::WRONLY | OFlags::CREATE,
        Mode::from_raw_mode(0o644),
    )
    .unwrap();
    File::from(f).write_all(&[0xab; 4096]).unwrap();
    for _ in 1..depth {
        mkdirat(root, "n", Mode::from_raw_mode(0o755)).unwrap();
        let n = openat(root, "n", OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
        renameat(root, "d", &n, "d").unwrap();
        renameat(root, "n", root, "d").unwrap();
    }
}

/// Removes what [`make_chain`] made, moving each grandchild up to depth 1
/// and deleting there. `remove_dir_all` would recurse once per level.
fn remove_chain(root: &OwnedFd) {
    loop {
        let d = openat(root, "d", OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
        let last = renameat(&d, "d", root, "n").is_err();
        if last {
            unlinkat(&d, "f", AtFlags::empty()).unwrap();
        }
        drop(d);
        unlinkat(root, "d", AtFlags::REMOVEDIR).unwrap();
        if last {
            return;
        }
        renameat(root, "n", root, "d").unwrap();
    }
}

/// 4096 nested `d` dirs: the path is far beyond PATH_MAX, and recursion per
/// level would risk the stack.
#[test]
fn scan_handles_chains_deeper_than_path_max() {
    let dir = tempfile::tempdir().unwrap();
    let root = openat(
        rustix::fs::CWD,
        dir.path(),
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .unwrap();
    make_chain(&root, 4096);
    let out = scan(dir.path(), &[]);
    remove_chain(&root);

    let path = dir.path().display();
    let expected = match own_bytes(dir.path()) {
        0 => format!("   4.0 KiB  {path}\n   4.0 KiB  d/\n"),
        4096 => format!("  16.0 MiB  {path}\n  16.0 MiB  d/\n   4.0 KiB  [files]\n"),
        d => panic!("unexpected dir size {d}"),
    };
    assert_eq!(out, expected);
}

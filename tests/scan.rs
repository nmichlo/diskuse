#![allow(clippy::disallowed_methods)] // fixtures create files and run the binary

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat};
use std::fs::{self, File, Permissions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// Runs `disksweep <cmd> <path> <args>` with its saved scans in `cache`, so
/// tests never touch the user's cache dir.
fn disksweep(cache: &Path, cmd: &str, path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_disksweep"))
        .env("DISKSWEEP_CACHE_DIR", cache)
        .arg(cmd)
        .arg(path)
        .args(args)
        .output()
        .unwrap()
}

/// Requires success and an empty stderr, and returns stdout.
fn ok(out: Output) -> String {
    assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
    assert!(out.status.success(), "exit status {}", out.status);
    String::from_utf8(out.stdout).unwrap()
}

/// `disksweep scan <path> <args>`, saving into a throwaway cache.
fn scan(path: &Path, args: &[&str]) -> String {
    let cache = tempfile::tempdir().unwrap();
    ok(disksweep(cache.path(), "scan", path, args))
}

/// The saved scan files in `cache`.
fn saved(cache: &Path) -> Vec<PathBuf> {
    fs::read_dir(cache)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

/// Extra args for each directory reader. On macOS this checks the bulk
/// reader against the portable one.
const READERS: [&[&str]; 2] = [&[], &["--reader", "portable"]];

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
    /// The `scan` text output.
    expected: String,
    /// Allocated bytes of each directory itself.
    dir_bytes: u64,
    /// Allocated bytes of `sym`.
    sym_bytes: u64,
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
        dir_bytes: d,
        sym_bytes: sym,
    }
}

#[test]
fn scan_reports_exact_sizes() {
    let f = fixture();
    for reader in READERS {
        assert_eq!(scan(f.dir.path(), reader), f.expected, "{reader:?}");
    }
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

#[test]
fn top_lists_the_largest_files() {
    let f = fixture();
    let root = f.dir.path().display();
    // h1 and h2 are one file, of 8192 like f2 and top: ties go by path
    let expected = format!(
        "{}\nlargest files:\n   \
         1.5 MiB  {root}/big/f\n  \
         12.0 KiB  {root}/a/b/c/f3\n   \
         8.0 KiB  {root}/a/b/f2\n",
        f.expected
    );
    for reader in READERS {
        let args = [reader, &["--top", "3"]].concat();
        assert_eq!(scan(f.dir.path(), &args), expected, "{reader:?}");
    }
}

/// A `--json` node with no `children` key. `extra` holds any flag keys.
fn leaf(name: &str, size: u64, own: u64, extra: &str) -> String {
    format!(r#"{{"name":"{name}","size":{size},"own":{own}{extra}}}"#)
}

/// The `--json` output of a root and its leaf children.
fn root(path: &Path, size: u64, own: u64, extra: &str, kids: &[String]) -> String {
    let path = path.display();
    let kids = kids.join(",");
    format!("{{\"name\":\"{path}\",\"size\":{size},\"own\":{own}{extra},\"children\":[{kids}]}}\n")
}

#[test]
fn json_nests_dirs_to_the_given_depth() {
    let f = fixture();
    let d = f.dir_bytes;
    let big = 1572864 + d;
    let a = 4096 + 8192 + 8192 + 12288 + 3 * d;
    let own = 8192 + f.sym_bytes + d;
    let mut kids = vec![leaf("big", big, big, ""), leaf("empty", d, d, "")];
    let expected = match rustix::process::geteuid().is_root() {
        true => {
            kids.insert(1, leaf("a", a, 4096 + 8192 + d, ""));
            root(f.dir.path(), own + big + a + d, own, "", &kids)
        }
        false => {
            let partial = r#","partial":true"#;
            kids.insert(1, leaf("a", a + d, 4096 + 8192 + d, partial));
            kids.push(leaf("locked", d, d, r#","denied":"EACCES""#));
            root(f.dir.path(), own + big + a + 3 * d, own, partial, &kids)
        }
    };
    assert_eq!(scan(f.dir.path(), &["--json"]), expected);
}

#[test]
fn json_escapes_names() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a\"b\\c", "d\te\nf\u{1}"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    let d = own_bytes(dir.path());
    let kids = [
        leaf(r#"a\"b\\c"#, d, d, ""),
        leaf(r#"d\te\nf\u0001"#, d, d, ""),
    ];
    assert_eq!(
        scan(dir.path(), &["--json"]),
        root(dir.path(), 3 * d, d, "", &kids)
    );
}

#[test]
fn show_prints_what_scan_printed() {
    let f = fixture();
    let cache = tempfile::tempdir().unwrap();
    let mut flags: Vec<&[&str]> = vec![&[], &["--json", "--depth", "2"], &["--top", "3"]];
    if cfg!(target_os = "macos") {
        flags.push(&["-r"]);
    }
    for args in flags {
        let scanned = ok(disksweep(cache.path(), "scan", f.dir.path(), args));
        let shown = ok(disksweep(cache.path(), "show", f.dir.path(), args));
        assert_eq!(shown, scanned, "{args:?}");
    }

    // saves are keyed by the real path, but print the path asked for
    let link = tempfile::tempdir().unwrap();
    let root = link.path().join("root");
    symlink(f.dir.path(), &root).unwrap();
    let expected = f.expected.replacen(
        &f.dir.path().display().to_string(),
        &root.display().to_string(),
        1,
    );
    ok(disksweep(cache.path(), "scan", f.dir.path(), &[]));
    assert_eq!(ok(disksweep(cache.path(), "show", &root, &[])), expected);
}

#[test]
fn saved_scans_are_owner_only() {
    let f = fixture();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    ok(disksweep(&cache, "scan", f.dir.path(), &[]));
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&cache), 0o700);
    let modes: Vec<u32> = saved(&cache).iter().map(|p| mode(p)).collect();
    assert_eq!(modes, [0o600]);
}

#[test]
fn show_fails_without_a_usable_saved_scan() {
    let f = fixture();
    let cache = tempfile::tempdir().unwrap();
    let path = f.dir.path().display();
    let fails = |args: &[&str], message: String| {
        let out = disksweep(cache.path(), "show", f.dir.path(), args);
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(
            (stderr, out.stdout, out.status.code()),
            (message, vec![], Some(1))
        );
    };
    let none = format!("disksweep: no saved scan for {path}\n");
    fails(&[], none.clone());

    ok(disksweep(cache.path(), "scan", f.dir.path(), &[]));
    if cfg!(target_os = "macos") {
        let message = format!("disksweep: saved scan for {path} has no reclaimable sizes\n");
        fails(&["-r"], message);
    }

    // a file of another format version is ignored, not misread
    let [file] = &saved(cache.path())[..] else {
        panic!("not one saved file");
    };
    let mut bytes = fs::read(file).unwrap();
    bytes[4] += 1;
    fs::write(file, bytes).unwrap();
    fails(&[], none);
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
    let outs = READERS.map(|reader| scan(dir.path(), reader));
    remove_chain(&root);

    let path = dir.path().display();
    let expected = match own_bytes(dir.path()) {
        0 => format!("   4.0 KiB  {path}\n   4.0 KiB  d/\n"),
        4096 => format!("  16.0 MiB  {path}\n  16.0 MiB  d/\n   4.0 KiB  [files]\n"),
        d => panic!("unexpected dir size {d}"),
    };
    assert_eq!(outs, [expected.clone(), expected]);
}

/// ```text
/// root/
///   orig/f      1 MiB
///   clone/f     `cp -c` of orig/f: shares all its blocks
///   solo/f      1 MiB, unshared
/// ```
#[cfg(target_os = "macos")]
#[test]
fn reclaimable_excludes_blocks_shared_with_a_clone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for d in ["orig", "clone", "solo"] {
        fs::create_dir(root.join(d)).unwrap();
    }
    file(&root.join("orig/f"), 1 << 20);
    file(&root.join("solo/f"), 1 << 20);
    let cp = Command::new("/bin/cp")
        .arg("-c")
        .arg(root.join("orig/f"))
        .arg(root.join("clone/f"))
        .status()
        .unwrap();
    assert!(cp.success());
    // Both copies still allocate 1 MiB each, like du says, but APFS counts
    // the shared blocks as private to neither, so deleting either alone
    // frees 0 B. The root's 1 MiB is a sum over files, so it also leaves
    // out the shared blocks, which deleting the whole root would free.
    // APFS dirs allocate 0 B.
    let expected = [
        &format!("   3.0 MiB     1.0 MiB  {}\n", root.display()),
        "   1.0 MiB         0 B  clone/\n",
        "   1.0 MiB         0 B  orig/\n",
        "   1.0 MiB     1.0 MiB  solo/\n",
    ]
    .concat();
    assert_eq!(scan(root, &["-r"]), expected);
}

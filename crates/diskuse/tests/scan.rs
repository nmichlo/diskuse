#![allow(clippy::disallowed_methods)] // fixtures create files and run the binary

#[path = "../../diskuse-core/tests/common/mod.rs"]
mod common;

use common::{append, file, fixture, own_bytes};
use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Runs `diskuse <cmd> <path> <args>` with its saved scans in `cache`, so
/// tests never touch the user's cache dir.
fn diskuse(cache: &Path, cmd: &str, path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_diskuse"))
        .env("DISKUSE_CACHE_DIR", cache)
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

/// `diskuse scan <path> <args>`, saving into a throwaway cache.
fn scan(path: &Path, args: &[&str]) -> String {
    let cache = tempfile::tempdir().unwrap();
    ok(diskuse(cache.path(), "scan", path, args))
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

#[test]
fn scan_reports_exact_sizes() {
    let f = fixture();
    for reader in READERS {
        assert_eq!(scan(f.dir.path(), reader), f.expected, "{reader:?}");
    }
}

/// ```text
/// root/
///   f    16384
/// ```
///
/// `--si` prints powers of 1000.
#[test]
fn scan_prints_si_units() {
    let dir = tempfile::tempdir().unwrap();
    file(&dir.path().join("f"), 16384);
    let total = 16384 + own_bytes(dir.path());
    // 16.4 kB, or 20.5 kB with a 4096 B dir (ext4)
    let kb = format!("{:.1} kB", total as f64 / 1000.0);
    let root = dir.path().display();
    let expected = format!("{kb:>10}  {root}\n{kb:>10}  [files]\n");
    assert_eq!(scan(dir.path(), &["--si"]), expected);
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

/// ```text
/// root/
///   e/
///   n/a/, n/b/
///   locked/     mode 000, empty
/// ```
///
/// The macOS reader knows from a parent's listing which dirs are empty, but
/// an empty dir that cannot be opened is still denied. `locked/` is left out
/// when running as root, which can open anything.
#[test]
fn scan_counts_empty_dirs_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    for d in ["e", "n/a", "n/b"] {
        fs::create_dir_all(path.join(d)).unwrap();
    }
    let as_root = rustix::process::geteuid().is_root();
    let locked = path.join("locked");
    let _unlock = (!as_root).then(|| {
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        common::Unlock(vec![locked])
    });
    let d = own_bytes(path);
    let empty = |name: &str, extra: &str| leaf(name, d, d, &format!("{extra},\"children\":[]"));
    let n = format!(
        r#"{{"name":"n","size":{},"own":{d},"children":[{},{}]}}"#,
        3 * d,
        leaf("a", d, d, ""),
        leaf("b", d, d, ""),
    );
    let mut kids = vec![empty("e", "")];
    if !as_root {
        kids.push(empty("locked", r#","denied":"EACCES""#));
    }
    // largest first, then by name
    match d {
        0 => kids.push(n),
        _ => kids.insert(0, n),
    }
    let (size, partial) = match as_root {
        true => (5 * d, ""),
        false => (6 * d, r#","partial":true"#),
    };
    let expected = root(path, size, d, partial, &kids);
    for reader in READERS {
        let args = [reader, &["--json", "--depth", "2"]].concat();
        assert_eq!(scan(path, &args), expected, "{reader:?}");
    }
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
        let scanned = ok(diskuse(cache.path(), "scan", f.dir.path(), args));
        let shown = ok(diskuse(cache.path(), "show", f.dir.path(), args));
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
    ok(diskuse(cache.path(), "scan", f.dir.path(), &[]));
    assert_eq!(ok(diskuse(cache.path(), "show", &root, &[])), expected);
}

#[test]
fn saved_scans_are_owner_only() {
    let f = fixture();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    ok(diskuse(&cache, "scan", f.dir.path(), &[]));
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
        let out = diskuse(cache.path(), "show", f.dir.path(), args);
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(
            (stderr, out.stdout, out.status.code()),
            (message, vec![], Some(1))
        );
    };
    let none = format!("diskuse: no saved scan for {path}\n");
    fails(&[], none.clone());

    ok(diskuse(cache.path(), "scan", f.dir.path(), &[]));
    if cfg!(target_os = "macos") {
        let message = format!("diskuse: saved scan for {path} has no reclaimable sizes\n");
        fails(&["-r"], message);
    }

    // a file of another format version is ignored, not misread
    let [file] = &saved(cache.path())[..] else {
        panic!("not one saved file");
    };
    let bytes = fs::read(file).unwrap();
    let mut other = bytes.clone();
    other[4] += 1;
    fs::write(file, other).unwrap();
    fails(&[], none.clone());

    // so is a damaged one: one bit flipped anywhere, here at places picked
    // by a seeded xorshift, so every run flips the same bits
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..32 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bit = (x % (bytes.len() as u64 * 8)) as usize;
        let mut damaged = bytes.clone();
        damaged[bit / 8] ^= 1 << (bit % 8);
        fs::write(file, damaged).unwrap();
        fails(&[], none.clone());
    }
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

#[cfg(target_os = "macos")]
#[test]
fn reclaimable_excludes_blocks_shared_with_a_clone() {
    let dir = common::clones();
    let root = dir.path();
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

/// Creates `a/b/new` (12288) in the fixture, deletes `a/f1`, and grows
/// `big/f` by 4096 in place. Returns what `big/f` then allocates.
fn change(f: &common::Fixture) -> u64 {
    let root = f.dir.path();
    file(&root.join("a/b/new"), 12288);
    fs::remove_file(root.join("a/f1")).unwrap();
    // no dir entry changes, which a replay of dir changes misses
    append(&root.join("big/f"), 4096)
}

/// The `--json` output of the fixture after [`change`], which left `big/f`
/// allocating `big_file` bytes.
fn changed_json(f: &common::Fixture, big_file: u64) -> String {
    let d = f.dir_bytes;
    let big = big_file + d;
    let a = 8192 + 8192 + 12288 + 12288 + 3 * d;
    let a_own = 8192 + d;
    let own = 8192 + f.sym_bytes + d;
    let mut kids = vec![leaf("big", big, big, ""), leaf("empty", d, d, "")];
    match rustix::process::geteuid().is_root() {
        true => {
            kids.insert(1, leaf("a", a, a_own, ""));
            root(f.dir.path(), own + big + a + d, own, "", &kids)
        }
        false => {
            let partial = r#","partial":true"#;
            kids.insert(1, leaf("a", a + d, a_own, partial));
            kids.push(leaf("locked", d, d, r#","denied":"EACCES""#));
            root(f.dir.path(), own + big + a + 3 * d, own, partial, &kids)
        }
    }
}

/// A saved scan is never used for a new one: changes made while
/// diskuse is not running always show, as no record of them is trusted.
#[test]
fn scan_never_starts_from_a_saved_scan() {
    let f = fixture();
    let cache = tempfile::tempdir().unwrap();
    ok(diskuse(cache.path(), "scan", f.dir.path(), &[]));
    let big_file = change(&f);
    let out = ok(diskuse(cache.path(), "scan", f.dir.path(), &["--json"]));
    assert_eq!(out, changed_json(&f, big_file));
}

/// A damaged saved scan, or one of another version, is not shown.
#[test]
fn show_ignores_a_damaged_saved_scan() {
    let corruptions: [fn(&mut Vec<u8>); 2] = [
        |bytes| bytes[4] += 1,
        |bytes| {
            bytes.pop();
        },
    ];
    for (i, corrupt) in corruptions.into_iter().enumerate() {
        let f = fixture();
        let cache = tempfile::tempdir().unwrap();
        ok(diskuse(cache.path(), "scan", f.dir.path(), &[]));
        let [saved] = &saved(cache.path())[..] else {
            panic!("not one saved file");
        };
        let mut bytes = fs::read(saved).unwrap();
        corrupt(&mut bytes);
        fs::write(saved, bytes).unwrap();
        let out = diskuse(cache.path(), "show", f.dir.path(), &[]);
        let err = format!("diskuse: no saved scan for {}\n", f.dir.path().display());
        assert_eq!(
            (out.status.code(), String::from_utf8(out.stderr).unwrap()),
            (Some(1), err),
            "corruption {i}"
        );
    }
}

/// After the first line of a scan that was stopped before it was done.
const INCOMPLETE: &str = "  (incomplete: scan stopped)";

/// A scan stopped after any number of dirs is printed and saved marked
/// incomplete, and the next scan is a full one.
#[test]
fn a_stopped_scan_is_marked_and_the_next_one_is_full() {
    let f = fixture();
    let root = f.dir.path();
    let full = ["--json", "--depth", "9", "--top", "9"];
    let expected = scan(root, &full);
    // `a/`, `a/b/`, `a/b/c/`, `big/`, `empty/`, and both `locked/`
    let dirs = match rustix::process::geteuid().is_root() {
        true => 5,
        false => 7,
    };
    for k in 0..=dirs {
        let cache = tempfile::tempdir().unwrap();
        let stopped = ok(diskuse(
            cache.path(),
            "scan",
            root,
            &["--stop-after", &k.to_string()],
        ));
        let first = stopped.lines().next().unwrap();
        assert_eq!(first.ends_with(INCOMPLETE), k < dirs, "{k}: {stopped}");
        if k == 0 {
            // the root alone was listed
            let files = common::kib(8192 + f.sym_bytes + f.dir_bytes);
            let shown = format!(
                "{files:>10}  {}{INCOMPLETE}\n{files:>10}  [files]\n",
                root.display()
            );
            assert_eq!(ok(diskuse(cache.path(), "show", root, &[])), shown);
        }
        let finished = ok(diskuse(cache.path(), "scan", root, &full));
        assert_eq!(finished, expected, "{k}");
        assert_eq!(
            ok(diskuse(cache.path(), "show", root, &[])),
            f.expected,
            "{k}"
        );
    }
}

/// The access time of `path`, which listing it moves when older than its
/// modification time, as after creating entries in it.
fn accessed(path: &Path) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(path).unwrap();
    (m.atime(), m.atime_nsec())
}

/// SIGTERM stops a running scan, which prints and saves what it found,
/// marked incomplete. The signal is sent once the scan has listed the root,
/// so it can stop, and the tree is made larger until the scan is still
/// running then.
#[test]
fn sigterm_stops_a_scan() {
    use rustix::process::{Pid, Signal, kill_process};
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    let args = ["--json", "--depth", "2"];
    // top dirs, each with 64 subdirs
    for top in [64, 256, 1024] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        for i in 0..top {
            for j in 0..64 {
                fs::create_dir_all(path.join(format!("{i}/{j}"))).unwrap();
            }
        }
        let cache = tempfile::tempdir().unwrap();
        let before = accessed(path);
        let mut child = Command::new(env!("CARGO_BIN_EXE_diskuse"))
            .env("DISKUSE_CACHE_DIR", cache.path())
            .arg("scan")
            .arg(path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        while child.try_wait().unwrap().is_none() {
            if accessed(path) != before {
                let pid = Pid::from_raw(child.id() as i32).unwrap();
                kill_process(pid, Signal::TERM).unwrap();
                break;
            }
            std::thread::yield_now();
        }
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            out.status.signal(),
            None,
            "ended by the signal, not stopped"
        );
        let stopped = String::from_utf8(out.stdout).unwrap();
        // done before the signal landed
        if !stopped.contains(r#""incomplete":true"#) {
            continue;
        }
        assert_eq!(out.status.code(), Some(128 + 15));
        assert_eq!(ok(diskuse(cache.path(), "show", path, &args)), stopped);
        return;
    }
    panic!("every scan was done before SIGTERM landed");
}

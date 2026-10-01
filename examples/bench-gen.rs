#![allow(clippy::disallowed_methods)] // the generator creates files

//! Generates a benchmark dataset, the same bytes every time for an ID.
//!
//! ```text
//! cargo run --release --example bench-gen -- <ID> <DIR>
//!
//! S1  binary tree, 12 levels (4,095 dirs), 100 files per dir
//! S2  one dir, 1,000,000 files
//! S3  1,000 dirs x 1,000 dirs, 1 file each
//! S4  a chain of 4,096 nested dirs, 1 file each
//! S5  100,000 files, each hard-linked into 10 dirs
//! S6  1,000 files of 1 MiB, each cloned once (macOS)
//! ```
//!
//! DIR is the dataset root and counts as one of its dirs.

use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags, mkdirat, openat, renameat};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};

/// splitmix64: tiny, seedable, and good enough to pick file sizes.
struct Rng(u64);

impl Rng {
    fn new(id: &str) -> Self {
        // FNV-1a, so each ID gets its own sequence
        let seed = id.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }

    /// Mostly small: 85% empty, 15% 1 B-16 KiB, 1 in 2,000 16 KiB-1 MiB.
    /// Every non-empty file allocates at least one 4 KiB block, so a 1M
    /// file dataset must be mostly empty files to stay near 2 GB.
    fn size(&mut self) -> usize {
        let n = match self.next() % 2000 {
            0 => self.range(16 << 10, 1 << 20),
            1..300 => self.range(1, 16 << 10),
            _ => 0,
        };
        n as usize
    }
}

#[derive(Default)]
struct Counts {
    dirs: u64,
    files: u64,
    links: u64,
    clones: u64,
}

struct Gen {
    rng: Rng,
    data: Vec<u8>,
    counts: Counts,
}

impl Gen {
    fn dir(&mut self, path: &Path) {
        fs::create_dir(path).unwrap();
        self.counts.dirs += 1;
    }

    fn file(&mut self, path: &Path, len: usize) {
        File::create_new(path)
            .unwrap()
            .write_all(&self.data[..len])
            .unwrap();
        self.counts.files += 1;
    }

    fn random_file(&mut self, path: &Path) {
        let len = self.rng.size();
        self.file(path, len);
    }

    fn files(&mut self, dir: &Path, n: usize) {
        for i in 0..n {
            self.random_file(&dir.join(format!("f{i}")));
        }
    }

    fn tree(&mut self, dir: &Path, levels: u32) {
        self.files(dir, 100);
        if levels > 1 {
            for child in ["a", "b"] {
                let sub = dir.join(child);
                self.dir(&sub);
                self.tree(&sub, levels - 1);
            }
        }
    }

    /// Nests each new level around the old top, all at depth 1: a mkdir at
    /// depth N walks N path components (O(N) on macOS), and the full path
    /// is far beyond PATH_MAX.
    fn chain(&mut self, root: &Path, depth: usize) {
        let fd = |dir: &OwnedFd, name: &str| {
            openat(dir, name, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap()
        };
        let root = openat(
            rustix::fs::CWD,
            root,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .unwrap();
        let mode = Mode::from_raw_mode(0o755);
        for level in 0..depth {
            let name = if level == 0 { "d" } else { "n" };
            mkdirat(&root, name, mode).unwrap();
            let new = fd(&root, name);
            let len = self.rng.size();
            let f = openat(
                &new,
                "f",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                Mode::from_raw_mode(0o644),
            )
            .unwrap();
            File::from(f).write_all(&self.data[..len]).unwrap();
            if level > 0 {
                renameat(&root, "d", &new, "d").unwrap();
                renameat(&root, "n", &root, "d").unwrap();
            }
            self.counts.dirs += 1;
            self.counts.files += 1;
        }
    }

    fn hard_links(&mut self, root: &Path, files: usize, dirs: usize) {
        let dirs: Vec<_> = (0..dirs).map(|i| root.join(format!("l{i}"))).collect();
        for d in &dirs {
            self.dir(d);
        }
        for i in 0..files {
            let name = format!("f{i}");
            self.random_file(&dirs[0].join(&name));
            for d in &dirs[1..] {
                fs::hard_link(dirs[0].join(&name), d.join(&name)).unwrap();
                self.counts.links += 1;
            }
        }
    }

    /// `cp -c` makes APFS clones: the copy shares all the original's blocks.
    fn clones(&mut self, root: &Path, files: usize) {
        let (orig, clone) = (root.join("orig"), root.join("clone"));
        self.dir(&orig);
        self.dir(&clone);
        let names: Vec<_> = (0..files).map(|i| orig.join(format!("f{i}"))).collect();
        for name in &names {
            self.file(name, 1 << 20);
        }
        let ok = Command::new("/bin/cp")
            .arg("-c")
            .args(&names)
            .arg(&clone)
            .status()
            .unwrap()
            .success();
        assert!(ok, "cp -c failed");
        self.counts.clones += files as u64;
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [id, dir] = args.as_slice() else {
        eprintln!("usage: bench-gen <S1..S6> <DIR>");
        return ExitCode::FAILURE;
    };
    let known = ["S1", "S2", "S3", "S4", "S5"].contains(&id.as_str())
        || (id == "S6" && cfg!(target_os = "macos"));
    if !known {
        eprintln!("bench-gen: unknown dataset {id} on this OS");
        return ExitCode::FAILURE;
    }
    let root = Path::new(dir);
    fs::create_dir_all(root).unwrap();
    if fs::read_dir(root).unwrap().next().is_some() {
        eprintln!("bench-gen: {dir} is not empty");
        return ExitCode::FAILURE;
    }
    let mut g = Gen {
        rng: Rng::new(id),
        data: vec![0xab; 1 << 20],
        counts: Counts {
            dirs: 1,
            ..Counts::default()
        },
    };
    match id.as_str() {
        "S1" => g.tree(root, 12),
        "S2" => g.files(root, 1_000_000),
        "S3" => {
            for i in 0..1000 {
                let a = root.join(format!("d{i}"));
                g.dir(&a);
                for j in 0..1000 {
                    let b = a.join(format!("d{j}"));
                    g.dir(&b);
                    g.random_file(&b.join("f"));
                }
            }
        }
        "S4" => g.chain(root, 4096),
        "S5" => g.hard_links(root, 100_000, 10),
        _ => g.clones(root, 1000),
    }
    let c = &g.counts;
    println!(
        "{id}: {} dirs, {} files, {} extra hard links, {} clones",
        c.dirs, c.files, c.links, c.clones
    );
    ExitCode::SUCCESS
}

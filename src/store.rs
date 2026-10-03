//! Saved scans, one file per scanned root, so `show` can print a result
//! without scanning, and the browser can show one while it scans afresh.
//! A saved scan is never brought up to date ([`crate::watch`] says why).
//! The only module that writes files, and only inside [`CacheDir`].
//!
//! A file is a fixed header, then an [rkyv] archive of [`Body`], which
//! `show` reads in place ([`SavedTree`]), then a CRC-32 of all the bytes
//! before it. The format is versioned, with no migrations: a file of another
//! version is ignored, and the next scan replaces it. So is a damaged one.
//! The CRC catches any one flipped bit and all but 1 in 2^32 other damage,
//! and rkyv's checks and [`valid`] keep a file that still passes from making
//! [`ReadTree`] index out of bounds.
//!
//! ```text
//! magic     b"DSWP"
//! version   u8 = 5
//! flags     u8: bit 0 = scanned with reclaimable sizes, bit 1 = stopped
//! 0         u16, so the archive starts aligned, at byte 8
//! archive   rkyv, of `Body`
//! crc       u32: CRC-32 of every byte before it
//! ```
//!
//! Numbers are little-endian, in the archive too.
//!
//! [rkyv]: https://rkyv.org

use crate::sys;
use crate::tree::{LargeFile, Links, ReadTree, Record, Tree};
use rkyv::rancor::Failure;
use rkyv::util::AlignedVec;
use rkyv::with::AsVec;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

const MAGIC: &[u8; 4] = b"DSWP";
const VERSION: u8 = 5;
const RECLAIMABLE: u8 = 1 << 0;
const STOPPED: u8 = 1 << 1;
const HEADER_LEN: usize = 8;

/// The directory saved scans and picks live in, the only place the store
/// writes.
#[derive(Clone)]
pub struct CacheDir(PathBuf);

/// A scan read back by [`CacheDir::load`], or in place by
/// [`SavedFile::check`].
pub struct Saved<T = Tree> {
    pub tree: T,
    /// Scanned with [`crate::ScanOptions::reclaimable`].
    pub reclaimable: bool,
    /// When the scan was saved: the file's modification time.
    pub modified: SystemTime,
}

impl CacheDir {
    /// The directory itself, which [`crate::update`] ignores changes in.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// The cache dir at `dir`, as for a test. disksweep itself only uses
    /// [`CacheDir::from_env`].
    pub fn at(dir: PathBuf) -> Self {
        Self(dir)
    }

    /// `$DISKSWEEP_CACHE_DIR` if set, else `~/Library/Caches/disksweep` on
    /// macOS, and `$XDG_CACHE_HOME/disksweep` or `~/.cache/disksweep`
    /// elsewhere. Not created until a save.
    pub fn from_env() -> io::Result<Self> {
        let var = |k| std::env::var_os(k).filter(|v| !v.is_empty());
        if let Some(dir) = var("DISKSWEEP_CACHE_DIR") {
            return Ok(Self(dir.into()));
        }
        // the XDG spec says to ignore a relative path
        #[cfg(not(target_os = "macos"))]
        if let Some(xdg) = var("XDG_CACHE_HOME").filter(|v| Path::new(v).is_absolute()) {
            return Ok(Self(Path::new(&xdg).join("disksweep")));
        }
        let home = var("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
        #[cfg(target_os = "macos")]
        return Ok(Self(Path::new(&home).join("Library/Caches/disksweep")));
        #[cfg(not(target_os = "macos"))]
        Ok(Self(Path::new(&home).join(".cache/disksweep")))
    }

    /// Saves `tree`, the scan of `root`, over any earlier save of it. Readers
    /// see the old file or the new one, never half of one.
    pub fn save(&self, root: &Path, tree: &Tree, reclaimable: bool) -> io::Result<()> {
        let (canonical, stem) = key(root)?;
        let bytes = encode(&canonical, tree, reclaimable);
        self.write(&format!("{stem}.scan"), &bytes)
    }

    /// Saves the paths picked below `root` ([`crate::Browser`]), each
    /// relative to it, over any earlier save of them.
    pub fn save_picks(&self, root: &Path, picks: &[Vec<u8>]) -> io::Result<()> {
        let (_, stem) = key(root)?;
        // a name may hold any byte but NUL
        let bytes: Vec<u8> = picks
            .iter()
            .flat_map(|p| [&p[..], b"\0"].concat())
            .collect();
        self.write(&format!("{stem}.picks"), &bytes)
    }

    /// The paths picked below `root`, as [`CacheDir::save_picks`] saved
    /// them, or none.
    pub fn load_picks(&self, root: &Path) -> io::Result<Vec<Vec<u8>>> {
        let (_, stem) = key(root)?;
        match std::fs::read(self.0.join(format!("{stem}.picks"))) {
            Ok(bytes) => Ok((bytes.split(|&b| b == 0))
                .filter(|p| !p.is_empty())
                .map(<[u8]>::to_vec)
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Writes file `name` in the cache dir, over any earlier one. Readers
    /// see the old file or the new one, never half of one.
    fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        // owner-only, like the files: they list directory names. The store
        // may create its own cache dir.
        #[allow(clippy::disallowed_methods)]
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.0)?;
        let path = self.0.join(name);
        let tmp = self.0.join(format!("{name}.{}.tmp", std::process::id()));
        // the store may write and rename its own files in the cache dir
        #[allow(clippy::disallowed_methods)]
        let saved = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(bytes))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if saved.is_err() {
            // the disk is often full when disksweep runs, so leave no
            // partial file behind. The store may delete its own temp file.
            #[allow(clippy::disallowed_methods)]
            let _ = std::fs::remove_file(&tmp);
        }
        saved
    }

    /// The saved file of `root`, unchecked: see [`SavedFile::check`].
    /// `None` if there is none.
    pub fn read(&self, root: &Path) -> io::Result<Option<SavedFile>> {
        let (canonical, stem) = key(root)?;
        let file = match File::open(self.0.join(format!("{stem}.scan"))) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(SavedFile {
            modified: file.metadata()?.modified()?,
            // page-aligned, so the archive is read in place
            bytes: sys::map(&file)?,
            canonical,
            root: root.into(),
        }))
    }

    /// The saved scan of `root`, with `root` as the root's name. `None` if
    /// there is none, or it is damaged or of another version.
    pub fn load(&self, root: &Path) -> io::Result<Option<Saved>> {
        let file = self.read(root)?;
        Ok(file.as_ref().and_then(SavedFile::check).map(|s| Saved {
            tree: s.tree.to_tree(),
            reclaimable: s.reclaimable,
            modified: s.modified,
        }))
    }
}

/// A saved file as read by [`CacheDir::read`], to check and read in place.
pub struct SavedFile {
    bytes: memmap2::Mmap,
    canonical: PathBuf,
    /// The root as asked for, the root's name.
    root: PathBuf,
    modified: SystemTime,
}

impl SavedFile {
    /// The scan in the file, read in place. `None` if the file is damaged
    /// or of another version.
    pub fn check(&self) -> Option<Saved<SavedTree<'_>>> {
        let (data, crc) = self.bytes.split_last_chunk::<4>()?;
        let (header, archive) = data.split_first_chunk::<HEADER_LEN>()?;
        if header[..4] != *MAGIC || header[4] != VERSION {
            return None;
        }
        if crc32fast::hash(data) != u32::from_le_bytes(*crc) {
            return None;
        }
        let body = rkyv::access::<ArchivedBody<'_>, Failure>(archive).ok()?;
        // guards against FNV collisions
        if body.root[..] != *self.canonical.as_os_str().as_bytes() || !valid(body) {
            return None;
        }
        Some(Saved {
            tree: SavedTree {
                body,
                root: self.root.as_os_str().as_bytes(),
                stopped: header[5] & STOPPED != 0,
            },
            reclaimable: header[5] & RECLAIMABLE != 0,
            modified: self.modified,
        })
    }
}

/// A saved scan, read in place from its [`SavedFile`].
pub struct SavedTree<'a> {
    body: &'a ArchivedBody<'a>,
    root: &'a [u8],
    stopped: bool,
}

impl SavedTree<'_> {
    /// An owned copy, which updates can change.
    fn to_tree(&self) -> Tree {
        let body = self.body;
        let names = boxcar::Vec::with_capacity(body.ends.len());
        for id in 0..body.ends.len() {
            names.push(self.name(id as u32).into());
        }
        let n = body.records.len();
        Tree {
            records: (0..n).map(|id| self.record(id as u32)).collect(),
            names: Arc::new(names),
            largest: (self.largest())
                .map(|(bytes, dir, name)| LargeFile {
                    bytes,
                    dir,
                    name: name.into(),
                })
                .collect(),
            links: Links::new(),
            since: 0,
            stopped: self.stopped,
        }
    }
}

impl ReadTree for SavedTree<'_> {
    fn len(&self) -> usize {
        self.body.records.len()
    }

    fn record(&self, id: u32) -> Record {
        let r = &self.body.records[id as usize];
        Record {
            parent: r.parent.to_native(),
            name: r.name.to_native(),
            flags: r.flags.to_native(),
            errno: r.errno.to_native(),
            own: r.own.to_native(),
            own_private: r.own_private.to_native(),
        }
    }

    fn name(&self, name: u32) -> &[u8] {
        let ends = &self.body.ends;
        match name as usize {
            0 => self.root,
            id => {
                &self.body.names[ends[id - 1].to_native() as usize..ends[id].to_native() as usize]
            }
        }
    }

    fn largest(&self) -> impl Iterator<Item = (u64, u32, &[u8])> {
        let files = self.body.largest.iter();
        files.map(|f| (f.bytes.to_native(), f.dir.to_native(), &f.name[..]))
    }

    fn stopped(&self) -> bool {
        self.stopped
    }
}

/// The archived part of a saved file.
#[derive(rkyv::Archive, rkyv::Serialize)]
struct Body<'a> {
    /// The canonical root.
    #[rkyv(with = AsVec)]
    root: &'a [u8],
    #[rkyv(with = AsVec)]
    records: &'a [Record],
    /// Every name but the root's, back to back.
    names: Vec<u8>,
    /// Where each name ends in `names`, by name id. Id 0 is the root's,
    /// which [`SavedFile::check`] sets to the path asked for, so 0.
    ends: Vec<u32>,
    /// In no particular order.
    #[rkyv(with = AsVec)]
    largest: &'a [LargeFile],
}

/// The canonical root, as the scan follows a symlinked root, and the stem
/// of its files' names: `<volume id>-<FNV-1a of the canonical root>`. The
/// volume id tells apart disks mounted at the same path.
fn key(root: &Path) -> io::Result<(PathBuf, String)> {
    let canonical = std::fs::canonicalize(root)?;
    let volume = sys::volume_id(&canonical)?;
    let hash = fnv1a(canonical.as_os_str().as_bytes());
    Ok((canonical, format!("{volume:032x}-{hash:016x}")))
}

/// FNV-1a, 64 bit. std's hasher may change between Rust versions, which
/// would orphan every saved scan.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn encode(canonical: &Path, tree: &Tree, reclaimable: bool) -> AlignedVec {
    let mut names = Vec::new();
    let mut ends = Vec::with_capacity(tree.names.count());
    ends.push(0);
    for (_, name) in tree.names.iter().skip(1) {
        names.extend_from_slice(name);
        // each name is stored once, so even a whole disk's take a few MB
        ends.push(u32::try_from(names.len()).expect("over 4 GiB of names"));
    }
    let body = Body {
        root: canonical.as_os_str().as_bytes(),
        records: &tree.records,
        names,
        ends,
        largest: &tree.largest,
    };
    // a little over the file's length, so it never grows, which would copy
    // it all
    let largest: usize = (tree.largest.iter()).map(|f| 32 + f.name.len()).sum();
    let len = HEADER_LEN
        + size_of_val(body.root)
        + size_of_val(body.records)
        + size_of_val(&body.names[..])
        + size_of_val(&body.ends[..])
        + largest
        + 256;
    let mut out = AlignedVec::with_capacity(len);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    let flags = [(reclaimable, RECLAIMABLE), (tree.stopped, STOPPED)];
    out.push(flags.iter().filter(|f| f.0).map(|f| f.1).sum());
    out.extend_from_slice(&[0; 2]);
    // the archive's positions count from the file's start, which is
    // aligned, and serializing to memory cannot fail
    let mut out = rkyv::api::high::to_bytes_in::<_, Failure>(&body, out).unwrap();
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Whether every id in `body` is in bounds, and parents come before their
/// children, as [`Tree`] needs.
fn valid(body: &ArchivedBody<'_>) -> bool {
    let (records, ends) = (&body.records, &body.ends);
    let Some((first, rest)) = records.split_first() else {
        return false;
    };
    let below = |id: u32, len: usize| (id as usize) < len;
    first.parent.to_native() == Record::NO_PARENT
        && first.name == 0
        && rest.iter().enumerate().all(|(i, r)| {
            // a parent always comes before its children
            (r.parent.to_native() as usize) <= i
                && r.name != 0
                && below(r.name.to_native(), ends.len())
        })
        && ends.first().is_some_and(|&e| e == 0)
        && ends.windows(2).all(|w| w[0] <= w[1])
        && ends
            .last()
            .is_some_and(|&e| e.to_native() as usize == body.names.len())
        && (body.largest.iter()).all(|f| below(f.dir.to_native(), records.len()))
}

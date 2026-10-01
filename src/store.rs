//! Saved scans, one file per scanned root, so `show` can print a result
//! without scanning, and the next `scan` can start from it. The only module
//! that writes files, and only inside [`CacheDir`].
//!
//! The format is little-endian and versioned, with no migrations: a file of
//! another version is ignored, and the next scan replaces it.
//!
//! ```text
//! magic     b"DSWP"
//! version   u8 = 2
//! flags     u8: bit 0 = scanned with reclaimable sizes
//! event_id  u64: the tree has every change up to this event, 0 if the OS
//!           keeps no record of changes
//! store     u128: the id of that record, 0 if none
//! root      u32 len + bytes: the canonical root
//! records   u32 count, then 32 bytes each: parent u32, name u32,
//!           flags u32, errno u16, 0 u16, own u64, own_private u64
//! names     u32 count, then u16 len + bytes each, for name ids from 1;
//!           id 0 is the root's, which `load` sets to the path asked for
//! largest   u32 count, then bytes u64, dir u32, u16 len + name bytes each
//! links     u32 count, then dev u64, ino u64, dir u32 each: the dir that
//!           counts each multiply-linked file
//! ```

use crate::sys;
use crate::tree::{LargeFile, Links, Record, Since, Tree};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

const MAGIC: &[u8; 4] = b"DSWP";
const VERSION: u8 = 2;
const RECLAIMABLE: u8 = 1 << 0;
const RECORD_LEN: usize = 32;

/// The directory saved scans live in. Only built from the environment, so
/// the store cannot write anywhere else.
#[derive(Clone)]
pub struct CacheDir(PathBuf);

/// A scan read back by [`CacheDir::load`].
pub struct Saved {
    pub tree: Tree,
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
        let (canonical, name) = key(root)?;
        let bytes = encode(&canonical, tree, reclaimable);
        // owner-only, like the files: they list directory names. The store
        // may create its own cache dir.
        #[allow(clippy::disallowed_methods)]
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.0)?;
        let path = self.0.join(&name);
        let tmp = self.0.join(format!("{name}.{}.tmp", std::process::id()));
        // the store may write and rename its own files in the cache dir
        #[allow(clippy::disallowed_methods)]
        let saved = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(&bytes))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if saved.is_err() {
            // the disk is often full when disksweep runs, so leave no
            // partial file behind. The store may delete its own temp file.
            #[allow(clippy::disallowed_methods)]
            let _ = std::fs::remove_file(&tmp);
        }
        saved
    }

    /// The saved scan of `root`, with `root` as the root's name. `None` if
    /// there is none, or it is malformed or of another version.
    pub fn load(&self, root: &Path) -> io::Result<Option<Saved>> {
        let (canonical, name) = key(root)?;
        let mut file = match File::open(self.0.join(name)) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let modified = file.metadata()?.modified()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(decode(&bytes, &canonical, root, modified))
    }
}

/// The canonical root, as the scan follows a symlinked root, and its file
/// name: `<volume id>-<FNV-1a of the canonical root>.scan`. The volume id
/// tells apart disks mounted at the same path.
fn key(root: &Path) -> io::Result<(PathBuf, String)> {
    let canonical = std::fs::canonicalize(root)?;
    let volume = sys::volume_id(&canonical)?;
    let hash = fnv1a(canonical.as_os_str().as_bytes());
    Ok((canonical, format!("{volume:032x}-{hash:016x}.scan")))
}

/// FNV-1a, 64 bit. std's hasher may change between Rust versions, which
/// would orphan every saved scan.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn encode(canonical: &Path, tree: &Tree, reclaimable: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + tree.records.len() * RECORD_LEN);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(if reclaimable { RECLAIMABLE } else { 0 });
    out.extend_from_slice(&tree.since.id.to_le_bytes());
    out.extend_from_slice(&tree.since.store.to_le_bytes());
    let root = canonical.as_os_str().as_bytes();
    out.extend_from_slice(&len32(root.len()).to_le_bytes());
    out.extend_from_slice(root);

    out.extend_from_slice(&len32(tree.records.len()).to_le_bytes());
    for r in &tree.records {
        out.extend_from_slice(&r.parent.to_le_bytes());
        out.extend_from_slice(&r.name.to_le_bytes());
        out.extend_from_slice(&r.flags.to_le_bytes());
        out.extend_from_slice(&r.errno.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&r.own.to_le_bytes());
        out.extend_from_slice(&r.own_private.to_le_bytes());
    }
    // name 0 is the root as given, which `show` replaces
    out.extend_from_slice(&len32(tree.names.count() - 1).to_le_bytes());
    for (_, name) in tree.names.iter().skip(1) {
        put16(&mut out, name);
    }
    out.extend_from_slice(&len32(tree.largest.len()).to_le_bytes());
    for f in &tree.largest {
        out.extend_from_slice(&f.bytes.to_le_bytes());
        out.extend_from_slice(&f.dir.to_le_bytes());
        put16(&mut out, &f.name);
    }
    out.extend_from_slice(&len32(tree.links.len()).to_le_bytes());
    for (&(dev, ino), &dir) in &tree.links {
        out.extend_from_slice(&dev.to_le_bytes());
        out.extend_from_slice(&ino.to_le_bytes());
        out.extend_from_slice(&dir.to_le_bytes());
    }
    out
}

fn len32(n: usize) -> u32 {
    // record and name ids are u32, and paths are far shorter
    u32::try_from(n).unwrap()
}

/// Writes a name with a u16 length. File names are a few hundred bytes at
/// most on every filesystem.
fn put16(out: &mut Vec<u8>, name: &[u8]) {
    out.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    out.extend_from_slice(name);
}

/// Parses a saved file, checking every id so a damaged file cannot make
/// [`Tree`] index out of bounds.
fn decode(bytes: &[u8], canonical: &Path, root: &Path, modified: SystemTime) -> Option<Saved> {
    let mut b = Bytes(bytes);
    if b.take(4)? != MAGIC || b.u8()? != VERSION {
        return None;
    }
    let flags = b.u8()?;
    let since = Since {
        id: b.u64()?,
        store: b.u128()?,
    };
    let len = b.u32()? as usize;
    // guards against FNV collisions
    if b.take(len)? != canonical.as_os_str().as_bytes() {
        return None;
    }

    let count = b.u32()? as usize;
    let mut raw = Bytes(b.take(count.checked_mul(RECORD_LEN)?)?);
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let (parent, name, flags, errno) = (raw.u32()?, raw.u32()?, raw.u32()?, raw.u16()?);
        let _padding = raw.u16()?;
        records.push(Record {
            parent,
            name,
            flags,
            errno,
            own: raw.u64()?,
            own_private: raw.u64()?,
        });
    }

    let count = b.u32()? as usize;
    let names = boxcar::Vec::with_capacity(1 + count.min(b.0.len() / 2));
    names.push(root.as_os_str().as_bytes().into());
    for _ in 0..count {
        let len = b.u16()? as usize;
        names.push(b.take(len)?.into());
    }

    let count = b.u32()? as usize;
    let mut largest = Vec::with_capacity(count.min(b.0.len() / 14));
    for _ in 0..count {
        let bytes = b.u64()?;
        let dir = b.u32()?;
        let len = b.u16()? as usize;
        let name = b.take(len)?.into();
        largest.push(LargeFile { bytes, dir, name });
    }

    let count = b.u32()? as usize;
    let mut links = Links::with_capacity(count.min(b.0.len() / 20));
    for _ in 0..count {
        let key = (b.u64()?, b.u64()?);
        links.insert(key, b.u32()?);
    }

    let (first, rest) = records.split_first()?;
    let valid = b.0.is_empty()
        && first.parent == Record::NO_PARENT
        && first.name == 0
        && rest.iter().enumerate().all(|(i, r)| {
            // a parent always comes before its children
            (r.parent as usize) <= i && r.name != 0 && (r.name as usize) < names.count()
        })
        && largest.iter().all(|f| (f.dir as usize) < records.len())
        && links.values().all(|&dir| (dir as usize) < records.len());
    valid.then_some(Saved {
        tree: Tree {
            records,
            names: Arc::new(names),
            largest,
            links,
            since,
        },
        reclaimable: flags & RECLAIMABLE != 0,
        modified,
    })
}

/// A cursor over a saved file. Every read is `None` past the end.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn u128(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }
}

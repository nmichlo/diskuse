//! The native half of the `diskuse` Python package, `diskuse._diskuse`.
//! `python/diskuse/__init__.py` wraps it; only that module is public.
//!
//! Every export is one Python cannot do cheaply itself: the scan, the
//! per-folder totals, the largest files kept during the scan, and the
//! tree as columns. A `Dir` is the tree and a record id, so walking it
//! never looks a path up.

use diskuse::{ChildIndex, ReadTree, Record, ScanOptions, Totals, Tree};
use pyo3::exceptions::PyOSError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A finished tree with what its queries need, shared by every `Dir`.
struct Data {
    tree: Tree,
    totals: Totals,
    index: ChildIndex,
}

impl Data {
    fn new(tree: Tree) -> Arc<Self> {
        let (totals, index) = (tree.totals(), tree.child_index());
        Arc::new(Self {
            tree,
            totals,
            index,
        })
    }
}

/// A scan of a folder: every folder's total, and the largest files.
#[pyclass(frozen, name = "Tree", module = "diskuse")]
struct PyTree(Arc<Data>);

/// One folder of a [`PyTree`].
#[pyclass(frozen, name = "Dir", module = "diskuse")]
struct PyDir {
    data: Arc<Data>,
    id: u32,
}

/// Scans `path` with `threads` threads, or as many as help, with the GIL
/// released.
#[pyfunction]
#[pyo3(signature = (path, threads = None))]
fn scan(py: Python<'_>, path: PathBuf, threads: Option<usize>) -> PyResult<PyTree> {
    let opts = ScanOptions {
        threads: threads.and_then(NonZeroUsize::new),
        ..ScanOptions::default()
    };
    let tree = py
        .detach(|| diskuse::scan(&path, &opts))
        .map_err(|e| PyOSError::new_err(format!("{}: {e}", path.display())))?;
    Ok(PyTree(Data::new(tree)))
}

/// Runs the `diskuse` command line with `args`, the program name first,
/// for the console script. Returns its exit code.
#[pyfunction]
fn main(py: Python<'_>, args: Vec<OsString>) -> u8 {
    py.detach(|| diskuse::cli(args))
}

#[pymethods]
impl PyTree {
    /// The scanned folder.
    #[getter]
    fn root(&self) -> PyDir {
        PyDir {
            data: Arc::clone(&self.0),
            id: 0,
        }
    }

    /// The folder at `path`, relative to the root or below it, or `None`.
    fn find(&self, path: PathBuf) -> Option<PyDir> {
        let tree = &self.0.tree;
        let root = Path::new(std::ffi::OsStr::from_bytes(tree.name(0)));
        let below = path.strip_prefix(root).unwrap_or(&path);
        let mut id = 0;
        for name in below.components() {
            let name = name.as_os_str().as_encoded_bytes();
            if name == b"." {
                continue;
            }
            let mut kids = self.0.index.children(id).iter();
            id = *kids.find(|&&k| tree.name(tree.record(k).name) == name)?;
        }
        Some(PyDir {
            data: Arc::clone(&self.0),
            id,
        })
    }

    /// The `n` largest files as `(path, bytes)`, largest first. Only the
    /// largest 1000 are kept.
    #[pyo3(signature = (n = 100))]
    fn largest_files(&self, n: usize) -> Vec<(PathBuf, u64)> {
        let files = diskuse::largest_files(&self.0.tree, n).into_iter();
        files
            .map(|(bytes, path)| (OsString::from_vec(path).into(), bytes))
            .collect()
    }

    /// Every folder as a row of a `pyarrow.Table`; row `i` is folder id `i`.
    /// Needs `pyarrow`. The columns cross as raw buffers, so no Python object
    /// is made per folder: see `python/diskuse/_arrow.py`.
    fn to_arrow<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let columns = columns(py, &self.0);
        py.import("diskuse._arrow")?
            .call_method1("table", (columns,))
    }

    fn __repr__(&self) -> String {
        let root = self.root();
        format!("<Tree {} {} bytes>", root.path().display(), root.size())
    }
}

/// The records of `data` as raw little-endian columns: `id` and `parent`
/// (u32, with `valid`, a validity bitmap that leaves the root's parent
/// null), `name` (u32 name id), `size` and `own` (u64), `flags` (u16), and
/// the names as UTF-8, lossy like the JSON output, with their `offsets`
/// (i64).
fn columns<'py>(py: Python<'py>, data: &Data) -> Vec<(&'static str, Bound<'py, PyAny>)> {
    let Data { tree, totals, .. } = data;
    let n = tree.len() as u32;
    let column = |f: &dyn Fn(u32, &mut Vec<u8>)| {
        let mut out = Vec::new();
        (0..n).for_each(|i| f(i, &mut out));
        PyBytes::new(py, &out).into_any()
    };
    let mut names = Vec::new();
    let mut offsets: Vec<u8> = 0i64.to_le_bytes().to_vec();
    for id in 0..tree.name_count() {
        names.extend_from_slice(String::from_utf8_lossy(tree.name(id)).as_bytes());
        offsets.extend((names.len() as i64).to_le_bytes());
    }
    // every bit set but the root's
    let mut valid = vec![0xffu8; (n as usize).div_ceil(8)];
    valid[0] &= !1;
    vec![
        ("n", n.into_pyobject(py).unwrap().into_any()),
        ("id", column(&|i, o| o.extend(i.to_le_bytes()))),
        (
            "parent",
            column(&|i, o| o.extend(tree.record(i).parent.to_le_bytes())),
        ),
        ("valid", PyBytes::new(py, &valid).into_any()),
        (
            "name",
            column(&|i, o| o.extend(tree.record(i).name.to_le_bytes())),
        ),
        (
            "size",
            column(&|i, o| o.extend(totals.size[i as usize].to_le_bytes())),
        ),
        (
            "own",
            column(&|i, o| o.extend(tree.record(i).own.to_le_bytes())),
        ),
        (
            "flags",
            column(&|i, o| o.extend(totals.flags[i as usize].to_le_bytes())),
        ),
        ("names", PyBytes::new(py, &names).into_any()),
        ("offsets", PyBytes::new(py, &offsets).into_any()),
    ]
}

#[pymethods]
impl PyDir {
    /// Its name; for the root, the path as scanned.
    #[getter]
    fn name(&self) -> OsString {
        let tree = &self.data.tree;
        OsString::from_vec(tree.name(tree.record(self.id).name).to_vec())
    }

    #[getter]
    fn path(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.data.tree.dir_path(self.id)))
    }

    /// Allocated bytes of it and everything below it.
    #[getter]
    fn size(&self) -> u64 {
        self.data.totals.size[self.id as usize]
    }

    /// Allocated bytes of the folder itself and its files.
    #[getter]
    fn own(&self) -> u64 {
        self.data.tree.record(self.id).own
    }

    /// Why it could not be read (`EACCES`, `EPERM`, `errno N`), or `None`.
    #[getter]
    fn error(&self) -> Option<String> {
        let r = self.data.tree.record(self.id);
        (r.flags & Record::DENIED != 0).then(|| diskuse::denied(&r))
    }

    /// Something below it could not be read, so `size` is a lower bound.
    #[getter]
    fn partial(&self) -> bool {
        self.data.totals.flags[self.id as usize] & Record::PARTIAL != 0
    }

    /// A mount point, not scanned into.
    #[getter]
    fn other_device(&self) -> bool {
        self.data.tree.record(self.id).flags & Record::OTHER_DEVICE != 0
    }

    /// Its subfolders, largest first, ties by name.
    fn children(&self) -> Vec<PyDir> {
        let Data {
            tree,
            totals,
            index,
        } = &*self.data;
        let mut kids = index.children(self.id).to_vec();
        let key = |&k: &u32| {
            let size = totals.size[k as usize];
            (std::cmp::Reverse(size), tree.name(tree.record(k).name))
        };
        kids.sort_by(|a, b| key(a).cmp(&key(b)));
        kids.into_iter()
            .map(|id| PyDir {
                data: Arc::clone(&self.data),
                id,
            })
            .collect()
    }

    fn __repr__(&self) -> String {
        format!("<Dir {} {} bytes>", self.path().display(), self.size())
    }
}

/// A live scan, wrapped by `diskuse.Live`: `next` returns one event as a
/// tuple, `("closed",)` once closed, or `None` if nothing happened within
/// `interval` seconds, waiting with the GIL released. `close` stops it.
#[pyclass(name = "_Live", module = "diskuse")]
struct PyLive {
    live: std::sync::Mutex<Option<diskuse::Live>>,
    /// Set by `close`, which may come while `next` has the scan out.
    closed: std::sync::atomic::AtomicBool,
}

#[pymethods]
impl PyLive {
    #[new]
    #[pyo3(signature = (path, threads = None))]
    fn new(path: PathBuf, threads: Option<usize>) -> Self {
        let live = diskuse::Live::start(&path, threads.and_then(NonZeroUsize::new));
        Self {
            live: std::sync::Mutex::new(Some(live)),
            closed: Default::default(),
        }
    }

    fn next<'py>(&self, py: Python<'py>, interval: f64) -> PyResult<Option<Bound<'py, PyAny>>> {
        // taken out while it waits, so the lock is not held without the GIL
        let closed = || ("closed",).into_pyobject(py).map(|t| Some(t.into_any()));
        let Some(mut live) = self.live.lock().unwrap().take() else {
            return closed();
        };
        let interval = std::time::Duration::from_secs_f64(interval);
        let event = py.detach(|| live.next(interval));
        if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            py.detach(|| drop(live));
            return closed();
        }
        *self.live.lock().unwrap() = Some(live);
        let event = event.map_err(|e| PyOSError::new_err(e.to_string()))?;
        let tree = |t: Tree| PyTree(Data::new(t));
        let event = match event {
            None => return Ok(None),
            Some(diskuse::Event::Scanning(t)) => ("scanning", tree(t)).into_pyobject(py)?,
            Some(diskuse::Event::Ready(t)) => ("ready", tree(t)).into_pyobject(py)?,
            Some(diskuse::Event::Changed(t, c)) => ("changed", tree(t), c).into_pyobject(py)?,
            Some(diskuse::Event::Rescanning(why)) => ("rescanning", why).into_pyobject(py)?,
        };
        Ok(Some(event.into_any()))
    }

    /// Stops the scan and the watch.
    fn close(&self, py: Python<'_>) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(live) = self.live.lock().unwrap().take() {
            py.detach(|| drop(live));
        }
    }
}

#[pymodule]
fn _diskuse(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyLive>()?;
    m.add_function(wrap_pyfunction!(scan, m)?)?;
    m.add_function(wrap_pyfunction!(main, m)?)?;
    m.add_class::<PyTree>()?;
    m.add_class::<PyDir>()?;
    Ok(())
}

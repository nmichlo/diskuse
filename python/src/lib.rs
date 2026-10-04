//! The `diskuse` Python package: the Rust library's API, name for name.
//! Folders are ids, the root is 0, as in Rust. Only Python has `~`
//! expansion, `async for`, and the Arrow PyCapsule interface.

use arrow_array::ffi_stream::FFI_ArrowArrayStream;
use arrow_array::types::UInt32Type;
use arrow_array::{
    ArrayRef, BooleanArray, DictionaryArray, LargeStringArray, RecordBatch, RecordBatchIterator,
    UInt32Array, UInt64Array,
};
use diskuse::{Event, LiveOptions, ReadTree, Reason, Record, ScanError, ScanOptions, Tree, Waker};
use pyo3::exceptions::{PyIndexError, PyOSError, PyStopAsyncIteration, PyStopIteration};
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyCapsule, PyTuple};
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often a sync `for` over a live scan checks for Ctrl-C: Python only
/// handles signals when the wait returns to it.
const SIGNALS: Duration = Duration::from_millis(50);

/// `path` with `~` expanded, as `os.path.expanduser`.
fn expand(py: Python<'_>, path: PathBuf) -> PyResult<PathBuf> {
    py.import("os.path")?
        .call_method1("expanduser", (path,))?
        .extract()
}

fn options(threads: Option<usize>) -> ScanOptions {
    ScanOptions {
        threads: threads.and_then(NonZeroUsize::new),
        ..ScanOptions::default()
    }
}

/// The error scanning `root`: for the root itself, the `OSError` subclass
/// of its errno with `root` as the filename, as `open` raises.
fn os_error(py: Python<'_>, e: ScanError, root: &Path) -> PyErr {
    let ScanError::Root(io) = &e else {
        return PyOSError::new_err(e.to_string());
    };
    let Some(errno) = io.raw_os_error() else {
        return PyOSError::new_err(format!("{}: {io}", root.display()));
    };
    match py
        .import("os")
        .and_then(|os| os.call_method1("strerror", (errno,)))
    {
        Ok(strerror) => PyOSError::new_err((errno, strerror.unbind(), root.to_path_buf())),
        Err(e) => e,
    }
}

/// Scans `path` with `threads` threads, or as many as help, with the GIL
/// released.
#[pyfunction]
#[pyo3(signature = (path, threads = None))]
fn scan(py: Python<'_>, path: PathBuf, threads: Option<usize>) -> PyResult<PyTree> {
    let path = expand(py, path)?;
    let tree = py.detach(|| diskuse::scan(&path, &options(threads)));
    let tree = tree.map_err(|e| os_error(py, e, &path))?;
    Ok(PyTree::new(tree))
}

/// The `diskuse` console script: the command line, from `sys.argv`.
#[pyfunction]
fn _cli(py: Python<'_>) -> PyResult<u8> {
    let args: Vec<OsString> = py.import("sys")?.getattr("argv")?.extract()?;
    Ok(py.detach(|| diskuse::cli(args)))
}

/// A scanned tree. Every method takes a folder id.
#[pyclass(frozen, from_py_object, name = "Tree", module = "diskuse")]
#[derive(Clone)]
struct PyTree(Arc<Tree>);

impl PyTree {
    fn new(tree: Tree) -> Self {
        // derived here, not on the first call, which holds the GIL
        tree.size(0);
        Self(Arc::new(tree))
    }

    /// `id`, if it is a folder of this tree, not one removed since the
    /// scan.
    fn id(&self, id: u32) -> PyResult<u32> {
        let there = |id| self.0.record(id).flags & Record::REMOVED == 0;
        match (id as usize) < self.0.len() && there(id) {
            true => Ok(id),
            false => Err(PyIndexError::new_err(format!("no folder {id}"))),
        }
    }
}

#[pymethods]
impl PyTree {
    fn name(&self, id: u32) -> PyResult<OsString> {
        Ok(OsString::from_vec(self.0.name(self.id(id)?).to_vec()))
    }

    fn path(&self, id: u32) -> PyResult<PathBuf> {
        Ok(self.0.path(self.id(id)?))
    }

    fn size(&self, id: u32) -> PyResult<u64> {
        Ok(self.0.size(self.id(id)?))
    }

    fn own(&self, id: u32) -> PyResult<u64> {
        Ok(self.0.own(self.id(id)?))
    }

    fn error(&self, id: u32) -> PyResult<Option<String>> {
        Ok(self.0.error(self.id(id)?))
    }

    fn partial(&self, id: u32) -> PyResult<bool> {
        Ok(self.0.partial(self.id(id)?))
    }

    fn other_device(&self, id: u32) -> PyResult<bool> {
        Ok(self.0.other_device(self.id(id)?))
    }

    fn children(&self, id: u32) -> PyResult<Vec<u32>> {
        Ok(self.0.children(self.id(id)?).to_vec())
    }

    fn find(&self, path: PathBuf) -> Option<u32> {
        self.0.find(&path)
    }

    /// Listed from disk now, as the tree keeps only folder totals.
    fn files(&self, py: Python<'_>, id: u32) -> PyResult<Vec<(OsString, u64)>> {
        let id = self.id(id)?;
        let files = py.detach(|| self.0.files(id, false))?;
        Ok((files.into_iter())
            .map(|f| (OsString::from_vec(f.name.into()), f.bytes))
            .collect())
    }

    #[pyo3(signature = (n = 100))]
    fn largest_files(&self, n: usize) -> Vec<(PathBuf, u64)> {
        self.0.largest_files(n)
    }

    fn stopped(&self) -> bool {
        self.0.stopped()
    }

    /// Every folder as a row, for `pyarrow.table(tree)`, polars, duckdb.
    #[pyo3(signature = (requested_schema = None))]
    fn __arrow_c_stream__<'py>(
        &self,
        py: Python<'py>,
        requested_schema: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyCapsule>> {
        // the one schema there is
        let _ = requested_schema;
        let batch = batch(&self.0);
        let schema = batch.schema();
        let reader = RecordBatchIterator::new([Ok(batch)], schema);
        let stream = FFI_ArrowArrayStream::new(Box::new(reader));
        PyCapsule::new_with_value(py, stream, c"arrow_array_stream")
    }

    fn __repr__(&self) -> String {
        format!(
            "<Tree {} {} bytes>",
            self.0.path(0).display(),
            self.0.size(0)
        )
    }
}

/// Every folder not removed, with its id, its parent's (none for the
/// root), its name as a dictionary of every name, lossy UTF-8, and sizes
/// and flags.
fn batch(tree: &Tree) -> RecordBatch {
    let ids: Vec<u32> = (0..tree.len() as u32)
        .filter(|&i| tree.record(i).flags & Record::REMOVED == 0)
        .collect();
    let names: LargeStringArray = (0..tree.name_count())
        .map(|n| Some(String::from_utf8_lossy(tree.raw_name(n)).into_owned()))
        .collect();
    let keys: UInt32Array = ids.iter().map(|&i| tree.record(i).name).collect();
    let name = DictionaryArray::<UInt32Type>::try_new(keys, Arc::new(names)).unwrap();
    let parent: UInt32Array = (ids.iter())
        .map(|&i| (i != 0).then(|| tree.record(i).parent))
        .collect();
    let u64s = |f: &dyn Fn(u32) -> u64| -> ArrayRef {
        Arc::new(ids.iter().map(|&i| f(i)).collect::<UInt64Array>())
    };
    let bools = |f: &dyn Fn(u32) -> bool| -> ArrayRef {
        Arc::new(ids.iter().map(|&i| Some(f(i))).collect::<BooleanArray>())
    };
    RecordBatch::try_from_iter([
        ("id", Arc::new(UInt32Array::from(ids.clone())) as ArrayRef),
        ("parent", Arc::new(parent)),
        ("name", Arc::new(name)),
        ("size", u64s(&|i| tree.size(i))),
        ("own", u64s(&|i| tree.own(i))),
        ("denied", bools(&|i| tree.error(i).is_some())),
        ("partial", bools(&|i| tree.partial(i))),
        ("other_device", bools(&|i| tree.other_device(i))),
    ])
    .unwrap()
}

/// Why a live scan scans the whole tree again, as in Rust. `str()` says it
/// in words.
#[pyclass(frozen, eq, from_py_object, name = "Reason", module = "diskuse")]
#[derive(Clone, Copy, PartialEq)]
enum PyReason {
    Dropped,
    IdsWrapped,
    RootMoved,
    NoReplay,
    MustScanAll,
}

impl From<Reason> for PyReason {
    fn from(reason: Reason) -> Self {
        match reason {
            Reason::Dropped => Self::Dropped,
            Reason::IdsWrapped => Self::IdsWrapped,
            Reason::RootMoved => Self::RootMoved,
            Reason::NoReplay => Self::NoReplay,
            Reason::MustScanAll => Self::MustScanAll,
        }
    }
}

impl From<PyReason> for Reason {
    fn from(reason: PyReason) -> Self {
        match reason {
            PyReason::Dropped => Self::Dropped,
            PyReason::IdsWrapped => Self::IdsWrapped,
            PyReason::RootMoved => Self::RootMoved,
            PyReason::NoReplay => Self::NoReplay,
            PyReason::MustScanAll => Self::MustScanAll,
        }
    }
}

#[pymethods]
impl PyReason {
    fn __str__(&self) -> String {
        Reason::from(*self).to_string()
    }
}

/// What a live scan reports, as in Rust.
#[pyclass(frozen, name = "Event", module = "diskuse")]
enum PyEvent {
    Scanning {
        tree: PyTree,
    },
    Ready {
        tree: PyTree,
    },
    Changed {
        tree: PyTree,
        changes: Vec<(PathBuf, i64)>,
    },
    Rescanning {
        reason: PyReason,
    },
}

impl From<Event> for PyEvent {
    fn from(event: Event) -> Self {
        match event {
            Event::Scanning(tree) => Self::Scanning {
                tree: PyTree::new(tree),
            },
            Event::Ready(tree) => Self::Ready {
                tree: PyTree::new(tree),
            },
            Event::Changed(tree, changes) => Self::Changed {
                tree: PyTree::new(tree),
                changes,
            },
            Event::Rescanning(reason) => Self::Rescanning {
                reason: reason.into(),
            },
        }
    }
}

/// A live scan: iterate it, sync or async. Leaving the loop stops it.
#[pyclass(frozen, name = "Live", module = "diskuse")]
struct PyLive {
    root: PathBuf,
    live: Mutex<diskuse::Live>,
    /// Wakes the wait holding `live`, which needs no lock.
    waker: Waker,
}

impl PyLive {
    /// The next event, waiting at most `timeout` without the GIL, or until
    /// woken. `Err(None)` once the scan failed and nothing comes again.
    fn wait(&self, py: Python<'_>, timeout: Duration) -> Result<Option<PyEvent>, Option<PyErr>> {
        let (event, done) = py.detach(|| {
            let mut live = self.live.lock().unwrap();
            (live.wait(timeout), live.is_done())
        });
        match event {
            Some(Ok(event)) => Ok(Some(event.into())),
            Some(Err(e)) => Err(Some(os_error(py, e, &self.root))),
            None if done => Err(None),
            None => Ok(None),
        }
    }
}

/// Scans `path` and keeps following it: see `Event`. `interval` is how
/// often, in seconds, a snapshot comes while scanning, and changes after.
/// `shown_only`: on Linux, follow only the folders given to `follow`.
#[pyfunction]
#[pyo3(signature = (path, interval = 0.5, threads = None, shown_only = false))]
fn live(
    py: Python<'_>,
    path: PathBuf,
    interval: f64,
    threads: Option<usize>,
    shown_only: bool,
) -> PyResult<PyLive> {
    let path = expand(py, path)?;
    let opts = LiveOptions {
        scan: options(threads),
        interval: Duration::try_from_secs_f64(interval)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?,
        shown_only,
        ..LiveOptions::default()
    };
    let live = py.detach(|| diskuse::live(&path, opts));
    Ok(PyLive {
        root: path,
        waker: live.waker(),
        live: Mutex::new(live),
    })
}

#[pymethods]
impl PyLive {
    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<PyEvent>> {
        loop {
            match self.wait(py, SIGNALS) {
                Ok(Some(event)) => return Ok(Some(event)),
                Ok(None) => py.check_signals()?,
                Err(None) => return Ok(None),
                Err(Some(e)) => return Err(e),
            }
        }
    }

    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __anext__(slf: Py<Self>) -> Next {
        Next {
            live: slf,
            step: None,
        }
    }

    /// With `shown_only`, or once inotify watches run out (Linux), follows
    /// only the folders `ids` of the latest tree. Ignored otherwise.
    /// Every change below the root is followed, not only those in the
    /// folders given to `follow`.
    fn follows_all(&self, py: Python<'_>) -> bool {
        self.waker.wake();
        py.detach(|| self.live.lock().unwrap().follows_all())
    }

    fn follow(&self, py: Python<'_>, ids: Vec<u32>) {
        self.waker.wake();
        py.detach(|| self.live.lock().unwrap().follow(&ids));
    }

    /// Scans folder `id` of the latest tree again, then reports `Changed`;
    /// 0 scans it all again.
    fn rescan(&self, py: Python<'_>, id: u32) {
        self.waker.wake();
        py.detach(|| self.live.lock().unwrap().rescan(id));
    }
}

/// The awaitable `__anext__` returns: a wait on a worker thread
/// (`asyncio.to_thread`), again after each wake, until one brings an
/// event. Cancelling it wakes the wait, so no thread is left waiting.
#[pyclass(module = "diskuse")]
struct Next {
    live: Py<PyLive>,
    /// The `__await__` iterator of the running wait.
    step: Option<Py<PyAny>>,
}

#[pymethods]
impl Next {
    fn __await__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        loop {
            let step = match &self.step {
                Some(step) => step.bind(py).clone(),
                None => {
                    let live = self.live.clone_ref(py);
                    let wait = PyCFunction::new_closure(
                        py,
                        None,
                        None,
                        move |args: &Bound<'_, PyTuple>, _| match live
                            .get()
                            .wait(args.py(), Duration::MAX)
                        {
                            Ok(event) => Ok(event),
                            Err(None) => Err(PyStopAsyncIteration::new_err(())),
                            Err(Some(e)) => Err(e),
                        },
                    )?;
                    let coro = py.import("asyncio")?.call_method1("to_thread", (wait,))?;
                    let step = coro.call_method0("__await__")?;
                    self.step = Some(step.clone().unbind());
                    step
                }
            };
            match step.call_method0("__next__") {
                // a future to wait on, for the event loop
                Ok(future) => return Ok(future.unbind()),
                Err(e) if e.is_instance_of::<PyStopIteration>(py) => {
                    self.step = None;
                    let event = e.value(py).getattr("value")?;
                    if !event.is_none() {
                        return Err(PyStopIteration::new_err((event.unbind(),)));
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Cancelling: wakes the wait, then passes the error to it.
    #[pyo3(signature = (typ, val = None, _tb = None))]
    fn throw(
        &mut self,
        py: Python<'_>,
        typ: Py<PyAny>,
        val: Option<Py<PyAny>>,
        _tb: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.live.get().waker.wake();
        // the single-argument form: the others are deprecated
        let exc = val.unwrap_or(typ);
        match self.step.take() {
            Some(step) => Ok(step.bind(py).call_method1("throw", (exc,))?.unbind()),
            None => Err(PyErr::from_value(exc.into_bound(py))),
        }
    }

    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        self.live.get().waker.wake();
        match self.step.take() {
            Some(step) => step.bind(py).call_method0("close").map(drop),
            None => Ok(()),
        }
    }
}

#[pymodule]
#[pyo3(name = "diskuse")]
fn diskuse_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(scan, m)?)?;
    m.add_function(wrap_pyfunction!(live, m)?)?;
    m.add_function(wrap_pyfunction!(_cli, m)?)?;
    m.add_class::<PyTree>()?;
    m.add_class::<PyEvent>()?;
    m.add_class::<PyReason>()?;
    m.add_class::<PyLive>()?;
    Ok(())
}

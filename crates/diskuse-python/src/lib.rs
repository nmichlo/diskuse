//! The `diskuse` Python package: the Rust library's API, name for name.
//! Folders are ids, the root is 0, as in Rust. Only Python has `~`
//! expansion, `async for`, and the Arrow PyCapsule interface.

use arrow_array::ffi_stream::FFI_ArrowArrayStream;
use arrow_array::types::UInt32Type;
use arrow_array::{
    ArrayRef, BooleanArray, DictionaryArray, LargeStringArray, RecordBatch, RecordBatchIterator,
    UInt32Array, UInt64Array,
};
use diskuse_core::{Event, LiveOptions, ReadTree, Reason, Record, ScanError, ScanOptions, Tree};
use pyo3::exceptions::{
    PyBaseException, PyIndexError, PyOSError, PyStopAsyncIteration, PyStopIteration,
};
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyCapsule, PyTuple};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once};
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

/// Sends what the library logs to Python's `logging`, as `diskuse_core.*`.
/// From the first scan, not on import: the console script logs to a file
/// instead (`DISKUSE_LOG`).
fn log_to_python() {
    static ONCE: Once = Once::new();
    // a logger set before, by another extension, stays
    ONCE.call_once(|| drop(pyo3_log::try_init()));
}

/// Scans `path` with `threads` threads, or as many as help, with the GIL
/// released.
#[pyfunction]
#[pyo3(signature = (path, threads = None))]
fn scan(py: Python<'_>, path: PathBuf, threads: Option<usize>) -> PyResult<PyTree> {
    log_to_python();
    let path = expand(py, path)?;
    let tree = py.detach(|| diskuse_core::scan(&path, &options(threads)));
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

/// Why changes were missed, as in Rust. `str()` says it
/// in words.
#[pyclass(frozen, eq, from_py_object, name = "Reason", module = "diskuse")]
#[derive(Clone, Copy, PartialEq)]
enum PyReason {
    Dropped,
    IdsWrapped,
    RootMoved,
    NoReplay,
    MustScan,
}

impl From<Reason> for PyReason {
    fn from(reason: Reason) -> Self {
        match reason {
            Reason::Dropped => Self::Dropped,
            Reason::IdsWrapped => Self::IdsWrapped,
            Reason::RootMoved => Self::RootMoved,
            Reason::NoReplay => Self::NoReplay,
            Reason::MustScan => Self::MustScan,
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
            PyReason::MustScan => Self::MustScan,
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
    Missed {
        reason: PyReason,
        path: PathBuf,
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
            Event::Missed(reason, path) => Self::Missed {
                reason: reason.into(),
                path,
            },
        }
    }
}

/// What a live scan has reported and no loop has taken yet.
#[derive(Default)]
struct Inbox {
    events: Mutex<VecDeque<Result<Event, ScanError>>>,
    ready: Condvar,
}

/// The asyncio loop an `async for` runs on, and the callback that hands it
/// the [`Inbox`]. Only locked with the GIL held.
type Wake = Mutex<Option<(Py<PyAny>, Py<PyAny>)>>;

/// A live scan: iterate it, sync or async, and `close()` it, or use it as
/// a context manager.
#[pyclass(frozen, name = "Live", module = "diskuse")]
struct PyLive {
    root: PathBuf,
    /// `None` once closed.
    live: Mutex<Option<diskuse_core::Live>>,
    inbox: Arc<Inbox>,
    /// Set by the first `async for`: events then also wake its loop.
    wake: Arc<Wake>,
    is_async: Arc<AtomicBool>,
    /// The `asyncio.Queue` an `async for` takes them from.
    queue: Mutex<Option<Py<PyAny>>>,
}

impl PyLive {
    /// `f` of the scan, unless closed. Never waits on it.
    fn with<T>(&self, f: impl FnOnce(&diskuse_core::Live) -> T) -> Option<T> {
        self.live.lock().unwrap().as_ref().map(f)
    }

    /// No event comes again: closed, or the scan failed.
    fn ended(&self) -> bool {
        self.with(diskuse_core::Live::is_done).unwrap_or(true)
    }

    /// `event` as a Python object: an `Event`, or the exception to raise.
    fn object(&self, py: Python<'_>, event: Result<Event, ScanError>) -> PyResult<Py<PyAny>> {
        match event {
            // not `Py::new`, which makes the base class, not the variant's
            Ok(event) => Ok(PyEvent::from(event).into_pyobject(py)?.into_any().unbind()),
            Err(e) => Ok(os_error(py, e, &self.root).into_value(py).into_any()),
        }
    }

    /// Hands the loop of an `async for` everything in the inbox. Called on
    /// that loop.
    fn deliver(&self, py: Python<'_>) -> PyResult<()> {
        let events: Vec<_> = self.inbox.events.lock().unwrap().drain(..).collect();
        if let Some(queue) = &*self.queue.lock().unwrap() {
            for event in events {
                queue
                    .bind(py)
                    .call_method1("put_nowait", (self.object(py, event)?,))?;
            }
        }
        Ok(())
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
    log_to_python();
    let path = expand(py, path)?;
    let opts = LiveOptions {
        scan: options(threads),
        interval: Duration::try_from_secs_f64(interval)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?,
        shown_only,
        ..LiveOptions::default()
    };
    let inbox = Arc::new(Inbox::default());
    let wake = Arc::new(Wake::default());
    let is_async = Arc::new(AtomicBool::new(false));
    // on the scan's own thread: never with the GIL, unless a loop waits
    let handler = {
        let (inbox, wake, is_async) = (inbox.clone(), wake.clone(), is_async.clone());
        move |event| {
            {
                let mut events = inbox.events.lock().unwrap();
                // a loop slower than the scan gets the latest snapshot, not
                // a queue of them: each is a copy of the whole tree
                if let (Ok(Event::Scanning(_)), Some(Ok(Event::Scanning(_)))) =
                    (&event, events.back())
                {
                    events.pop_back();
                }
                events.push_back(event);
            }
            inbox.ready.notify_all();
            if is_async.load(Ordering::Relaxed) {
                Python::attach(|py| {
                    if let Some((event_loop, deliver)) = &*wake.lock().unwrap() {
                        // a loop closed since takes no more
                        let _ = event_loop
                            .bind(py)
                            .call_method1("call_soon_threadsafe", (deliver,));
                    }
                });
            }
        }
    };
    let live = diskuse_core::live(&path, opts, handler);
    Ok(PyLive {
        root: path,
        live: Mutex::new(Some(live)),
        inbox,
        wake,
        is_async,
        queue: Mutex::new(None),
    })
}

#[pymethods]
impl PyLive {
    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<PyEvent>> {
        loop {
            // waits without the GIL, a little at a time: Python only
            // handles Ctrl-C when the wait returns to it
            let event = py.detach(|| {
                let events = self.inbox.events.lock().unwrap();
                let wait = self
                    .inbox
                    .ready
                    .wait_timeout_while(events, SIGNALS, |e| e.is_empty());
                wait.unwrap().0.pop_front()
            });
            match event {
                Some(Ok(event)) => return Ok(Some(event.into())),
                Some(Err(e)) => return Err(os_error(py, e, &self.root)),
                None if self.ended() => return Ok(None),
                None => py.check_signals()?,
            }
        }
    }

    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __anext__(slf: Bound<'_, Self>) -> PyResult<Next> {
        let (py, this) = (slf.py(), slf.get());
        let queue = {
            let mut queue = this.queue.lock().unwrap();
            if queue.is_none() {
                // the first time: from now on events also wake this loop
                let asyncio = py.import("asyncio")?;
                *queue = Some(asyncio.call_method0("Queue")?.unbind());
                let live = slf.clone().unbind();
                let deliver = PyCFunction::new_closure(
                    py,
                    None,
                    None,
                    move |args: &Bound<'_, PyTuple>, _| live.get().deliver(args.py()),
                )?;
                let event_loop = asyncio.call_method0("get_running_loop")?;
                let wake = (event_loop.unbind(), deliver.into_any().unbind());
                *this.wake.lock().unwrap() = Some(wake);
                this.is_async.store(true, Ordering::Relaxed);
            }
            queue.as_ref().unwrap().bind(py).clone()
        };
        // what came before the loop was told of
        this.deliver(py)?;
        if this.ended() && queue.call_method0("empty")?.extract()? {
            return Err(PyStopAsyncIteration::new_err(()));
        }
        let get = queue.call_method0("get")?.call_method0("__await__")?;
        Ok(Next { get: get.unbind() })
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, PyTuple>) -> PyResult<()> {
        self.close(py)
    }

    /// Stops the scan and ends every loop over it. Safe to call again.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let live = self.live.lock().unwrap().take();
        // without the GIL: its thread may be waiting for it, to wake a loop
        py.detach(|| drop(live));
        self.inbox.ready.notify_all();
        // what an `async for` waits on: `None` ends it
        if let Some(queue) = &*self.queue.lock().unwrap() {
            queue.bind(py).call_method1("put_nowait", (py.None(),))?;
        }
        Ok(())
    }

    /// Every change below the root is followed, not only those in the
    /// folders given to `follow`.
    fn follows_all(&self) -> bool {
        self.with(diskuse_core::Live::follows_all).unwrap_or(false)
    }

    /// With `shown_only`, or once inotify watches run out (Linux), follows
    /// only the folders `ids` of the latest tree. Ignored otherwise.
    fn follow(&self, ids: Vec<u32>) {
        self.with(|live| live.follow(&ids));
    }

    /// Lists the folders `ids` of the latest tree again, and reports what
    /// changed in them: for folders looked at after `Missed`.
    fn relist(&self, ids: Vec<u32>) {
        self.with(|live| live.relist(&ids));
    }

    /// Scans folder `id` of the latest tree again, then reports `Changed`;
    /// 0 scans it all again.
    fn rescan(&self, id: u32) {
        self.with(|live| live.rescan(id));
    }
}

impl Drop for PyLive {
    fn drop(&mut self) {
        let live = self.live.get_mut().unwrap().take();
        // as `close`: never join the scan's thread with the GIL held
        Python::attach(|py| py.detach(|| drop(live)));
    }
}

/// The awaitable `__anext__` returns: the `asyncio.Queue.get()` of the
/// next event, so waiting and cancelling are asyncio's own. What comes out
/// is the event, an exception to raise, or `None` once closed.
#[pyclass(module = "diskuse")]
struct Next {
    /// The `__await__` iterator of the `get()`.
    get: Py<PyAny>,
}

impl Next {
    /// What a step of `get` gave: a future to wait on, or its result.
    fn step(py: Python<'_>, step: PyResult<Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
        match step {
            Ok(future) => Ok(future.unbind()),
            Err(e) if e.is_instance_of::<PyStopIteration>(py) => {
                let event = e.value(py).getattr("value")?;
                if event.is_none() {
                    Err(PyStopAsyncIteration::new_err(()))
                } else if event.is_instance_of::<PyBaseException>() {
                    Err(PyErr::from_value(event))
                } else {
                    Err(PyStopIteration::new_err((event.unbind(),)))
                }
            }
            Err(e) => Err(e),
        }
    }
}

#[pymethods]
impl Next {
    fn __await__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Self::step(py, self.get.bind(py).call_method0("__next__"))
    }

    fn send(&self, py: Python<'_>, value: Py<PyAny>) -> PyResult<Py<PyAny>> {
        Self::step(py, self.get.bind(py).call_method1("send", (value,)))
    }

    /// Cancelling, as asyncio does it: passed on to the `get()`.
    #[pyo3(signature = (typ, val = None, _tb = None))]
    fn throw(
        &self,
        py: Python<'_>,
        typ: Py<PyAny>,
        val: Option<Py<PyAny>>,
        _tb: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        // the single-argument form: the others are deprecated
        let exc = val.unwrap_or(typ);
        Self::step(py, self.get.bind(py).call_method1("throw", (exc,)))
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        self.get.bind(py).call_method0("close").map(drop)
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

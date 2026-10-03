//! FSEvents: macOS records every change to a volume, by path, in the
//! volume's `.fseventsd`. A stream first replays that record from an event
//! id, then reports changes live. Declared by hand: about a dozen functions
//! of CoreServices, CoreFoundation and libdispatch, which no small crate
//! covers together.

#![allow(unsafe_code)]

use super::Event;
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::mpsc::Sender;

type CFIndex = isize;
type CFRef = *const c_void;
type StreamRef = *mut c_void;
type Queue = *mut c_void;

/// `FSEventStreamContext`.
#[repr(C)]
struct Context {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

/// `CFArrayCallBacks`.
#[repr(C)]
struct ArrayCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

/// `FSEventStreamCallback`. Without `kFSEventStreamCreateFlagUseCFTypes`,
/// the paths are a C array of C strings.
type Callback = extern "C" fn(StreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

/// The CoreServices and CoreFoundation functions used here, loaded on
/// first use rather than linked, so a run that never watches (`scan`,
/// `show`) skips loading both frameworks: about 1.1 ms of the 4 ms a run
/// takes to start.
struct Api {
    stream_create:
        unsafe extern "C" fn(CFRef, Callback, *const Context, CFRef, u64, f64, u32) -> StreamRef,
    set_dispatch_queue: unsafe extern "C" fn(StreamRef, Queue),
    start: unsafe extern "C" fn(StreamRef) -> u8,
    stop: unsafe extern "C" fn(StreamRef),
    flush_sync: unsafe extern "C" fn(StreamRef),
    invalidate: unsafe extern "C" fn(StreamRef),
    release: unsafe extern "C" fn(StreamRef),
    current_event_id: unsafe extern "C" fn() -> u64,
    copy_uuid_for_device: unsafe extern "C" fn(libc::dev_t) -> CFRef,
    array_callbacks: *const ArrayCallBacks,
    string_create: unsafe extern "C" fn(CFRef, *const c_char) -> CFRef,
    array_create:
        unsafe extern "C" fn(CFRef, *const CFRef, CFIndex, *const ArrayCallBacks) -> CFRef,
    cf_release: unsafe extern "C" fn(CFRef),
}

// SAFETY: function pointers and a pointer to an immutable static of the
// framework, which stays loaded.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// `p` as a function pointer of type `F`.
///
/// # Safety
/// `p` must point to a function of type `F`.
unsafe fn cast<F: Copy>(p: *mut c_void) -> F {
    const { assert!(size_of::<F>() == size_of::<*mut c_void>()) };
    // SAFETY: same size, and the caller vouches for the type.
    unsafe { std::mem::transmute_copy(&p) }
}

/// The functions, or `None` if a framework or symbol is missing, which no
/// macOS since 10.5 lacks.
fn api() -> Option<&'static Api> {
    static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();
    API.get_or_init(|| {
        let open = |path: &CStr| {
            // SAFETY: a NUL-terminated path; a system framework has no
            // initializers that depend on us.
            let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
            (!handle.is_null()).then_some(handle)
        };
        let cs = open(c"/System/Library/Frameworks/CoreServices.framework/CoreServices")?;
        let cf = open(c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")?;
        let sym = |handle, name: &CStr| {
            // SAFETY: a live handle and a NUL-terminated name.
            let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
            (!p.is_null()).then_some(p)
        };
        // SAFETY: each symbol is the framework function of that name, with
        // the signature declared in `Api` (from the framework headers).
        unsafe {
            Some(Api {
                stream_create: cast(sym(cs, c"FSEventStreamCreate")?),
                set_dispatch_queue: cast(sym(cs, c"FSEventStreamSetDispatchQueue")?),
                start: cast(sym(cs, c"FSEventStreamStart")?),
                stop: cast(sym(cs, c"FSEventStreamStop")?),
                flush_sync: cast(sym(cs, c"FSEventStreamFlushSync")?),
                invalidate: cast(sym(cs, c"FSEventStreamInvalidate")?),
                release: cast(sym(cs, c"FSEventStreamRelease")?),
                current_event_id: cast(sym(cs, c"FSEventsGetCurrentEventId")?),
                copy_uuid_for_device: cast(sym(cs, c"FSEventsCopyUUIDForDevice")?),
                array_callbacks: sym(cf, c"kCFTypeArrayCallBacks")?.cast(),
                string_create: cast(sym(cf, c"CFStringCreateWithFileSystemRepresentation")?),
                array_create: cast(sym(cf, c"CFArrayCreate")?),
                cf_release: cast(sym(cf, c"CFRelease")?),
            })
        }
    })
    .as_ref()
}

// libdispatch, part of libSystem
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> Queue;
    fn dispatch_sync_f(queue: Queue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}

const FILE_EVENTS: u32 = 0x10;
/// Reports the root itself being moved or deleted, as `ROOT_CHANGED`.
const WATCH_ROOT: u32 = 0x04;
/// Delivers the first event after a quiet spell at once, rather than
/// `LATENCY` later.
const NO_DEFER: u32 = 0x02;

const MUST_SCAN_SUB_DIRS: u32 = 0x01;
const USER_DROPPED: u32 = 0x02;
const KERNEL_DROPPED: u32 = 0x04;
const EVENT_IDS_WRAPPED: u32 = 0x08;
const HISTORY_DONE: u32 = 0x10;
const ROOT_CHANGED: u32 = 0x20;

/// Seconds the stream gathers events into one batch. Short, as callers
/// batch again, and a waiting caller must tell a gap between batches from
/// the end of the events.
const LATENCY: f64 = 0.05;

/// A running stream, stopped when dropped.
pub struct Stream {
    api: &'static Api,
    stream: StreamRef,
    queue: Queue,
    /// Owned by the stream's callback until the stream is drained.
    tx: *mut Sender<Vec<Event>>,
}

// SAFETY: the stream and queue are thread-safe CoreFoundation and
// libdispatch objects, and `tx` is only used by the stream's callback on
// its own queue and freed in `Drop`, after that queue is drained.
unsafe impl Send for Stream {}

impl Stream {
    /// Delivers every event that happened before the call, held back by the
    /// latency or still with the FSEvents service, before returning.
    pub fn flush(&self) {
        // SAFETY: `stream` is started and stays valid until `Drop`; the
        // callback runs on our queue, not on this thread, so this cannot
        // wait on itself.
        unsafe { (self.api.flush_sync)(self.stream) }
    }

    /// Sends the changes below `path` with event ids after `since`, first
    /// the recorded ones, then live ones, in batches to `tx`.
    pub fn start(path: &CStr, since: u64, tx: Sender<Vec<Event>>) -> Option<Self> {
        let api = api()?;
        let tx = Box::into_raw(Box::new(tx));
        let context = Context {
            version: 0,
            info: tx.cast(),
            retain: ptr::null(),
            release: ptr::null(),
            copy_description: ptr::null(),
        };
        // SAFETY: `path` is NUL-terminated, and the array and string are
        // released once the stream holds its own copy. `context` is copied
        // by the call; its `info` stays valid until `Drop` frees it.
        let stream = unsafe {
            let string = (api.string_create)(ptr::null(), path.as_ptr());
            if string.is_null() {
                drop(Box::from_raw(tx));
                return None;
            }
            let paths = (api.array_create)(ptr::null(), &string, 1, api.array_callbacks);
            (api.cf_release)(string);
            let flags = FILE_EVENTS | WATCH_ROOT | NO_DEFER;
            let stream = (api.stream_create)(
                ptr::null(),
                callback,
                &context,
                paths,
                since,
                LATENCY,
                flags,
            );
            (api.cf_release)(paths);
            stream
        };
        if stream.is_null() {
            // SAFETY: no stream took it.
            drop(unsafe { Box::from_raw(tx) });
            return None;
        }
        // SAFETY: a serial queue of our own, and the live `stream`.
        let queue = unsafe { dispatch_queue_create(c"diskuse.fsevents".as_ptr(), ptr::null()) };
        let s = Self {
            api,
            stream,
            queue,
            tx,
        };
        // SAFETY: as above. If it does not start, `Drop` cleans up.
        let started = unsafe {
            (api.set_dispatch_queue)(stream, queue);
            (api.start)(stream)
        };
        (started != 0).then_some(s)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        extern "C" fn nothing(_: *mut c_void) {}
        // SAFETY: stops the stream, then waits on its serial queue for any
        // callback already running, so none can use `tx` once it is freed.
        unsafe {
            (self.api.stop)(self.stream);
            (self.api.invalidate)(self.stream);
            dispatch_sync_f(self.queue, ptr::null_mut(), nothing);
            (self.api.release)(self.stream);
            dispatch_release(self.queue);
            drop(Box::from_raw(self.tx));
        }
    }
}

extern "C" fn callback(
    _: StreamRef,
    info: *mut c_void,
    n: usize,
    paths: *mut c_void,
    flags: *const u32,
    _: *const u64,
) {
    if n == 0 {
        return;
    }
    // SAFETY: FSEvents passes `n` paths and flags, valid for this call,
    // and `info` is the sender `start` gave it, freed only after the queue
    // has run this.
    let (tx, paths, flags) = unsafe {
        (
            &*info.cast::<Sender<Vec<Event>>>(),
            std::slice::from_raw_parts(paths.cast::<*const c_char>(), n),
            std::slice::from_raw_parts(flags, n),
        )
    };
    let events = (0..n).map(|i| {
        // SAFETY: each path is a NUL-terminated C string.
        let path = unsafe { CStr::from_ptr(paths[i]) }.to_bytes();
        event(flags[i], path)
    });
    // the receiver goes away only with the stream
    let _ = tx.send(events.collect());
}

fn event(flags: u32, path: &[u8]) -> Event {
    if flags & (USER_DROPPED | KERNEL_DROPPED) != 0 {
        Event::Lost("macOS dropped change events")
    } else if flags & EVENT_IDS_WRAPPED != 0 {
        Event::Lost("macOS event ids wrapped")
    } else if flags & ROOT_CHANGED != 0 {
        Event::Lost("the scanned dir moved")
    } else if flags & HISTORY_DONE != 0 {
        Event::HistoryDone
    } else if flags & MUST_SCAN_SUB_DIRS != 0 {
        Event::Rescan(path.into())
    } else {
        Event::Changed(path.into())
    }
}

/// The id of the latest event on the system, or 0 without FSEvents.
pub fn current_event_id() -> u64 {
    // SAFETY: takes nothing, touches no memory of ours.
    api().map_or(0, |api| unsafe { (api.current_event_id)() })
}

/// Whether device `dev` keeps a record of changes. A network share or a
/// read-only volume does not, so FSEvents only sees the changes this Mac
/// makes there.
pub fn records_changes(dev: u64) -> bool {
    let Some(api) = api() else {
        return false;
    };
    // `st_dev` is an `i32` on macOS, widened by `DirStat`
    // SAFETY: returns a new reference or null.
    let uuid = unsafe { (api.copy_uuid_for_device)(dev as libc::dev_t) };
    if uuid.is_null() {
        return false;
    }
    // SAFETY: a live reference of ours.
    unsafe { (api.cf_release)(uuid) };
    true
}

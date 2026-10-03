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

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFRef,
        callback: Callback,
        context: *const Context,
        paths: CFRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> StreamRef;
    fn FSEventStreamSetDispatchQueue(stream: StreamRef, queue: Queue);
    fn FSEventStreamStart(stream: StreamRef) -> u8;
    fn FSEventStreamStop(stream: StreamRef);
    fn FSEventStreamFlushSync(stream: StreamRef);
    fn FSEventStreamInvalidate(stream: StreamRef);
    fn FSEventStreamRelease(stream: StreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: ArrayCallBacks;
    fn CFStringCreateWithFileSystemRepresentation(alloc: CFRef, path: *const c_char) -> CFRef;
    fn CFArrayCreate(
        alloc: CFRef,
        values: *const CFRef,
        count: CFIndex,
        callbacks: *const ArrayCallBacks,
    ) -> CFRef;
    fn CFRelease(cf: CFRef);
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
    stream: StreamRef,
    queue: Queue,
    /// Owned by the stream's callback until the stream is drained.
    tx: *mut Sender<Vec<Event>>,
}

impl Stream {
    /// Delivers every event that happened before the call, held back by the
    /// latency or still with the FSEvents service, before returning.
    pub fn flush(&self) {
        // SAFETY: `stream` is started and stays valid until `Drop`; the
        // callback runs on our queue, not on this thread, so this cannot
        // wait on itself.
        unsafe { FSEventStreamFlushSync(self.stream) }
    }

    /// Sends the changes below `path` with event ids after `since`, first
    /// the recorded ones, then live ones, in batches to `tx`.
    pub fn start(path: &CStr, since: u64, tx: Sender<Vec<Event>>) -> Option<Self> {
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
            let string = CFStringCreateWithFileSystemRepresentation(ptr::null(), path.as_ptr());
            if string.is_null() {
                drop(Box::from_raw(tx));
                return None;
            }
            let paths = CFArrayCreate(ptr::null(), &string, 1, &raw const kCFTypeArrayCallBacks);
            CFRelease(string);
            let flags = FILE_EVENTS | WATCH_ROOT | NO_DEFER;
            let stream = FSEventStreamCreate(
                ptr::null(),
                callback,
                &context,
                paths,
                since,
                LATENCY,
                flags,
            );
            CFRelease(paths);
            stream
        };
        if stream.is_null() {
            // SAFETY: no stream took it.
            drop(unsafe { Box::from_raw(tx) });
            return None;
        }
        // SAFETY: a serial queue of our own, and the live `stream`.
        let queue = unsafe { dispatch_queue_create(c"disksweep.fsevents".as_ptr(), ptr::null()) };
        let s = Self { stream, queue, tx };
        // SAFETY: as above. If it does not start, `Drop` cleans up.
        let started = unsafe {
            FSEventStreamSetDispatchQueue(stream, queue);
            FSEventStreamStart(stream)
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
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            dispatch_sync_f(self.queue, ptr::null_mut(), nothing);
            FSEventStreamRelease(self.stream);
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
    if flags & (USER_DROPPED | KERNEL_DROPPED | EVENT_IDS_WRAPPED | ROOT_CHANGED) != 0 {
        Event::Lost
    } else if flags & HISTORY_DONE != 0 {
        Event::HistoryDone
    } else if flags & MUST_SCAN_SUB_DIRS != 0 {
        Event::Rescan(path.into())
    } else {
        Event::Changed(path.into())
    }
}

/// The id of the latest event on the system.
pub fn current_event_id() -> u64 {
    // SAFETY: takes nothing, touches no memory of ours.
    unsafe { FSEventsGetCurrentEventId() }
}

/// Whether device `dev` keeps a record of changes. A network share or a
/// read-only volume does not, so FSEvents only sees the changes this Mac
/// makes there.
pub fn records_changes(dev: u64) -> bool {
    // `st_dev` is an `i32` on macOS, widened by `DirStat`
    // SAFETY: returns a new reference or null.
    let uuid = unsafe { FSEventsCopyUUIDForDevice(dev as libc::dev_t) };
    if uuid.is_null() {
        return false;
    }
    // SAFETY: a live reference of ours.
    unsafe { CFRelease(uuid) };
    true
}

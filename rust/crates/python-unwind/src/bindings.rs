//! CPython 3.15 execution-scoped shadow bindings, maintained outside SIGPROF.
//!
//! Python entry/line events publish the current state in a profiler-owned pthread
//! slot. Return, yield, and unwind events clear it *before the last Python frame
//! leaves*. This lifetime does not depend on thread-dictionary destruction: an
//! extension can retain that dictionary after PyThreadState_Clear/Delete.
//!
//! Only an active Python execution scope may have a binding. Native work called
//! within that scope, including work without the GIL, retains the Python caller.
//! A native thread outside Python has no binding, even if it retains its state.

use crate::layout;
use alloc::boxed::Box;
use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};
use pyo3_ffi as ffi;

static KEY: AtomicUsize = AtomicUsize::new(0); // pthread key + 1, never deleted/reused
static NEXT_EPOCH: AtomicUsize = AtomicUsize::new(1);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

struct Binding {
    state: AtomicUsize,
    epoch: AtomicUsize,
}

unsafe extern "C" fn thread_exited(pointer: *mut c_void) {
    // pthread clears the slot before calling its destructor. No handler on this
    // thread can find the allocation again, regardless of retained Python states.
    unsafe { drop(Box::from_raw(pointer.cast::<Binding>())) };
}

/// Start a binding epoch before enabling execution events or arming SIGPROF.
///
/// # Safety
/// Call on an attached main-interpreter thread holding the GIL, outside signal handlers.
/// Serialize all starts/stops; Python must remain initialized throughout the session.
/// Maintain the entry/exit event protocol documented by [`bind`] and [`leave`].
pub unsafe fn start() -> Result<usize, &'static str> {
    if ACTIVE.load(Ordering::Relaxed) != 0 {
        return Err("another Signal binding session is active");
    }
    if KEY.load(Ordering::Relaxed) == 0 {
        let mut key = 0;
        if unsafe { libc::pthread_key_create(&mut key, Some(thread_exited)) } != 0 {
            return Err("cannot allocate Signal's pthread key");
        }
        // Resolve the native symbol binding outside SIGPROF, also on Darwin.
        unsafe { libc::pthread_getspecific(key) };
        KEY.store(key as usize + 1, Ordering::Release);
    }
    let epoch = NEXT_EPOCH.load(Ordering::Relaxed);
    NEXT_EPOCH.store(
        epoch
            .checked_add(1)
            .ok_or("Signal binding epochs exhausted")?,
        Ordering::Relaxed,
    );
    ACTIVE.store(epoch, Ordering::Release);
    Ok(epoch)
}

/// Invalidate a stopped session. Call after stopping its timers.
pub fn stop(epoch: usize) {
    let _ = ACTIVE.compare_exchange(epoch, 0, Ordering::AcqRel, Ordering::Relaxed);
}

fn slot() -> *mut Binding {
    let key = (KEY.load(Ordering::Acquire) - 1) as libc::pthread_key_t;
    unsafe { libc::pthread_getspecific(key) }.cast()
}

fn clear() {
    let pointer = slot();
    if !pointer.is_null() {
        unsafe { &*pointer }.state.store(0, Ordering::Release);
    }
}

// These normal-thread reads use the currently attached state's valid frame chain,
// not a cached address. Monitoring invokes native callbacks without adding Python
// frames. 3.15's interpreter-owned sentinels include both the base and C evaluator
// entry frames. A sentinel alone is not the root: C reentry links through it.
unsafe fn word(base: usize, offset: usize) -> usize {
    unsafe { ptr::read_unaligned((base + offset) as *const usize) }
}

unsafe fn python_frame(frame: usize) -> bool {
    frame != 0
        && unsafe { ptr::read((frame + layout::OWNER) as *const u8) }
            != layout::FRAME_OWNED_BY_INTERPRETER
}

unsafe fn has_python_frame(mut frame: usize) -> bool {
    for _ in 0..crate::MAX_FRAMES {
        if frame == 0 {
            return false;
        }
        if unsafe { python_frame(frame) } {
            return true;
        }
        frame = unsafe { word(frame, layout::PREVIOUS) };
    }
    false
}

/// Publish the attached state during a Python entry, resume, throw, or line event.
/// May also seed the configure caller if it has active Python frames.
///
/// # Safety
/// Requires the main interpreter's GIL, outside signal handlers. While sampling,
/// every last-frame return, yield, or exception unwind must call [`leave`] before
/// unlinking the frame. Applications must not suppress those events, replace the
/// monitoring tool, delete an executing state, or migrate/swap live states.
pub unsafe fn bind() -> Result<(), &'static str> {
    let epoch = ACTIVE.load(Ordering::Acquire);
    if epoch == 0 {
        return Ok(());
    }
    let mut pointer = slot();
    if !pointer.is_null() {
        let binding = unsafe { &*pointer };
        // During one supported execution scope the attached state cannot change.
        // Most line events therefore need no CPython TLS or frame-field access.
        if binding.epoch.load(Ordering::Relaxed) == epoch
            && binding.state.load(Ordering::Relaxed) != 0
        {
            return Ok(());
        }
    }
    let state = unsafe { ffi::PyThreadState_Get() } as usize;
    let status = unsafe { ptr::read_unaligned((state + layout::THREAD_STATUS) as *const u32) };
    let frame = unsafe { word(state, layout::THREAD_FRAME) };
    if status as usize & layout::THREAD_FINALIZING != 0 || !unsafe { has_python_frame(frame) } {
        clear();
        return Ok(());
    }
    if pointer.is_null() {
        pointer = Box::into_raw(Box::new(Binding {
            state: AtomicUsize::new(0),
            epoch: AtomicUsize::new(0),
        }));
        let key = (KEY.load(Ordering::Relaxed) - 1) as libc::pthread_key_t;
        if unsafe { libc::pthread_setspecific(key, pointer.cast()) } != 0 {
            unsafe { drop(Box::from_raw(pointer)) };
            return Err("cannot publish Signal's thread binding");
        }
    }
    let binding = unsafe { &*pointer };
    // Only this thread writes the cell. Clear before changing epochs so a signal
    // sees an old ineligible cell, zero, or a completely published current binding.
    binding.state.store(0, Ordering::Release);
    binding.epoch.store(epoch, Ordering::Release);
    binding.state.store(state, Ordering::Release);
    Ok(())
}

/// Retire a binding before its last Python frame returns, yields, or unwinds.
/// Nested calls retain the binding for native work that resumes in their caller.
///
/// # Safety
/// Same attached-thread/event protocol as [`bind`]. CPython must still have the
/// departing frame as current, before clearing it (the sys.monitoring exit events).
pub unsafe fn leave() {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    let state = unsafe { ffi::PyThreadState_Get() } as usize;
    let frame = unsafe { word(state, layout::THREAD_FRAME) };
    if !unsafe { python_frame(frame) }
        || !unsafe { has_python_frame(word(frame, layout::PREVIOUS)) }
    {
        clear();
    }
}

/// Return the current session's recorded state address, or zero for no binding.
/// This only reads profiler-owned storage; it does not dereference that address.
/// Successful [`start`] is a caller precondition, including from signal handlers.
pub fn current() -> usize {
    // pthread_getspecific is a direct TSD lookup on supported Linux/Darwin libcs,
    // without allocation, a dynamic TLS resolver, a Python call, or a syscall.
    let pointer = slot();
    if pointer.is_null() {
        return 0;
    }
    // The interrupted thread alone owns this allocation. All fields use atomics;
    // no exclusive Rust reference can overlap the handler's read.
    let binding = unsafe { &*pointer };
    if binding.epoch.load(Ordering::Acquire) != ACTIVE.load(Ordering::Acquire) {
        return 0;
    }
    binding.state.load(Ordering::Acquire)
}

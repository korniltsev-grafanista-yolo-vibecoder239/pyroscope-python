//! CPython 3.15 uses compiler TLS for its bound (including detached) GIL state.
//! Discover the slot once with the GIL, then read only the interrupted thread's TLS.
//! Thread IDs and CPython's bound flags can survive OS-thread teardown and are not identity proofs.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::{Error, read_usize};

#[cfg(target_os = "linux")]
#[path = "thread_state/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "thread_state/macos.rs"]
mod platform;

pub(super) struct Slot {
    // ELF module ID on Linux, pthread key + 1 on macOS. Zero is reserved for initialization.
    module: usize,
    offset: usize,
}

static MODULE: AtomicUsize = AtomicUsize::new(0);
static OFFSET: AtomicUsize = AtomicUsize::new(0);

pub(super) unsafe fn initialize() -> Result<(), &'static str> {
    if MODULE.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    // SAFETY: normal attached Python thread, with the GIL. Never run in a handler.
    let bound = unsafe { pyo3_ffi::PyGILState_GetThisThreadState() } as usize;
    if bound == 0 {
        return Err("no bound Python state while preparing compiler TLS");
    }
    // The attached and bound slots usually contain the same pointer. Detaching
    // clears only the attached slot, unambiguously identifying the bound one.
    let saved = unsafe { pyo3_ffi::PyEval_SaveThread() };
    let result = unsafe { platform::find(bound) };
    // Restore on *every* error path as well as success.
    unsafe { pyo3_ffi::PyEval_RestoreThread(saved) };
    let slot = result?;
    if read_slot(&slot).map_err(|_| "unreadable compiler TLS during initialization")? != bound {
        return Err("compiler TLS ABI validation failed");
    }
    OFFSET.store(slot.offset, Ordering::Relaxed);
    MODULE.store(slot.module, Ordering::Release);
    Ok(())
}

fn read_slot(slot: &Slot) -> Result<usize, Error> {
    let base = platform::base(slot.module)?;
    if base == 0 {
        return Ok(0); // This native thread has not allocated the Python module's TLS.
    }
    read_usize(base, slot.offset)
}

pub(super) fn current() -> Result<usize, Error> {
    // Successful preparation is a caller precondition. No initialization or
    // recovery-handler checks, allocation, loader calls or state-list searches.
    let module = MODULE.load(Ordering::Acquire);
    read_slot(&Slot {
        module,
        offset: OFFSET.load(Ordering::Relaxed),
    })
}

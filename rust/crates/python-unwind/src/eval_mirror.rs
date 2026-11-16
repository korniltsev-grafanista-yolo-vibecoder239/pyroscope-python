//! An SDK-owned mirror, populated by PEP 523 on normal threads. The capsule in
//! PyThreadState.dict retains a weak watcher of threading_local_sentinel. That
//! sentinel's death invalidates the binding even if the dictionary is retained,
//! and waits for current readers before state deletion. A pthread value retains
//! the mirror until replacement/native exit.
//! There are no thread identity comparisons or reads of CPython's TLS layout.

use core::ffi::{CStr, c_char, c_void};

unsafe extern "C" {
    fn pyroscope_mirror_install() -> *const c_char;
    fn pyroscope_mirror_uninstall();
    fn pyroscope_mirror_acquire(guard: *mut *mut c_void) -> *mut pyo3_ffi::PyThreadState;
    fn pyroscope_mirror_release(guard: *mut c_void);
}

/// Install the evaluation hook and seed the configuring thread's mirror.
///
/// # Safety
/// Hold the GIL in the main interpreter, on a normal thread, with Python alive
/// until [`uninstall`] and every active walk finish. Only one session may own
/// this hook. Keep the SDK loaded; neither another evaluator nor an embedding
/// application may replace it or modify its private thread-dictionary capsule.
pub unsafe fn install() -> Result<(), &'static str> {
    let error = unsafe { pyroscope_mirror_install() };
    if error.is_null() {
        // Resolve even the native helper bindings before any signal can call them.
        drop(ReadGuard::acquire());
        Ok(())
    } else {
        // The C shim returns only pointers to static, ASCII error strings.
        Err(unsafe { CStr::from_ptr(error) }.to_str().unwrap())
    }
}

/// Remove the hook once sampling stops. In-flight callbacks and lifetime tokens
/// remain valid, including across restarts; the pthread key is never recycled.
///
/// # Safety
/// Hold the GIL in the same live main interpreter as [`install`].
pub unsafe fn uninstall() {
    unsafe { pyroscope_mirror_uninstall() };
}

pub(super) struct ReadGuard {
    pub state: usize,
    token: *mut c_void,
}

impl ReadGuard {
    pub fn acquire() -> Self {
        let mut token = core::ptr::null_mut();
        // Installation is a caller precondition. This only reads existing TSD
        // and increments a lock-free reader count; it cannot allocate or wait.
        let state = unsafe { pyroscope_mirror_acquire(&mut token) } as usize;
        Self { state, token }
    }
}

impl Drop for ReadGuard {
    fn drop(&mut self) {
        unsafe { pyroscope_mirror_release(self.token) };
    }
}

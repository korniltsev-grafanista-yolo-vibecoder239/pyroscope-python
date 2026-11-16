use core::ffi::{c_int, c_void};

unsafe extern "C" {
    fn pyroscope_signal_prepare() -> c_int;
    fn pyroscope_signal_acquire(cookie: *mut *mut c_void) -> *mut pyo3_ffi::PyThreadState;
    fn pyroscope_signal_release(cookie: *mut c_void);
}

/// Initializes the 3.15 evaluator publisher and registers the configuring thread.
/// A failure leaves a Python exception set. Older versions need no preparation.
///
/// # Safety
/// Call on a normal thread with the GIL, in the main interpreter, before starting
/// any sampling. Keep the extension and its evaluator loaded for the process lifetime.
pub unsafe fn prepare_thread_lookup() -> bool {
    unsafe { pyroscope_signal_prepare() == 0 }
}

pub(super) struct Current {
    pub state: usize,
    cookie: *mut c_void,
}

impl Current {
    pub fn acquire() -> Self {
        let mut cookie = core::ptr::null_mut();
        // SAFETY: preparation is a caller precondition. No Python API is called.
        let state = unsafe { pyroscope_signal_acquire(&raw mut cookie) } as usize;
        Self { state, cookie }
    }
}

impl Drop for Current {
    fn drop(&mut self) {
        // SAFETY: the key still owns the lease; this releases only a reader count.
        unsafe { pyroscope_signal_release(self.cookie) };
    }
}

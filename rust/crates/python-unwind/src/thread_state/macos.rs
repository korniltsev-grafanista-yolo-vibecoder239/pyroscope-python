use super::{Error, Slot, read_usize};

unsafe extern "C" {
    fn getsectiondata(
        header: *const libc::c_void,
        segment: *const libc::c_char,
        section: *const libc::c_char,
        size: *mut libc::c_ulong,
    ) -> *const u8;
}

// Mach-O's thread-variable descriptors contain a resolver, a module pthread
// key, and an offset into that key's per-thread block. Never invoke the resolver
// in a signal handler: it can allocate a block on first access.
// This is the Mach-O TLV descriptor ABI used by dyld's _tlv_bootstrap.
pub(super) unsafe fn find(bound: usize) -> Result<Slot, &'static str> {
    let mut info: libc::Dl_info = unsafe { core::mem::zeroed() };
    let function = pyo3_ffi::PyGILState_GetThisThreadState as *const () as *const libc::c_void;
    if unsafe { libc::dladdr(function, &mut info) } == 0 {
        return Err("cannot locate CPython's Mach-O image");
    }
    let mut found = None;
    for segment in [c"__DATA", c"__DATA_CONST"] {
        let mut size = 0;
        // SAFETY: dladdr supplies the loaded image header. getsectiondata applies its slide.
        let descriptors = unsafe {
            getsectiondata(
                info.dli_fbase,
                segment.as_ptr(),
                c"__thread_vars".as_ptr(),
                &mut size,
            )
        };
        if descriptors.is_null() {
            continue;
        }
        if size > 1024 * 1024 || size % 24 != 0 {
            return Err("unsupported Mach-O TLS descriptor section");
        }
        for offset in (0..size as usize).step_by(24) {
            // SAFETY: dyld owns these descriptors; accessing the GIL-state API on
            // this normal thread has already initialized the Python TLS module.
            let key = read_usize(descriptors as usize, offset + 8)
                .map_err(|_| "unreadable Mach-O TLS descriptor")?;
            let slot_offset = read_usize(descriptors as usize, offset + 16)
                .map_err(|_| "unreadable Mach-O TLS descriptor")?;
            let base = unsafe { libc::pthread_getspecific(key as libc::pthread_key_t) } as usize;
            if base != 0
                && read_usize(base, slot_offset).map_err(|_| "unreadable CPython TLS image")?
                    == bound
            {
                if found.is_some() {
                    return Err("ambiguous CPython bound-state TLS slot");
                }
                found = Some(Slot {
                    module: key.checked_add(1).ok_or("invalid Mach-O TLS key")?,
                    offset: slot_offset,
                });
            }
        }
    }
    found.ok_or("cannot identify CPython's bound-state TLS slot")
}

pub(super) fn base(module: usize) -> Result<usize, Error> {
    // SAFETY: preparation recorded this existing dyld-owned key and resolved
    // pthread_getspecific on a normal thread. It never allocates TLS or uses an
    // old thread's identity. A new native thread has a null value for the key.
    Ok(unsafe { libc::pthread_getspecific((module - 1) as libc::pthread_key_t) } as usize)
}

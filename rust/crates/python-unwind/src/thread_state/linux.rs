use super::{Error, Slot, read_usize};

/// Only used during normal-thread initialization. dl_iterate_phdr may take loader locks
/// and, on musl, allocate TLS. The handler never calls it or __tls_get_addr.
pub(super) unsafe fn find(bound: usize) -> Result<Slot, &'static str> {
    struct Image {
        function: usize,
        module: usize,
        base: usize,
        size: usize,
    }
    unsafe extern "C" fn visit(
        info: *mut libc::dl_phdr_info,
        size: usize,
        arg: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: dl_iterate_phdr supplies valid metadata and our caller owns Image.
        unsafe {
            if size < core::mem::size_of::<libc::dl_phdr_info>() {
                return 0;
            }
            let image = &mut *arg.cast::<Image>();
            let info = &*info;
            let headers = core::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum));
            let contains_function = headers.iter().any(|p| {
                p.p_type == libc::PT_LOAD
                    && image
                        .function
                        .wrapping_sub(info.dlpi_addr as usize + p.p_vaddr as usize)
                        < p.p_memsz as usize
            });
            if !contains_function {
                return 0;
            }
            if let Some(tls) = headers.iter().find(|p| p.p_type == libc::PT_TLS) {
                image.module = info.dlpi_tls_modid;
                image.base = info.dlpi_tls_data as usize;
                image.size = tls.p_memsz as usize;
            }
            1
        }
    }
    let mut image = Image {
        function: pyo3_ffi::PyGILState_GetThisThreadState as *const () as usize,
        module: 0,
        base: 0,
        size: 0,
    };
    unsafe { libc::dl_iterate_phdr(Some(visit), (&raw mut image).cast()) };
    if image.module == 0 || image.base == 0 || image.size > 1024 * 1024 {
        return Err("cannot locate CPython's ELF TLS image");
    }
    if base(image.module).map_err(|_| "cannot read the ELF TLS vector")? != image.base {
        return Err("ELF TLS ABI validation failed");
    }
    let mut found = None;
    for offset in (0..image.size.saturating_sub(7)).step_by(8) {
        // Copy bytes without source references, including any padding in the TLS image.
        if read_usize(image.base, offset).map_err(|_| "unreadable CPython TLS image")? == bound {
            if found.is_some() {
                return Err("ambiguous CPython bound-state TLS slot");
            }
            found = Some(Slot {
                module: image.module,
                offset,
            });
        }
    }
    found.ok_or("cannot identify CPython's bound-state TLS slot")
}

#[inline]
fn thread_pointer() -> usize {
    let pointer;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: the x86_64 pthread ABI stores the TCB's own address at FS:0.
    unsafe {
        core::arch::asm!("mov {}, fs:0", out(reg) pointer, options(nostack, readonly, preserves_flags));
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: reading the userspace thread pointer neither traps nor enters the kernel.
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) pointer, options(nostack, nomem, preserves_flags));
    }
    pointer
}

pub(super) fn base(module: usize) -> Result<usize, Error> {
    let pointer = thread_pointer();
    #[cfg(target_arch = "x86_64")]
    let dtv = read_usize(pointer, 8)?;
    #[cfg(all(target_arch = "aarch64", target_env = "gnu"))]
    let dtv = read_usize(pointer, 0)?;
    #[cfg(all(target_arch = "aarch64", target_env = "musl"))]
    let dtv = read_usize(pointer.wrapping_sub(8), 0)?;
    dtv_base(dtv, module)
}

fn dtv_base(dtv: usize, module: usize) -> Result<usize, Error> {
    // The GNU DTV has two-word entries, with capacity in dtv[-1].counter.
    // musl uses one-word entries with its module count in dtv[0].
    // Read existing storage only. Never update a stale DTV generation or allocate a slot.
    #[cfg(target_env = "gnu")]
    let (capacity, stride) = (read_usize(dtv.wrapping_sub(16), 0)?, 16);
    #[cfg(target_env = "musl")]
    let (capacity, stride) = (read_usize(dtv, 0)?, 8);
    if module > capacity {
        return Ok(0);
    }
    let value = read_usize(dtv, module.wrapping_mul(stride))?;
    // glibc's TLS_DTV_UNALLOCATED is (void *) -1; musl uses null/missing entries.
    Ok(if value == usize::MAX { 0 } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_tls_is_not_allocated_or_dereferenced() {
        unsafe { kindasafe_init::init() }.unwrap();
        #[cfg(target_env = "gnu")]
        let words = [2usize, 0, 123, 0, usize::MAX, 0, 456, 0];
        #[cfg(target_env = "gnu")]
        let dtv = words.as_ptr().wrapping_add(2) as usize;
        #[cfg(target_env = "musl")]
        let words = [2usize, 0, 456];
        #[cfg(target_env = "musl")]
        let dtv = words.as_ptr() as usize;
        assert_eq!(dtv_base(dtv, 1), Ok(0));
        assert_eq!(dtv_base(dtv, 2), Ok(456));
        assert_eq!(dtv_base(dtv, 3), Ok(0));
    }
}

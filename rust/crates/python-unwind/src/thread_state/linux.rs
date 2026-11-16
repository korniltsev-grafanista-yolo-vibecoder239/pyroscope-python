//! Startup TLS has a fixed offset from the architectural thread pointer on ELF
//! Linux, on both glibc and musl. Unlike the DTV, that storage never moves during
//! dlopen/TLS allocation. Only accept the executable or a direct DT_NEEDED
//! dependency of the executable; dlopen-loaded libpython is unsupported.

use super::{Error, PrepareError, find_offset, read_usize};
use core::{ffi::CStr, mem, ptr};

#[derive(Clone, Copy)]
struct Module {
    base: usize,
    headers: *const libc::Elf64_Phdr,
    count: usize,
    name: *const libc::c_char,
    tls: usize,
    tls_size: usize,
}

impl Module {
    unsafe fn headers(&self) -> &[libc::Elf64_Phdr] {
        unsafe { core::slice::from_raw_parts(self.headers, self.count) }
    }

    unsafe fn strings_and_dynamic(&self) -> Option<(&[u8], &[Dynamic])> {
        let headers = unsafe { self.headers() };
        let dynamic = headers.iter().find(|h| h.p_type == libc::PT_DYNAMIC)?;
        let entries = unsafe {
            core::slice::from_raw_parts(
                self.base.checked_add(dynamic.p_vaddr as usize)? as *const Dynamic,
                dynamic.p_memsz as usize / mem::size_of::<Dynamic>(),
            )
        };
        let value = |tag| {
            entries
                .iter()
                .take_while(|e| e.tag != 0)
                .find(|e| e.tag == tag)
                .map(|e| e.value)
        };
        let address = value(5)?; // DT_STRTAB
        // glibc can relocate dynamic entries in place; musl leaves them relative.
        let absolute = headers.iter().any(|h| {
            h.p_type == libc::PT_LOAD
                && address >= self.base.wrapping_add(h.p_vaddr as usize)
                && address
                    < self
                        .base
                        .wrapping_add(h.p_vaddr as usize)
                        .wrapping_add(h.p_memsz as usize)
        });
        let address = if absolute {
            address
        } else {
            self.base.checked_add(address)?
        };
        let size = value(10)?; // DT_STRSZ
        Some((
            unsafe { core::slice::from_raw_parts(address as *const u8, size) },
            entries,
        ))
    }

    unsafe fn soname(&self) -> Option<&CStr> {
        if let Some((strings, entries)) = unsafe { self.strings_and_dynamic() }
            && let Some(entry) = entries
                .iter()
                .take_while(|e| e.tag != 0)
                .find(|e| e.tag == 14)
        // DT_SONAME
        {
            return CStr::from_bytes_until_nul(strings.get(entry.value..)?).ok();
        }
        let name = unsafe { CStr::from_ptr(self.name) }.to_bytes_with_nul();
        let start = name.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
        CStr::from_bytes_with_nul(&name[start..]).ok()
    }

    unsafe fn needs(&self, library: &CStr) -> bool {
        let Some((strings, entries)) = (unsafe { self.strings_and_dynamic() }) else {
            return false;
        };
        entries
            .iter()
            .take_while(|e| e.tag != 0)
            .filter(|e| e.tag == 1)
            .any(|e| {
                // DT_NEEDED
                strings
                    .get(e.value..)
                    .and_then(|s| CStr::from_bytes_until_nul(s).ok())
                    == Some(library)
            })
    }
}

#[repr(C)]
struct Dynamic {
    tag: isize,
    value: usize,
}

pub(super) fn locate(bound: usize) -> Result<usize, PrepareError> {
    struct Search {
        main: Option<Module>,
        python: Option<Module>,
        getter: usize,
        program_headers: usize,
    }
    unsafe extern "C" fn visit(
        info: *mut libc::dl_phdr_info,
        size: usize,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        if size < mem::offset_of!(libc::dl_phdr_info, dlpi_tls_data) + mem::size_of::<usize>() {
            return 0;
        }
        let info = unsafe { &*info };
        if info.dlpi_phdr.is_null() || info.dlpi_phnum == 0 {
            return 0;
        }
        let search = unsafe { &mut *data.cast::<Search>() };
        let mut module = Module {
            base: info.dlpi_addr as usize,
            headers: info.dlpi_phdr,
            count: info.dlpi_phnum as usize,
            name: info.dlpi_name,
            tls: info.dlpi_tls_data as usize,
            tls_size: 0,
        };
        let headers = unsafe { module.headers() };
        let owns_getter = headers.iter().any(|h| {
            h.p_type == libc::PT_LOAD
                && search.getter >= module.base.wrapping_add(h.p_vaddr as usize)
                && search.getter
                    < module
                        .base
                        .wrapping_add(h.p_vaddr as usize)
                        .wrapping_add(h.p_memsz as usize)
        });
        module.tls_size = headers
            .iter()
            .find(|h| h.p_type == libc::PT_TLS)
            .map_or(0, |h| h.p_memsz as usize);
        if module.headers as usize == search.program_headers {
            search.main = Some(module);
        }
        if owns_getter {
            search.python = Some(module);
        }
        0
    }
    let mut search = Search {
        main: None,
        python: None,
        getter: pyo3_ffi::PyGILState_GetThisThreadState as *const () as usize,
        // AT_PHDR identifies the actual executable, including in the presence
        // of dlmopen namespaces whose first object is a dynamically loaded DSO.
        program_headers: unsafe { libc::getauxval(libc::AT_PHDR) } as usize,
    };
    unsafe { libc::dl_iterate_phdr(Some(visit), ptr::from_mut(&mut search).cast()) };
    let python = search.python.ok_or(PrepareError::UnsupportedTls)?;
    let main = search.main.ok_or(PrepareError::UnsupportedTls)?;
    let startup =
        python.base == main.base || unsafe { python.soname().is_some_and(|name| main.needs(name)) };
    if !startup || python.tls == 0 || python.tls_size == 0 {
        return Err(PrepareError::UnsupportedTls);
    }
    let offset = find_offset(python.tls, python.tls_size, bound)?;
    Ok(python
        .tls
        .wrapping_add(offset)
        .wrapping_sub(thread_pointer()))
}

#[inline(always)]
fn thread_pointer() -> usize {
    let pointer;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mov {}, fs:[0]", out(reg) pointer, options(nostack, preserves_flags, readonly));
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) pointer, options(nostack, preserves_flags, nomem));
    }
    pointer
}

#[inline(always)]
pub(super) fn read(offset: usize) -> Result<usize, Error> {
    read_usize(thread_pointer(), offset)
}

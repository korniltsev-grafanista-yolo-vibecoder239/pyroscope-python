//! Darwin's TLV descriptors identify a pthread key and an offset in that image's
//! per-thread allocation. Resolve descriptors only during startup. The handler
//! uses pthread_getspecific, which is a direct TSD read in libpthread, and never
//! calls the allocating TLV thunk. Supports both original and 2025 v2 thunks.
//!
//! See <https://github.com/apple-oss-distributions/dyld/blob/main/libdyld/ThreadLocalVariables.h>
//! and <https://github.com/apple-oss-distributions/libpthread/blob/main/src/pthread_tsd.c>.

use super::{Error, MAX_TLS_BYTES, PrepareError, read_usize};
use core::{mem, ptr};

// These are the 64-bit Mach-O ABI structures from mach-o/loader.h. The OS has
// validated the loaded image. It must remain loaded throughout discovery/walks.
#[repr(C)]
struct Header {
    magic: u32,
    cpu: u32,
    subtype: u32,
    kind: u32,
    commands: u32,
    command_bytes: u32,
    flags: u32,
    reserved: u32,
}
#[repr(C)]
struct Command {
    kind: u32,
    size: u32,
}
#[repr(C)]
struct Segment {
    command: Command,
    name: [u8; 16],
    address: u64,
    size: u64,
    file_offset: u64,
    file_size: u64,
    max_protection: u32,
    protection: u32,
    sections: u32,
    flags: u32,
}
#[repr(C)]
struct Section {
    name: [u8; 16],
    segment: [u8; 16],
    address: u64,
    size: u64,
    offset: u32,
    alignment: u32,
    relocations: u32,
    relocation_count: u32,
    flags: u32,
    reserved: [u32; 3],
}

pub(super) fn locate(bound: usize) -> Result<usize, PrepareError> {
    let mut info = mem::MaybeUninit::<libc::Dl_info>::uninit();
    if unsafe {
        libc::dladdr(
            pyo3_ffi::PyGILState_GetThisThreadState as *const () as *const _,
            info.as_mut_ptr(),
        )
    } == 0
    {
        return Err(PrepareError::UnsupportedTls);
    }
    let info = unsafe { info.assume_init() };
    let header = unsafe { &*info.dli_fbase.cast::<Header>() };
    if header.magic != 0xfeedfacf || header.command_bytes > (1 << 24) {
        return Err(PrepareError::UnsupportedTls);
    }
    let first = info.dli_fbase as usize + mem::size_of::<Header>();
    let end = first + header.command_bytes as usize;
    let mut slide = None;
    // Find __TEXT to translate section virtual addresses into mapped addresses.
    let mut address = first;
    for _ in 0..header.commands {
        let command = unsafe { &*(address as *const Command) };
        if command.size < 8 || address + command.size as usize > end {
            return Err(PrepareError::UnsupportedTls);
        }
        if command.kind == 0x19 && command.size as usize >= mem::size_of::<Segment>() {
            let segment = unsafe { &*(address as *const Segment) };
            if segment.name == *b"__TEXT\0\0\0\0\0\0\0\0\0\0" {
                slide = Some((info.dli_fbase as usize).wrapping_sub(segment.address as usize));
            }
        }
        address += command.size as usize;
    }
    let slide = slide.ok_or(PrepareError::UnsupportedTls)?;
    let mut found = None;
    address = first;
    for _ in 0..header.commands {
        let command = unsafe { &*(address as *const Command) };
        if command.kind == 0x19 {
            let segment = unsafe { &*(address as *const Segment) };
            if mem::size_of::<Segment>() + segment.sections as usize * mem::size_of::<Section>()
                > command.size as usize
            {
                return Err(PrepareError::UnsupportedTls);
            }
            for index in 0..segment.sections as usize {
                let section = unsafe {
                    &*((address + mem::size_of::<Segment>() + index * mem::size_of::<Section>())
                        as *const Section)
                };
                if section.flags & 0xff != 0x13 {
                    continue;
                } // S_THREAD_LOCAL_VARIABLES
                if section.size > MAX_TLS_BYTES as u64 || section.size % 24 != 0 {
                    return Err(PrepareError::UnsupportedTls);
                }
                let start = (section.address as usize).wrapping_add(slide);
                for descriptor in (start..start + section.size as usize).step_by(24) {
                    let thunk: unsafe extern "C" fn(*const usize) -> *const usize =
                        unsafe { mem::transmute(ptr::read(descriptor as *const usize)) };
                    let slot = unsafe { thunk(descriptor as *const usize) } as usize;
                    if read_usize(slot, 0).map_err(|_| PrepareError::Read)? != bound {
                        continue;
                    }
                    let packed_key = unsafe { ptr::read((descriptor + 8) as *const usize) };
                    // The low 32 bits hold the key in both ABI versions. The
                    // old ABI's upper bits are zero; v2 packs the offset there.
                    let key = packed_key as u32 as libc::pthread_key_t;
                    let base = unsafe { libc::pthread_getspecific(key) } as usize;
                    let offset = slot
                        .checked_sub(base)
                        .filter(|&offset| base != 0 && offset < MAX_TLS_BYTES)
                        .ok_or(PrepareError::UnsupportedTls)?;
                    let old_offset = unsafe { ptr::read((descriptor + 16) as *const usize) };
                    if offset != old_offset && offset != packed_key >> 32 {
                        return Err(PrepareError::UnsupportedTls);
                    }
                    if found.replace((key as usize, offset)).is_some() {
                        return Err(PrepareError::AmbiguousTls);
                    }
                }
            }
        }
        address += command.size as usize;
    }
    // The validated offset uses at most 20 bits, and both descriptor ABIs use
    // a 32-bit key. Publish both together, without a torn key/offset pair.
    found
        .map(|(key, offset)| (key << 20) | offset)
        .ok_or(PrepareError::UnsupportedTls)
}

#[inline(always)]
pub(super) fn read(location: usize) -> Result<usize, Error> {
    // Warmed in locate(), before sampling. This reads the current thread's TSD;
    // a newly created thread has a null value even if its pthread ID is reused.
    let key = (location >> 20) as libc::pthread_key_t;
    let offset = location & (MAX_TLS_BYTES - 1);
    let base = unsafe { libc::pthread_getspecific(key) } as usize;
    if base == 0 {
        Ok(0)
    } else {
        read_usize(base, offset)
    }
}

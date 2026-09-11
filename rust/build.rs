use cmake::Config;
use std::env;
use std::path::{Path, PathBuf};

const NATIVE_SOURCES: &[&str] = &[
    "CMakeLists.txt",
    "BundleStaticLibrary.cmake",
    "cpu",
    "vendor/dd-trace-py",
    "Pyroscope.h",
    "_memalloc.cpp",
    "_memalloc_debug.h",
    "_memalloc_frame.h",
    "_memalloc_gc_guard.hpp",
    "_memalloc_heap.cpp",
    "_memalloc_heap.h",
    "_memalloc_reentrant.cpp",
    "_memalloc_reentrant.h",
    "_memalloc_tb.cpp",
    "_memalloc_tb.h",
    "_pymacro.h",
    "profiling_helpers/frame_accessors.h",
    "profiling_helpers/linetable_parser.h",
    "profiling_helpers/version_compat.h",
];

fn main() {
    // Miri interprets Rust and cannot execute the native profiler.
    if env::var_os("CARGO_CFG_MIRI").is_some() {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let cpp_dir = manifest_dir.join("../cpp");
    let cpp_dir = cpp_dir.canonicalize().unwrap();

    rerun_if_native_sources_changed(&manifest_dir, &cpp_dir);

    // PYROSCOPE_PATCH: cpu-build — always compile CPU sources, independently of memory.
    let mut cfg = Config::new(&cpp_dir);
    cfg.define(
        "PYROSCOPE_BUILD_MEMORY",
        if cfg!(feature = "memory") {
            "ON"
        } else {
            "OFF"
        },
    );

    println!("cargo:rerun-if-env-changed=Python3_ROOT_DIR");
    let python_root = env::var_os("Python3_ROOT_DIR")
        .expect("Python3_ROOT_DIR must be set (passed from setup.py) so the C++ profilers are compiled against the target Python version");
    cfg.define("Python3_ROOT_DIR", &python_root);
    println!("cargo:rerun-if-env-changed=Python3_EXECUTABLE");
    let python_executable = env::var_os("Python3_EXECUTABLE")
        .expect("Python3_EXECUTABLE must be set (passed from setup.py) so the C++ profilers are compiled against the exact target Python interpreter");
    cfg.define("Python3_EXECUTABLE", &python_executable);
    cfg.define("Python3_FIND_STRATEGY", "LOCATION");

    let dst = cfg.build();

    println!("cargo:rustc-link-search=native={}", dst.display());
    if cfg!(feature = "memory") {
        println!("cargo:rustc-link-lib=static=datadog_mem_profiler_bundled");
    }

    // PYROSCOPE_PATCH: cpu-build — retain the inactive stack module and its sampler
    // dependencies in the cdylib. Keep the archive itself out of Rust test links:
    // shared C++ template symbols can otherwise pull in Python-dependent objects.
    println!(
        "cargo:rustc-link-arg-cdylib={}",
        dst.join("libdatadog_cpu_profiler.a").display()
    );

    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "macos" {
        println!("cargo:rustc-link-arg-cdylib=-Wl,-u,_PyInit__stack");
        println!("cargo:rustc-link-arg-cdylib=-lc++");
        println!("cargo:rustc-link-lib=static=c++");
        println!("cargo:rustc-link-arg=-undefined");
        println!("cargo:rustc-link-arg=dynamic_lookup");
    } else {
        println!("cargo:rustc-link-arg-cdylib=-Wl,-u,PyInit__stack");
        // Repeat the runtime after the CPU archive for linkers that scan once.
        println!("cargo:rustc-link-arg-cdylib=-Wl,-Bstatic");
        println!("cargo:rustc-link-arg-cdylib=-lstdc++");
        println!("cargo:rustc-link-arg-cdylib=-Wl,-Bdynamic");
        println!("cargo:rustc-link-lib=static=stdc++");
    }
}

fn rerun_if_native_sources_changed(manifest_dir: &Path, cpp_dir: &Path) {
    for source in NATIVE_SOURCES {
        let path = cpp_dir.join(source);
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let ffi_header = manifest_dir.join("include/pyroscope_ffi.h");
    println!("cargo:rerun-if-changed={}", ffi_header.display());
}

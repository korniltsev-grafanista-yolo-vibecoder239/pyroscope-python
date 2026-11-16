fn main() {
    pyo3_build_config::use_pyo3_cfgs();
    if pyo3_build_config::get().version().minor != 15 {
        return;
    }
    println!("cargo:rerun-if-env-changed=Python3_EXECUTABLE");
    println!("cargo:rerun-if-changed=src/thread_state.c");
    println!("cargo:rerun-if-changed=tests/state_lifecycle.c");
    let python = std::env::var_os("Python3_EXECUTABLE").expect("target Python interpreter");
    let output = std::process::Command::new(python)
        .args([
            "-c",
            "import sysconfig; print(sysconfig.get_paths()['include'])",
        ])
        .output()
        .expect("query target Python headers");
    assert!(output.status.success(), "query target Python headers");
    cc::Build::new()
        .file("src/thread_state.c")
        // A separate archive member: linked only by the native lifecycle tests.
        .file("tests/state_lifecycle.c")
        .include(String::from_utf8(output.stdout).unwrap().trim())
        .std("c11")
        .warnings_into_errors(true)
        .flag_if_supported("-fvisibility=hidden")
        .compile("signal_thread_state");
}

fn main() {
    pyo3_build_config::use_pyo3_cfgs();
    if pyo3_build_config::get().version().minor < 15 {
        return;
    }
    println!("cargo:rerun-if-env-changed=Python3_EXECUTABLE");
    println!("cargo:rerun-if-changed=eval_mirror.c");
    let python = std::env::var_os("Python3_EXECUTABLE").expect("target Python interpreter");
    let output = std::process::Command::new(python)
        .args([
            "-c",
            "import sysconfig; print(sysconfig.get_paths()['include'])",
        ])
        .output()
        .expect("query Python headers");
    assert!(output.status.success(), "query Python headers: {output:?}");
    cc::Build::new()
        .file("eval_mirror.c")
        .include(String::from_utf8(output.stdout).unwrap().trim())
        .flag_if_supported("-std=c11")
        .warnings_into_errors(true)
        .compile("pyroscope_eval_mirror");
}

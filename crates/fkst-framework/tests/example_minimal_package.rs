use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).unwrap();
        }
    }
}

fn run_department(host: &Path, runtime: &Path, lua: &str, event: &str) -> Output {
    Command::new(framework_bin())
        .arg("run")
        .arg(host.join(lua))
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(host)
        .arg("--event")
        .arg(event)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", runtime)
        .output()
        .unwrap()
}

fn run_conformance(host: &Path, runtime: &Path) -> Output {
    Command::new(framework_bin())
        .arg("conformance")
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(host)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", runtime)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn minimal_package_loads_and_logs_event() {
    let host = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let package = repo_root().join("examples/minimal-package");
    copy_dir(&package, host.path());

    let conformance = run_conformance(host.path(), runtime.path());
    assert_success(&conformance);

    let logger = run_department(
        host.path(),
        runtime.path(),
        "departments/logger/main.lua",
        r#"{"type":"tick","payload":{}}"#,
    );
    assert_success(&logger);

    let err = stderr(&logger);
    assert!(
        err.lines().any(|line| {
            line.contains("TIMESTAMP=")
                && line.contains(" LEVEL=info ")
                && line.contains(" MSG=event received: tick")
        }),
        "stderr: {err}"
    );
}

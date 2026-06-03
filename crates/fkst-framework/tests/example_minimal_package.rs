use base64::Engine;
use serde_json::Value;
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn raised_entries(output: &Output) -> Vec<Value> {
    let out = stdout(output);
    let line = out
        .lines()
        .find(|line| line.starts_with("RAISED: "))
        .expect("missing RAISED line");
    let encoded = line.trim_start_matches("RAISED: ");
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(encoded)
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn file_count(root: &Path) -> usize {
    fn walk(path: &Path, count: &mut usize) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(&path, count);
            } else {
                *count += 1;
            }
        }
    }

    let mut count = 0;
    walk(root, &mut count);
    count
}

#[test]
fn minimal_package_scanner_raises_work_and_worker_logs_completion() {
    let host = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let package = repo_root().join("examples/minimal-package");
    copy_dir(&package, host.path());

    let scanner = run_department(
        host.path(),
        runtime.path(),
        "departments/scanner/main.lua",
        r#"{"type":"reconcile_tick","payload":{}}"#,
    );
    assert_success(&scanner);
    let raised = raised_entries(&scanner);
    assert_eq!(raised.len(), 1);
    assert_eq!(raised[0]["queue"], "work");
    assert_eq!(raised[0]["payload"]["id"], "req-001");
    assert_eq!(raised[0]["payload"]["request_path"], "requests/req-001.md");

    let host_file_count_before_worker = file_count(host.path());
    let worker = run_department(
        host.path(),
        runtime.path(),
        "departments/worker/main.lua",
        r#"{"type":"work","payload":{"id":"req-001","request_path":"requests/req-001.md"}}"#,
    );
    assert_success(&worker);
    let host_file_count_after_worker = file_count(host.path());
    assert_eq!(
        host_file_count_before_worker, host_file_count_after_worker,
        "worker must not write runtime files into host"
    );

    let err = stderr(&worker);
    assert!(
        err.lines().any(|line| {
            line.contains("TIMESTAMP=")
                && line.contains(" LEVEL=info ")
                && line.contains(" MSG=work completed: req-001")
        }),
        "stderr: {err}"
    );
}

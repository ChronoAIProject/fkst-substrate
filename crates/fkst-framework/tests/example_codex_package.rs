use base64::Engine;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod support;

use support::manifest_fixture::write_single_package_workspace;

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

fn copy_codex_package(host: &Path) {
    let package = repo_root().join("examples/codex-package");
    copy_dir(&package, host);
    write_single_package_workspace(host);
}

fn run_conformance(host: &Path, runtime: &Path) -> Output {
    Command::new(framework_bin())
        .arg("conformance")
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(host)
        .current_dir(host)
        .env_remove("FKST_SUPERVISOR_PID")
        .env("FKST_RUNTIME_ROOT", runtime)
        .output()
        .unwrap()
}

fn run_department(host: &Path, runtime: &Path, lua: &str, event: &str) -> Command {
    let mut command = Command::new(framework_bin());
    command
        .arg("run")
        .arg(host.join(lua))
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(host)
        .arg("--event")
        .arg(event)
        .current_dir(host)
        .env_remove("FKST_SUPERVISOR_PID")
        .env("FKST_RUNTIME_ROOT", runtime);
    command
}

fn run_lua_tests(host: &Path, runtime: &Path) -> Output {
    Command::new(framework_bin())
        .arg("test")
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(host)
        .current_dir(host)
        .env_remove("FKST_SUPERVISOR_PID")
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

fn decode_raised(stdout: &str) -> Value {
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("RAISED: "))
        .unwrap_or_else(|| panic!("missing RAISED line in stdout: {stdout}"));
    let encoded = line.trim_start().trim_start_matches("RAISED: ").trim();
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(encoded)
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[cfg(unix)]
fn install_codex_script(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir).unwrap();
    let codex = dir.join("codex");
    std::fs::write(&codex, body).unwrap();
    let mut permissions = std::fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&codex, permissions).unwrap();
    codex
}

#[test]
fn codex_package_loads_graph() {
    let host = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    copy_codex_package(host.path());

    let conformance = run_conformance(host.path(), runtime.path());
    assert_success(&conformance);
    let out = stdout(&conformance);
    assert!(
        out.contains("loaded 1 departments, 1 raisers, 2 queues"),
        "stdout: {out}"
    );
}

#[cfg(unix)]
#[test]
fn codex_demo_raises_fake_codex_result() {
    let host = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let fake_bin = tempfile::tempdir().unwrap();
    copy_codex_package(host.path());
    let request_path = host.path().join("requests/manual.md");
    fs::write(&request_path, "hello task").unwrap();
    let prompt_path = runtime.path().join("codex-stdin.txt");
    install_codex_script(
        fake_bin.path(),
        &format!(
            r#"#!/bin/sh
cat > "{}"
echo '{{"type":"item.completed","item":{{"type":"agent_message","text":"FAKE_CODEX_OK"}}}}'
exit 0
"#,
            prompt_path.display()
        ),
    );

    let original_path = std::env::var("PATH").unwrap_or_default();
    let fake_path = format!("{}:{}", fake_bin.path().display(), original_path);
    let event = serde_json::json!({
        "queue": "codex_request",
        "payload": { "path": request_path }
    })
    .to_string();
    let output = run_department(
        host.path(),
        runtime.path(),
        "departments/codex_demo/main.lua",
        &event,
    )
    .env("PATH", fake_path)
    .env("FKST_CODEX_PERMIT_SLOTS", "4")
    .output()
    .unwrap();

    assert_success(&output);
    let logs = stderr(&output);
    assert!(logs.contains("codex exit_code=0"), "stderr: {logs}");
    assert!(logs.contains(" log="), "stderr: {logs}");

    let raised = decode_raised(&stdout(&output));
    assert_eq!(raised.as_array().unwrap().len(), 1, "raised={raised}");
    assert_eq!(raised[0]["queue"], "codex_result");
    assert_eq!(raised[0]["payload"]["exit_code"], 0);
    assert_eq!(raised[0]["payload"]["summary"], "FAKE_CODEX_OK");
    assert!(raised[0].get("ts").is_none(), "raised={raised}");

    let prompt = fs::read_to_string(&prompt_path).unwrap();
    assert!(prompt.contains("hello task"), "prompt={prompt}");
}

#[test]
fn codex_demo_lua_unit_tests_run_with_test_runner() {
    let host = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    copy_codex_package(host.path());

    let output = run_lua_tests(host.path(), runtime.path());

    assert_success(&output);
    let out = stdout(&output);
    assert!(
        out.contains("PASS departments/codex_demo/codex_demo_test.lua::test_build"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS departments/codex_demo/codex_demo_test.lua::test_parse"),
        "stdout: {out}"
    );
    assert!(out.contains("2 passed, 0 failed"), "stdout: {out}");
}

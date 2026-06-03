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

fn copy_codex_package(host: &Path) {
    copy_dir(&repo_root().join("examples/codex-package"), host);
}

fn run_lua_tests(host: &Path, package: &Path) -> Output {
    Command::new(framework_bin())
        .arg("test")
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(package)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn test_runner_runs_codex_package_tests() {
    let host = tempfile::tempdir().unwrap();
    copy_codex_package(host.path());

    let output = run_lua_tests(host.path(), host.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("PASS departments/codex_demo/codex_demo_test.lua::test_build"),
        "stdout: {out}"
    );
    assert!(out.contains("2 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn test_runner_runs_minimal_package_sanity_tests() {
    let package = repo_root().join("examples/minimal-package");

    let output = run_lua_tests(&package, &package);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_sanity"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_raises"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_nil"),
        "stdout: {out}"
    );
    assert!(out.contains("3 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn test_runner_continues_after_failure() {
    let host = tempfile::tempdir().unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("tests/failure_test.lua"),
        r#"
local t = fkst.test
return {
  test_a_pass = function() t.eq(1, 1) end,
  test_b_fail = function() t.eq(1, 2) end,
  test_c_pass = function() t.is_true(true) end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(host.path(), host.path());

    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("PASS tests/failure_test.lua::test_a_pass"),
        "stdout: {out}"
    );
    assert!(
        out.contains("FAIL tests/failure_test.lua::test_b_fail"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/failure_test.lua::test_c_pass"),
        "stdout: {out}"
    );
    assert!(out.contains("2 passed, 1 failed"), "stdout: {out}");
}

#[test]
fn test_surface_does_not_leak_to_production_run() {
    let host = tempfile::tempdir().unwrap();
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    let probe = host.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
function pipeline(event)
  assert(fkst == nil or fkst.test == nil, "test surface leaked to production")
end
"#,
    )
    .unwrap();

    let output = Command::new(framework_bin())
        .arg("run")
        .arg(&probe)
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--event")
        .arg("{}")
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
}

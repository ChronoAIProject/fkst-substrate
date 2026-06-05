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

fn run_lua_tests_with_packages(host: &Path, packages: &[&Path]) -> Output {
    let mut cmd = Command::new(framework_bin());
    cmd.arg("test").arg("--project-root").arg(host);
    for package in packages {
        cmd.arg("--package-root").arg(package);
    }
    cmd.current_dir(host)
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

fn run_command(host: &Path, lua: &Path) -> Command {
    let mut cmd = Command::new(framework_bin());
    cmd.arg("run")
        .arg(lua)
        .arg("--project-root")
        .arg(host)
        .arg("--event")
        .arg(r#"{"payload":{"value":"ok"}}"#)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .env_remove("FKST_PACKAGE_ROOT")
        .env_remove("FKST_PACKAGE_ROOTS");
    cmd
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
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_json_decode"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_json_decode_invalid_input_raises"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/run_department_test.lua::test_run_department_captures_raises"),
        "stdout: {out}"
    );
    assert!(out.contains("6 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn test_runner_isolates_each_test_file_to_its_owner_root() {
    let host = tempfile::tempdir().unwrap();
    let package_a = tempfile::tempdir().unwrap();
    let package_b = tempfile::tempdir().unwrap();

    for (package, label) in [(package_a.path(), "a"), (package_b.path(), "b")] {
        fs::create_dir_all(package.join("departments/probe")).unwrap();
        fs::create_dir_all(package.join("tests")).unwrap();
        fs::write(
            package.join("core.lua"),
            format!(r#"return {{ value = "{label}" }}"#),
        )
        .unwrap();
        fs::write(
            package.join("departments/probe/main.lua"),
            r#"
local core = require("core")
function pipeline(event)
  raise("seen", { value = core.value, expected = event.payload.expected })
end
"#,
        )
        .unwrap();
        fs::write(
            package.join("tests/owner_test.lua"),
            format!(
                r#"
local t = fkst.test
local core = require("core")
return {{
  test_require_core_uses_owner = function()
    t.eq(core.value, "{label}")
  end,
  test_run_department_uses_owner = function()
    local result = fkst.test.run_department("departments/probe/main.lua", {{ payload = {{ expected = "{label}" }} }})
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].payload.value, "{label}")
    t.eq(result.raises[1].payload.expected, "{label}")
  end,
}}
"#
            ),
        )
        .unwrap();
    }

    let output = run_lua_tests_with_packages(host.path(), &[package_a.path(), package_b.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert_eq!(
        out.matches("PASS tests/owner_test.lua::test_require_core_uses_owner")
            .count(),
        2,
        "stdout: {out}"
    );
    assert_eq!(
        out.matches("PASS tests/owner_test.lua::test_run_department_uses_owner")
            .count(),
        2,
        "stdout: {out}"
    );
    assert!(out.contains("4 passed, 0 failed"), "stdout: {out}");
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

#[test]
fn production_run_does_not_require_from_host_cwd_when_owner_lacks_module() {
    let host = tempfile::tempdir().unwrap();
    let package = tempfile::tempdir().unwrap();
    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(host.path().join("core.lua"), r#"return { value = "host" }"#).unwrap();
    let probe = package.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local core = require("core")
function pipeline(event)
  raise("seen", { value = core.value })
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
        .arg(package.path())
        .arg("--event")
        .arg("{}")
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(err.contains("module 'core' not found"), "{err}");
}

#[test]
fn run_rejects_multiple_package_root_flags() {
    let host = tempfile::tempdir().unwrap();
    let package_a = tempfile::tempdir().unwrap();
    let package_b = tempfile::tempdir().unwrap();
    fs::create_dir_all(package_a.path().join("departments/probe")).unwrap();
    let probe = package_a.path().join("departments/probe/main.lua");
    fs::write(&probe, "function pipeline(event) end\n").unwrap();

    let output = run_command(host.path(), &probe)
        .arg("--package-root")
        .arg(package_a.path())
        .arg("--package-root")
        .arg(package_b.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(stderr(&output).contains("duplicate --package-root for run"));
}

#[test]
fn run_rejects_package_roots_env_even_with_singular_env() {
    let host = tempfile::tempdir().unwrap();
    let package = tempfile::tempdir().unwrap();
    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    let probe = package.path().join("departments/probe/main.lua");
    fs::write(&probe, "function pipeline(event) end\n").unwrap();
    let joined = std::env::join_paths([package.path()]).unwrap();

    let output = run_command(host.path(), &probe)
        .env("FKST_PACKAGE_ROOTS", joined)
        .env("FKST_PACKAGE_ROOT", package.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("FKST_PACKAGE_ROOTS is not valid for `run`"),
        "{err}"
    );
}

#[test]
fn run_single_package_entrypoints_are_equivalent() {
    let host = tempfile::tempdir().unwrap();
    let package = tempfile::tempdir().unwrap();
    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("core.lua"),
        r#"return { value = "owner" }"#,
    )
    .unwrap();
    let probe = package.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local core = require("core")
function pipeline(event)
  raise("seen", { core = core.value, input = event.payload.value })
end
"#,
    )
    .unwrap();

    let flag = run_command(host.path(), &probe)
        .arg("--package-root")
        .arg(package.path())
        .output()
        .unwrap();
    let singular = run_command(host.path(), &probe)
        .env("FKST_PACKAGE_ROOT", package.path())
        .output()
        .unwrap();
    let package_is_host = run_command(package.path(), &probe)
        .arg("--package-root")
        .arg(package.path())
        .output()
        .unwrap();

    for output in [&flag, &singular, &package_is_host] {
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            stdout(output),
            stderr(output)
        );
    }
    assert_eq!(stdout(&flag), stdout(&singular));
    assert_eq!(stdout(&flag), stdout(&package_is_host));
}

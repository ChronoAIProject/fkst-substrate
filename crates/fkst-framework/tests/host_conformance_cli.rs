use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const RUNTIME_ROOT_ENV: &str = "FKST_RUNTIME_ROOT";

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn run_conformance(args: &[&std::ffi::OsStr], cwd: &std::path::Path) -> Output {
    let runtime = tempfile::tempdir().unwrap();
    run_conformance_with_env(args, cwd, &[(RUNTIME_ROOT_ENV, runtime.path())])
}

fn run_conformance_with_env(
    args: &[&std::ffi::OsStr],
    cwd: &std::path::Path,
    envs: &[(&str, &std::path::Path)],
) -> Output {
    let mut cmd = Command::new(framework_bin());
    cmd.arg("conformance").current_dir(cwd);
    cmd.env_remove("FKST_PACKAGE_ROOT");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    for arg in args {
        cmd.arg(arg);
    }
    if !args
        .iter()
        .any(|arg| *arg == std::ffi::OsStr::new("--package-root"))
    {
        if let Some(project_root) = project_root_arg(args) {
            cmd.arg("--package-root").arg(project_root);
        }
    }
    cmd.output().unwrap()
}

fn project_root_arg<'a>(args: &'a [&std::ffi::OsStr]) -> Option<&'a std::ffi::OsStr> {
    args.windows(2)
        .find(|pair| pair[0] == std::ffi::OsStr::new("--project-root"))
        .map(|pair| pair[1])
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn combined_log(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn write_minimal_host(root: &std::path::Path) {
    fs::create_dir_all(root.join("departments/hello")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    write_host_defaults(root);
    fs::write(
        root.join("departments/hello/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"tick"}, timeout = "30s" }
function pipeline(_) end
return M
"#,
    )
    .unwrap();
    fs::write(
        root.join("raisers/tick.lua"),
        r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
    )
    .unwrap();
}

fn write_host_defaults(root: &std::path::Path) {
    fs::create_dir_all(root.join("tunables")).unwrap();
    fs::write(root.join("tunables/queue_capacity.txt"), "100\n").unwrap();
    fs::write(
        root.join("tunables/department_default_timeout.txt"),
        "30m\n",
    )
    .unwrap();
    fs::write(root.join("tunables/codex_permit_slots.txt"), "20\n").unwrap();
}

fn write_package_raiser(root: &std::path::Path) {
    write_host_defaults(root);
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::write(
        root.join("raisers/tick.lua"),
        r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
    )
    .unwrap();
}

fn write_host_department(root: &std::path::Path) {
    fs::create_dir_all(root.join("departments/hello")).unwrap();
    fs::write(
        root.join("departments/hello/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"tick"}, timeout = "30s" }
function pipeline(_) end
return M
"#,
    )
    .unwrap();
}

fn path_arg(path: &std::path::Path) -> &std::ffi::OsStr {
    path.as_os_str()
}

fn crate_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn valid_minimal_host_exits_zero() {
    let host = tempfile::tempdir().unwrap();
    write_minimal_host(host.path());

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS runtime-layout"), "{log}");
    assert!(log.contains("PASS project-layout"), "{log}");
    assert!(log.contains("PASS graph-scan"), "{log}");
    assert!(log.contains("PASS department-non-empty"), "{log}");
    assert!(log.contains("PASS schema-validation"), "{log}");
}

#[test]
fn package_root_flag_supplies_standard_assets_for_host_department() {
    let package = tempfile::tempdir().unwrap();
    write_package_raiser(package.path());
    let host = tempfile::tempdir().unwrap();
    write_host_department(host.path());

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--package-root"),
        path_arg(package.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS graph-scan"), "{log}");
    assert!(log.contains("loaded 1 departments, 1 raisers"), "{log}");
}

#[test]
fn rejected_package_root_env_exits_two() {
    let host = tempfile::tempdir().unwrap();
    write_minimal_host(host.path());

    let output = Command::new(framework_bin())
        .arg("conformance")
        .arg("--project-root")
        .arg(host.path())
        .current_dir(host.path())
        .env(RUNTIME_ROOT_ENV, host.path().join(".fkst/runtime"))
        .env_remove("FKST_PACKAGE_ROOT")
        .env("FKST_GRAPH_ROOTS", host.path())
        .output()
        .unwrap();

    assert_exit(&output, 2);
    let log = combined_log(&output);
    assert!(log.contains("FKST_GRAPH_ROOTS"), "{log}");
    assert!(log.contains("removed package root surface"), "{log}");
}

#[test]
fn valid_host_with_custom_runtime_root_exits_zero() {
    let host = tempfile::tempdir().unwrap();
    write_minimal_host(host.path());
    let runtime = tempfile::tempdir().unwrap();
    let runtime_root = runtime.path().join("custom-runtime-root");

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance_with_env(
        &args,
        host.path(),
        &[(RUNTIME_ROOT_ENV, runtime_root.as_path())],
    );

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(
        log.contains(&format!(
            "PASS runtime-layout runtime root accepted: {}",
            runtime_root.display()
        )),
        "{log}"
    );
    assert!(log.contains("PASS graph-scan"), "{log}");
}

#[test]
fn missing_runtime_root_reports_runtime_layout_failure() {
    let host = tempfile::tempdir().unwrap();
    write_minimal_host(host.path());

    let output = Command::new(framework_bin())
        .arg("conformance")
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .current_dir(host.path())
        .env_remove(RUNTIME_ROOT_ENV)
        .env_remove("FKST_PACKAGE_ROOT")
        .output()
        .unwrap();

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL runtime-layout"), "{log}");
    assert!(log.contains("FKST_RUNTIME_ROOT must be set"), "{log}");
}

#[test]
fn missing_project_root_exits_two() {
    let cwd = tempfile::tempdir().unwrap();
    let output = run_conformance(&[], cwd.path());
    assert_exit(&output, 2);
}

#[test]
fn duplicate_project_root_exits_two() {
    let host = tempfile::tempdir().unwrap();
    write_minimal_host(host.path());

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 2);
}

#[test]
fn output_json_exits_two() {
    let host = tempfile::tempdir().unwrap();
    write_minimal_host(host.path());

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--output"),
        std::ffi::OsStr::new("json"),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 2);
}

#[test]
fn quick_exits_two() {
    let host = tempfile::tempdir().unwrap();
    write_minimal_host(host.path());

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--quick"),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 2);
}

#[test]
fn nonexistent_project_root_exits_two() {
    let cwd = tempfile::tempdir().unwrap();
    let missing = cwd.path().join("missing");
    let args = [std::ffi::OsStr::new("--project-root"), path_arg(&missing)];
    let output = run_conformance(&args, cwd.path());

    assert_exit(&output, 2);
}

#[test]
fn valid_cwd_without_project_root_exits_two() {
    let host = tempfile::tempdir().unwrap();
    write_minimal_host(host.path());

    let output = run_conformance(&[], host.path());

    assert_exit(&output, 2);
}

#[test]
fn invalid_cwd_plus_valid_project_root_uses_explicit_root() {
    let host = tempfile::tempdir().unwrap();
    write_minimal_host(host.path());
    let cwd = crate_fixture_root();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, &cwd);

    assert_exit(&output, 0);
}

#[test]
fn host_failures_exit_one_with_check_ids() {
    let missing_departments = tempfile::tempdir().unwrap();
    fs::create_dir_all(missing_departments.path().join("raisers")).unwrap();
    write_host_defaults(missing_departments.path());
    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(missing_departments.path()),
    ];
    let output = run_conformance(&args, missing_departments.path());
    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("PASS project-layout"), "{log}");
    assert!(log.contains("FAIL department-non-empty"), "{log}");

    let empty_graph = tempfile::tempdir().unwrap();
    fs::create_dir_all(empty_graph.path().join("departments/empty")).unwrap();
    fs::create_dir_all(empty_graph.path().join("raisers")).unwrap();
    write_host_defaults(empty_graph.path());
    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(empty_graph.path()),
    ];
    let output = run_conformance(&args, empty_graph.path());
    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL department-non-empty"), "{log}");

    let schema_invalid = tempfile::tempdir().unwrap();
    fs::create_dir_all(schema_invalid.path().join("departments/bad")).unwrap();
    write_host_defaults(schema_invalid.path());
    fs::write(
        schema_invalid.path().join("departments/bad/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"tick"}, produces = {"tick"}, timeout = "30x" }
function pipeline(_) end
return M
"#,
    )
    .unwrap();
    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(schema_invalid.path()),
    ];
    let output = run_conformance(&args, schema_invalid.path());
    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL schema-validation"), "{log}");
}

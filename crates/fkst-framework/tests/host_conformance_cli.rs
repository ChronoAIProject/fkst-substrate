use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

mod support;

use support::manifest_fixture::{write_single_package_workspace, write_workspace_for_roots};

const RUNTIME_ROOT_ENV: &str = "FKST_RUNTIME_ROOT";

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn run_conformance(args: &[&std::ffi::OsStr], cwd: &std::path::Path) -> Output {
    let runtime = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
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
    write_single_package_workspace(root);
    fs::create_dir_all(root.join("departments/hello")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    write_host_defaults(root);
    fs::write(
        root.join("departments/hello/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"tick"}, stall_window = "30s" }
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
    fs::write(
        root.join("fkst.env"),
        "FKST_QUEUE_CAPACITY=100\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=30m\nFKST_CODEX_PERMIT_SLOTS=20\n",
    )
    .unwrap();
}

fn write_package_raiser(root: &std::path::Path) {
    write_single_package_workspace(root);
    write_host_defaults(root);
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::write(
        root.join("raisers/tick.lua"),
        r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
    )
    .unwrap();
}

fn write_host_department(root: &std::path::Path) {
    write_single_package_workspace(root);
    fs::create_dir_all(root.join("departments/hello")).unwrap();
    fs::write(
        root.join("departments/hello/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"tick"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
    )
    .unwrap();
}

fn write_package_consumer(root: &std::path::Path, queue: &str) {
    write_single_package_workspace(root);
    fs::create_dir_all(root.join("departments/consumer")).unwrap();
    fs::write(
        root.join("departments/consumer/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{ consumes = {{"{queue}"}}, stall_window = "30s" }}
function pipeline(_) end
return M
"#
        ),
    )
    .unwrap();
}

fn write_package_producer(root: &std::path::Path, queue: &str) {
    write_single_package_workspace(root);
    fs::create_dir_all(root.join("departments/producer")).unwrap();
    fs::write(
        root.join("departments/producer/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{ consumes = {{}}, produces = {{"{queue}"}}, stall_window = "30s" }}
function pipeline(_) end
return M
"#
        ),
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
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
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
    assert!(log.contains("PASS locale-catalogs"), "{log}");
    assert!(log.contains("PASS graph-scan"), "{log}");
    assert!(log.contains("PASS department-non-empty"), "{log}");
    assert!(log.contains("PASS schema-validation"), "{log}");
}

#[test]
fn locale_catalogs_pass_with_complete_non_reference_locale() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    fs::create_dir_all(host.path().join("locales")).unwrap();
    fs::write(
        host.path().join("locales/en.lua"),
        r#"return { ["greeting.title"] = "Hello {name}", body = "Ready" }"#,
    )
    .unwrap();
    fs::write(
        host.path().join("locales/zh-CN.lua"),
        r#"return { ["greeting.title"] = "你好 {name}", body = "就绪" }"#,
    )
    .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS locale-catalogs"), "{log}");
    assert!(
        log.contains("validated locale catalogs for 1 graph roots"),
        "{log}"
    );
}

#[test]
fn locale_catalogs_reject_missing_non_reference_key() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    fs::create_dir_all(host.path().join("locales")).unwrap();
    fs::write(
        host.path().join("locales/en.lua"),
        r#"return { title = "Title", body = "Body" }"#,
    )
    .unwrap();
    fs::write(
        host.path().join("locales/zh.lua"),
        r#"return { title = "标题" }"#,
    )
    .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL locale-catalogs"), "{log}");
    assert!(log.contains("missing reference key `body`"), "{log}");
}

#[test]
fn locale_catalogs_reject_decode_helper_wrapped_literals() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    fs::create_dir_all(host.path().join("locales")).unwrap();
    fs::write(
        host.path().join("locales/en.lua"),
        r#"return { title = string.char(84) }"#,
    )
    .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL locale-catalogs"), "{log}");
    assert!(log.contains("forbidden decode helper pattern"), "{log}");
}

#[test]
fn locale_catalogs_reject_machine_tokens() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    fs::create_dir_all(host.path().join("locales")).unwrap();
    fs::write(
        host.path().join("locales/en.lua"),
        r#"return { title = "RAISED: hidden" }"#,
    )
    .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL locale-catalogs"), "{log}");
    assert!(log.contains("forbidden machine token"), "{log}");
}

#[test]
fn package_root_flag_supplies_graph_root_for_host_department() {
    let package = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_package_raiser(package.path());
    let package_namespace = package.path().file_name().unwrap().to_string_lossy();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("departments/hello")).unwrap();
    fs::write(
        host.path().join("departments/hello/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{ consumes = {{"{package_namespace}.tick"}}, stall_window = "30s" }}
function pipeline(_) end
return M
"#
        ),
    )
    .unwrap();
    write_workspace_for_roots(host.path(), &[package.path()]);

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
fn package_consensus_dead_letter_consumer_proves_no_producer_gap_closed() {
    let missing_producer_root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let missing_producer_package = missing_producer_root.path().join("consensus");
    fs::create_dir_all(&missing_producer_package).unwrap();
    write_host_defaults(&missing_producer_package);
    write_package_consumer(&missing_producer_package, "proposal");
    let missing_producer_host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(missing_producer_host.path());
    write_workspace_for_roots(missing_producer_host.path(), &[&missing_producer_package]);

    let missing_producer_args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(missing_producer_host.path()),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&missing_producer_package),
    ];
    let missing_producer_output =
        run_conformance(&missing_producer_args, missing_producer_host.path());

    assert_exit(&missing_producer_output, 1);
    let missing_producer_log = combined_log(&missing_producer_output);
    assert!(
        missing_producer_log.contains("FAIL schema-validation"),
        "{missing_producer_log}"
    );
    assert!(
        missing_producer_log.contains(
            "queue 'consensus.proposal' is consumed by department 'consensus.consumer' but has no producer"
        ),
        "{missing_producer_log}"
    );

    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("consensus");
    fs::create_dir_all(package.join("departments/dead_handler")).unwrap();
    write_host_defaults(&package);
    fs::write(
        package.join("departments/dead_handler/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "dead_letter" },
  ephemeral = { "dead_letter" },
  stall_window = "30s",
  retry = false,
}
function pipeline(_) end
return M
"#,
    )
    .unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&package),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS schema-validation"), "{log}");
    assert!(
        !log.contains(
            "queue 'consensus.dead_letter' is consumed by department 'consensus.dead_handler' but has no producer"
        ),
        "{log}"
    );
}

#[test]
fn single_root_flat_consumed_queue_without_producer_warns() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_host_department(host.path());

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS schema-validation"), "{log}");
    assert!(
        log.contains("schema validation passed with 1 warnings"),
        "{log}"
    );
}

#[test]
fn composed_graph_sibling_producer_satisfies_consumed_queue() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let consumer_package = root.path().join("consensus");
    let producer_package = root.path().join("github-devloop");
    fs::create_dir_all(&consumer_package).unwrap();
    fs::create_dir_all(&producer_package).unwrap();
    write_host_defaults(&consumer_package);
    write_host_defaults(&producer_package);
    write_package_consumer(&consumer_package, "proposal");
    write_package_producer(&producer_package, "consensus.proposal");
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&consumer_package, &producer_package]);

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&consumer_package),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&producer_package),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS schema-validation"), "{log}");
    assert!(!log.contains("no producer"), "{log}");
}

#[test]
fn composed_graph_consumed_queue_without_any_producer_fails() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let consumer_package = root.path().join("consensus");
    let sibling_package = root.path().join("github-devloop");
    fs::create_dir_all(&consumer_package).unwrap();
    fs::create_dir_all(&sibling_package).unwrap();
    write_host_defaults(&consumer_package);
    write_host_defaults(&sibling_package);
    write_package_consumer(&consumer_package, "proposal");
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&consumer_package, &sibling_package]);

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&consumer_package),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&sibling_package),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL schema-validation"), "{log}");
    assert!(log.contains("queue 'consensus.proposal'"), "{log}");
    assert!(log.contains("department 'consensus.consumer'"), "{log}");
    assert!(log.contains("has no producer"), "{log}");
}

#[test]
fn rejected_package_root_env_exits_two() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
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
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    let runtime = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
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
fn missing_runtime_root_reports_runtime_scratch_unused() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
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

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS runtime-layout"), "{log}");
    assert!(
        log.contains("FKST_RUNTIME_ROOT not set; runtime scratch unused by conformance"),
        "{log}"
    );
}

#[test]
fn missing_project_root_exits_two() {
    let cwd = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let output = run_conformance(&[], cwd.path());
    assert_exit(&output, 2);
}

#[test]
fn duplicate_project_root_exits_two() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
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
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
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
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
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
    let cwd = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let missing = cwd.path().join("missing");
    let args = [std::ffi::OsStr::new("--project-root"), path_arg(&missing)];
    let output = run_conformance(&args, cwd.path());

    assert_exit(&output, 2);
}

#[test]
fn valid_cwd_without_project_root_exits_two() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());

    let output = run_conformance(&[], host.path());

    assert_exit(&output, 2);
}

#[test]
fn invalid_cwd_plus_valid_project_root_uses_explicit_root() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
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
    let missing_departments = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(missing_departments.path().join("raisers")).unwrap();
    write_host_defaults(missing_departments.path());
    write_single_package_workspace(missing_departments.path());
    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(missing_departments.path()),
    ];
    let output = run_conformance(&args, missing_departments.path());
    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("PASS project-layout"), "{log}");
    assert!(log.contains("FAIL department-non-empty"), "{log}");

    let empty_graph = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(empty_graph.path().join("departments/empty")).unwrap();
    fs::create_dir_all(empty_graph.path().join("raisers")).unwrap();
    write_host_defaults(empty_graph.path());
    write_single_package_workspace(empty_graph.path());
    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(empty_graph.path()),
    ];
    let output = run_conformance(&args, empty_graph.path());
    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL department-non-empty"), "{log}");

    let schema_invalid = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(schema_invalid.path().join("departments/bad")).unwrap();
    write_host_defaults(schema_invalid.path());
    write_single_package_workspace(schema_invalid.path());
    fs::write(
        schema_invalid.path().join("departments/bad/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"tick"}, produces = {"tick"}, stall_window = "30x" }
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

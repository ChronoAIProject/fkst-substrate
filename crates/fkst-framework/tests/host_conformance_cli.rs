use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};

mod support;

use support::manifest_fixture::{
    unit_name, write_library_manifest, write_package_manifest, write_single_package_workspace,
    write_workspace, write_workspace_for_roots,
};

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

fn stdout_json_report(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().last().expect("stdout should contain JSON");
    serde_json::from_str(line).unwrap_or_else(|err| panic!("invalid JSON report {line}: {err}"))
}

fn assert_fail_closed(output: &Output, expected: &[&str]) {
    assert_exit(output, 1);
    let log = combined_log(output);
    assert!(log.contains("FAIL"), "{log}");
    for needle in expected {
        assert!(log.contains(needle), "missing `{needle}` in:\n{log}");
    }
    let report = stdout_json_report(output);
    assert_eq!(report["ok"], false, "{report}");
    assert_eq!(report["counts"]["failed"].as_u64().unwrap() > 0, true);
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
    write_package_consumer_with_published_seam(root, queue, false);
}

fn write_package_consumer_with_published_seam(root: &std::path::Path, queue: &str, publish: bool) {
    write_single_package_workspace(root);
    fs::create_dir_all(root.join("departments/consumer")).unwrap();
    let published_seam = if publish {
        format!(r#", published_seam = {{"{queue}"}}"#)
    } else {
        String::new()
    };
    fs::write(
        root.join("departments/consumer/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{ consumes = {{"{queue}"}}{published_seam}, stall_window = "30s" }}
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

fn write_declarative_pack_package(root: &std::path::Path, pack_body: &str) {
    write_declarative_pack_package_with_manifest_name(root, &unit_name(root), pack_body);
}

fn write_declarative_pack_library(root: &std::path::Path, name: &str, pack_body: &str) {
    write_library_manifest(root, name, &[]);
    append_conformance_manifest(root, "conformance/pack.toml");
    fs::create_dir_all(root.join("conformance")).unwrap();
    fs::write(
        root.join("conformance/pack.toml"),
        pack_body.replace("{{name}}", name),
    )
    .unwrap();
}

fn write_declarative_pack_package_with_manifest_name(
    root: &std::path::Path,
    manifest_name: &str,
    pack_body: &str,
) {
    write_package_manifest(root, manifest_name, &[]);
    write_workspace(root, &[root]);
    append_conformance_manifest(root, "conformance/pack.toml");
    write_host_defaults(root);
    fs::create_dir_all(root.join("departments/consumer")).unwrap();
    fs::create_dir_all(root.join("conformance")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::write(
        root.join("departments/consumer/main.lua"),
        r#"
local M = {}
M.spec = { consumes = { "tick" }, stall_window = "30s" }
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
    fs::write(
        root.join("conformance/pack.toml"),
        pack_body
            .replace("{{name}}", manifest_name)
            .replace("{{active}}", &unit_name(root)),
    )
    .unwrap();
}

fn append_conformance_manifest(root: &std::path::Path, pack_path: &str) {
    fs::OpenOptions::new()
        .append(true)
        .open(root.join("fkst.toml"))
        .unwrap()
        .write_all(
            format!(
                r#"
[conformance]
pack = "{pack_path}"
"#
            )
            .as_bytes(),
        )
        .unwrap();
}

fn append_conformance_function_manifest(root: &std::path::Path, function_ref: &str) {
    fs::OpenOptions::new()
        .append(true)
        .open(root.join("fkst.toml"))
        .unwrap()
        .write_all(
            format!(
                r#"
[conformance]
function = "{function_ref}"
"#
            )
            .as_bytes(),
        )
        .unwrap();
}

fn write_semantic_conformance_package(root: &std::path::Path, function_body: &str) {
    write_package_manifest(root, "traveler", &[]);
    write_workspace(root, &[root]);
    append_conformance_function_manifest(root, "core.conformance_errors");
    write_host_defaults(root);
    fs::create_dir_all(root.join("core")).unwrap();
    fs::create_dir_all(root.join("departments/consumer")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::write(
        root.join("core/init.lua"),
        format!(
            r#"
local M = {{}}
function M.conformance_errors()
{function_body}
end
return M
"#
        ),
    )
    .unwrap();
    fs::write(
        root.join("departments/consumer/main.lua"),
        r#"
local M = {}
M.spec = { consumes = { "tick" }, stall_window = "30s" }
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

fn write_semantic_conformance_library(root: &std::path::Path, name: &str, function_body: &str) {
    write_library_manifest(root, name, &[]);
    append_conformance_function_manifest(root, &format!("{name}.conformance_errors"));
    fs::create_dir_all(root.join("public")).unwrap();
    fs::write(
        root.join("public/init.lua"),
        format!(
            r#"
local M = {{}}
function M.conformance_errors()
{function_body}
end
return M
"#
        ),
    )
    .unwrap();
}

fn max_line_count_pack(max: usize) -> String {
    format!(
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{{{name}}}}"

[[rules]]
id = "source.max-lines"
severity = "error"
kind = "max_line_count"
include = ["src/**/*.lua"]
exclude = ["tests/**", "**/*_test.lua"]
max = {max}
message = "source files must stay under {max} lines"
"#
    )
}

fn pack_with_include(include: &str) -> String {
    format!(
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{{{name}}}}"

[[rules]]
id = "source.max-lines"
severity = "error"
kind = "max_line_count"
include = ["{include}"]
max = 10
message = "source files must stay under 10 lines"
"#
    )
}

fn text_forbid_regex_pack(pattern: &str) -> String {
    format!(
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{{{name}}}}"

[[rules]]
id = "source.no-forbidden-text"
severity = "error"
kind = "text_forbid_regex"
include = ["src/**/*.lua"]
exclude = ["tests/**"]
pattern = "{pattern}"
message = "source files must not contain forbidden text"
"#
    )
}

fn text_require_regex_pack(pattern: &str) -> String {
    format!(
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{{{name}}}}"

[[rules]]
id = "source.requires-boundary-budget"
severity = "error"
kind = "text_require_regex"
include = ["src/**/*.lua"]
exclude = ["tests/**"]
pattern = "{pattern}"
message = "source files must call the boundary budget helper"
"#
    )
}

fn run_package_conformance(host: &std::path::Path, package: &std::path::Path) -> Output {
    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host),
        std::ffi::OsStr::new("--package-root"),
        path_arg(package),
    ];
    run_conformance(&args, host)
}

#[cfg(unix)]
fn make_dir_symlink(target: &std::path::Path, link: &std::path::Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn make_dir_symlink(target: &std::path::Path, link: &std::path::Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap();
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
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["violations"], serde_json::json!([]));
    assert_eq!(report["counts"]["packs"], 1);
    assert_eq!(report["counts"]["checks"], 7);
    assert_eq!(report["counts"]["failed"], 0);
}

#[test]
fn package_manifest_missing_persistence_class_fails_conformance() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    fs::write(
        host.path().join("fkst.toml"),
        format!(
            r#"
kind = "package"
name = "{}"

[code]
root = "."

[lib_deps]
libraries = []
"#,
            unit_name(host.path())
        ),
    )
    .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_fail_closed(
        &output,
        &[
            "FAIL persistence-class",
            "package manifest must declare `persistence_class`",
            "engine.persistence-class",
        ],
    );
    let report = stdout_json_report(&output);
    assert_eq!(report["violations"][0]["rule"], "engine.persistence-class");
    assert_eq!(report["violations"][0]["package"], unit_name(host.path()));
}

#[test]
fn independent_host_root_without_persistence_class_passes_conformance() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("pkg");
    fs::create_dir_all(&package).unwrap();
    write_package_raiser(&package);

    let host = root.path().join("host-root");
    fs::create_dir_all(host.join("departments/hello")).unwrap();
    write_host_defaults(&host);
    fs::write(
        host.join("fkst.toml"),
        r#"
kind = "package"
name = "host_root"

[code]
root = "."
"#,
    )
    .unwrap();
    write_workspace(&host, &[&host]);
    fs::write(
        host.join("departments/hello/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"pkg.tick"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
    )
    .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(&host),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&package),
    ];
    let output = run_conformance(&args, &host);

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS persistence-class"), "{log}");
    assert!(
        log.contains("validated persistence_class for 1 package roots"),
        "{log}"
    );
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["violations"], serde_json::json!([]));
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
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(report["violations"][0]["rule"], "engine.locale-catalogs");
    assert!(report["violations"][0]["package"].is_string());
    assert!(
        report["violations"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("missing reference key `body`"),
        "{report}"
    );
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
    write_package_consumer_with_published_seam(&consumer_package, "proposal", true);
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
fn composed_graph_sibling_producer_to_unpublished_queue_fails_graph_scan() {
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

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL graph-scan"), "{log}");
    assert!(
        log.contains("produces sibling queue `consensus.proposal`"),
        "{log}"
    );
    assert!(log.contains("M.spec.published_seam"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(report["violations"][0]["rule"], "engine.graph-scan");
    assert!(
        report["violations"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("M.spec.published_seam"),
        "{report}"
    );
}

#[test]
fn config_file_is_accepted_as_rule_pack_selection_seam() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    let config = host.path().join("conformance.toml");
    fs::write(
        &config,
        r#"
[rule_packs]
engine = {}
"#,
    )
    .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--config"),
        path_arg(&config),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["counts"]["packs"], 1);
}

#[test]
fn declarative_pack_max_line_count_fails_for_package_owned_source_file() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &max_line_count_pack(2));
    fs::write(package.join("src/short.lua"), "return 1\n").unwrap();
    fs::write(package.join("src/long.lua"), "one\ntwo\nthree\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&package),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL source.max-lines:src/long.lua"), "{log}");
    assert!(
        log.contains("source files must stay under 2 lines"),
        "{log}"
    );
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(
        report["violations"][0]["rule"],
        "declarative:traveler.source.max-lines:src/long.lua"
    );
    assert_eq!(report["violations"][0]["package"], "traveler");
}

#[test]
fn declarative_pack_max_line_count_passes_for_clean_package() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &max_line_count_pack(3));
    fs::write(package.join("src/short.lua"), "return 1\n").unwrap();
    fs::write(package.join("src/also_short.lua"), "one\ntwo\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&package),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS source.max-lines"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["counts"]["packs"], 2);
    assert_eq!(report["violations"], serde_json::json!([]));
}

#[test]
fn declarative_pack_max_line_count_passes_for_clean_library() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    let library = host.path().join("libraries/stdlib");
    fs::create_dir_all(library.join("src")).unwrap();
    write_workspace(host.path(), &[host.path(), &library]);
    write_declarative_pack_library(&library, "stdlib", &max_line_count_pack(3));
    fs::write(library.join("src/short.lua"), "return 1\n").unwrap();
    fs::write(library.join("src/also_short.lua"), "one\ntwo\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS source.max-lines"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["counts"]["packs"], 2);
    assert_eq!(report["violations"], serde_json::json!([]));
}

#[test]
fn declarative_pack_max_line_count_fails_for_library_owned_source_file() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    let library = host.path().join("libraries/stdlib");
    fs::create_dir_all(library.join("src")).unwrap();
    write_workspace(host.path(), &[host.path(), &library]);
    write_declarative_pack_library(&library, "stdlib", &max_line_count_pack(2));
    fs::write(library.join("src/long.lua"), "one\ntwo\nthree\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL source.max-lines:src/long.lua"), "{log}");
    assert!(
        log.contains("source files must stay under 2 lines"),
        "{log}"
    );
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(
        report["violations"][0]["rule"],
        "declarative:stdlib.source.max-lines:src/long.lua"
    );
    assert_eq!(report["violations"][0]["package"], "stdlib");
}

#[test]
fn declarative_pack_for_library_cannot_inspect_host_sibling_or_package_files() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    let library = host.path().join("libraries/stdlib");
    let sibling_library = host.path().join("libraries/sibling");
    let package = host.path().join("packages/traveler");
    fs::create_dir_all(library.join("src")).unwrap();
    fs::create_dir_all(sibling_library.join("src")).unwrap();
    fs::create_dir_all(package.join("src")).unwrap();
    fs::create_dir_all(host.path().join("src")).unwrap();
    write_workspace(
        host.path(),
        &[host.path(), &library, &sibling_library, &package],
    );
    write_declarative_pack_library(
        &library,
        "stdlib",
        &text_forbid_regex_pack("forbidden_call"),
    );
    write_library_manifest(&sibling_library, "sibling", &[]);
    write_package_manifest(&package, "traveler", &[]);
    fs::write(library.join("src/clean.lua"), "return 1\n").unwrap();
    fs::write(sibling_library.join("src/bad.lua"), "forbidden_call()\n").unwrap();
    fs::write(package.join("src/bad.lua"), "forbidden_call()\n").unwrap();
    fs::write(host.path().join("src/bad.lua"), "forbidden_call()\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS source.no-forbidden-text"), "{log}");
    assert!(!log.contains("src/bad.lua matches forbidden text"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["violations"], serde_json::json!([]));
}

#[test]
fn library_without_conformance_section_does_not_register_declarative_pack() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    let library = host.path().join("libraries/stdlib");
    fs::create_dir_all(library.join("src")).unwrap();
    write_workspace(host.path(), &[host.path(), &library]);
    write_library_manifest(&library, "stdlib", &[]);
    fs::write(library.join("src/short.lua"), "return 1\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(!log.contains("conformance-pack-loader"), "{log}");
    assert!(!log.contains("declarative:stdlib"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["counts"]["packs"], 1);
}

#[test]
fn declarative_pack_owner_package_must_match_library_name() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    let library = host.path().join("libraries/stdlib");
    fs::create_dir_all(library.join("src")).unwrap();
    write_workspace(host.path(), &[host.path(), &library]);
    write_declarative_pack_library(
        &library,
        "stdlib",
        &max_line_count_pack(10).replace("{{name}}", "wronglib"),
    );
    fs::write(library.join("src/short.lua"), "return 1\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_fail_closed(
        &output,
        &[
            "owner_package `wronglib` does not match active package `stdlib`",
            "declarative:stdlib.conformance-pack-loader",
        ],
    );
    let report = stdout_json_report(&output);
    assert_eq!(report["violations"][0]["package"], "stdlib");
}

#[test]
fn declarative_pack_runs_for_folded_project_package_root() {
    let package = tempfile::Builder::new()
        .prefix("traveler")
        .tempdir()
        .unwrap();
    fs::create_dir_all(package.path().join("src")).unwrap();
    write_declarative_pack_package(package.path(), &max_line_count_pack(2));
    fs::write(package.path().join("src/long.lua"), "one\ntwo\nthree\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(package.path()),
    ];
    let output = run_conformance(&args, package.path());

    assert_exit(&output, 1);
    let report = stdout_json_report(&output);
    let package_name = unit_name(package.path());
    assert_eq!(
        report["violations"][0]["rule"],
        format!("declarative:{package_name}.source.max-lines:src/long.lua")
    );
    assert_eq!(report["violations"][0]["package"], package_name);
}

#[test]
fn declarative_pack_unknown_rule_kind_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{name}}"

[[rules]]
id = "source.future"
severity = "error"
kind = "future_rule_kind"
include = ["src/**/*.lua"]
message = "future rules are not supported in this runner"
"#,
    );
    fs::write(package.join("src/short.lua"), "return 1\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&package),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL conformance-pack-loader"), "{log}");
    assert!(log.contains("unknown kind `future_rule_kind`"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(
        report["violations"][0]["rule"],
        "declarative:traveler.conformance-pack-loader"
    );
    assert_eq!(report["violations"][0]["package"], "traveler");
}

#[test]
fn declarative_pack_text_forbid_regex_fails_for_package_owned_source_file() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &text_forbid_regex_pack("forbidden_call"));
    fs::write(package.join("src/clean.lua"), "return 1\n").unwrap();
    fs::write(
        package.join("src/bad.lua"),
        "local x = 1\nforbidden_call()\nreturn x\n",
    )
    .unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(
        log.contains("FAIL source.no-forbidden-text:src/bad.lua"),
        "{log}"
    );
    assert!(
        log.contains("source files must not contain forbidden text"),
        "{log}"
    );
    assert!(log.contains("line 2"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(
        report["violations"][0]["rule"],
        "declarative:traveler.source.no-forbidden-text:src/bad.lua"
    );
    assert_eq!(report["violations"][0]["package"], "traveler");
}

#[test]
fn declarative_pack_text_forbid_regex_passes_for_clean_package() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &text_forbid_regex_pack("forbidden_call"));
    fs::write(package.join("src/clean.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS source.no-forbidden-text"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["violations"], serde_json::json!([]));
}

#[test]
fn declarative_pack_text_forbid_regex_supports_real_regex_features() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{name}}"

[[rules]]
id = "source.no-local-parser"
severity = "error"
kind = "text_forbid_regex"
include = ["src/**/*.lua"]
exclude = ["tests/**"]
pattern = '\blocal\s+function\s+parse_name_only_paths\s*\('
message = "source files must not define the local parser"
"#,
    );
    fs::write(
        package.join("src/bad.lua"),
        "return 1\nlocal  function\nparse_name_only_paths (\nend\n",
    )
    .unwrap();
    fs::write(
        package.join("src/allowed.lua"),
        "core.parse_name_only_paths(input)\n",
    )
    .unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(
        log.contains("FAIL source.no-local-parser:src/bad.lua"),
        "{log}"
    );
    assert!(
        log.contains("source files must not define the local parser"),
        "{log}"
    );
    assert!(log.contains("line 2"), "{log}");
    assert!(
        !log.contains("source.no-local-parser:src/allowed.lua"),
        "{log}"
    );
    let report = stdout_json_report(&output);
    assert_eq!(report["violations"].as_array().unwrap().len(), 1);
    assert_eq!(
        report["violations"][0]["rule"],
        "declarative:traveler.source.no-local-parser:src/bad.lua"
    );
    assert_eq!(report["violations"][0]["package"], "traveler");
}

#[test]
fn declarative_pack_text_forbid_regex_invalid_pattern_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &text_forbid_regex_pack("("));
    fs::write(package.join("src/clean.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "invalid text_forbid_regex pattern",
            "unclosed group",
        ],
    );
}

#[test]
fn declarative_pack_text_forbid_regex_cannot_inspect_host_or_sibling_files() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    let sibling = root.path().join("sibling");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::create_dir_all(sibling.join("src")).unwrap();
    fs::write(sibling.join("src/bad.lua"), "forbidden_call()\n").unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    fs::create_dir_all(host.path().join("src")).unwrap();
    fs::write(host.path().join("src/bad.lua"), "forbidden_call()\n").unwrap();
    write_workspace_for_roots(host.path(), &[&package, &sibling]);
    write_declarative_pack_package(&package, &text_forbid_regex_pack("forbidden_call"));
    write_package_consumer(&sibling, "unused");
    fs::write(package.join("src/clean.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS source.no-forbidden-text"), "{log}");
    assert!(!log.contains("src/bad.lua matches forbidden text"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["violations"], serde_json::json!([]));
}

#[test]
fn declarative_pack_text_forbid_regex_forbids_max_field() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{name}}"

[[rules]]
id = "source.no-forbidden-text"
severity = "error"
kind = "text_forbid_regex"
include = ["src/**/*.lua"]
pattern = "forbidden_call"
max = 1
message = "source files must not contain forbidden text"
"#,
    );
    fs::write(package.join("src/clean.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "field `max` is not allowed for kind `text_forbid_regex`",
        ],
    );
}

#[test]
fn declarative_pack_text_forbid_regex_requires_pattern_field() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{name}}"

[[rules]]
id = "source.no-forbidden-text"
severity = "error"
kind = "text_forbid_regex"
include = ["src/**/*.lua"]
message = "source files must not contain forbidden text"
"#,
    );
    fs::write(package.join("src/clean.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "missing required field `pattern`",
        ],
    );
}

#[test]
fn declarative_pack_text_require_regex_passes_when_package_owned_source_matches() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &text_require_regex_pack("boundary_budget"));
    fs::write(
        package.join("src/loop.lua"),
        "local budget = boundary_budget()\nreturn budget\n",
    )
    .unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(
        log.contains("PASS source.requires-boundary-budget"),
        "{log}"
    );
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["violations"], serde_json::json!([]));
}

#[test]
fn declarative_pack_text_require_regex_fails_when_package_owned_source_does_not_match() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &text_require_regex_pack("boundary_budget"));
    fs::write(package.join("src/loop.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(
        log.contains("FAIL source.requires-boundary-budget:src/loop.lua"),
        "{log}"
    );
    assert!(
        log.contains("source files must call the boundary budget helper"),
        "{log}"
    );
    assert!(
        log.contains("src/loop.lua does not match required text"),
        "{log}"
    );
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(
        report["violations"][0]["rule"],
        "declarative:traveler.source.requires-boundary-budget:src/loop.lua"
    );
    assert_eq!(report["violations"][0]["package"], "traveler");
}

#[test]
fn declarative_pack_text_require_regex_fails_when_include_matches_no_files() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &text_require_regex_pack("boundary_budget"));

    let output = run_package_conformance(host.path(), &package);

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(
        log.contains("FAIL source.requires-boundary-budget"),
        "{log}"
    );
    assert!(
        log.contains("no files matched required include globs"),
        "{log}"
    );
    assert!(!log.contains("source.requires-boundary-budget:"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(
        report["violations"][0]["rule"],
        "declarative:traveler.source.requires-boundary-budget"
    );
    assert_eq!(report["violations"][0]["package"], "traveler");
}

#[test]
fn declarative_pack_text_require_regex_invalid_pattern_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &text_require_regex_pack("("));
    fs::write(package.join("src/loop.lua"), "boundary_budget()\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "invalid text_require_regex pattern",
            "unclosed group",
        ],
    );
}

#[test]
fn declarative_pack_text_require_regex_forbids_max_field() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{name}}"

[[rules]]
id = "source.requires-boundary-budget"
severity = "error"
kind = "text_require_regex"
include = ["src/**/*.lua"]
pattern = "boundary_budget"
max = 1
message = "source files must call the boundary budget helper"
"#,
    );
    fs::write(package.join("src/loop.lua"), "boundary_budget()\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "field `max` is not allowed for kind `text_require_regex`",
        ],
    );
}

#[test]
fn declarative_pack_text_require_regex_cannot_inspect_host_or_sibling_files() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    let sibling = root.path().join("sibling");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::create_dir_all(sibling.join("src")).unwrap();
    fs::write(sibling.join("src/loop.lua"), "boundary_budget()\n").unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    fs::create_dir_all(host.path().join("src")).unwrap();
    fs::write(host.path().join("src/loop.lua"), "boundary_budget()\n").unwrap();
    write_workspace_for_roots(host.path(), &[&package, &sibling]);
    write_declarative_pack_package(&package, &text_require_regex_pack("boundary_budget"));
    write_package_consumer(&sibling, "unused");
    fs::write(package.join("src/loop.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(
        log.contains("FAIL source.requires-boundary-budget:src/loop.lua"),
        "{log}"
    );
    let report = stdout_json_report(&output);
    let violations = report["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{report}");
    assert_eq!(
        report["violations"][0]["rule"],
        "declarative:traveler.source.requires-boundary-budget:src/loop.lua"
    );
    assert_eq!(report["violations"][0]["package"], "traveler");
}

#[test]
fn declarative_pack_max_line_count_forbids_pattern_field() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{name}}"

[[rules]]
id = "source.max-lines"
severity = "error"
kind = "max_line_count"
include = ["src/**/*.lua"]
max = 10
pattern = "forbidden_call"
message = "source files must stay under 10 lines"
"#,
    );
    fs::write(package.join("src/clean.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "field `pattern` is not allowed for kind `max_line_count`",
        ],
    );
}

#[test]
fn declarative_pack_max_line_count_requires_max_field() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{name}}"

[[rules]]
id = "source.max-lines"
severity = "error"
kind = "max_line_count"
include = ["src/**/*.lua"]
message = "source files must stay under 10 lines"
"#,
    );
    fs::write(package.join("src/clean.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "missing required field `max`",
        ],
    );
}

#[test]
fn declarative_pack_rule_unknown_field_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{name}}"

[[rules]]
id = "source.no-forbidden-text"
severity = "error"
kind = "text_forbid_regex"
include = ["src/**/*.lua"]
pattern = "forbidden_call"
message = "source files must not contain forbidden text"
unexpected = true
"#,
    );
    fs::write(package.join("src/clean.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &["FAIL conformance-pack-loader", "unknown field `unexpected`"],
    );
}

#[test]
fn declarative_manifest_unknown_conformance_field_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &max_line_count_pack(10));
    fs::OpenOptions::new()
        .append(true)
        .open(package.join("fkst.toml"))
        .unwrap()
        .write_all(b"extra = true\n")
        .unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "unknown field `extra`",
            "declarative:traveler.conformance-pack-loader",
        ],
    );
}

#[test]
fn declarative_manifest_malformed_conformance_section_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &max_line_count_pack(10));
    fs::write(
        package.join("fkst.toml"),
        r#"
kind = "package"
name = "traveler"
persistence_class = "stateless_adapter"

[code]
root = "."

[lib_deps]
libraries = []

[conformance]
pack = 42
"#,
    )
    .unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "invalid type: integer `42`",
            "declarative:traveler.conformance-pack-loader",
        ],
    );
}

#[test]
fn declarative_pack_owner_package_must_match_active_identity() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("activepkg");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package_with_manifest_name(
        &package,
        "declaredpkg",
        &max_line_count_pack(10),
    );
    fs::write(package.join("src/short.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "owner_package `declaredpkg` does not match active package `activepkg`",
            "declarative:activepkg.conformance-pack-loader",
        ],
    );
}

#[test]
fn declarative_packs_are_namespaced_by_active_identity_not_manifest_name() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package_a = root.path().join("alpha");
    let package_b = root.path().join("beta");
    fs::create_dir_all(package_a.join("src")).unwrap();
    fs::create_dir_all(package_b.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_package_manifest(host.path(), "host", &[]);
    write_workspace(host.path(), &[host.path()]);
    write_declarative_pack_package_with_manifest_name(
        &package_a,
        "same-manifest",
        &max_line_count_pack(2).replace("{{name}}", "{{active}}"),
    );
    write_declarative_pack_package_with_manifest_name(
        &package_b,
        "same-manifest",
        &max_line_count_pack(2).replace("{{name}}", "{{active}}"),
    );
    fs::write(package_a.join("src/long.lua"), "one\ntwo\nthree\n").unwrap();
    fs::write(package_b.join("src/long.lua"), "one\ntwo\nthree\n").unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&package_a),
        std::ffi::OsStr::new("--package-root"),
        path_arg(&package_b),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 1);
    let report = stdout_json_report(&output);
    let rules = report["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|violation| violation["rule"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        rules.contains(&"declarative:alpha.source.max-lines:src/long.lua".to_string()),
        "{report}"
    );
    assert!(
        rules.contains(&"declarative:beta.source.max-lines:src/long.lua".to_string()),
        "{report}"
    );
}

#[test]
fn declarative_pack_missing_pack_file_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(&package).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_package_manifest(&package, "traveler", &[]);
    append_conformance_manifest(&package, "conformance/missing.toml");
    write_host_defaults(&package);
    fs::create_dir_all(package.join("departments/consumer")).unwrap();
    fs::write(
        package.join("departments/consumer/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
    )
    .unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-pack-loader",
            "canonicalize conformance pack",
            "missing.toml",
        ],
    );
}

#[test]
fn declarative_pack_invalid_toml_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, "schema = ");

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &["FAIL conformance-pack-loader", "parse conformance pack"],
    );
}

#[test]
fn declarative_pack_schema_mismatch_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        &max_line_count_pack(10).replacen("schema = 1", "schema = 2", 1),
    );

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "unsupported declarative conformance schema 2",
            "conformance-pack-loader",
        ],
    );
}

#[test]
fn declarative_pack_runner_protocol_mismatch_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        &max_line_count_pack(10).replacen(
            r#"runner_protocol = "fkst-declarative-rulepack@1""#,
            r#"runner_protocol = "fkst-declarative-rulepack@2""#,
            1,
        ),
    );

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "unsupported runner_protocol `fkst-declarative-rulepack@2`",
            "conformance-pack-loader",
        ],
    );
}

#[test]
fn declarative_pack_empty_rules_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        r#"
schema = 1
runner_protocol = "fkst-declarative-rulepack@1"
owner_package = "{{name}}"
rules = []
"#,
    );

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "declarative conformance pack must declare at least one rule",
            "conformance-pack-loader",
        ],
    );
}

#[test]
fn declarative_pack_bad_rule_id_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(
        &package,
        &max_line_count_pack(10).replacen("source.max-lines", "source max-lines", 1),
    );

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "rule id `source max-lines` must match [A-Za-z0-9._-]+",
            "conformance-pack-loader",
        ],
    );
}

#[test]
fn declarative_pack_absolute_include_glob_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &pack_with_include("/src/**/*.lua"));

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(&output, &["glob `/src/**/*.lua`", "package-relative"]);
}

#[test]
fn declarative_pack_parent_include_glob_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &pack_with_include("../host.lua"));

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(&output, &["glob `../host.lua`", "must not contain `..`"]);
}

#[test]
fn declarative_pack_empty_or_dot_glob_segments_fail_closed() {
    for include in ["src/", "src//*.rs", "./src/**/*.rs", "src/./**"] {
        let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
        let package = root.path().join("traveler");
        fs::create_dir_all(package.join("src")).unwrap();
        let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
        write_host_defaults(host.path());
        write_workspace_for_roots(host.path(), &[&package]);
        write_declarative_pack_package(&package, &pack_with_include(include));

        let output = run_package_conformance(host.path(), &package);

        assert_fail_closed(&output, &[&format!("glob `{include}`")]);
    }
}

#[test]
fn declarative_pack_symlinked_directory_alias_fails_closed() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &max_line_count_pack(10));
    fs::write(package.join("src/short.lua"), "return 1\n").unwrap();
    make_dir_symlink(&package.join("src"), &package.join("alias"));

    let output = run_package_conformance(host.path(), &package);

    assert_fail_closed(
        &output,
        &[
            "scan owner package files failed",
            "symlink entries are not allowed",
            "alias",
        ],
    );
}

#[test]
fn declarative_pack_cannot_inspect_host_or_sibling_files() {
    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    let sibling = root.path().join("sibling");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::create_dir_all(sibling.join("src")).unwrap();
    fs::write(sibling.join("src/long.lua"), "one\ntwo\nthree\n").unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    fs::create_dir_all(host.path().join("src")).unwrap();
    fs::write(host.path().join("src/long.lua"), "one\ntwo\nthree\n").unwrap();
    write_workspace_for_roots(host.path(), &[&package, &sibling]);
    write_declarative_pack_package(&package, &max_line_count_pack(2));
    write_package_consumer(&sibling, "unused");
    fs::write(package.join("src/short.lua"), "return 1\n").unwrap();

    let output = run_package_conformance(host.path(), &package);

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS source.max-lines"), "{log}");
    assert!(!log.contains("src/long.lua has 3 lines"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["violations"], serde_json::json!([]));
}

#[cfg(unix)]
#[test]
fn declarative_pack_non_utf8_relative_path_fails_closed() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let root = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = root.path().join("traveler");
    fs::create_dir_all(package.join("src")).unwrap();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(host.path());
    write_workspace_for_roots(host.path(), &[&package]);
    write_declarative_pack_package(&package, &max_line_count_pack(10));
    let bad_name = OsString::from_vec(b"bad-\xFF.lua".to_vec());
    let mut bad_path = package.join("src");
    bad_path.push(&bad_name);
    if fs::write(&bad_path, "return 1\n").is_err() {
        return;
    }

    let output = run_package_conformance(host.path(), &package);

    // A non-UTF-8 path under the package is rejected fail-closed. The engine's
    // module-path scan catches a non-UTF-8 `*.lua` module at startup (exit 2,
    // "non-utf8 module path") before the declarative runner ever sees it; the
    // runner's own non-utf8 guard is defense-in-depth for non-module files.
    // Assert fail-closed at whichever layer catches it (non-zero exit + a
    // non-utf8 rejection message), not a single layer's exit code.
    let code = output.status.code();
    assert!(
        matches!(code, Some(c) if c != 0),
        "expected non-zero fail-closed exit, got {code:?}\n{}",
        combined_log(&output)
    );
    let log = combined_log(&output);
    assert!(
        log.contains("non-utf8 module path")
            || log.contains("non-utf8 relative path component")
            || log.contains("invalid relative path"),
        "expected a non-utf8 rejection message in:\n{log}"
    );
}

#[test]
fn package_without_conformance_section_does_not_register_declarative_pack() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(!log.contains("conformance-pack-loader"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["counts"]["packs"], 1);
}

#[test]
fn semantic_conformance_function_returning_empty_errors_passes() {
    let package = tempfile::Builder::new()
        .prefix("traveler")
        .tempdir()
        .unwrap();
    write_semantic_conformance_package(package.path(), "    return {}\n");

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(package.path()),
    ];
    let output = run_conformance(&args, package.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(log.contains("PASS conformance-function"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["counts"]["packs"], 2);
    assert_eq!(report["violations"], serde_json::json!([]));
}

#[test]
fn semantic_conformance_function_returning_error_record_fails_for_unit() {
    let package = tempfile::Builder::new()
        .prefix("traveler")
        .tempdir()
        .unwrap();
    write_semantic_conformance_package(
        package.path(),
        r#"    return {{ id = "x", message = "bad" }}
"#,
    );

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(package.path()),
    ];
    let output = run_conformance(&args, package.path());

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL x bad"), "{log}");
    let report = stdout_json_report(&output);
    let package_name = unit_name(package.path());
    assert_eq!(report["ok"], false);
    assert_eq!(
        report["violations"][0]["rule"],
        format!("semantic:{package_name}.x")
    );
    assert_eq!(report["violations"][0]["package"], package_name);
    assert_eq!(report["violations"][0]["detail"], "bad");
}

#[test]
fn semantic_conformance_function_runs_for_library_unit() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());
    let library = host.path().join("libraries/stdlib");
    write_semantic_conformance_library(
        &library,
        "stdlib",
        r#"    return {{ id = "library.rule", message = "library bad" }}
"#,
    );
    write_workspace(host.path(), &[host.path(), &library]);

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 1);
    let log = combined_log(&output);
    assert!(log.contains("FAIL library.rule library bad"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(
        report["violations"][0]["rule"],
        "semantic:stdlib.library.rule"
    );
    assert_eq!(report["violations"][0]["package"], "stdlib");
}

#[test]
fn semantic_conformance_function_missing_module_fails_closed() {
    let package = tempfile::Builder::new()
        .prefix("traveler")
        .tempdir()
        .unwrap();
    write_semantic_conformance_package(package.path(), "    return {}\n");
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(package.path().join("fkst.toml"))
        .unwrap()
        .write_all(
            r#"
kind = "package"
name = "traveler"
persistence_class = "stateless_adapter"

[code]
root = "."

[lib_deps]
libraries = []

[conformance]
function = "missing.conformance_errors"
"#
            .as_bytes(),
        )
        .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(package.path()),
    ];
    let output = run_conformance(&args, package.path());

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-function-loader",
            "require.denied module `missing`",
            "conformance-function-loader",
        ],
    );
}

#[test]
fn semantic_manifest_malformed_function_field_fails_closed() {
    let package = tempfile::Builder::new()
        .prefix("traveler")
        .tempdir()
        .unwrap();
    write_semantic_conformance_package(package.path(), "    return {}\n");
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(package.path().join("fkst.toml"))
        .unwrap()
        .write_all(
            r#"
kind = "package"
name = "traveler"
persistence_class = "stateless_adapter"

[code]
root = "."

[lib_deps]
libraries = []

[conformance]
function = 42
"#
            .as_bytes(),
        )
        .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(package.path()),
    ];
    let output = run_conformance(&args, package.path());

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-function-loader",
            "invalid type: integer `42`",
            "conformance-function-loader",
        ],
    );
}

#[test]
fn semantic_conformance_function_missing_function_fails_closed() {
    let package = tempfile::Builder::new()
        .prefix("traveler")
        .tempdir()
        .unwrap();
    write_semantic_conformance_package(package.path(), "    return {}\n");
    fs::OpenOptions::new()
        .append(true)
        .open(package.path().join("fkst.toml"))
        .unwrap()
        .write_all(
            br#"
# keep the original conformance table unchanged
"#,
        )
        .unwrap();
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(package.path().join("core/init.lua"))
        .unwrap()
        .write_all(
            br#"
return {}
"#,
        )
        .unwrap();

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(package.path()),
    ];
    let output = run_conformance(&args, package.path());

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-function-loader",
            "function `conformance_errors` missing from module `core`",
            "conformance-function-loader",
        ],
    );
}

#[test]
fn semantic_conformance_function_returning_non_table_fails_closed() {
    let package = tempfile::Builder::new()
        .prefix("traveler")
        .tempdir()
        .unwrap();
    write_semantic_conformance_package(package.path(), r#"    return "not-table""#);

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(package.path()),
    ];
    let output = run_conformance(&args, package.path());

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-function-loader",
            "returned string, expected table",
            "conformance-function-loader",
        ],
    );
}

#[test]
fn semantic_conformance_function_raised_error_fails_closed() {
    let package = tempfile::Builder::new()
        .prefix("traveler")
        .tempdir()
        .unwrap();
    write_semantic_conformance_package(package.path(), r#"    error("semantic exploded")"#);

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(package.path()),
    ];
    let output = run_conformance(&args, package.path());

    assert_fail_closed(
        &output,
        &[
            "FAIL conformance-function-loader",
            "semantic exploded",
            "conformance-function-loader",
        ],
    );
}

#[test]
fn package_without_conformance_function_does_not_register_semantic_pack() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_minimal_host(host.path());

    let args = [
        std::ffi::OsStr::new("--project-root"),
        path_arg(host.path()),
    ];
    let output = run_conformance(&args, host.path());

    assert_exit(&output, 0);
    let log = combined_log(&output);
    assert!(!log.contains("conformance-function"), "{log}");
    assert!(!log.contains("semantic:"), "{log}");
    let report = stdout_json_report(&output);
    assert_eq!(report["ok"], true);
    assert_eq!(report["counts"]["packs"], 1);
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

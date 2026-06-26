use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn framework_command() -> Command {
    let mut command = Command::new(framework_bin());
    command.env_remove("FKST_SUPERVISOR_PID");
    command
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn write_generator_package(root: &Path) {
    write_generator_package_with_code_root(root, ".");
}

fn write_generator_package_with_code_root(root: &Path, code_root: &str) {
    write(
        &root.join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["."]

[generators.generator]
output_roots = ["dist"]
project_input_roots = ["content"]
"#,
    );
    let manifest = format!(
        r#"
kind = "package"
name = "generator"
persistence_class = "stateless_generator"

[code]
root = "{code_root}"

[generator]
suggested_output_roots = ["dist"]
package_input_roots = ["inputs"]
"#
    );
    write(&root.join("fkst.toml"), &manifest);
    fs::create_dir_all(root.join("inputs")).unwrap();
    fs::create_dir_all(root.join("content")).unwrap();
    fs::create_dir_all(root.join("dist")).unwrap();
    fs::create_dir_all(root.join(code_root)).unwrap();
    fs::write(root.join("inputs/source.txt"), "source").unwrap();
    fs::write(root.join("content/project.txt"), "project").unwrap();
}

fn run_department(package: &Path, department: &str) -> Output {
    framework_command()
        .arg("run")
        .arg(package.join(format!("departments/{department}/main.lua")))
        .arg("--project-root")
        .arg(package)
        .arg("--package-root")
        .arg(package)
        .arg("--event")
        .arg(r#"{"queue":"generator","payload":{}}"#)
        .current_dir(package)
        .env("FKST_RUNTIME_ROOT", package.join(".fkst/runtime"))
        .output()
        .unwrap()
}

fn run_department_with_host(host: &Path, package: &Path, department: &str) -> Output {
    framework_command()
        .arg("run")
        .arg(package.join(format!("departments/{department}/main.lua")))
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(package)
        .arg("--event")
        .arg(r#"{"queue":"generator","payload":{}}"#)
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
fn stateless_generator_splits_package_inputs_from_host_granted_outputs() {
    let host = tempfile::Builder::new()
        .prefix("stateless-generator-host")
        .tempdir()
        .unwrap();
    let package = tempfile::Builder::new()
        .prefix("generator")
        .tempdir()
        .unwrap();
    write(
        &host.path().join("fkst.workspace.toml"),
        r#"
[workspace]
units = []

[generators.generator]
output_roots = ["dist"]
project_input_roots = ["content"]
"#,
    );
    fs::create_dir_all(host.path().join("content")).unwrap();
    fs::write(host.path().join("content/project.txt"), "project").unwrap();
    write_generator_package(package.path());
    write(
        &package.path().join("departments/generate/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  assert(file.read("inputs/source.txt") == "source")
  assert(file.read("content/project.txt") == "project")
  file.write("dist/generated/out.txt", "ok")
end
return M
"#,
    );

    let output = run_department_with_host(host.path(), package.path(), "generate");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(host.path().join("dist/generated/out.txt")).unwrap(),
        "ok"
    );
    assert!(!package.path().join("dist/generated/out.txt").exists());
}

#[test]
fn stateless_generator_omits_effect_primitives_and_confines_file_writes() {
    let package = tempfile::Builder::new()
        .prefix("stateless-generator")
        .tempdir()
        .unwrap();
    write_generator_package(package.path());
    write(
        &package.path().join("departments/generate/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  assert(raise == nil, "raise must be omitted")
  assert(exec_argv == nil, "exec_argv must be omitted")
  assert(exec_sync == nil, "exec_sync must be omitted")
  assert(cache_set == nil, "cache_set must be omitted")
  assert(spawn_codex == nil, "spawn_codex must be omitted")
  assert(spawn_codex_sync == nil, "spawn_codex_sync must be omitted")
  assert(with_lock == nil, "with_lock must be omitted")
  assert(now == nil, "now must be omitted")
  assert(file.read("inputs/source.txt") == "source")
  assert(file.read("content/project.txt") == "project")
  file.write("dist/generated/out.txt", "ok")
end
return M
"#,
    );

    let output = run_department(package.path(), "generate");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(package.path().join("dist/generated/out.txt")).unwrap(),
        "ok"
    );
}

#[test]
fn stateless_generator_denies_file_write_outside_output_roots() {
    let package = tempfile::Builder::new()
        .prefix("stateless-generator-deny")
        .tempdir()
        .unwrap();
    write_generator_package(package.path());
    write(
        &package.path().join("departments/generate/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  file.write("fkst.toml", "bad")
end
return M
"#,
    );

    let output = run_department(package.path(), "generate");

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("outside stateless_generator roots"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(fs::read_to_string(package.path().join("fkst.toml"))
        .unwrap()
        .contains("stateless_generator"));
}

#[test]
fn stateless_generator_requires_host_output_grant() {
    let package = tempfile::Builder::new()
        .prefix("stateless-generator-no-grant")
        .tempdir()
        .unwrap();
    write_generator_package(package.path());
    write(
        &package.path().join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["."]
"#,
    );
    write(
        &package.path().join("departments/generate/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  file.write("dist/generated/out.txt", "ok")
end
return M
"#,
    );

    let output = run_department(package.path(), "generate");

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("stateless_generator_host_grant_missing"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn stateless_generator_denies_project_input_without_host_grant() {
    let package = tempfile::Builder::new()
        .prefix("stateless-generator-project-input-deny")
        .tempdir()
        .unwrap();
    write_generator_package(package.path());
    write(
        &package.path().join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["."]

[generators.generator]
output_roots = ["dist"]
"#,
    );
    write(
        &package.path().join("departments/generate/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  file.read("content/project.txt")
end
return M
"#,
    );

    let output = run_department(package.path(), "generate");

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("stateless_generator_fs_read_denied"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn stateless_generator_allows_host_source_mutation_only_with_opt_in() {
    let package = tempfile::Builder::new()
        .prefix("stateless-generator-host-source-opt-in")
        .tempdir()
        .unwrap();
    write_generator_package(package.path());
    write(
        &package.path().join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["."]

[generators.generator]
output_roots = ["."]
allow_host_source_mutation = true
"#,
    );
    write(
        &package.path().join("departments/generate/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  file.write("host-owned.txt", "ok")
end
return M
"#,
    );

    let output = run_department(package.path(), "generate");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(package.path().join("host-owned.txt")).unwrap(),
        "ok"
    );
}

#[test]
fn stateless_generator_host_grants_are_host_root_relative_not_code_root_relative() {
    let package = tempfile::Builder::new()
        .prefix("stateless-generator-code-root")
        .tempdir()
        .unwrap();
    write_generator_package_with_code_root(package.path(), "lua");
    write(
        &package.path().join("departments/generate/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  file.write("dist/generated.txt", "ok")
end
return M
"#,
    );

    let output = run_department(package.path(), "generate");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(package.path().join("dist/generated.txt")).unwrap(),
        "ok"
    );
    assert!(!package.path().join("lua/dist/generated.txt").exists());
}

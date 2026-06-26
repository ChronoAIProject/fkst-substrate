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
output_roots = ["dist"]
input_roots = ["inputs"]
"#
    );
    write(&root.join("fkst.toml"), &manifest);
    fs::create_dir_all(root.join("inputs")).unwrap();
    fs::create_dir_all(root.join("dist")).unwrap();
    fs::create_dir_all(root.join(code_root)).unwrap();
    fs::write(root.join("inputs/source.txt"), "source").unwrap();
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
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
fn stateless_generator_roots_are_unit_root_relative_not_code_root_relative() {
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

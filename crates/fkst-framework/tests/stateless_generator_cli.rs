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

fn write_host(root: &Path, generated_root: Option<&str>) {
    let output_root = generated_root.unwrap_or("src/_generated");
    write_host_with_generator_grant(
        root,
        generated_root,
        &format!(r#"["{output_root}"]"#),
        Some(r#"["content"]"#),
        false,
    );
}

fn write_host_with_generator_grant(
    root: &Path,
    generated_root: Option<&str>,
    output_roots: &str,
    project_input_roots: Option<&str>,
    allow_host_source_mutation: bool,
) {
    let project_input_roots = project_input_roots
        .map(|roots| format!("project_input_roots = {roots}\n"))
        .unwrap_or_default();
    let allow_host_source_mutation = if allow_host_source_mutation {
        "allow_host_source_mutation = true\n"
    } else {
        ""
    };
    write(
        &root.join("fkst.workspace.toml"),
        &format!(
            r#"
[workspace]
units = ["."]

[generators.generator]
output_roots = {output_roots}
{project_input_roots}{allow_host_source_mutation}
"#,
        ),
    );
    let generated_section = generated_root
        .map(|root| format!("\n[generated]\nroot = \"{root}\"\n"))
        .unwrap_or_default();
    write(
        &root.join("fkst.toml"),
        &format!(
            r#"
kind = "package"
name = "host"
persistence_class = "stateless_adapter"

[code]
root = "."
{generated_section}
"#
        ),
    );
    if let Some(generated_root) = generated_root {
        fs::create_dir_all(root.join(generated_root)).unwrap();
    }
    fs::create_dir_all(root.join("content")).unwrap();
    fs::write(root.join("content/project.txt"), "project").unwrap();
}

fn write_generator_package(root: &Path) {
    write_generator_package_with_code_root(root, ".");
}

fn write_generator_package_with_code_root(root: &Path, code_root: &str) {
    write_generator_package_with_roots(root, code_root, r#"["dist"]"#);
}

fn write_generator_package_with_roots(root: &Path, code_root: &str, output_roots: &str) {
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

[generated]
root = "dist"

[generator]
suggested_output_roots = {output_roots}
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

fn write_full_package(root: &Path) {
    write(
        &root.join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["."]
"#,
    );
    write(
        &root.join("fkst.toml"),
        r#"
kind = "package"
name = "full"
persistence_class = "stateless_adapter"

[code]
root = "."
"#,
    );
}

fn run_department(host: &Path, package: &Path, department: &str) -> Output {
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
    write_host(host.path(), Some("dist"));
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
    let host = tempfile::Builder::new()
        .prefix("stateless-generator-host")
        .tempdir()
        .unwrap();
    write_host(host.path(), Some("src/_generated"));
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
  file.write("src/_generated/generated/out.txt", "ok")
end
return M
"#,
    );

    let output = run_department(host.path(), package.path(), "generate");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(host.path().join("src/_generated/generated/out.txt")).unwrap(),
        "ok"
    );
    assert!(!package
        .path()
        .join("src/_generated/generated/out.txt")
        .exists());
}

#[test]
fn stateless_generator_denies_file_write_outside_output_roots() {
    let package = tempfile::Builder::new()
        .prefix("stateless-generator-deny")
        .tempdir()
        .unwrap();
    let host = tempfile::Builder::new()
        .prefix("stateless-generator-deny-host")
        .tempdir()
        .unwrap();
    write_host(host.path(), Some("src/_generated"));
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

    let output = run_department(host.path(), package.path(), "generate");

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

    let output = run_department(package.path(), package.path(), "generate");

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

    let output = run_department(package.path(), package.path(), "generate");

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("stateless_generator_fs_read_denied"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn stateless_generator_rejects_host_source_mutation_even_with_opt_in() {
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

    let output = run_department(package.path(), package.path(), "generate");

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("escapes generated namespace"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(!package.path().join("host-owned.txt").exists());
}

#[test]
fn stateless_generator_host_grants_are_host_root_relative_not_code_root_relative() {
    let package = tempfile::Builder::new()
        .prefix("stateless-generator-code-root")
        .tempdir()
        .unwrap();
    let host = tempfile::Builder::new()
        .prefix("stateless-generator-code-root-host")
        .tempdir()
        .unwrap();
    write_host(host.path(), Some("src/_generated"));
    write_generator_package_with_code_root(package.path(), "lua");
    write(
        &package.path().join("departments/generate/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  file.write("src/_generated/generated.txt", "ok")
end
return M
"#,
    );

    let output = run_department(host.path(), package.path(), "generate");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(host.path().join("src/_generated/generated.txt")).unwrap(),
        "ok"
    );
    assert!(!package.path().join("src/_generated/generated.txt").exists());
    assert!(!package
        .path()
        .join("lua/src/_generated/generated.txt")
        .exists());
}

#[test]
fn stateless_generator_rejects_output_roots_outside_host_generated_namespace() {
    for (output_roots, allow_host_source_mutation) in [
        (r#"["."]"#, true),
        (r#"["src"]"#, false),
        (r#"["../x"]"#, false),
    ] {
        let package = tempfile::Builder::new()
            .prefix("stateless-generator-escape")
            .tempdir()
            .unwrap();
        let host = tempfile::Builder::new()
            .prefix("stateless-generator-escape-host")
            .tempdir()
            .unwrap();
        write_host_with_generator_grant(
            host.path(),
            Some("src/_generated"),
            output_roots,
            Some(r#"["content"]"#),
            allow_host_source_mutation,
        );
        write_generator_package(package.path());
        write(
            &package.path().join("departments/generate/main.lua"),
            r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  file.write("src/_generated/out.txt", "bad")
end
return M
"#,
        );

        let output = run_department(host.path(), package.path(), "generate");

        assert!(
            !output.status.success(),
            "{output_roots} stdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        let err = stderr(&output);
        assert!(
            err.contains("escapes generated namespace") || err.contains("must not contain `..`"),
            "{output_roots} stderr: {err}"
        );
        assert!(!host.path().join("src/_generated/out.txt").exists());
    }
}

#[test]
fn stateless_generator_requires_host_generated_root() {
    let package = tempfile::Builder::new()
        .prefix("stateless-generator-missing-generated")
        .tempdir()
        .unwrap();
    let host = tempfile::Builder::new()
        .prefix("stateless-generator-missing-generated-host")
        .tempdir()
        .unwrap();
    write_host(host.path(), None);
    write_generator_package(package.path());
    write(
        &package.path().join("departments/generate/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  file.write("src/_generated/out.txt", "bad")
end
return M
"#,
    );

    let output = run_department(host.path(), package.path(), "generate");

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("requires host `[generated].root`"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(!host.path().join("src/_generated/out.txt").exists());
}

#[test]
fn full_department_keeps_unconfined_file_writes_without_generated_root() {
    let package = tempfile::Builder::new()
        .prefix("full-department")
        .tempdir()
        .unwrap();
    let host = tempfile::Builder::new()
        .prefix("full-department-host")
        .tempdir()
        .unwrap();
    write_host(host.path(), None);
    write_full_package(package.path());
    write(
        &package.path().join("departments/full/main.lua"),
        r#"
local M = {}
M.spec = {}
function M.pipeline(_)
  file.write("full-output.txt", "ok")
end
return M
"#,
    );

    let output = run_department(host.path(), package.path(), "full");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(host.path().join("full-output.txt")).unwrap(),
        "ok"
    );
}

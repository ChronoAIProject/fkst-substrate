use serde_json::Value as JsonValue;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn command() -> Command {
    Command::new(framework_bin())
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn quoted(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!(r#""{value}""#))
        .collect::<Vec<_>>()
        .join(", ")
}

fn workspace(root: &Path, units: &[&str]) {
    write(
        &root.join("fkst.workspace.toml"),
        &format!(
            r#"
[workspace]
units = [{}]
"#,
            quoted(units)
        ),
    );
}

fn package(root: &Path, name: &str, libs: &[&str], events: &[&str]) {
    write(
        &root.join(format!("packages/{name}/fkst.toml")),
        &format!(
            r#"
kind = "package"
name = "{name}"

[code]
root = "."

[lib_deps]
libraries = [{}]

[event_deps]
packages = [{}]
"#,
            quoted(libs),
            quoted(events)
        ),
    );
}

fn library(root: &Path, name: &str, libs: &[&str], allow: Option<&[&str]>) {
    let visibility = allow
        .map(|allow| {
            format!(
                r#"
[visibility]
allow = [{}]
"#,
                quoted(allow)
            )
        })
        .unwrap_or_default();
    write(
        &root.join(format!("libraries/{name}/fkst.toml")),
        &format!(
            r#"
kind = "library"
name = "{name}"

[code]
root = "."

[lib_deps]
libraries = [{}]

[library]
name = "{name}"
stable_id = "{name}"
version = "0.1.0"
{visibility}
"#,
            quoted(libs)
        ),
    );
}

fn deps(root: &Path) -> Command {
    let mut cmd = command();
    cmd.arg("deps").arg("--project-root").arg(root);
    cmd
}

#[test]
fn deps_passes_valid_workspace_and_reports_warnings() {
    let temp = tempfile::tempdir().unwrap();
    workspace(
        temp.path(),
        &[
            "packages/valid",
            "packages/unused",
            "libraries/std",
            "libraries/extra",
        ],
    );
    package(temp.path(), "valid", &["std"], &["unused"]);
    write(
        &temp.path().join("packages/valid/main.lua"),
        r#"
local json = require("std.fkst.json")
return json
"#,
    );
    write(
        &temp.path().join("packages/valid/composed.deps"),
        "unused\n",
    );
    package(temp.path(), "unused", &["extra"], &[]);
    write(&temp.path().join("packages/unused/main.lua"), "return {}\n");
    library(temp.path(), "std", &[], None);
    write(
        &temp.path().join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );
    library(temp.path(), "extra", &[], None);
    write(
        &temp.path().join("libraries/extra/public/tool.lua"),
        "return {}\n",
    );

    let output = deps(temp.path()).output().unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("fkst deps: PASS"), "{out}");
    assert!(out.contains("valid -> std"), "{out}");
    assert!(out.contains("[unused-lib-dep]"), "{out}");
    assert!(out.contains("unused declares library `extra`"), "{out}");
}

#[test]
fn deps_fails_for_undeclared_require_visibility_violation_and_cycle() {
    let temp = tempfile::tempdir().unwrap();
    workspace(
        temp.path(),
        &[
            "packages/valid",
            "packages/bad",
            "packages/ambiguous",
            "libraries/std",
            "libraries/restricted",
            "libraries/alpha",
            "libraries/beta",
            "libraries/cycle-a",
            "libraries/cycle-b",
        ],
    );
    package(temp.path(), "valid", &["std"], &[]);
    write(
        &temp.path().join("packages/valid/main.lua"),
        r#"return require("std.fkst.json")"#,
    );
    package(temp.path(), "bad", &["restricted", "ghost"], &[]);
    write(
        &temp.path().join("packages/bad/main.lua"),
        r#"
local json = require("std.fkst.json")
local missing = require("restricted.missing")
return { json = json, missing = missing }
"#,
    );
    write(&temp.path().join("packages/bad/composed.deps"), "valid\n");
    package(temp.path(), "ambiguous", &["alpha", "beta"], &[]);
    write(
        &temp.path().join("packages/ambiguous/main.lua"),
        "return {}\n",
    );
    library(temp.path(), "std", &[], None);
    write(
        &temp.path().join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );
    library(temp.path(), "restricted", &[], Some(&["valid"]));
    write(
        &temp.path().join("libraries/restricted/public/tool.lua"),
        "return {}\n",
    );
    library(temp.path(), "alpha", &[], None);
    write(
        &temp.path().join("libraries/alpha/public/shared.lua"),
        "return {}\n",
    );
    library(temp.path(), "beta", &[], None);
    write(
        &temp.path().join("libraries/beta/public/shared.lua"),
        "return {}\n",
    );
    library(temp.path(), "cycle-a", &["cycle-b"], None);
    write(
        &temp.path().join("libraries/cycle-a/public/a.lua"),
        "return {}\n",
    );
    library(temp.path(), "cycle-b", &["cycle-a"], None);
    write(
        &temp.path().join("libraries/cycle-b/public/b.lua"),
        "return {}\n",
    );

    let output = deps(temp.path()).output().unwrap();

    assert_exit(&output, 1);
    let out = stdout(&output);
    assert!(out.contains("fkst deps: FAIL"), "{out}");
    assert!(out.contains("[cycle]"), "{out}");
    assert!(out.contains("[missing-lib]"), "{out}");
    assert!(out.contains("[visibility]"), "{out}");
    assert!(
        out.contains("bad is not allowed to declare library `restricted`"),
        "{out}"
    );
    assert!(out.contains("[ambiguous-export]"), "{out}");
    assert!(out.contains("[undeclared-require]"), "{out}");
    assert!(out.contains("bad requires library `std`"), "{out}");
    assert!(out.contains("[missing-export]"), "{out}");
    assert!(out.contains("[event-deps]"), "{out}");
    assert!(stderr(&output).is_empty(), "stderr: {}", stderr(&output));
}

#[test]
fn deps_json_output_has_stable_shape() {
    let temp = tempfile::tempdir().unwrap();
    workspace(temp.path(), &["packages/app", "libraries/std"]);
    package(temp.path(), "app", &["std"], &[]);
    write(
        &temp.path().join("packages/app/main.lua"),
        r#"return require("std.fkst.json")"#,
    );
    library(temp.path(), "std", &[], None);
    write(
        &temp.path().join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );

    let output = deps(temp.path()).arg("--json").output().unwrap();

    assert_exit(&output, 0);
    let value: JsonValue = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], JsonValue::Bool(true));
    assert!(value["workspace_root"]
        .as_str()
        .unwrap()
        .contains(temp.path().file_name().unwrap().to_str().unwrap()));
    assert_eq!(value["units"].as_array().unwrap().len(), 2);
    assert_eq!(value["lib_edges"].as_array().unwrap().len(), 1);
    assert_eq!(value["event_edges"].as_array().unwrap().len(), 0);
    assert_eq!(value["failures"].as_array().unwrap().len(), 0);
    assert_eq!(value["warnings"].as_array().unwrap().len(), 0);
}

#[test]
fn deps_help_prints_usage() {
    let output = command().arg("deps").arg("--help").output().unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(
        out.contains("fkst-framework deps --project-root <root>"),
        "{out}"
    );
    assert!(out.contains("--json"), "{out}");
}

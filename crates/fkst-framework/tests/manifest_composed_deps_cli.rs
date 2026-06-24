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
        stdout(output),
        stderr(output)
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

fn manifest_composed_deps(path: &Path) -> Command {
    let mut cmd = command();
    cmd.arg("manifest")
        .arg("composed-deps")
        .arg("--manifest")
        .arg(path);
    cmd
}

#[test]
fn composed_manifest_prints_event_deps_in_declared_order() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("fkst.toml");
    write(
        &manifest,
        r#"
kind = "package.composed"
name = "app"

[code]
root = "."

[event_deps]
packages = ["b", "a"]
"#,
    );

    let output = manifest_composed_deps(&manifest).output().unwrap();

    assert_exit(&output, 0);
    assert_eq!(stdout(&output), "b\na\n");
    assert!(stderr(&output).is_empty(), "stderr: {}", stderr(&output));
}

#[test]
fn flat_manifest_exits_ten_with_empty_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("fkst.toml");
    write(
        &manifest,
        r#"
kind = "package"
name = "app"

[code]
root = "."

[event_deps]
packages = ["b", "a"]
"#,
    );

    let output = manifest_composed_deps(&manifest).output().unwrap();

    assert_exit(&output, 10);
    assert!(stdout(&output).is_empty(), "stdout: {}", stdout(&output));
    assert!(stderr(&output).is_empty(), "stderr: {}", stderr(&output));
}

#[test]
fn malformed_or_missing_manifest_exits_one_with_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let malformed = temp.path().join("fkst.toml");
    write(
        &malformed,
        r#"
kind = "package.composed"
name = "app"
root = "."
"#,
    );

    let malformed_output = manifest_composed_deps(&malformed).output().unwrap();

    assert_exit(&malformed_output, 1);
    assert!(stdout(&malformed_output).is_empty());
    assert!(
        stderr(&malformed_output).contains("manifest composed-deps error"),
        "stderr: {}",
        stderr(&malformed_output)
    );

    let missing_output = manifest_composed_deps(&temp.path().join("missing.toml"))
        .output()
        .unwrap();

    assert_exit(&missing_output, 1);
    assert!(stdout(&missing_output).is_empty());
    assert!(
        stderr(&missing_output).contains("manifest composed-deps error"),
        "stderr: {}",
        stderr(&missing_output)
    );
}

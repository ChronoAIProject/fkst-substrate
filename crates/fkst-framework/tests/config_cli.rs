use std::process::{Command, Output};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

const CONFIG_ENVS: &[&str] = &[
    "FKST_QUEUE_CAPACITY",
    "FKST_DEPARTMENT_DEFAULT_TIMEOUT",
    "FKST_CODEX_PERMIT_SLOTS",
    "FKST_CANDIDATE_PREFIX",
    "FKST_CANDIDATE_FROM_SEP",
    "FKST_PACKAGE_ROOT",
];

fn config_command(cwd: &std::path::Path) -> Command {
    let mut cmd = Command::new(framework_bin());
    cmd.arg("config").current_dir(cwd);
    for key in CONFIG_ENVS {
        cmd.env_remove(key);
    }
    cmd
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

#[test]
fn config_reads_host_fkst_env_from_project_root_when_cwd_differs() {
    let host = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(
        host.path().join("fkst.env"),
        "FKST_QUEUE_CAPACITY=31\nFKST_DEPARTMENT_DEFAULT_TIMEOUT=7m\nFKST_CODEX_PERMIT_SLOTS=9\nFKST_CANDIDATE_PREFIX=host-rc\nFKST_CANDIDATE_FROM_SEP=__from__\n",
    )
    .unwrap();

    let output = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert_eq!(out.lines().count(), 5, "{out}");
    assert!(out.contains("name=queue_capacity"), "{out}");
    assert!(out.contains("resolved=31 source=fkst.env"), "{out}");
    assert!(out.contains("name=candidate_prefix"), "{out}");
    assert!(out.contains("resolved=host-rc source=fkst.env"), "{out}");
}

#[test]
fn config_env_overrides_host_fkst_env() {
    let host = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(host.path().join("fkst.env"), "FKST_QUEUE_CAPACITY=31\n").unwrap();

    let output = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .env("FKST_QUEUE_CAPACITY", "44")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("name=queue_capacity"), "{out}");
    assert!(out.contains("resolved=44 source=env"), "{out}");
}

#[test]
fn config_operational_defaults_and_missing_host_facts_are_reported() {
    let host = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let output = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("name=queue_capacity"), "{out}");
    assert!(out.contains("resolved=16 source=default"), "{out}");
    assert!(out.contains("name=candidate_prefix"), "{out}");
    assert!(out.contains("resolved=missing source=missing"), "{out}");
}

#[test]
fn config_rejects_duplicate_unknown_and_missing_project_root_flags() {
    let host = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let duplicate = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .output()
        .unwrap();
    assert_exit(&duplicate, 2);
    assert!(stderr(&duplicate).contains("duplicate --project-root"));

    let unknown = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--output")
        .arg("json")
        .output()
        .unwrap();
    assert_exit(&unknown, 2);
    assert!(stderr(&unknown).contains("unknown config argument"));

    let missing_root = config_command(cwd.path())
        .arg("--package-root")
        .arg(host.path())
        .output()
        .unwrap();
    assert_exit(&missing_root, 2);
    assert!(stderr(&missing_root).contains("missing --project-root"));
}

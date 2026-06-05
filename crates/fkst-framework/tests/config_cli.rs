use std::process::{Command, Output};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

const CONFIG_ENVS: &[&str] = &[
    "FKST_QUEUE_CAPACITY",
    "FKST_DEPARTMENT_DEFAULT_STALL_WINDOW",
    "FKST_CODEX_PERMIT_SLOTS",
    "FKST_RETRY_DEFAULT_MAX_ATTEMPTS",
    "FKST_RETRY_DEFAULT_BASE",
    "FKST_RETRY_DEFAULT_CAP",
    "FKST_CANDIDATE_PREFIX",
    "FKST_CANDIDATE_FROM_SEP",
    "FKST_PACKAGE_ROOT",
    "FKST_PACKAGE_ROOTS",
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
        "FKST_QUEUE_CAPACITY=31\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=7m\nFKST_CODEX_PERMIT_SLOTS=9\nFKST_CANDIDATE_PREFIX=host-rc\nFKST_CANDIDATE_FROM_SEP=__from__\n",
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
    assert_eq!(out.lines().count(), 8, "{out}");
    assert!(out.contains("name=queue_capacity"), "{out}");
    assert!(out.contains("resolved=31 source=fkst.env"), "{out}");
    assert!(out.contains("name=retry_default_max_attempts"), "{out}");
    assert!(out.contains("resolved=5 source=default"), "{out}");
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
fn config_accepts_repeated_package_root_flags_over_package_roots_env() {
    let host = tempfile::tempdir().unwrap();
    let package_a = tempfile::tempdir().unwrap();
    let package_b = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let joined_env = std::env::join_paths([package_b.path()]).unwrap();

    let output = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(package_a.path())
        .arg("--package-root")
        .arg(package_b.path())
        .env("FKST_PACKAGE_ROOTS", joined_env)
        .output()
        .unwrap();

    assert_exit(&output, 0);
}

#[test]
fn config_uses_package_roots_env_and_rejects_plural_singular_conflict() {
    let host = tempfile::tempdir().unwrap();
    let package_a = tempfile::tempdir().unwrap();
    let package_b = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let joined = std::env::join_paths([package_a.path(), package_b.path()]).unwrap();

    let output = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .env("FKST_PACKAGE_ROOTS", &joined)
        .output()
        .unwrap();
    assert_exit(&output, 0);

    let conflict = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .env("FKST_PACKAGE_ROOTS", joined)
        .env("FKST_PACKAGE_ROOT", package_a.path())
        .output()
        .unwrap();
    assert_exit(&conflict, 2);
    let err = stderr(&conflict);
    assert!(err.contains("FKST_PACKAGE_ROOTS"), "{err}");
    assert!(err.contains("FKST_PACKAGE_ROOT"), "{err}");
    assert!(err.contains("mutually exclusive"), "{err}");
}

#[test]
fn config_rejects_duplicate_package_roots_after_canonicalization() {
    let host = tempfile::tempdir().unwrap();
    let package = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let duplicate_flags = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(package.path())
        .arg("--package-root")
        .arg(package.path())
        .output()
        .unwrap();
    assert_exit(&duplicate_flags, 2);
    let err = stderr(&duplicate_flags);
    assert!(err.contains("duplicate package root:"), "{err}");
    assert!(
        err.contains(&package.path().canonicalize().unwrap().display().to_string()),
        "{err}"
    );

    let joined = std::env::join_paths([package.path(), package.path()]).unwrap();
    let duplicate_env = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .env("FKST_PACKAGE_ROOTS", joined)
        .output()
        .unwrap();
    assert_exit(&duplicate_env, 2);
    assert!(stderr(&duplicate_env).contains("duplicate package root:"));
}

#[test]
fn config_single_package_entrypoints_are_equivalent() {
    let host = tempfile::tempdir().unwrap();
    let package = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let flag = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(package.path())
        .output()
        .unwrap();
    let singular = config_command(cwd.path())
        .arg("--project-root")
        .arg(host.path())
        .env("FKST_PACKAGE_ROOT", package.path())
        .output()
        .unwrap();
    let package_is_host = config_command(cwd.path())
        .arg("--project-root")
        .arg(package.path())
        .arg("--package-root")
        .arg(package.path())
        .output()
        .unwrap();

    assert_exit(&flag, 0);
    assert_exit(&singular, 0);
    assert_exit(&package_is_host, 0);
    assert_eq!(stdout(&flag), stdout(&singular));
    assert_eq!(stdout(&flag), stdout(&package_is_host));
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
    assert!(out.contains("name=retry_default_base"), "{out}");
    assert!(out.contains("resolved=60s source=default"), "{out}");
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

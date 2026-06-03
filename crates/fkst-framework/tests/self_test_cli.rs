use std::path::Path;
use std::process::Command;

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

const CODEX_PERMIT_SLOTS_ENV: &str = "FKST_CODEX_PERMIT_SLOTS";
const PACKAGE_ROOT_ENV: &str = "FKST_PACKAGE_ROOT";
const RUNTIME_ROOT_ENV: &str = "FKST_RUNTIME_ROOT";

fn run_git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(repo: &Path) {
    run_git(repo, &["init", "-q"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);
    run_git(repo, &["config", "user.name", "Test User"]);
    std::fs::write(repo.join("file.txt"), "content\n").unwrap();
    run_git(repo, &["add", "file.txt"]);
    run_git(
        repo,
        &["commit", "-q", "-m", "sdk git host fact regression"],
    );
}

#[test]
fn self_test_succeeds_in_temp_cwd() {
    let tmp = tempfile::tempdir().unwrap();

    let output = Command::new(framework_bin())
        .arg("--self-test")
        .env(RUNTIME_ROOT_ENV, ".fkst/runtime")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(tmp
        .path()
        .join(".fkst/runtime/codex-permits/permit-0")
        .is_file());
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("SELF_TEST_FAILED"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn self_test_rejects_config_file_input() {
    let tmp = tempfile::tempdir().unwrap();

    let output = Command::new(framework_bin())
        .arg("--self-test")
        .arg("--config")
        .arg(tmp.path().join("ignored"))
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown --self-test option: --config"),
        "{stderr}"
    );
}

#[test]
fn self_test_reports_permit_pool_failure() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".fkst/runtime")).unwrap();
    std::fs::write(
        tmp.path().join(".fkst/runtime/codex-permits"),
        "not a directory\n",
    )
    .unwrap();

    let output = Command::new(framework_bin())
        .arg("--self-test")
        .env(RUNTIME_ROOT_ENV, ".fkst/runtime")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SELF_TEST_FAILED:permit-pool"), "{stderr}");
}

#[test]
fn self_test_reports_permit_pool_slot_env_failure() {
    let tmp = tempfile::tempdir().unwrap();

    let output = Command::new(framework_bin())
        .arg("--self-test")
        .env(RUNTIME_ROOT_ENV, ".fkst/runtime")
        .env(CODEX_PERMIT_SLOTS_ENV, "0")
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SELF_TEST_FAILED:permit-pool"), "{stderr}");
    assert!(stderr.contains(CODEX_PERMIT_SLOTS_ENV), "{stderr}");
}

#[test]
fn run_subcommand_still_executes_pipeline() {
    let tmp = tempfile::tempdir().unwrap();
    let lua = tmp.path().join("dept.lua");
    std::fs::write(
        &lua,
        r#"
function pipeline(event)
    assert(event.name == "ok", "expected event name")
end
"#,
    )
    .unwrap();

    let output = Command::new(framework_bin())
        .arg("run")
        .arg(&lua)
        .arg("--event")
        .arg(r#"{"name":"ok"}"#)
        .env(PACKAGE_ROOT_ENV, tmp.path())
        .current_dir(tmp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn run_project_root_controls_host_facts_and_git_sdk_when_cwd_differs() {
    let root = tempfile::tempdir().unwrap();
    let host = root.path().join("host");
    let cwd = root.path().join("unrelated");
    std::fs::create_dir_all(host.join("departments/worker")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    init_repo(&host);
    std::fs::write(
        host.join("fkst.env"),
        "FKST_CANDIDATE_PREFIX=host-rc\nFKST_CANDIDATE_FROM_SEP=__base__\n",
    )
    .unwrap();
    let witness = host.join("witness.txt");
    let lua = host.join("departments/worker/main.lua");
    std::fs::write(
        &lua,
        format!(
            r#"
function pipeline(event)
    local count = git_log_count("sdk git host fact regression", "1970-01-01T00:00:00Z")
    local worktree = setup_worktree("host-root-test")
    local f = assert(io.open({:?}, "w"))
    f:write("count=" .. tostring(count) .. "\n")
    f:write("worktree=" .. worktree .. "\n")
    f:close()
    raise("done", {{ count = count }})
end
"#,
            witness.to_string_lossy()
        ),
    )
    .unwrap();

    let runtime_root = root.path().join("runtime");
    let output = Command::new(framework_bin())
        .arg("run")
        .arg(&lua)
        .arg("--project-root")
        .arg(&host)
        .arg("--package-root")
        .arg(&host)
        .arg("--event")
        .arg(r#"{"name":"ok"}"#)
        .env(RUNTIME_ROOT_ENV, &runtime_root)
        .env_remove(PACKAGE_ROOT_ENV)
        .env_remove(CODEX_PERMIT_SLOTS_ENV)
        .current_dir(&cwd)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("RAISED: "), "{stdout}");
    let body = std::fs::read_to_string(&witness).unwrap();
    assert!(body.contains("count=1\n"), "{body}");
    assert!(
        body.contains(&format!(
            "worktree={}/worktrees/host-root-test-",
            runtime_root.display()
        )),
        "{body}"
    );
    assert!(
        !cwd.join(".fkst/runtime/worktrees").exists(),
        "launcher cwd must not receive runtime worktrees"
    );
}

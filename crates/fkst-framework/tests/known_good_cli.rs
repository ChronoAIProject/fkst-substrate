use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn run_git(root: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write_and_commit(root: &std::path::Path, name: &str, body: &str) -> String {
    fs::write(root.join(name), body).unwrap();
    run_git(root, &["add", name]);
    run_git(root, &["commit", "-m", name]);
    run_git(root, &["rev-parse", "HEAD"])
}

fn write_minimal_host(root: &std::path::Path) {
    fs::create_dir_all(root.join("departments/hello")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::write(
        root.join("departments/hello/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"tick"}, timeout = "30s" }
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

fn repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    run_git(tmp.path(), &["init", "-q"]);
    run_git(tmp.path(), &["config", "user.name", "fkst-test"]);
    run_git(
        tmp.path(),
        &["config", "user.email", "fkst-test@example.invalid"],
    );
    write_minimal_host(tmp.path());
    run_git(tmp.path(), &["add", "departments", "raisers"]);
    run_git(tmp.path(), &["commit", "-m", "host graph"]);
    tmp
}

fn runtime_root(root: &std::path::Path) -> std::path::PathBuf {
    root.parent().unwrap().join("fkst-runtime")
}

#[test]
fn framework_known_good_bootstrap_creates_ref() {
    let tmp = repo();
    let candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    run_git(tmp.path(), &["branch", "integration/test", &candidate]);

    let output = Command::new(framework_bin())
        .arg("known-good")
        .arg("bootstrap")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--integration-ref")
        .arg("integration/test")
        .arg("--review")
        .arg("pass")
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env("FKST_RUNTIME_ROOT", runtime_root(tmp.path()))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("event=known-good-swap result=bootstrapped"));
    assert!(stdout.contains("trigger=manual-bootstrap"));
    assert_eq!(
        run_git(tmp.path(), &["rev-parse", "refs/known-good"]),
        candidate
    );
}

#[test]
fn framework_known_good_bootstrap_uses_env_integration_ref() {
    let tmp = repo();
    let candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    run_git(tmp.path(), &["branch", "integration/test", &candidate]);

    let output = Command::new(framework_bin())
        .arg("known-good")
        .arg("bootstrap")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--review")
        .arg("pass")
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env("FKST_INTEGRATION_BRANCH", "integration/test")
        .env("FKST_RUNTIME_ROOT", runtime_root(tmp.path()))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        run_git(tmp.path(), &["rev-parse", "refs/known-good"]),
        candidate
    );
}

#[test]
fn framework_known_good_bootstrap_uses_host_env_file_integration_ref() {
    let tmp = repo();
    let candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    run_git(tmp.path(), &["branch", "integration/test", &candidate]);
    fs::write(
        tmp.path().join("fkst.env"),
        "FKST_INTEGRATION_BRANCH=integration/test\n",
    )
    .unwrap();

    let output = Command::new(framework_bin())
        .arg("known-good")
        .arg("bootstrap")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--review")
        .arg("pass")
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env_remove("FKST_INTEGRATION_BRANCH")
        .env("FKST_RUNTIME_ROOT", runtime_root(tmp.path()))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        run_git(tmp.path(), &["rev-parse", "refs/known-good"]),
        candidate
    );
}

#[test]
fn framework_known_good_bootstrap_ignores_removed_tunable_integration_ref() {
    let tmp = repo();
    let candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    run_git(tmp.path(), &["branch", "integration/test", &candidate]);
    fs::create_dir_all(tmp.path().join("tunables")).unwrap();
    fs::write(
        tmp.path().join("tunables/integration_branch.txt"),
        "integration/test\n",
    )
    .unwrap();

    let output = Command::new(framework_bin())
        .arg("known-good")
        .arg("bootstrap")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--review")
        .arg("pass")
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env_remove("FKST_INTEGRATION_BRANCH")
        .env("FKST_RUNTIME_ROOT", runtime_root(tmp.path()))
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "removed tunable unexpectedly configured integration ref"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("integration-branch-unconfigured"),
        "{stderr}"
    );
    assert!(
        stderr.contains("FKST_INTEGRATION_BRANCH missing"),
        "{stderr}"
    );
}

#[test]
fn framework_known_good_bootstrap_missing_integration_ref_fails_closed() {
    let tmp = repo();
    fs::remove_dir_all(tmp.path().join("departments")).unwrap();
    run_git(tmp.path(), &["add", "-A"]);
    run_git(tmp.path(), &["commit", "-m", "break host graph"]);
    let candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    let head = run_git(tmp.path(), &["rev-parse", "HEAD"]);
    run_git(tmp.path(), &["branch", "integration/test", &candidate]);
    let log = tmp.path().join("supervisor.log");
    let runtime = tmp.path().join("runtime");

    let output = Command::new(framework_bin())
        .arg("known-good")
        .arg("bootstrap")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--review")
        .arg("pass")
        .arg("--supervisor-log")
        .arg(&log)
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env_remove("FKST_INTEGRATION_BRANCH")
        .env("FKST_RUNTIME_ROOT", &runtime)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "missing config unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("integration-branch-unconfigured"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.is_empty(),
        "missing integration config wrote machine evidence output: {stdout}"
    );
    assert!(
        !stdout.contains("graph-scan"),
        "missing integration config ran host conformance: {stdout}"
    );
    assert!(
        !stdout.contains("runtime-layout"),
        "missing integration config ran host conformance: {stdout}"
    );
    assert!(
        !stdout.contains("SELF_TEST_FAILED"),
        "missing integration config ran self-test: {stdout}"
    );
    assert!(
        !log.exists(),
        "missing integration config wrote supervisor log before failing"
    );
    assert!(
        !runtime.exists(),
        "missing integration config ran self-test permit setup before failing"
    );
    let missing_ref = Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["rev-parse", "--verify", "refs/known-good"])
        .output()
        .unwrap();
    assert!(
        !missing_ref.status.success(),
        "missing config wrote refs/known-good"
    );
    assert_eq!(run_git(tmp.path(), &["rev-parse", "HEAD"]), head);
}

#[test]
fn framework_known_good_promote_preserves_health_evidence() {
    let tmp = repo();
    let old = write_and_commit(tmp.path(), "base.txt", "base\n");
    run_git(tmp.path(), &["update-ref", "refs/known-good", &old]);
    run_git(tmp.path(), &["switch", "-c", "integration/test"]);
    let candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    run_git(tmp.path(), &["checkout", "--detach", "-q", &old]);
    let log = tmp.path().join("supervisor.log");

    let output = Command::new(framework_bin())
        .arg("known-good")
        .arg("promote")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--integration-ref")
        .arg("integration/test")
        .arg("--review")
        .arg("pass")
        .arg("--health-window-seconds")
        .arg("0")
        .arg("--health-poll-ms")
        .arg("1")
        .arg("--supervisor-log")
        .arg(&log)
        .env("FKST_INTEGRATION_BRANCH", "unused/default-guard")
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env("FKST_RUNTIME_ROOT", runtime_root(tmp.path()))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("event=known-good-swap result=promoted"));
    assert!(stdout.contains("KNOWN_GOOD_HEALTH:pass"));
    assert!(stdout.contains("reusable_by=known-good-promote,auto-rollback"));
    assert_eq!(
        run_git(tmp.path(), &["rev-parse", "refs/known-good"]),
        candidate
    );
    assert_eq!(run_git(tmp.path(), &["rev-parse", "HEAD"]), candidate);
    assert!(runtime_root(tmp.path())
        .join("locks/known-good-health.lock")
        .exists());
    assert!(!tmp.path().join(".framework.lock").exists());
    let log_body = fs::read_to_string(log).unwrap();
    assert!(log_body.contains("KNOWN_GOOD_HEALTH:start"));
    assert!(log_body.contains("KNOWN_GOOD_HEALTH:pass"));
}

#[test]
fn framework_known_good_promote_uses_runtime_locks_dir() {
    let tmp = repo();
    let old = write_and_commit(tmp.path(), "base.txt", "base\n");
    run_git(tmp.path(), &["update-ref", "refs/known-good", &old]);
    run_git(tmp.path(), &["switch", "-c", "integration/test"]);
    let _candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    let log = tmp.path().join("supervisor.log");
    let runtime = runtime_root(tmp.path());

    let output = Command::new(framework_bin())
        .arg("known-good")
        .arg("promote")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--integration-ref")
        .arg("integration/test")
        .arg("--review")
        .arg("pass")
        .arg("--health-window-seconds")
        .arg("0")
        .arg("--health-poll-ms")
        .arg("1")
        .arg("--supervisor-log")
        .arg(&log)
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env("FKST_RUNTIME_ROOT", &runtime)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(runtime.join("locks/known-good-health.lock").exists());
    assert!(!tmp.path().join(".framework.lock").exists());
}

#[test]
fn framework_known_good_promote_missing_runtime_root_fails_before_side_effects() {
    let tmp = repo();
    let old = write_and_commit(tmp.path(), "base.txt", "base\n");
    run_git(tmp.path(), &["update-ref", "refs/known-good", &old]);
    run_git(tmp.path(), &["switch", "-c", "integration/test"]);
    let _candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    run_git(tmp.path(), &["checkout", "--detach", "-q", &old]);
    let log = tmp.path().join("supervisor.log");
    let runtime = tmp.path().join("runtime-not-used");

    let output = Command::new(framework_bin())
        .arg("known-good")
        .arg("promote")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--integration-ref")
        .arg("integration/test")
        .arg("--review")
        .arg("pass")
        .arg("--health-window-seconds")
        .arg("0")
        .arg("--health-poll-ms")
        .arg("1")
        .arg("--supervisor-log")
        .arg(&log)
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env_remove("FKST_RUNTIME_ROOT")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error_class=conformance-fail"), "{stderr}");
    assert_eq!(run_git(tmp.path(), &["rev-parse", "refs/known-good"]), old);
    assert_eq!(run_git(tmp.path(), &["rev-parse", "HEAD"]), old);
    assert!(!log.exists());
    assert!(!runtime.exists());
    assert!(!tmp.path().join(".framework.lock").exists());
}

#[test]
fn framework_known_good_promote_runs_machine_evidence_before_ref_swap() {
    let tmp = repo();
    fs::remove_dir_all(tmp.path().join("departments")).unwrap();
    run_git(tmp.path(), &["add", "-A"]);
    run_git(tmp.path(), &["commit", "-m", "break host graph"]);
    let old = run_git(tmp.path(), &["rev-parse", "HEAD"]);
    run_git(tmp.path(), &["update-ref", "refs/known-good", &old]);
    run_git(tmp.path(), &["switch", "-c", "integration/test"]);
    let candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    let log = tmp.path().join("supervisor.log");

    let output = Command::new(framework_bin())
        .arg("known-good")
        .arg("promote")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--integration-ref")
        .arg("integration/test")
        .arg("--review")
        .arg("pass")
        .arg("--health-window-seconds")
        .arg("0")
        .arg("--health-poll-ms")
        .arg("1")
        .arg("--supervisor-log")
        .arg(&log)
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env("FKST_RUNTIME_ROOT", runtime_root(tmp.path()))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error_class=conformance-fail"), "{stderr}");
    assert_eq!(run_git(tmp.path(), &["rev-parse", "refs/known-good"]), old);
    assert_eq!(run_git(tmp.path(), &["rev-parse", "HEAD"]), candidate);
}

#[test]
fn framework_known_good_promote_failure_preserves_rollback_evidence() {
    let tmp = repo();
    let old = write_and_commit(tmp.path(), "base.txt", "base\n");
    run_git(tmp.path(), &["update-ref", "refs/known-good", &old]);
    run_git(tmp.path(), &["switch", "-c", "integration/test"]);
    let candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    let log = tmp.path().join("supervisor.log");
    let log_watcher = watch_log(&log);

    let child = Command::new(framework_bin())
        .arg("known-good")
        .arg("promote")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--integration-ref")
        .arg("integration/test")
        .arg("--review")
        .arg("pass")
        .arg("--health-window-seconds")
        .arg("5")
        .arg("--health-poll-ms")
        .arg("20")
        .arg("--supervisor-log")
        .arg(&log)
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env("FKST_RUNTIME_ROOT", runtime_root(tmp.path()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    log_watcher.wait_for("KNOWN_GOOD_HEALTH:start");
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(
            b"event=framework-failed dept=evolve exit_code=7 timed_out=false elapsed_ms=42 stderr=boom\n",
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("KNOWN_GOOD_HEALTH:fail"), "{stderr}");
    assert!(stderr.contains("event=known-good-rollback"), "{stderr}");
    assert!(stderr.contains("action=checkout-known-good"), "{stderr}");
    assert!(
        stderr.contains("error_class=framework-exit-nonzero:7"),
        "{stderr}"
    );
    assert_eq!(run_git(tmp.path(), &["rev-parse", "refs/known-good"]), old);
    assert_eq!(run_git(tmp.path(), &["rev-parse", "HEAD"]), old);
    assert_ne!(candidate, old);
}

#[test]
fn framework_known_good_promote_stall_timeout_preserves_rollback_evidence() {
    let tmp = repo();
    let old = write_and_commit(tmp.path(), "base.txt", "base\n");
    run_git(tmp.path(), &["update-ref", "refs/known-good", &old]);
    run_git(tmp.path(), &["switch", "-c", "integration/test"]);
    let _candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    let log = tmp.path().join("supervisor.log");
    let log_watcher = watch_log(&log);

    let child = Command::new(framework_bin())
        .arg("known-good")
        .arg("promote")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--integration-ref")
        .arg("integration/test")
        .arg("--review")
        .arg("pass")
        .arg("--health-window-seconds")
        .arg("5")
        .arg("--health-poll-ms")
        .arg("20")
        .arg("--supervisor-log")
        .arg(&log)
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env("FKST_RUNTIME_ROOT", runtime_root(tmp.path()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    log_watcher.wait_for("KNOWN_GOOD_HEALTH:start");
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(
            b"event=framework-stall-timeout dept=evolve stall_window=600s elapsed_ms=600000\n",
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("KNOWN_GOOD_HEALTH:fail"), "{stderr}");
    assert!(stderr.contains("event=known-good-rollback"), "{stderr}");
    assert!(stderr.contains("action=checkout-known-good"), "{stderr}");
    assert!(
        stderr.contains("error_class=framework-stall-timeout"),
        "{stderr}"
    );
    assert_eq!(run_git(tmp.path(), &["rev-parse", "refs/known-good"]), old);
    assert_eq!(run_git(tmp.path(), &["rev-parse", "HEAD"]), old);
}

#[test]
fn framework_known_good_promote_dirty_checkout_blocks_rollback() {
    let tmp = repo();
    let old = write_and_commit(tmp.path(), "base.txt", "base\n");
    run_git(tmp.path(), &["update-ref", "refs/known-good", &old]);
    run_git(tmp.path(), &["switch", "-c", "integration/test"]);
    let candidate = write_and_commit(tmp.path(), "candidate.txt", "candidate\n");
    fs::write(tmp.path().join("candidate.txt"), "dirty\n").unwrap();
    fs::write(tmp.path().join(".framework.lock"), "dirty lock\n").unwrap();
    let log = tmp.path().join("supervisor.log");
    let log_watcher = watch_log(&log);

    let child = Command::new(framework_bin())
        .arg("known-good")
        .arg("promote")
        .arg("--project-root")
        .arg(tmp.path())
        .arg("--integration-ref")
        .arg("integration/test")
        .arg("--review")
        .arg("pass")
        .arg("--health-window-seconds")
        .arg("5")
        .arg("--health-poll-ms")
        .arg("20")
        .arg("--supervisor-log")
        .arg(&log)
        .env("FKST_PACKAGE_ROOT", tmp.path())
        .env("FKST_RUNTIME_ROOT", runtime_root(tmp.path()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    log_watcher.wait_for("KNOWN_GOOD_HEALTH:start");
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(
            b"event=framework-failed dept=evolve exit_code=7 timed_out=false elapsed_ms=42 stderr=boom\n",
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error_class=rollback-blocked-dirty-worktree"),
        "{stderr}"
    );
    assert!(stderr.contains("original_failure_class=framework-exit-nonzero:7"));
    assert!(stderr.contains("action=blocked"));
    assert_eq!(run_git(tmp.path(), &["rev-parse", "refs/known-good"]), old);
    assert_eq!(run_git(tmp.path(), &["rev-parse", "HEAD"]), candidate);
    assert!(tmp.path().join(".framework.lock").exists());
}

struct LogWatcher {
    path: std::path::PathBuf,
    rx: Receiver<notify::Result<notify::Event>>,
    _watcher: RecommendedWatcher,
}

fn watch_log(path: &std::path::Path) -> LogWatcher {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx).unwrap();
    watcher
        .watch(path.parent().unwrap(), RecursiveMode::NonRecursive)
        .unwrap();
    LogWatcher {
        path: path.to_path_buf(),
        rx,
        _watcher: watcher,
    }
}

impl LogWatcher {
    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if fs::read_to_string(&self.path)
                .map(|body| body.contains(needle))
                .unwrap_or(false)
            {
                return;
            }
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for {needle} in {}",
                self.path.display()
            );
            match self.rx.recv_timeout(deadline - now) {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => panic!("log watch failed: {err}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for {needle} in {}", self.path.display())
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("log watch disconnected"),
            }
        }
    }
}

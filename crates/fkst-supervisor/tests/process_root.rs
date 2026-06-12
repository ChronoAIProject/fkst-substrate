use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn write_fake_runtime(dir: &std::path::Path, exit_code: i32) -> std::path::PathBuf {
    let path = dir.join("fkst-framework");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FAKE_RUNTIME_ARGS\"\nexit {}\n",
            exit_code
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn write_signal_runtime(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("fkst-framework");
    fs::write(
        &path,
        r#"#!/bin/sh
printf '%s\n' "$$" > "$FAKE_RUNTIME_PID"
trap 'printf TERM > "$FAKE_RUNTIME_SIGNAL"; exit 0' TERM
trap 'printf INT > "$FAKE_RUNTIME_SIGNAL"; exit 0' INT
printf ready > "$FAKE_RUNTIME_READY"
while :; do
  :
done
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn supervisor_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-supervisor")
}

fn copy_supervisor_to(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("fkst-supervisor");
    fs::copy(supervisor_bin(), &path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn process_root_returns_runtime_success() {
    let tmp = tempfile::tempdir().unwrap();
    let args_path = tmp.path().join("args.txt");
    let runtime = write_fake_runtime(tmp.path(), 0);
    let output = Command::new(supervisor_bin())
        .current_dir(tmp.path())
        .env("FKST_FRAMEWORK_BIN", &runtime)
        .env("FAKE_RUNTIME_ARGS", &args_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args = fs::read_to_string(args_path).unwrap();
    assert!(args.contains("supervise"));
    assert!(args.contains("--project-root"));
    assert!(args.contains("--framework-bin"));
    assert!(!args.contains("departments/"));
    assert!(!args.contains("raisers/"));
}

#[test]
fn process_root_does_not_forward_external_cli_arguments() {
    let tmp = tempfile::tempdir().unwrap();
    let args_path = tmp.path().join("args.txt");
    let runtime = write_fake_runtime(tmp.path(), 0);
    let output = Command::new(supervisor_bin())
        .current_dir(tmp.path())
        .arg("external-command")
        .arg("external-action")
        .env("FKST_FRAMEWORK_BIN", &runtime)
        .env("FAKE_RUNTIME_ARGS", &args_path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args = fs::read_to_string(args_path).unwrap();
    assert!(args.contains("supervise"));
    assert!(!args.contains("external-command"));
    assert!(!args.contains("external-action"));
}

#[test]
fn process_root_returns_runtime_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let args_path = tmp.path().join("args.txt");
    let runtime = write_fake_runtime(tmp.path(), 7);
    let output = Command::new(supervisor_bin())
        .current_dir(tmp.path())
        .env("FKST_FRAMEWORK_BIN", &runtime)
        .env("FAKE_RUNTIME_ARGS", &args_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
}

#[test]
fn process_root_uses_default_sibling_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let supervisor = copy_supervisor_to(tmp.path());
    let args_path = tmp.path().join("args.txt");
    write_fake_runtime(tmp.path(), 0);

    let output = Command::new(supervisor)
        .current_dir(tmp.path())
        .env_remove("FKST_FRAMEWORK_BIN")
        .env("FAKE_RUNTIME_ARGS", &args_path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args = fs::read_to_string(args_path).unwrap();
    assert!(args.contains("supervise"));
    assert!(args.contains("--framework-bin"));
    assert!(args.contains("fkst-framework"));
}

#[test]
fn process_root_reports_missing_default_sibling_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let supervisor = copy_supervisor_to(tmp.path());

    let output = Command::new(supervisor)
        .current_dir(tmp.path())
        .env_remove("FKST_FRAMEWORK_BIN")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("fkst-framework not found at"),
        "stderr={stderr}"
    );
    assert!(
        stderr.contains("build the workspace or set FKST_FRAMEWORK_BIN"),
        "stderr={stderr}"
    );
}

fn wait_for_process_exit(pid: i32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn assert_process_root_terminates_runtime_group(
    signal: nix::sys::signal::Signal,
    expected_code: i32,
) {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = write_signal_runtime(tmp.path());
    let ready_path = tmp.path().join("ready.txt");
    let signal_path = tmp.path().join("signal.txt");
    let pid_path = tmp.path().join("runtime.pid");
    let mut child = Command::new(supervisor_bin())
        .current_dir(tmp.path())
        .env("FKST_FRAMEWORK_BIN", &runtime)
        .env("FAKE_RUNTIME_READY", &ready_path)
        .env("FAKE_RUNTIME_SIGNAL", &signal_path)
        .env("FAKE_RUNTIME_PID", &pid_path)
        .spawn()
        .unwrap();

    loop {
        if ready_path.is_file() {
            break;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "supervisor exited before runtime was ready"
        );
        std::thread::yield_now();
    }

    nix::sys::signal::kill(nix::unistd::Pid::from_raw(child.id() as i32), signal).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(expected_code), "status={status}");
    assert!(signal_path.exists(), "runtime signal trap did not fire");
    assert!(
        ["TERM", "INT"].contains(&fs::read_to_string(&signal_path).unwrap().as_str()),
        "runtime signal trap fired with {}",
        fs::read_to_string(&signal_path).unwrap_or_default()
    );
    let runtime_pid: i32 = fs::read_to_string(pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_for_process_exit(runtime_pid, std::time::Duration::from_secs(2)),
        "runtime process survived supervisor termination"
    );
}

#[test]
fn process_root_terminates_runtime_group_on_sigterm() {
    assert_process_root_terminates_runtime_group(nix::sys::signal::Signal::SIGTERM, 143);
}

#[test]
fn process_root_terminates_runtime_group_on_sigint() {
    assert_process_root_terminates_runtime_group(nix::sys::signal::Signal::SIGINT, 130);
}

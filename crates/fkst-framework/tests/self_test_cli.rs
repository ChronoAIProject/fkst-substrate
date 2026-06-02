use std::process::Command;

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

const CODEX_PERMIT_SLOTS_ENV: &str = "FKST_CODEX_PERMIT_SLOTS";
const PACKAGE_ROOT_ENV: &str = "FKST_PACKAGE_ROOT";
const RUNTIME_ROOT_ENV: &str = "FKST_RUNTIME_ROOT";

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

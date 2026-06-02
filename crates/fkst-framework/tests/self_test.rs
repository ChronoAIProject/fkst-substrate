use std::process::Command;

#[test]
fn self_test_reports_ready() {
    let cwd = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .arg("--self-test")
        .env("FKST_RUNTIME_ROOT", ".fkst/runtime")
        .current_dir(cwd.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("SELF_TEST_FAILED"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

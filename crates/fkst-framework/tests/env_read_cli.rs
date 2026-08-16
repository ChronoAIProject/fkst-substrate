use std::fs;
use std::process::Command;

mod support;

use support::manifest_fixture::write_single_package_workspace;

#[test]
fn conformance_graph_scan_exposes_env_read_without_reading_environment() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    write_single_package_workspace(host.path());
    fs::create_dir_all(host.path().join("departments/hello")).unwrap();
    fs::create_dir_all(host.path().join("raisers")).unwrap();
    fs::write(
        host.path().join("fkst.env"),
        "FKST_QUEUE_CAPACITY=100\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=30m\nFKST_CODEX_PERMIT_SLOTS=20\n",
    )
    .unwrap();
    fs::write(
        host.path().join("departments/hello/main.lua"),
        r#"
local M = {}
assert(type(env_read) == "function", "env_read-missing: this package requires an engine providing env_read")
local ok, err = pcall(env_read, "FKST_RUNTIME_ROOT")
assert(ok == false, "env_read must not read host environment during graph scan")
assert(tostring(err):find("graph scan", 1, true) ~= nil, "env_read graph-scan denial must be explicit")
M.spec = { consumes = {"tick"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
    )
    .unwrap();
    fs::write(
        host.path().join("raisers/tick.lua"),
        r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .arg("conformance")
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .current_dir(host.path())
        .env_remove("FKST_PACKAGE_ROOT")
        .env_remove("FKST_PACKAGE_ROOTS")
        .env_remove("FKST_SUPERVISOR_PID")
        .env("FKST_RUNTIME_ROOT", runtime.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

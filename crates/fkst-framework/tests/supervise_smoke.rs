use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod sdk_codex {
    pub const CODEX_PERMIT_SLOTS_ENV: &str = "FKST_CODEX_PERMIT_SLOTS";
}
#[path = "../src/supervise/spawner.rs"]
mod spawner;
mod support;

use spawner::{spawn_framework, SpawnResult};
use support::process_sandbox::ProcessSandbox;

fn write_executable(path: &std::path::Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_graph_defaults(root: &std::path::Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("fkst.env"),
        "FKST_QUEUE_CAPACITY=100\nFKST_DEPARTMENT_DEFAULT_TIMEOUT=30m\nFKST_CODEX_PERMIT_SLOTS=20\n",
    )
    .unwrap();
}

fn fake_framework(root: &Path, body: &str) -> PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = root.join(format!("fkst-framework-{id}"));
    write_executable(&path, &format!("#!/bin/sh\n{body}\n"));
    path
}

fn read_log(result: &SpawnResult) -> String {
    let path = result
        .log_path
        .as_ref()
        .expect("spawn result should include log path");
    std::fs::read_to_string(path).unwrap()
}

fn lua_string(path: &Path) -> String {
    format!("{:?}", path.to_string_lossy())
}

#[test]
fn supervise_dispatches_file_watch_event_to_department() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("departments/recorder")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::write(
        root.join("fkst.env"),
        "FKST_QUEUE_CAPACITY=100\nFKST_DEPARTMENT_DEFAULT_TIMEOUT=30m\nFKST_CODEX_PERMIT_SLOTS=20\n",
    )
    .unwrap();
    fs::write(
        root.join("departments/recorder/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"files"}, timeout = "5s" }
function pipeline(event)
  local f = assert(io.open("seen.txt", "w"))
  f:write(event.payload.path or "")
  f:close()
end
return M
"#,
    )
    .unwrap();
    fs::write(
        root.join("raisers/files.lua"),
        format!(
            r#"return {{ type = "file_watch", glob = "{}", produces = "files" }}"#,
            root.join("input.txt").display()
        ),
    )
    .unwrap();
    fs::write(root.join("input.txt"), "ready").unwrap();

    let fake = root.join("fkst-framework");
    write_executable(
        &fake,
        "#!/bin/sh\n{\nprintf '%s\\n' \"$*\"\nprintf 'slots=%s\\n' \"$FKST_CODEX_PERMIT_SLOTS\"\n} > seen.txt\nkill -TERM \"$PPID\"\n",
    );

    let status = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .current_dir(root)
        .arg("supervise")
        .arg("--project-root")
        .arg(root)
        .arg("--package-root")
        .arg(root)
        .arg("--framework-bin")
        .arg(&fake)
        .env("FKST_RUNTIME_ROOT", ".fkst/runtime")
        .status()
        .unwrap();

    assert!(status.success(), "status={status}");
    let seen = fs::read_to_string(root.join("seen.txt")).unwrap();
    assert!(seen.contains("run"), "seen={seen}");
    assert!(
        seen.contains("departments/recorder/main.lua"),
        "seen={seen}"
    );
    assert!(seen.contains("--event"), "seen={seen}");
    assert!(seen.contains("slots=20"), "seen={seen}");
    assert!(
        !root.join(".fkst/runtime/codex-permits").exists(),
        "supervise should not create codex permits"
    );
}

#[test]
fn supervise_env_package_root_reaches_child_framework() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let package = root.join("package-root");
    let host = root.join("host-root");
    let fact = host.join("package-root-fact.txt");
    write_graph_defaults(&package);
    fs::create_dir_all(package.join("fkst")).unwrap();
    fs::create_dir_all(package.join("raisers")).unwrap();
    fs::create_dir_all(host.join("departments/host_worker")).unwrap();
    fs::write(package.join("input.txt"), "ready").unwrap();
    fs::write(
        package.join("fkst/standard_asset.lua"),
        r#"
return {
  marker = function() return "package-standard-marker" end,
  timeout = function() return "5s" end,
}
"#,
    )
    .unwrap();
    fs::write(
        package.join("raisers/standard_input.lua"),
        format!(
            r#"return {{ type = "file_watch", glob = {}, produces = "standard_input" }}"#,
            lua_string(&package.join("input.txt"))
        ),
    )
    .unwrap();
    fs::write(
        host.join("departments/host_worker/main.lua"),
        format!(
            r#"
local standard = require("fkst.standard_asset")
local M = {{}}
M.spec = {{ consumes = {{"standard_input"}}, timeout = standard.timeout() }}
function pipeline(event)
  local f = assert(io.open({}, "w"))
  f:write("marker=" .. standard.marker() .. "\n")
  f:write("event_path=" .. tostring(event.payload.path) .. "\n")
  f:close()
  exec_sync([[child_ppid=$(ps -o ppid= -p $$ | tr -d ' ')
supervise_pid=$(ps -o ppid= -p "$child_ppid" | tr -d ' ')
kill -TERM "$supervise_pid"]])
end
return M
"#,
            lua_string(&fact)
        ),
    )
    .unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .current_dir(&host)
        .arg("supervise")
        .arg("--project-root")
        .arg(&host)
        .arg("--framework-bin")
        .arg(env!("CARGO_BIN_EXE_fkst-framework"))
        .env("FKST_PACKAGE_ROOT", &package)
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .status()
        .unwrap();

    assert!(status.success(), "status={status}");
    let body = fs::read_to_string(&fact).unwrap();
    assert!(
        body.contains("marker=package-standard-marker\n"),
        "body={body}"
    );
    let input_path = package.join("input.txt").canonicalize().unwrap();
    assert!(
        body.contains(&format!("event_path={}", input_path.display())),
        "body={body}"
    );
}

#[tokio::test]
async fn spawn_framework_passes_codex_permit_slots_env() {
    let sandbox = ProcessSandbox::new();
    let binary = fake_framework(
        sandbox.root(),
        r#"printf '%s\n' "$FKST_CODEX_PERMIT_SLOTS"; exit 0"#,
    );
    let lua_dummy = sandbox.temp_path("dept.lua");
    fs::write(&lua_dummy, "return {}\n").unwrap();
    let logs = sandbox.temp_path("logs");

    let result = spawn_framework(
        &binary,
        &lua_dummy,
        sandbox.root(),
        sandbox.root(),
        "{}",
        Duration::from_secs(5),
        7,
        "permit",
        &logs,
    )
    .await
    .unwrap();

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "7");
}

#[tokio::test]
async fn framework_child_log_records_final_metadata_for_exit_modes() {
    let sandbox = ProcessSandbox::new();
    let lua_dummy = sandbox.temp_path("dept.lua");
    fs::write(&lua_dummy, "return {}\n").unwrap();
    let logs = sandbox.temp_path("logs");

    let success = fake_framework(sandbox.root(), r#"printf 'ok\n'; exit 0"#);
    let success_result = spawn_framework(
        &success,
        &lua_dummy,
        sandbox.root(),
        sandbox.root(),
        "{}",
        Duration::from_secs(5),
        20,
        "success",
        &logs,
    )
    .await
    .unwrap();
    let success_log = read_log(&success_result);
    assert!(success_log.contains("CMD="));
    assert!(success_log.contains("PID="));
    assert!(success_log.contains("LUA="));
    assert!(success_log.contains("PACKAGE_ROOT="));
    assert!(success_log.contains("DEPT=success"));
    assert!(success_log.contains("EXIT=0\n"));
    assert!(success_log.contains("STALLED=false\n"));
    assert!(success_log.contains("ELAPSED_MS="));
    assert!(success_log.contains("LAST_OUTPUT_AGE_MS="));

    let nonzero = fake_framework(sandbox.root(), r#"printf 'bad\n' >&2; exit 9"#);
    let nonzero_result = spawn_framework(
        &nonzero,
        &lua_dummy,
        sandbox.root(),
        sandbox.root(),
        "{}",
        Duration::from_secs(5),
        20,
        "nonzero-meta",
        &logs,
    )
    .await
    .unwrap();
    let nonzero_log = read_log(&nonzero_result);
    assert!(nonzero_log.contains("CMD="));
    assert!(nonzero_log.contains("PID="));
    assert!(nonzero_log.contains("LUA="));
    assert!(nonzero_log.contains("DEPT=nonzero-meta"));
    assert!(nonzero_log.contains("EXIT=9\n"));
    assert!(nonzero_log.contains("STALLED=false\n"));
    assert!(nonzero_log.contains("ELAPSED_MS="));
    assert!(nonzero_log.contains("LAST_OUTPUT_AGE_MS="));

    let stall = fake_framework(sandbox.root(), r#"while :; do :; done"#);
    let stall_result = spawn_framework(
        &stall,
        &lua_dummy,
        sandbox.root(),
        sandbox.root(),
        "{}",
        Duration::from_millis(120),
        20,
        "stall-meta",
        &logs,
    )
    .await
    .unwrap();
    let stall_log = read_log(&stall_result);
    assert!(stall_log.contains("CMD="));
    assert!(stall_log.contains("PID="));
    assert!(stall_log.contains("LUA="));
    assert!(stall_log.contains("DEPT=stall-meta"));
    assert!(stall_log.contains("STALL_KILL_PID="));
    assert!(stall_log.contains("EXIT=124\n"));
    assert!(stall_log.contains("STALLED=true\n"));
    assert!(stall_log.contains("ELAPSED_MS="));
    assert!(stall_log.contains("LAST_OUTPUT_AGE_MS="));
}

#[tokio::test]
async fn framework_child_log_failure_preserves_spawn_result() {
    let sandbox = ProcessSandbox::new();
    let binary = fake_framework(
        sandbox.root(),
        r#"printf 'stdout-stays\n'; printf 'stderr-stays\n' >&2; exit 6"#,
    );
    let lua_dummy = sandbox.temp_path("dept.lua");
    fs::write(&lua_dummy, "return {}\n").unwrap();
    let blocked_log_path = sandbox.temp_path("not-a-directory");
    fs::write(&blocked_log_path, "occupied").unwrap();

    let result = spawn_framework(
        &binary,
        &lua_dummy,
        sandbox.root(),
        sandbox.root(),
        "{}",
        Duration::from_secs(5),
        20,
        "blocked",
        &blocked_log_path,
    )
    .await
    .unwrap();

    assert_eq!(result.exit_code, 6);
    assert_eq!(result.stdout, "stdout-stays\n");
    assert_eq!(result.stderr, "stderr-stays\n");
    assert!(!result.stalled);
    assert!(result.log_path.is_none());
}

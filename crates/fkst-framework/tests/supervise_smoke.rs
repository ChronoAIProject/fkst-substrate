use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

mod sdk_codex {
    pub const CODEX_PERMIT_SLOTS_ENV: &str = "FKST_CODEX_PERMIT_SLOTS";
}
#[path = "../src/process_tree.rs"]
mod process_tree;
#[path = "../src/provenance.rs"]
mod provenance;
#[path = "../src/supervise/spawner.rs"]
mod spawner;
mod support;

use spawner::{spawn_framework, SpawnResult};
use support::manifest_fixture::{write_single_package_workspace, write_workspace_for_roots};
use support::process_sandbox::ProcessSandbox;

static SUPERVISE_SMOKE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn supervise_smoke_lock() -> MutexGuard<'static, ()> {
    SUPERVISE_SMOKE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap()
}

fn write_executable(path: &std::path::Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_graph_defaults(root: &std::path::Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("fkst.env"),
        "FKST_QUEUE_CAPACITY=100\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=30m\nFKST_CODEX_PERMIT_SLOTS=20\n",
    )
    .unwrap();
}

fn write_fkst_env(root: &std::path::Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("fkst.env"),
        "FKST_QUEUE_CAPACITY=100\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=30s\nFKST_CODEX_PERMIT_SLOTS=20\n",
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

fn namespace(root: &Path) -> String {
    root.canonicalize()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn wait_for_file_containing(path: &Path, needle: &str, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(body) = fs::read_to_string(path) {
            if body.contains(needle) {
                return Some(body);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn process_exists(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn process_parent_pid(pid: i32) -> i32 {
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    assert!(output.status.success(), "ps status={}", output.status);
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn read_single_supervisor_journal(runtime_root: &Path) -> String {
    let entries = fs::read_dir(runtime_root.join("logs"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("supervisor-") && name.ends_with(".log"))
        })
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "journal files={entries:?}");
    fs::read_to_string(&entries[0]).unwrap()
}

fn record_field<'a>(line: &'a str, key: &str) -> &'a str {
    let prefix = format!("{key}=");
    line.split_ascii_whitespace()
        .find_map(|field| field.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing {key} in record: {line}"))
}

fn record_millis(line: &str, key: &str) -> u128 {
    record_field(line, key)
        .parse()
        .unwrap_or_else(|err| panic!("invalid {key} in record: {err}: {line}"))
}

fn wait_for_journal_event(
    runtime_root: &Path,
    event: &str,
    dept: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(entries) = fs::read_dir(runtime_root.join("logs")) {
            for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
                let is_journal = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("supervisor-") && name.ends_with(".log"));
                if !is_journal {
                    continue;
                }
                if let Ok(body) = fs::read_to_string(path) {
                    let matched = body.lines().any(|line| {
                        line.starts_with(&format!("event={event} "))
                            && line.contains(&format!(" dept={dept} "))
                    });
                    if matched {
                        return Some(body);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn supervise_dispatches_file_watch_event_to_department() {
    let _lock = supervise_smoke_lock();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("departments/recorder")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::write(
        root.join("fkst.env"),
        "FKST_QUEUE_CAPACITY=100\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=30m\nFKST_CODEX_PERMIT_SLOTS=20\n",
    )
    .unwrap();
    fs::write(
        root.join("departments/recorder/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"files"}, ephemeral = {"files"}, stall_window = "5s" }
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
    write_single_package_workspace(root);

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
        .env("FKST_DURABLE_ROOT", root.join(".fkst/durable"))
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
fn supervise_starts_when_journal_log_dir_cannot_be_created() {
    let _lock = supervise_smoke_lock();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let runtime_root = root.join("runtime-file");
    let fact = root.join("started.txt");
    fs::write(&runtime_root, "not a directory").unwrap();
    fs::create_dir_all(root.join("departments/idle")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    write_fkst_env(root);
    fs::write(root.join("input.txt"), "ready").unwrap();
    fs::write(
        root.join("raisers/input.lua"),
        format!(
            r#"return {{ type = "file_watch", glob = {}, produces = "idle" }}"#,
            lua_string(&root.join("input.txt"))
        ),
    )
    .unwrap();
    fs::write(
        root.join("departments/idle/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{ consumes = {{"idle"}}, ephemeral = {{"idle"}}, stall_window = "5s" }}
function pipeline(event)
  local f = assert(io.open({}, "w"))
  f:write("started")
  f:close()
end
return M
"#,
            lua_string(&fact)
        ),
    )
    .unwrap();
    write_single_package_workspace(root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .current_dir(root)
        .arg("supervise")
        .arg("--project-root")
        .arg(root)
        .arg("--package-root")
        .arg(root)
        .arg("--framework-bin")
        .arg(env!("CARGO_BIN_EXE_fkst-framework"))
        .env("FKST_RUNTIME_ROOT", &runtime_root)
        .env("FKST_DURABLE_ROOT", root.join(".fkst/durable"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_file_containing(&fact, "started", Duration::from_secs(10)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("timed out waiting for {}", fact.display());
    });
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "status={}", output.status);
    let trace_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        trace_output.contains("MSG=supervisor journal disabled"),
        "trace_output={trace_output}"
    );
}

#[test]
fn supervise_survives_launcher_parent_exit() {
    let _lock = supervise_smoke_lock();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let input_dir = root.join("input");
    let fact = root.join("seen.txt");
    let supervise_pid = root.join("supervise.pid");
    let launcher = root.join("launch-supervise.sh");
    fs::create_dir_all(root.join("departments/recorder")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::create_dir_all(&input_dir).unwrap();
    write_fkst_env(root);
    fs::write(
        root.join("departments/recorder/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "files" }},
  ephemeral = {{ "files" }},
  stall_window = "5s",
}}
function pipeline(event)
  local f = assert(io.open({}, "w"))
  f:write(event.payload.path or "")
  f:close()
end
return M
"#,
            lua_string(&fact)
        ),
    )
    .unwrap();
    fs::write(
        root.join("raisers/files.lua"),
        format!(
            r#"return {{ type = "file_watch", glob = {}, produces = "files" }}"#,
            lua_string(&input_dir.join("*.txt"))
        ),
    )
    .unwrap();
    write_executable(
        &launcher,
        &format!(
            r#"#!/bin/sh
"{}" supervise \
  --project-root "{}" \
  --package-root "{}" \
  --framework-bin "{}" \
  > "{}" 2> "{}" &
printf '%s\n' "$!" > "{}"
exit 0
"#,
            env!("CARGO_BIN_EXE_fkst-framework"),
            root.display(),
            root.display(),
            env!("CARGO_BIN_EXE_fkst-framework"),
            root.join("supervise.stdout").display(),
            root.join("supervise.stderr").display(),
            supervise_pid.display()
        ),
    );
    write_single_package_workspace(root);

    let status = Command::new(&launcher)
        .current_dir(root)
        .env("FKST_RUNTIME_ROOT", root.join(".fkst/runtime"))
        .env("FKST_DURABLE_ROOT", root.join(".fkst/durable"))
        .status()
        .unwrap();
    assert!(status.success(), "launcher status={status}");
    let pid: i32 = wait_for_file_containing(&supervise_pid, "\n", Duration::from_secs(5))
        .unwrap_or_else(|| panic!("timed out waiting for {}", supervise_pid.display()))
        .trim()
        .parse()
        .unwrap();

    std::thread::sleep(Duration::from_secs(2));
    assert!(
        process_exists(pid),
        "supervise exited after launcher parent exit"
    );
    fs::write(input_dir.join("after-parent-exit.txt"), "ready").unwrap();
    let body = wait_for_file_containing(&fact, "after-parent-exit.txt", Duration::from_secs(10))
        .unwrap_or_else(|| {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
            panic!("timed out waiting for {}", fact.display());
        });
    assert!(body.contains("after-parent-exit.txt"), "body={body}");
    assert!(
        process_exists(pid),
        "supervise exited before explicit signal"
    );

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    assert!(
        wait_for_process_exit(pid, Duration::from_secs(5)),
        "supervise process survived cleanup SIGTERM"
    );
}

#[test]
fn supervise_child_lifecycle_reconciles_external_command_attribution() {
    let _lock = supervise_smoke_lock();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let package = root.join("package-root");
    let host = root.join("host-root");
    let runtime_root = host.join(".fkst/runtime");
    let fact = host.join("package-root-fact.txt");
    write_graph_defaults(&package);
    write_fkst_env(&host);
    fs::create_dir_all(host.join("fkst")).unwrap();
    fs::create_dir_all(package.join("raisers")).unwrap();
    fs::create_dir_all(host.join("departments/host_worker")).unwrap();
    fs::write(package.join("input.txt"), "ready").unwrap();
    fs::write(
        host.join("fkst/standard_asset.lua"),
        r#"
return {
  marker = function() return "host-standard-marker" end,
  stall_window = function() return "5s" end,
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
M.spec = {{ consumes = {{"{}.standard_input"}}, ephemeral = {{"{}.standard_input"}}, stall_window = standard.stall_window() }}
function pipeline(event)
  print("milestone-output")
  assert(exec_sync({{ cmd = "sleep 1.5" }}).exit_code == 0)
  assert(exec_sync({{ cmd = "sleep 1.0" }}).exit_code == 0)
  assert(exec_sync({{ cmd = "printf attribution-probe" }}).exit_code == 0)
  local f = assert(io.open({}, "w"))
  f:write("marker=" .. standard.marker() .. "\n")
  f:write("event_path=" .. tostring(event.payload.path) .. "\n")
  f:close()
end
return M
"#,
            namespace(&package),
            namespace(&package),
            lua_string(&fact)
        ),
    )
    .unwrap();
    write_workspace_for_roots(&host, &[&package]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .current_dir(&host)
        .arg("supervise")
        .arg("--project-root")
        .arg(&host)
        .arg("--package-root")
        .arg(&package)
        .arg("--framework-bin")
        .arg(env!("CARGO_BIN_EXE_fkst-framework"))
        .env("FKST_RUNTIME_ROOT", &runtime_root)
        .env("FKST_DURABLE_ROOT", host.join(".fkst/durable"))
        .spawn()
        .unwrap();

    let body = wait_for_file_containing(
        &fact,
        "marker=host-standard-marker",
        Duration::from_secs(10),
    )
    .unwrap_or_else(|| {
        let _ = child.kill();
        panic!("timed out waiting for {}", fact.display());
    });
    wait_for_journal_event(
        &runtime_root,
        "dept_child_exit",
        "host.host_worker",
        Duration::from_secs(10),
    )
    .unwrap_or_else(|| {
        let _ = child.kill();
        panic!("timed out waiting for successful Department child exit");
    });
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "supervise should exit successfully after SIGTERM"
    );
    assert!(
        body.contains("marker=host-standard-marker\n"),
        "body={body}"
    );
    let input_path = package.join("input.txt").canonicalize().unwrap();
    assert!(
        body.contains(&format!("event_path={}", input_path.display())),
        "body={body}"
    );
    let journal = read_single_supervisor_journal(&runtime_root);
    assert!(journal.contains("event=startup "), "journal={journal}");
    assert!(
        journal.contains("event=raiser_fired ")
            && journal.contains(" name=package-root.standard_input "),
        "journal={journal}"
    );
    assert!(
        journal.contains("event=dept_child_spawn ") && journal.contains(" dept=host.host_worker "),
        "journal={journal}"
    );
    let exit_record = journal
        .lines()
        .find(|line| {
            line.starts_with("event=dept_child_exit ")
                && line.contains(" dept=host.host_worker ")
                && line.contains(" exit_code=0 ")
        })
        .unwrap_or_else(|| panic!("missing successful child exit record: {journal}"));
    let spawn_return_ms = record_millis(exit_record, "spawn_return_ms");
    let first_pipe_read_ms = record_millis(exit_record, "first_pipe_read_ms");
    let wait_complete_ms = record_millis(exit_record, "wait_complete_ms");
    let capture_complete_ms = record_millis(exit_record, "capture_complete_ms");
    let elapsed_ms = record_millis(exit_record, "elapsed_ms");
    assert!(spawn_return_ms <= first_pipe_read_ms, "{exit_record}");
    assert!(spawn_return_ms <= wait_complete_ms, "{exit_record}");
    assert!(first_pipe_read_ms <= capture_complete_ms, "{exit_record}");
    assert!(wait_complete_ms <= capture_complete_ms, "{exit_record}");
    assert_eq!(capture_complete_ms, elapsed_ms, "{exit_record}");

    let child_log = fs::read_to_string(record_field(exit_record, "log_path")).unwrap();
    let mut duration_by_command_class = BTreeMap::new();
    for audit in child_log
        .lines()
        .filter(|line| line.contains("EVENT=external_command"))
    {
        let class = if audit.contains(r"CMD=/bin/sh\s-c\s'sleep\s") {
            "sleep"
        } else if audit.contains(r"CMD=/bin/sh\s-c\s'printf\s") {
            "printf"
        } else {
            panic!("unexpected command class in audit record: {audit}");
        };
        *duration_by_command_class.entry(class).or_insert(0_u128) +=
            record_millis(audit, "ELAPSED_MS");
    }
    assert_eq!(
        duration_by_command_class
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec!["printf", "sleep"],
        "child_log={child_log}"
    );
    let command_attempt_ms = duration_by_command_class.values().sum::<u128>();
    let overlap_aware_residual_ms = elapsed_ms as i128 - command_attempt_ms as i128;
    const ATTRIBUTION_TOLERANCE_MS: u128 = 1_000;
    assert!(
        overlap_aware_residual_ms.unsigned_abs() <= ATTRIBUTION_TOLERANCE_MS,
        "duration_by_command_class={duration_by_command_class:?} department_elapsed_ms={elapsed_ms} overlap_aware_residual_ms={overlap_aware_residual_ms} tolerance_ms={ATTRIBUTION_TOLERANCE_MS}; a positive residual is non-command wall time and a negative residual is overlapping command-attempt time"
    );
    assert!(
        journal.contains("event=shutdown_initiated ") && journal.contains(" reason=signal:SIGTERM"),
        "journal={journal}"
    );
}

#[test]
fn supervise_sigterm_terminates_department_process_tree() {
    let _lock = supervise_smoke_lock();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let ready = root.join("ready.txt");
    let dept_pid = root.join("dept.pid");
    let descendant_pid = root.join("descendant.pid");
    fs::create_dir_all(root.join("departments/sleeper")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    write_fkst_env(root);
    fs::write(root.join("input.txt"), "ready").unwrap();
    fs::write(
        root.join("raisers/input.lua"),
        format!(
            r#"return {{ type = "file_watch", glob = {}, produces = "input" }}"#,
            lua_string(&root.join("input.txt"))
        ),
    )
    .unwrap();
    fs::write(
        root.join("departments/sleeper/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "input" }},
  ephemeral = {{ "input" }},
  stall_window = "30s",
}}
function pipeline(event)
  local shell = [[
printf '%s\n' $$ > {}
(sleep 60) &
printf '%s\n' $! > {}
printf ready > {}
sleep 60
]]
  exec_sync({{ cmd = shell, timeout = 120 }})
end
return M
"#,
            lua_string(&dept_pid),
            lua_string(&descendant_pid),
            lua_string(&ready)
        ),
    )
    .unwrap();
    write_single_package_workspace(root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .current_dir(root)
        .arg("supervise")
        .arg("--project-root")
        .arg(root)
        .arg("--package-root")
        .arg(root)
        .arg("--framework-bin")
        .arg(env!("CARGO_BIN_EXE_fkst-framework"))
        .env("FKST_RUNTIME_ROOT", root.join(".fkst/runtime"))
        .env("FKST_DURABLE_ROOT", root.join(".fkst/durable"))
        .spawn()
        .unwrap();

    wait_for_file_containing(&ready, "ready", Duration::from_secs(10)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("timed out waiting for {}", ready.display());
    });
    let dept_pid: i32 = fs::read_to_string(&dept_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let descendant_pid: i32 = fs::read_to_string(&descendant_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "status={status}");
    assert!(
        wait_for_process_exit(dept_pid, Duration::from_secs(5)),
        "department process survived supervise SIGTERM"
    );
    assert!(
        wait_for_process_exit(descendant_pid, Duration::from_secs(5)),
        "department descendant survived supervise SIGTERM"
    );
}

#[test]
fn supervise_restart_terminates_codex_tree_and_redelivers_expired_lease() {
    let _lock = supervise_smoke_lock();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let bin_dir = root.join("bin");
    let capture_dir = root.join("capture");
    let worktree = root.join("worktree");
    let runtime_one = root.join("runtime-one");
    let runtime_two = root.join("runtime-two");
    let durable_root = root.join("durable");
    let input = root.join("input.txt");
    let completed = root.join("completed.txt");
    let second_release = root.join("second-release");
    fs::create_dir_all(root.join("departments/worker")).unwrap();
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&capture_dir).unwrap();
    fs::create_dir_all(&worktree).unwrap();
    write_fkst_env(root);
    fs::write(&input, "ready").unwrap();
    fs::write(
        root.join("raisers/input.lua"),
        format!(
            r#"return {{ type = "file_watch", glob = {}, produces = "jobs" }}"#,
            lua_string(&input)
        ),
    )
    .unwrap();
    fs::write(
        root.join("departments/worker/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{ consumes = {{ "jobs" }}, stall_window = "1s" }}
function pipeline(event)
  local result = spawn_codex_sync({{
    prompt = "restart-owned-work",
    worktree = {},
    dedup_key = "restart-owned-work",
    timeout = 60,
  }})
  assert(result.exit_code == 0, result.error or result.stderr)
  local f = assert(io.open({}, "w"))
  f:write(result.stdout)
  f:close()
end
return M
"#,
            lua_string(&worktree),
            lua_string(&completed)
        ),
    )
    .unwrap();
    write_executable(
        &bin_dir.join("codex"),
        r#"#!/bin/sh
set -eu
cat >/dev/null
count_file="$CAPTURE_DIR/spawn-count"
count=0
if [ -f "$count_file" ]; then
  count=$(cat "$count_file")
fi
count=$((count + 1))
printf '%s' "$count" > "$count_file"
printf '%s\n' "$$" > "$CAPTURE_DIR/codex-$count.pid"
printf '%s\n' "$PPID" > "$CAPTURE_DIR/worker-$count.pid"
if [ "$count" -eq 1 ]; then
  printf 'started' > "$CAPTURE_DIR/first-started"
  while :; do sleep 1; done
fi
printf 'started' > "$CAPTURE_DIR/second-started"
while [ ! -f "$SECOND_RELEASE" ]; do sleep 0.05; done
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"restart-complete"}}'
"#,
    );
    write_single_package_workspace(root);

    let mut search_path = vec![bin_dir.clone()];
    search_path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let search_path = std::env::join_paths(search_path).unwrap();
    let start_runtime = |runtime_root: &Path| {
        Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
            .current_dir(root)
            .arg("supervise")
            .arg("--project-root")
            .arg(root)
            .arg("--package-root")
            .arg(root)
            .arg("--framework-bin")
            .arg(env!("CARGO_BIN_EXE_fkst-framework"))
            .env("PATH", &search_path)
            .env("CAPTURE_DIR", &capture_dir)
            .env("SECOND_RELEASE", &second_release)
            .env("FKST_RUNTIME_ROOT", runtime_root)
            .env("FKST_RUNTIME_LOG_DIR", runtime_root.join("codex-logs"))
            .env("FKST_DURABLE_ROOT", &durable_root)
            .spawn()
            .unwrap()
    };

    let mut first = start_runtime(&runtime_one);
    wait_for_file_containing(
        &capture_dir.join("first-started"),
        "started",
        Duration::from_secs(10),
    )
    .unwrap_or_else(|| {
        let _ = first.kill();
        let _ = first.wait();
        panic!("timed out waiting for first Codex invocation");
    });
    let worker_pid: i32 = fs::read_to_string(capture_dir.join("worker-1.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let codex_pid: i32 = fs::read_to_string(capture_dir.join("codex-1.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let department_pid = process_parent_pid(worker_pid);

    first.kill().unwrap();
    let first_status = first.wait().unwrap();
    assert!(!first_status.success(), "status={first_status}");
    let survivors = [
        ("Department", department_pid),
        ("adoption worker", worker_pid),
        ("Codex", codex_pid),
    ]
    .into_iter()
    .filter(|(_, pid)| !wait_for_process_exit(*pid, Duration::from_secs(8)))
    .collect::<Vec<_>>();
    for (_, pid) in &survivors {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(*pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    assert!(survivors.is_empty(), "surviving processes={survivors:?}");

    let mut second = start_runtime(&runtime_two);
    wait_for_file_containing(
        &capture_dir.join("second-started"),
        "started",
        Duration::from_secs(15),
    )
    .unwrap_or_else(|| {
        let _ = second.kill();
        let _ = second.wait();
        panic!("timed out waiting for redelivered Codex invocation");
    });
    let observe = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .arg("observe")
        .arg("--durable-root")
        .arg(&durable_root)
        .arg("--json")
        .env_remove(process_tree::SUPERVISOR_PID_ENV)
        .output()
        .unwrap();
    assert!(
        observe.status.success(),
        "observe stderr={}",
        String::from_utf8_lossy(&observe.stderr)
    );
    let snapshot: serde_json::Value = serde_json::from_slice(&observe.stdout).unwrap();
    assert_eq!(snapshot["deliveries"][0]["lease_generation"], 2);

    fs::write(&second_release, "release").unwrap();
    let result = wait_for_file_containing(&completed, "restart-complete", Duration::from_secs(10))
        .unwrap_or_else(|| {
            let _ = second.kill();
            let _ = second.wait();
            panic!("timed out waiting for redelivered result");
        });
    wait_for_journal_event(&runtime_two, "acked", "worker", Duration::from_secs(5)).unwrap_or_else(
        || {
            let _ = second.kill();
            let _ = second.wait();
            panic!("timed out waiting for redelivered delivery ACK");
        },
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(second.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let second_status = second.wait().unwrap();
    assert!(second_status.success(), "status={second_status}");
    assert_eq!(result, "restart-complete");
    assert_eq!(
        fs::read_to_string(capture_dir.join("spawn-count")).unwrap(),
        "2"
    );
}

#[test]
fn supervise_delivers_cross_package_raise_from_composed_child() {
    let _lock = supervise_smoke_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let host = root.join("host");
    let producer_pkg = root.join("github-devloop");
    let consumer_pkg = root.join("consensus");
    let fact = host.join("proposal-fact.txt");
    fs::create_dir_all(producer_pkg.join("departments/producer")).unwrap();
    fs::create_dir_all(producer_pkg.join("raisers")).unwrap();
    fs::create_dir_all(consumer_pkg.join("departments/proposal_sink")).unwrap();
    write_fkst_env(&host);
    fs::write(producer_pkg.join("input.txt"), "ready").unwrap();
    fs::write(
        producer_pkg.join("raisers/input.lua"),
        format!(
            r#"return {{ type = "file_watch", glob = {}, produces = "tick" }}"#,
            lua_string(&producer_pkg.join("input.txt"))
        ),
    )
    .unwrap();
    fs::write(
        producer_pkg.join("departments/producer/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "consensus.proposal" },
  ephemeral = { "tick" },
  stall_window = "5s",
}
function pipeline(event)
  raise("consensus.proposal", { seen = "cross-package" })
end
return M
"#,
    )
    .unwrap();
    fs::write(
        consumer_pkg.join("departments/proposal_sink/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "proposal" }},
  published_seam = {{ "proposal" }},
  ephemeral = {{ "proposal" }},
  stall_window = "5s",
}}
function pipeline(event)
  local f = assert(io.open({}, "w"))
  f:write(event.queue .. "\n")
  f:write(event.payload.seen .. "\n")
  f:close()
end
return M
"#,
            lua_string(&fact)
        ),
    )
    .unwrap();
    write_workspace_for_roots(&host, &[&producer_pkg, &consumer_pkg]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .current_dir(&host)
        .arg("supervise")
        .arg("--project-root")
        .arg(&host)
        .arg("--package-root")
        .arg(&producer_pkg)
        .arg("--package-root")
        .arg(&consumer_pkg)
        .arg("--framework-bin")
        .arg(env!("CARGO_BIN_EXE_fkst-framework"))
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .env("FKST_DURABLE_ROOT", host.join(".fkst/durable"))
        .spawn()
        .unwrap();

    let body = wait_for_file_containing(&fact, "cross-package\n", Duration::from_secs(10))
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("timed out waiting for {}", fact.display());
        });
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "supervise should be killed after fact write"
    );
    assert!(body.contains("consensus.proposal\n"), "body={body}");
    assert!(body.contains("cross-package\n"), "body={body}");
}

#[test]
fn supervise_replay_reemits_before_ack_despite_preexisting_scratch() {
    let _lock = supervise_smoke_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let package = root.join("package");
    let facts = root.join("facts");
    let runtime_root = root.join("runtime");
    let durable_root = root.join("durable");
    let first_attempt = facts.join("first-attempt");
    let result_fact = facts.join("result");
    fs::create_dir_all(package.join("departments/worker")).unwrap();
    fs::create_dir_all(package.join("departments/sink")).unwrap();
    fs::create_dir_all(package.join("raisers")).unwrap();
    fs::create_dir_all(&facts).unwrap();
    write_fkst_env(&package);
    fs::write(package.join("input.txt"), "ready").unwrap();
    fs::write(
        package.join("raisers/input.lua"),
        format!(
            r#"return {{ type = "file_watch", glob = {}, produces = "jobs" }}"#,
            lua_string(&package.join("input.txt"))
        ),
    )
    .unwrap();
    fs::write(
        package.join("departments/worker/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "jobs" }},
  produces = {{ "missing", "result" }},
  stall_window = "5s",
  retry = {{ max_attempts = 3, base = "1s", cap = "1s" }},
}}
function pipeline(event)
  local attempt = io.open({}, "r")
  if attempt == nil then
    local created = assert(io.open({}, "w"))
    created:write("first")
    created:close()
    local ran = once("completion/result", function()
      cache_set("completion/result", "stale")
      raise("missing", {{ value = "publication-failure" }})
    end)
    assert(ran)
    return
  end
  attempt:close()

  if cache_get("completion/result") ~= nil then
    return
  end
  local replayed = once("completion/result", function()
    local payload = {{ dedup_key = "replay-result", value = "ready" }}
    raise("result", payload)
    raise("result", payload)
  end)
  assert(replayed)
end
return M
"#,
            lua_string(&first_attempt),
            lua_string(&first_attempt)
        ),
    )
    .unwrap();
    fs::write(
        package.join("departments/sink/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{ consumes = {{ "result" }}, stall_window = "5s" }}
function pipeline(event)
  local f = assert(io.open({}, "a"))
  f:write(event.payload.value .. "\n")
  f:close()
end
return M
"#,
            lua_string(&result_fact)
        ),
    )
    .unwrap();
    write_single_package_workspace(&package);
    let manifest_path = package.join("fkst.toml");
    let manifest = fs::read_to_string(&manifest_path).unwrap().replace(
        "persistence_class = \"stateless_adapter\"",
        "persistence_class = \"saga\"",
    );
    fs::write(manifest_path, manifest).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_fkst-framework"))
        .current_dir(&package)
        .arg("supervise")
        .arg("--project-root")
        .arg(&package)
        .arg("--package-root")
        .arg(&package)
        .arg("--framework-bin")
        .arg(env!("CARGO_BIN_EXE_fkst-framework"))
        .env("FKST_RUNTIME_ROOT", &runtime_root)
        .env("FKST_DURABLE_ROOT", &durable_root)
        .spawn()
        .unwrap();

    let result = wait_for_file_containing(&result_fact, "ready\n", Duration::from_secs(15))
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!(
                "timed out waiting for replay result {}",
                result_fact.display()
            );
        });
    wait_for_journal_event(&runtime_root, "acked", "sink", Duration::from_secs(5)).unwrap_or_else(
        || {
            let _ = child.kill();
            panic!("timed out waiting for sink ACK");
        },
    );
    std::thread::sleep(Duration::from_secs(1));

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "status={status}");
    let journal = read_single_supervisor_journal(&runtime_root);

    assert_eq!(result.lines().collect::<Vec<_>>(), ["ready"]);
    assert!(runtime_root.join("cache/completion/result/=value").exists());
    assert!(runtime_root.join("marks/completion/result/=mark").exists());
    assert!(journal.lines().any(|line| {
        line.starts_with("event=retried ")
            && line.contains(" dept=worker ")
            && line.contains("reason=raised\\spublish\\serror:")
    }));
    assert!(journal.lines().any(|line| {
        line.starts_with("event=acked ")
            && line.contains(" dept=worker ")
            && line.contains(" reason=completed")
    }));
    assert_eq!(
        journal
            .lines()
            .filter(|line| {
                line.starts_with("event=dept_child_spawn ") && line.contains(" dept=sink ")
            })
            .count(),
        1,
        "duplicate replay raises must collapse to one durable sink delivery"
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
        &[sandbox.root().to_path_buf()],
        "pkg",
        "{}",
        7,
        "permit",
        &logs,
        process_tree::ProcessGroupRegistry::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "7");
}

#[tokio::test]
async fn spawn_framework_removes_engine_binary_path_env() {
    let mut sandbox = ProcessSandbox::new();
    sandbox
        .set_env("BIN", "/tmp/fkst-framework-bin")
        .set_env("FKST_FRAMEWORK_BIN", "/tmp/fkst-framework")
        .set_env("FKST_CODEX_WORKER_BIN", "/tmp/fkst-worker");
    let (_lock, _guard) = sandbox.enter();
    let binary = fake_framework(
        sandbox.root(),
        r#"printf 'BIN=%s\nFKST_FRAMEWORK_BIN=%s\nFKST_CODEX_WORKER_BIN=%s\n' "$BIN" "$FKST_FRAMEWORK_BIN" "$FKST_CODEX_WORKER_BIN"; exit 0"#,
    );
    let lua_dummy = sandbox.temp_path("dept.lua");
    fs::write(&lua_dummy, "return {}\n").unwrap();
    let logs = sandbox.temp_path("logs");

    let result = spawn_framework(
        &binary,
        &lua_dummy,
        sandbox.root(),
        &[sandbox.root().to_path_buf()],
        "pkg",
        "{}",
        7,
        "env-scrub",
        &logs,
        process_tree::ProcessGroupRegistry::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.exit_code, 0);
    assert_eq!(
        result.stdout,
        "BIN=\nFKST_FRAMEWORK_BIN=\nFKST_CODEX_WORKER_BIN=\n"
    );
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
        &[sandbox.root().to_path_buf()],
        "pkg",
        "{}",
        20,
        "success",
        &logs,
        process_tree::ProcessGroupRegistry::default(),
    )
    .await
    .unwrap();
    let success_log = read_log(&success_result);
    assert!(success_log.contains("CMD="));
    assert!(success_log.contains("PID="));
    assert!(success_log.contains("LUA="));
    assert!(success_log.contains("PACKAGE_ROOTS="));
    assert!(success_log.contains("OWNER_NAMESPACE=pkg"));
    assert!(success_log.contains("ENGINE_VER="));
    assert!(success_log.contains("PKG_VER="));
    assert!(success_log.contains("PKG_VERS="));
    assert!(success_log.contains("DEPT=success"));
    assert!(success_log.contains("EXIT=0\n"));
    assert!(success_log.contains("ELAPSED_MS="));
    assert!(!success_log.contains("STALLED="));
    assert!(!success_log.contains("LAST_OUTPUT_AGE_MS="));

    let nonzero = fake_framework(sandbox.root(), r#"printf 'bad\n' >&2; exit 9"#);
    let nonzero_result = spawn_framework(
        &nonzero,
        &lua_dummy,
        sandbox.root(),
        &[sandbox.root().to_path_buf()],
        "pkg",
        "{}",
        20,
        "nonzero-meta",
        &logs,
        process_tree::ProcessGroupRegistry::default(),
    )
    .await
    .unwrap();
    let nonzero_log = read_log(&nonzero_result);
    assert!(nonzero_log.contains("CMD="));
    assert!(nonzero_log.contains("PID="));
    assert!(nonzero_log.contains("LUA="));
    assert!(nonzero_log.contains("DEPT=nonzero-meta"));
    assert!(nonzero_log.contains("EXIT=9\n"));
    assert!(nonzero_log.contains("ELAPSED_MS="));
    assert!(!nonzero_log.contains("STALLED="));
    assert!(!nonzero_log.contains("LAST_OUTPUT_AGE_MS="));

    let silent = fake_framework(sandbox.root(), r#"sleep 1; exit 0"#);
    let silent_result = spawn_framework(
        &silent,
        &lua_dummy,
        sandbox.root(),
        &[sandbox.root().to_path_buf()],
        "pkg",
        "{}",
        20,
        "silent-meta",
        &logs,
        process_tree::ProcessGroupRegistry::default(),
    )
    .await
    .unwrap();
    let silent_log = read_log(&silent_result);
    assert_eq!(silent_result.exit_code, 0);
    assert!(silent_log.contains("CMD="));
    assert!(silent_log.contains("PID="));
    assert!(silent_log.contains("LUA="));
    assert!(silent_log.contains("DEPT=silent-meta"));
    assert!(silent_log.contains("EXIT=0\n"));
    assert!(silent_log.contains("ELAPSED_MS="));
    assert!(!silent_log.contains("STALL_KILL_PID="));
    assert!(!silent_log.contains("STALLED="));
    assert!(!silent_log.contains("LAST_OUTPUT_AGE_MS="));
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
        &[sandbox.root().to_path_buf()],
        "pkg",
        "{}",
        20,
        "blocked",
        &blocked_log_path,
        process_tree::ProcessGroupRegistry::default(),
    )
    .await
    .unwrap();

    assert_eq!(result.exit_code, 6);
    assert_eq!(result.stdout, "stdout-stays\n");
    assert_eq!(result.stderr, "stderr-stays\n");
    assert!(result.log_path.is_none());
}

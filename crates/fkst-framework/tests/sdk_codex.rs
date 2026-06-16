// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/boundary_resource.rs"]
mod boundary_resource;
#[path = "../src/config_registry.rs"]
mod config_registry;
#[path = "../src/external_command.rs"]
mod external_command;
#[path = "../src/process_tree.rs"]
mod process_tree;
#[path = "../src/provenance.rs"]
mod provenance;
#[path = "../src/rate_pool.rs"]
mod rate_pool;
#[path = "../src/rate_shim.rs"]
mod rate_shim;
#[path = "../src/runtime_context.rs"]
mod runtime_context;
#[path = "../src/sdk_codex.rs"]
mod sdk_codex;
mod support;

use mlua::{AnyUserData, Function, Lua, Table};
use nix::fcntl::{flock, FlockArg};
use sdk_codex::{
    acquire_permit, ensure_pool, CodexResult, CodexTaskHandle, CODEX_PERMIT_SLOTS_ENV,
};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use support::process_sandbox::ProcessSandbox;

const DEFAULT_CODEX_PERMIT_SLOTS: usize = 20;
const CODEX_WORKER_BIN_ENV: &str = "FKST_CODEX_WORKER_BIN";

fn register(lua: &Lua) -> mlua::Result<()> {
    register_with_dept(lua, None)
}

fn register_with_dept(lua: &Lua, dept: Option<String>) -> mlua::Result<()> {
    let host_root = std::env::current_dir().map_err(mlua::Error::external)?;
    let config = config_registry::ConfigContext::from_host_root(&host_root)
        .map_err(mlua::Error::external)?;
    sdk_codex::register(lua, &host_root, config, dept)
}

#[cfg(unix)]
fn install_codex_script(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir).unwrap();
    let codex = dir.join("codex");
    std::fs::write(&codex, body).unwrap();
    let mut permissions = std::fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&codex, permissions).unwrap();
    codex
}

#[cfg(unix)]
fn make_fifo(path: &Path) {
    nix::unistd::mkfifo(
        path,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
}

#[cfg(unix)]
fn write_fifo(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
}

#[cfg(unix)]
fn read_fifo(path: &Path) -> String {
    let path = path.to_path_buf();
    let display = path.display().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = std::fs::read_to_string(&path);
        let _ = tx.send(result);
    });
    let result = rx
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|err| panic!("timed out reading FIFO {display}: {err}"));
    result.unwrap_or_else(|err| panic!("failed reading FIFO {display}: {err}"))
}

#[cfg(unix)]
fn recv_result(rx: &std::sync::mpsc::Receiver<String>, label: &str) -> String {
    rx.recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|err| panic!("timed out waiting for {label}: {err}"))
}

#[cfg(unix)]
fn continuous_output_codex_script(stream_redirect: &'static str) -> String {
    format!(
        r#"#!/bin/sh
printf 'tick:start\n' {stream_redirect}
cat >/dev/null
i=0
while :; do
  i=$((i + 1))
  printf 'tick:%s\n' "$i" {stream_redirect}
  sleep 0.05
done
"#
    )
}

struct ActivityResult {
    exit_code: i64,
    stdout: String,
    stderr: String,
    error_kind: String,
    error: String,
}

enum TestStream {
    Stdout,
    Stderr,
}

#[cfg(unix)]
fn run_timeout_activity_test(output_stream: TestStream) -> ActivityResult {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let stream_redirect = match output_stream {
        TestStream::Stdout => "",
        TestStream::Stderr => ">&2",
    };
    install_codex_script(&bin_dir, &continuous_output_codex_script(stream_redirect));

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let spawn_thread = std::thread::spawn(move || {
        let lua = Lua::new();
        register(&lua).unwrap();
        let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
        let opts = lua_opts(&lua, "active");
        opts.set("timeout", 3).unwrap();
        let result: Table = spawn.call(opts).unwrap();
        result_tx
            .send(ActivityResult {
                exit_code: result.get::<i64>("exit_code").unwrap(),
                stdout: result.get::<String>("stdout").unwrap(),
                stderr: result.get::<String>("stderr").unwrap(),
                error_kind: result.get::<String>("error_kind").unwrap(),
                error: result.get::<String>("error").unwrap(),
            })
            .unwrap();
    });

    let result = result_rx
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|err| panic!("timed out waiting for codex activity result: {err}"));
    spawn_thread.join().unwrap();
    result
}

fn lua_opts(lua: &Lua, prompt: &str) -> Table {
    let opts = lua.create_table().unwrap();
    opts.set("prompt", prompt).unwrap();
    opts
}

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn write_log_with_mtime(path: &Path, body: &str, modified: SystemTime) {
    std::fs::write(path, body).unwrap();
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(modified)).unwrap();
}

fn table_len(table: &Table, key: &str) -> usize {
    table
        .get::<Table>(key)
        .unwrap()
        .sequence_values::<Table>()
        .count()
}

fn codex_runs_fn(lua: &Lua) -> Function {
    lua.globals()
        .get::<Table>("fkst")
        .unwrap()
        .get("codex_runs")
        .unwrap()
}

#[test]
fn fixed_surface_does_not_register_await_any_await_or_sleep() {
    let lua = Lua::new();
    register(&lua).unwrap();

    lua.load(
        r#"
        assert(type(spawn_codex) == "function")
        assert(type(await_all) == "function")
        assert(type(fkst) == "table")
        assert(type(fkst.codex_runs) == "function")
        assert(codex_status == nil)
        assert(await_any == nil)
        assert(await == nil)
        assert(sleep == nil)
        "#,
    )
    .exec()
    .unwrap();
}

#[cfg(unix)]
#[test]
fn codex_runs_reports_running_and_recent_with_bounded_output_tail_without_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let started_fifo = tmp.path().join("started.fifo");
    let release_fifo = tmp.path().join("release.fifo");
    make_fifo(&started_fifo);
    make_fifo(&release_fifo);
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'started' > "$STARTED_FIFO"
read _ < "$RELEASE_FIFO"
printf 'final output visible through bounded output_tail'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("STARTED_FIFO", started_fifo.to_string_lossy().into_owned());
    sandbox.set_env("RELEASE_FIFO", release_fifo.to_string_lossy().into_owned());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register_with_dept(&lua, Some("pkg.reviewer".to_string())).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex").unwrap();
    let opts = lua_opts(&lua, "status");
    opts.set("label", "review").unwrap();
    opts.set("role", "reviewer").unwrap();
    opts.set("proposal_id", "proposal-43").unwrap();
    let handle: AnyUserData = spawn.call(opts).unwrap();
    assert_eq!(read_fifo(&started_fifo), "started");

    let status_fn: mlua::Function = codex_runs_fn(&lua);
    let status: Table = status_fn.call(()).unwrap();
    let running: Table = status.get("running").unwrap();
    assert_eq!(running.raw_len(), 1);
    let active: Table = running.get(1).unwrap();
    assert_eq!(active.get::<String>("status").unwrap(), "running");
    assert_eq!(active.get::<String>("role").unwrap(), "reviewer");
    assert_eq!(active.get::<String>("label").unwrap(), "review");
    assert_eq!(active.get::<String>("proposal_id").unwrap(), "proposal-43");
    assert_eq!(
        active.get::<String>("proposal_id_or_key").unwrap(),
        "proposal-43"
    );
    assert_eq!(active.get::<String>("dept").unwrap(), "pkg.reviewer");
    assert!(active.get::<u64>("elapsed_ms").is_ok());
    assert!(active.get::<i64>("exit_code").is_err());
    assert_eq!(active.get::<String>("output_tail").unwrap(), "");
    assert!(active.get::<String>("output_excerpt").is_err());
    assert!(active.get::<String>("log_path").is_err());
    assert_eq!(table_len(&status, "recent"), 0);

    write_fifo(&release_fifo, "go\n");
    let await_all: mlua::Function = lua.globals().get("await_all").unwrap();
    let handles = lua.create_sequence_from(vec![handle]).unwrap();
    let results: Table = await_all.call(handles).unwrap();
    let result: Table = results.get(1).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);

    let status: Table = status_fn.call(()).unwrap();
    assert_eq!(table_len(&status, "running"), 0);
    let recent: Table = status.get("recent").unwrap();
    assert_eq!(recent.raw_len(), 1);
    let completed: Table = recent.get(1).unwrap();
    assert_eq!(completed.get::<String>("status").unwrap(), "done");
    assert_eq!(completed.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(
        completed.get::<String>("output_tail").unwrap(),
        "final output visible through bounded output_tail"
    );
    assert!(completed.get::<String>("ended_at").unwrap().ends_with('Z'));
    assert!(completed.get::<u64>("ended_at_ms").is_ok());
    assert!(
        completed.get::<u64>("elapsed_ms").unwrap() >= active.get::<u64>("elapsed_ms").unwrap()
    );
    assert!(completed.get::<String>("output_excerpt").is_err());
    assert!(completed.get::<String>("log_path").is_err());
}

#[cfg(unix)]
#[test]
fn codex_runs_recent_is_bounded_to_last_fifty_completions() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'ok'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    for index in 0..55 {
        let opts = lua_opts(&lua, "done");
        opts.set("label", format!("job-{index:02}")).unwrap();
        let result: Table = spawn.call(opts).unwrap();
        assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    }

    let status_fn: mlua::Function = codex_runs_fn(&lua);
    let status: Table = status_fn.call(()).unwrap();
    assert_eq!(table_len(&status, "running"), 0);
    let recent: Table = status.get("recent").unwrap();
    assert_eq!(recent.raw_len(), 50);
    let labels = recent
        .sequence_values::<Table>()
        .map(|item| item.unwrap().get::<String>("label").unwrap())
        .collect::<Vec<_>>();
    assert!(!labels.iter().any(|label| label == "job-00"));
    assert!(labels.iter().any(|label| label == "job-54"));
    assert!(tmp.path().join("runtime/codex").exists());
    assert!(!tmp.path().join(".fkst/runtime/codex-status").exists());
}

#[cfg(unix)]
#[test]
fn codex_runs_exposes_bounded_live_output_tail_while_run_is_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let started_fifo = tmp.path().join("started.fifo");
    let release_fifo = tmp.path().join("release.fifo");
    make_fifo(&started_fifo);
    make_fifo(&release_fifo);
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
i=0
while [ "$i" -lt 60 ]; do
  i=$((i + 1))
  printf 'line-%02d\n' "$i"
done
printf 'started' > "$STARTED_FIFO"
read _ < "$RELEASE_FIFO"
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("STARTED_FIFO", started_fifo.to_string_lossy().into_owned());
    sandbox.set_env("RELEASE_FIFO", release_fifo.to_string_lossy().into_owned());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex").unwrap();
    let opts = lua_opts(&lua, "live tail");
    opts.set("role", "implementer").unwrap();
    opts.set("dedup_key", "issue-74").unwrap();
    let handle: AnyUserData = spawn.call(opts).unwrap();
    assert_eq!(read_fifo(&started_fifo), "started");

    let status_fn: mlua::Function = codex_runs_fn(&lua);
    let mut observed_tail = String::new();
    let mut observed_role = String::new();
    let mut observed_key = String::new();
    let mut observed_log_path_is_hidden = false;
    for _ in 0..250 {
        let status: Table = status_fn.call(()).unwrap();
        let running: Table = status.get("running").unwrap();
        if running.raw_len() == 1 {
            let active: Table = running.get(1).unwrap();
            assert_eq!(active.get::<String>("status").unwrap(), "running");
            observed_role = active.get::<String>("role").unwrap();
            observed_key = active.get::<String>("proposal_id_or_key").unwrap();
            observed_tail = active.get::<String>("output_tail").unwrap();
            observed_log_path_is_hidden = active.get::<String>("log_path").is_err();
            if observed_tail.contains("line-60") {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    write_fifo(&release_fifo, "go\n");
    let await_all: mlua::Function = lua.globals().get("await_all").unwrap();
    let handles = lua.create_sequence_from(vec![handle]).unwrap();
    let results: Table = await_all.call(handles).unwrap();
    let result: Table = results.get(1).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);

    assert_eq!(observed_role, "implementer");
    assert_eq!(observed_key, "issue-74");
    assert!(!observed_tail.contains("line-20"), "{observed_tail}");
    assert!(observed_tail.contains("line-21"), "{observed_tail}");
    assert!(observed_tail.contains("line-60"), "{observed_tail}");
    assert!(observed_tail.len() <= 4096, "{observed_tail}");
    assert_eq!(observed_tail.lines().count(), 40);
    assert!(observed_log_path_is_hidden);
}

#[test]
fn ensure_pool_honors_configured_slot_count() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.set_env(CODEX_PERMIT_SLOTS_ENV, "3");
    let (_lock, _guard) = sandbox.enter();
    ensure_pool().unwrap();
    for i in 0..3 {
        assert!(tmp
            .path()
            .join(format!(".fkst/runtime/codex-permits/permit-{}", i))
            .exists());
    }
    assert!(!tmp
        .path()
        .join(".fkst/runtime/codex-permits/permit-3")
        .exists());
}

#[test]
fn ensure_pool_requires_runtime_root() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path());
    sandbox.unset_env(fkst_common::runtime_layout::RUNTIME_ROOT_ENV);
    let (_lock, _guard) = sandbox.enter();
    let err = ensure_pool().unwrap_err().to_string();
    assert!(err.contains("FKST_RUNTIME_ROOT must be set"), "{err}");
}

#[test]
fn ensure_pool_creates_permits_under_configured_runtime_root() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(runtime.path());
    let (_lock, _guard) = sandbox.enter();
    ensure_pool().unwrap();
    for i in 0..DEFAULT_CODEX_PERMIT_SLOTS {
        assert!(runtime
            .path()
            .join(format!("codex-permits/permit-{}", i))
            .exists());
    }
    assert!(!tmp.path().join(".fkst/runtime/codex-permits").exists());
}

#[test]
fn acquire_two_permits_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    let (_lock, _guard) = sandbox.enter();
    ensure_pool().unwrap();
    let p1 = acquire_permit().unwrap();
    let p2 = acquire_permit().unwrap();
    assert_ne!(p1.slot(), p2.slot());

    drop(p1);
    drop(p2);
}

#[test]
fn invalid_permit_slot_count_fails_closed() {
    for value in ["0", "not-a-number"] {
        let tmp = tempfile::tempdir().unwrap();
        let mut sandbox = ProcessSandbox::new();
        sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
        sandbox.set_env(CODEX_PERMIT_SLOTS_ENV, value);
        let (_lock, _guard) = sandbox.enter();
        let err = ensure_pool().unwrap_err().to_string();
        assert!(
            err.contains(CODEX_PERMIT_SLOTS_ENV),
            "value={value} err={err}"
        );
    }
}

#[test]
fn empty_permit_slot_count_uses_operational_default() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.set_env(CODEX_PERMIT_SLOTS_ENV, "");
    let (_lock, _guard) = sandbox.enter();

    ensure_pool().unwrap();

    for i in 0..DEFAULT_CODEX_PERMIT_SLOTS {
        assert!(tmp
            .path()
            .join(format!(".fkst/runtime/codex-permits/permit-{}", i))
            .exists());
    }
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_sends_prompt_through_stdin_after_options() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let capture_dir = tmp.path().join("capture");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&capture_dir).unwrap();
    fs::create_dir_all(tmp.path().join("wt")).unwrap();

    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
printf '%s
' "$@" > "$CAPTURE_DIR/argv"
cat > "$CAPTURE_DIR/stdin"
printf 'ok'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("CAPTURE_DIR", capture_dir.to_string_lossy().into_owned());
    sandbox.set_env(CODEX_WORKER_BIN_ENV, framework_bin());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let prompt = "long prompt ".repeat(800);
    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua.create_table().unwrap();
    opts.set("prompt", prompt.as_str()).unwrap();
    opts.set("context", "ctx.json").unwrap();
    opts.set("worktree", "wt").unwrap();
    opts.set("timeout", 42).unwrap();

    let result: mlua::Table = spawn.call(opts).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(result.get::<String>("stdout").unwrap(), "ok");
    let log_path = PathBuf::from(result.get::<String>("log_path").unwrap());
    assert_eq!(log_path.parent().unwrap(), tmp.path().join("runtime/codex"));
    assert!(log_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("wt-"));
    let log_body = fs::read_to_string(log_path).unwrap();
    assert!(log_body.contains("ok\n"));
    assert!(log_body.contains("EXIT=0\n"));
    assert!(log_body.contains("DONE_AT="));
    assert!(log_body.contains(
        "CMD=codex exec --dangerously-bypass-approvals-and-sandbox --context ctx.json -C wt -\n"
    ));
    assert!(log_body.contains("TIMEOUT_SECONDS=42\n"));

    let argv = fs::read_to_string(capture_dir.join("argv")).unwrap();
    let args: Vec<&str> = argv.lines().collect();
    assert_eq!(
        args,
        vec![
            "exec",
            "--dangerously-bypass-approvals-and-sandbox",
            "--context",
            "ctx.json",
            "-C",
            "wt",
            "-"
        ]
    );
    assert_eq!(
        fs::read_to_string(capture_dir.join("stdin")).unwrap(),
        prompt
    );
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_adopts_completed_worktree_result_without_respawn() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let worktree = tmp.path().join("wt");
    let capture_dir = tmp.path().join("capture");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::create_dir_all(&capture_dir).unwrap();
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
count_file="$CAPTURE_DIR/spawns"
count=0
if [ -f "$count_file" ]; then
  count=$(cat "$count_file")
fi
count=$((count + 1))
printf '%s' "$count" > "$count_file"
printf 'result-%s' "$count"
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("CAPTURE_DIR", capture_dir.to_string_lossy().into_owned());
    sandbox.set_env(CODEX_WORKER_BIN_ENV, framework_bin());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let first_opts = lua_opts(&lua, "same-work");
    first_opts
        .set("worktree", worktree.to_string_lossy().into_owned())
        .unwrap();
    first_opts.set("dedup_key", "same-dedup").unwrap();
    let first: Table = spawn.call(first_opts).unwrap();
    assert_eq!(first.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(first.get::<String>("stdout").unwrap(), "result-1");

    let second_opts = lua_opts(&lua, "same-work");
    second_opts
        .set("worktree", worktree.to_string_lossy().into_owned())
        .unwrap();
    second_opts.set("dedup_key", "same-dedup").unwrap();
    let second: Table = spawn.call(second_opts).unwrap();
    assert_eq!(second.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(second.get::<String>("stdout").unwrap(), "result-1");
    assert_eq!(
        std::fs::read_to_string(capture_dir.join("spawns")).unwrap(),
        "1"
    );
    assert!(tmp
        .path()
        .join(".fkst/runtime/logs/codex-adoption")
        .exists());
    assert!(!worktree.join(".fkst-codex").exists());
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_uses_runtime_adoption_dir_for_read_only_worktree() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let worktree = tmp.path().join("readonly-wt");
    std::fs::create_dir_all(&worktree).unwrap();
    let mut permissions = std::fs::metadata(&worktree).unwrap().permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(&worktree, permissions).unwrap();
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'readonly-ok'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env(CODEX_WORKER_BIN_ENV, framework_bin());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua_opts(&lua, "read only worktree");
    opts.set("worktree", worktree.to_string_lossy().into_owned())
        .unwrap();
    opts.set("dedup_key", "readonly-dedup").unwrap();

    let result: Table = spawn.call(opts).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(result.get::<String>("stdout").unwrap(), "readonly-ok");
    assert!(tmp
        .path()
        .join(".fkst/runtime/logs/codex-adoption")
        .exists());
    assert!(!worktree.join(".fkst-codex").exists());
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_uses_distinct_adoption_dirs_for_different_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let first_worktree = tmp.path().join("wt-one");
    let second_worktree = tmp.path().join("wt-two");
    let capture_dir = tmp.path().join("capture");
    std::fs::create_dir_all(&first_worktree).unwrap();
    std::fs::create_dir_all(&second_worktree).unwrap();
    std::fs::create_dir_all(&capture_dir).unwrap();
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
count_file="$CAPTURE_DIR/spawns"
count=0
if [ -f "$count_file" ]; then
  count=$(cat "$count_file")
fi
count=$((count + 1))
printf '%s' "$count" > "$count_file"
printf 'result-%s' "$count"
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("CAPTURE_DIR", capture_dir.to_string_lossy().into_owned());
    sandbox.set_env(CODEX_WORKER_BIN_ENV, framework_bin());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let first_opts = lua_opts(&lua, "same-work");
    first_opts
        .set("worktree", first_worktree.to_string_lossy().into_owned())
        .unwrap();
    first_opts.set("dedup_key", "same-dedup").unwrap();
    let first: Table = spawn.call(first_opts).unwrap();
    assert_eq!(first.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(first.get::<String>("stdout").unwrap(), "result-1");

    let second_opts = lua_opts(&lua, "same-work");
    second_opts
        .set("worktree", second_worktree.to_string_lossy().into_owned())
        .unwrap();
    second_opts.set("dedup_key", "same-dedup").unwrap();
    let second: Table = spawn.call(second_opts).unwrap();
    assert_eq!(second.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(second.get::<String>("stdout").unwrap(), "result-2");
    assert_eq!(
        std::fs::read_to_string(capture_dir.join("spawns")).unwrap(),
        "2"
    );

    let adoption_dir = tmp.path().join(".fkst/runtime/logs/codex-adoption");
    let dirs: Vec<_> = std::fs::read_dir(&adoption_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(dirs.len(), 2);
    assert_ne!(dirs[0].file_name().unwrap(), dirs[1].file_name().unwrap());
    assert!(!first_worktree.join(".fkst-codex").exists());
    assert!(!second_worktree.join(".fkst-codex").exists());
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_does_not_reuse_completed_result_for_different_work_unit() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let worktree = tmp.path().join("wt");
    let capture_dir = tmp.path().join("capture");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::create_dir_all(&capture_dir).unwrap();
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
count_file="$CAPTURE_DIR/spawns"
count=0
if [ -f "$count_file" ]; then
  count=$(cat "$count_file")
fi
count=$((count + 1))
printf '%s' "$count" > "$count_file"
printf 'result-%s' "$count"
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("CAPTURE_DIR", capture_dir.to_string_lossy().into_owned());
    sandbox.set_env(CODEX_WORKER_BIN_ENV, framework_bin());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let first_opts = lua_opts(&lua, "first-work");
    first_opts
        .set("worktree", worktree.to_string_lossy().into_owned())
        .unwrap();
    first_opts.set("dedup_key", "dedup-one").unwrap();
    let first: Table = spawn.call(first_opts).unwrap();
    assert_eq!(first.get::<String>("stdout").unwrap(), "result-1");

    let second_opts = lua_opts(&lua, "second-work");
    second_opts
        .set("worktree", worktree.to_string_lossy().into_owned())
        .unwrap();
    second_opts.set("dedup_key", "dedup-two").unwrap();
    let second: Table = spawn.call(second_opts).unwrap();
    assert_eq!(second.get::<String>("stdout").unwrap(), "result-2");
    assert_eq!(
        std::fs::read_to_string(capture_dir.join("spawns")).unwrap(),
        "2"
    );
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_adopts_running_worktree_result_without_respawn() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let worktree = tmp.path().join("wt");
    let capture_dir = tmp.path().join("capture");
    let started_fifo = tmp.path().join("started.fifo");
    let release_fifo = tmp.path().join("release.fifo");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::create_dir_all(&capture_dir).unwrap();
    make_fifo(&started_fifo);
    make_fifo(&release_fifo);
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
count_file="$CAPTURE_DIR/spawns"
count=0
if [ -f "$count_file" ]; then
  count=$(cat "$count_file")
fi
count=$((count + 1))
printf '%s' "$count" > "$count_file"
printf 'started' > "$STARTED_FIFO"
read _ < "$RELEASE_FIFO"
printf 'adopted-%s' "$count"
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("CAPTURE_DIR", capture_dir.to_string_lossy().into_owned());
    sandbox.set_env("STARTED_FIFO", started_fifo.to_string_lossy().into_owned());
    sandbox.set_env("RELEASE_FIFO", release_fifo.to_string_lossy().into_owned());
    sandbox.set_env(CODEX_WORKER_BIN_ENV, framework_bin());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let worktree_arg = worktree.to_string_lossy().into_owned();
    let (first_tx, first_rx) = std::sync::mpsc::channel();
    let first_thread = std::thread::spawn({
        let worktree_arg = worktree_arg.clone();
        move || {
            let lua = Lua::new();
            register(&lua).unwrap();
            let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
            let opts = lua_opts(&lua, "running-work");
            opts.set("worktree", worktree_arg).unwrap();
            opts.set("dedup_key", "running-dedup").unwrap();
            let result: Table = spawn.call(opts).unwrap();
            first_tx
                .send(result.get::<String>("stdout").unwrap())
                .unwrap();
        }
    });
    assert_eq!(read_fifo(&started_fifo), "started");

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua_opts(&lua, "running-work");
    opts.set("worktree", worktree_arg).unwrap();
    opts.set("dedup_key", "running-dedup").unwrap();
    let (second_tx, second_rx) = std::sync::mpsc::channel();
    let second_thread = std::thread::spawn(move || {
        let result: Table = spawn.call(opts).unwrap();
        second_tx
            .send(result.get::<String>("stdout").unwrap())
            .unwrap();
    });

    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        std::fs::read_to_string(capture_dir.join("spawns")).unwrap(),
        "1"
    );
    write_fifo(&release_fifo, "go\n");
    assert_eq!(recv_result(&first_rx, "first adopted result"), "adopted-1");
    assert_eq!(
        recv_result(&second_rx, "second adopted result"),
        "adopted-1"
    );
    first_thread.join().unwrap();
    second_thread.join().unwrap();
    assert_eq!(
        std::fs::read_to_string(capture_dir.join("spawns")).unwrap(),
        "1"
    );
}

#[cfg(unix)]
#[test]
fn codex_runs_reads_running_adoption_record_without_status_log() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let worktree = tmp.path().join("wt");
    let started_fifo = tmp.path().join("started.fifo");
    let release_fifo = tmp.path().join("release.fifo");
    std::fs::create_dir_all(&worktree).unwrap();
    make_fifo(&started_fifo);
    make_fifo(&release_fifo);
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'adoption-live-tail\n'
printf 'started' > "$STARTED_FIFO"
read _ < "$RELEASE_FIFO"
printf 'adoption-done'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("STARTED_FIFO", started_fifo.to_string_lossy().into_owned());
    sandbox.set_env("RELEASE_FIFO", release_fifo.to_string_lossy().into_owned());
    sandbox.set_env(CODEX_WORKER_BIN_ENV, framework_bin());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let worktree_arg = worktree.to_string_lossy().into_owned();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let worker_thread = std::thread::spawn({
        let worktree_arg = worktree_arg.clone();
        move || {
            let lua = Lua::new();
            register_with_dept(&lua, Some("pkg.implementer".to_string())).unwrap();
            let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
            let opts = lua_opts(&lua, "adoption status");
            opts.set("worktree", worktree_arg).unwrap();
            opts.set("role", "implementer").unwrap();
            opts.set("proposal_id", "issue-74").unwrap();
            let result: Table = spawn.call(opts).unwrap();
            result_tx
                .send(result.get::<String>("stdout").unwrap())
                .unwrap();
        }
    });
    assert_eq!(read_fifo(&started_fifo), "started");

    let log_dir = tmp.path().join("runtime/codex");
    for entry in std::fs::read_dir(&log_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("log") {
            std::fs::remove_file(path).unwrap();
        }
    }

    let lua = Lua::new();
    register(&lua).unwrap();
    let status_fn: mlua::Function = codex_runs_fn(&lua);
    let mut active: Option<Table> = None;
    for _ in 0..50 {
        let status: Table = status_fn.call(()).unwrap();
        let running: Table = status.get("running").unwrap();
        if running.raw_len() == 1 {
            let item: Table = running.get(1).unwrap();
            if item.get::<String>("output_tail").unwrap() == "adoption-live-tail\n" {
                active = Some(item);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let active = active.expect("running adoption status with live output tail");
    assert_eq!(active.get::<String>("status").unwrap(), "running");
    assert_eq!(active.get::<String>("role").unwrap(), "implementer");
    assert_eq!(active.get::<String>("proposal_id").unwrap(), "issue-74");
    assert_eq!(
        active.get::<String>("proposal_id_or_key").unwrap(),
        "issue-74"
    );
    assert_eq!(active.get::<String>("dept").unwrap(), "pkg.implementer");
    assert_eq!(
        active.get::<String>("output_tail").unwrap(),
        "adoption-live-tail\n"
    );
    assert!(tmp
        .path()
        .join(".fkst/runtime/logs/codex-adoption")
        .exists());
    assert!(!tmp.path().join("runtime/codex-adoption").exists());
    assert!(active.get::<i64>("exit_code").is_err());
    assert!(active.get::<String>("log_path").is_err());

    write_fifo(&release_fifo, "go\n");
    assert_eq!(
        recv_result(&result_rx, "adopted status result"),
        "adoption-live-tail\nadoption-done"
    );
    worker_thread.join().unwrap();
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_records_minimal_completed_status() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'sensitive output body'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register_with_dept(&lua, Some("pkg.writer".to_string())).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua_opts(&lua, "status");
    opts.set("label", "draft").unwrap();
    opts.set("proposal_id", "proposal-43").unwrap();

    let result: Table = spawn.call(opts).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);

    let status_fn: mlua::Function = codex_runs_fn(&lua);
    let status: Table = status_fn.call(()).unwrap();
    let running: Table = status.get("running").unwrap();
    let recent: Table = status.get("recent").unwrap();
    assert_eq!(running.raw_len(), 0);
    assert_eq!(recent.raw_len(), 1);

    let item: Table = recent.get(1).unwrap();
    assert_eq!(item.get::<String>("label").unwrap(), "draft");
    assert_eq!(item.get::<String>("dept").unwrap(), "pkg.writer");
    assert_eq!(item.get::<String>("proposal_id").unwrap(), "proposal-43");
    assert_eq!(item.get::<String>("status").unwrap(), "done");
    assert_eq!(item.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(
        item.get::<String>("output_tail").unwrap(),
        "sensitive output body"
    );
    assert!(item.get::<String>("run_id").unwrap().starts_with("codex-"));
    assert!(item.get::<u64>("started_at_ms").unwrap() > 0);
    assert!(item.get::<u64>("ended_at_ms").unwrap() >= item.get::<u64>("started_at_ms").unwrap());
    assert!(item.get::<u64>("elapsed_ms").unwrap() <= 60_000);
    assert!(item.get::<String>("started_at").unwrap().ends_with('Z'));
    assert!(item.get::<String>("ended_at").unwrap().ends_with('Z'));
    assert!(item.get::<String>("log_path").is_err());
    assert!(item.get::<String>("output_excerpt").is_err());

    let log_body = std::fs::read_to_string(result.get::<String>("log_path").unwrap()).unwrap();
    assert!(log_body.contains("CODEX_STATUS:"));
    assert!(!tmp.path().join(".fkst/runtime/codex-status").exists());
}

#[cfg(unix)]
#[test]
fn spawn_codex_running_status_is_queryable_before_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let started_fifo = tmp.path().join("started.fifo");
    let release_fifo = tmp.path().join("release.fifo");
    make_fifo(&started_fifo);
    make_fifo(&release_fifo);
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'started' > "$STARTED_FIFO"
read _ < "$RELEASE_FIFO"
printf 'done'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("STARTED_FIFO", started_fifo.to_string_lossy().into_owned());
    sandbox.set_env("RELEASE_FIFO", release_fifo.to_string_lossy().into_owned());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register_with_dept(&lua, Some("pkg.runner".to_string())).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex").unwrap();
    let opts = lua_opts(&lua, "running");
    opts.set("label", "live").unwrap();
    let handle: AnyUserData = spawn.call(opts).unwrap();
    assert_eq!(read_fifo(&started_fifo), "started");

    let status_fn: mlua::Function = codex_runs_fn(&lua);
    let status: Table = status_fn.call(()).unwrap();
    let running: Table = status.get("running").unwrap();
    let recent: Table = status.get("recent").unwrap();
    assert_eq!(running.raw_len(), 1);
    assert_eq!(recent.raw_len(), 0);
    let item: Table = running.get(1).unwrap();
    assert_eq!(item.get::<String>("label").unwrap(), "live");
    assert_eq!(item.get::<String>("dept").unwrap(), "pkg.runner");
    assert_eq!(item.get::<String>("status").unwrap(), "running");
    assert!(item.get::<i64>("exit_code").is_err());
    assert!(item.get::<usize>("permit_slot").unwrap() < DEFAULT_CODEX_PERMIT_SLOTS);
    assert!(item.get::<u64>("elapsed_ms").unwrap() <= 60_000);
    assert!(item.get::<String>("log_path").is_err());

    write_fifo(&release_fifo, "go\n");
    let await_all: mlua::Function = lua.globals().get("await_all").unwrap();
    let handles = lua.create_sequence_from(vec![handle]).unwrap();
    let results: Table = await_all.call(handles).unwrap();
    let result: Table = results.get(1).unwrap();
    assert_eq!(result.get::<String>("stdout").unwrap(), "done");

    let status: Table = status_fn.call(()).unwrap();
    let running: Table = status.get("running").unwrap();
    let recent: Table = status.get("recent").unwrap();
    assert_eq!(running.raw_len(), 0);
    assert_eq!(recent.raw_len(), 1);
    let completed: Table = recent.get(1).unwrap();
    assert_eq!(completed.get::<String>("status").unwrap(), "done");
    assert_eq!(completed.get::<i64>("exit_code").unwrap(), 0);
}

#[cfg(unix)]
#[test]
fn codex_runs_retains_last_fifty_completed_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'ok'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    for index in 0..55 {
        let opts = lua_opts(&lua, "retention");
        opts.set("label", format!("run-{index:02}")).unwrap();
        let result: Table = spawn.call(opts).unwrap();
        assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    }

    let status_fn: mlua::Function = codex_runs_fn(&lua);
    let status: Table = status_fn.call(()).unwrap();
    let recent: Table = status.get("recent").unwrap();
    assert_eq!(recent.raw_len(), 50);
    let newest: Table = recent.get(1).unwrap();
    assert_eq!(newest.get::<String>("label").unwrap(), "run-54");
    let oldest: Table = recent.get(50).unwrap();
    assert_eq!(oldest.get::<String>("label").unwrap(), "run-05");

    assert!(tmp.path().join("runtime/codex").exists());
    assert!(!tmp.path().join(".fkst/runtime/codex-status").exists());
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_keeps_codex_permit_and_prepares_rate_shims() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let pool_root = tmp.path().join("rate-pools");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'codex-ok'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    sandbox.set_env("FKST_RATE_POOL_ROOT", pool_root.as_os_str().to_owned());
    sandbox.set_env("FKST_RATE_POOL_CODEX", "1,1");
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let result: Table = spawn.call(lua_opts(&lua, "bypass")).unwrap();

    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(result.get::<String>("stdout").unwrap(), "codex-ok");
    assert!(pool_root.join("shims").exists());
    assert!(!pool_root.join("codex.bucket").exists());
    assert!(tmp
        .path()
        .join(".fkst/runtime/codex-permits/permit-0")
        .exists());
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_returns_visible_spawn_error() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("empty-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.set_env("PATH", bin_dir.to_string_lossy().into_owned());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua.create_table().unwrap();
    opts.set("prompt", "hello").unwrap();

    let result: mlua::Table = spawn.call(opts).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), -1);
    assert_eq!(result.get::<String>("error_kind").unwrap(), "spawn");
    assert_eq!(
        result.get::<String>("error_class").unwrap(),
        "provider-unavailable"
    );
    assert!(result
        .get::<String>("error")
        .unwrap()
        .contains("codex spawn failed"));
    assert!(result
        .get::<String>("stderr")
        .unwrap()
        .contains("codex spawn failed"));
    let log_path = PathBuf::from(result.get::<String>("log_path").unwrap());
    let log_body = std::fs::read_to_string(log_path).unwrap();
    assert!(log_body.contains("codex spawn failed"));
    assert!(log_body.contains("EXIT=-1\n"));
    assert!(log_body.contains("CMD=codex exec --dangerously-bypass-approvals-and-sandbox -\n"));

    let status_fn: mlua::Function = codex_runs_fn(&lua);
    let status: Table = status_fn.call(()).unwrap();
    let recent: Table = status.get("recent").unwrap();
    assert_eq!(recent.raw_len(), 1);
    let failed: Table = recent.get(1).unwrap();
    assert_eq!(failed.get::<String>("status").unwrap(), "failed");
    assert_eq!(failed.get::<i64>("exit_code").unwrap(), -1);
    assert!(failed
        .get::<String>("output_tail")
        .unwrap()
        .contains("codex spawn failed"));
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_classifies_provider_output() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'HTTP 401 bad credentials' >&2
exit 1
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let result: mlua::Table = spawn.call(lua_opts(&lua, "hello")).unwrap();

    assert_eq!(result.get::<i64>("exit_code").unwrap(), 1);
    assert!(result.get::<String>("error_kind").is_err());
    assert_eq!(
        result.get::<String>("error_class").unwrap(),
        "auth-degraded"
    );
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_continues_when_log_write_fails() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'still-ok'
"#,
    );

    let log_root_file = tmp.path().join("not-a-dir");
    fs::write(&log_root_file, "blocks directory creation").unwrap();
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(&log_root_file);
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua.create_table().unwrap();
    opts.set("prompt", "hello").unwrap();

    let result: mlua::Table = spawn.call(opts).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(result.get::<String>("stdout").unwrap(), "still-ok");
    assert!(result
        .get::<String>("log_path")
        .unwrap()
        .contains("not-a-dir"));
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_prunes_aged_codex_logs_and_retains_fresh_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'ok'
"#,
    );

    let log_dir = tmp.path().join("runtime/codex");
    std::fs::create_dir_all(&log_dir).unwrap();
    let old_log = log_dir.join("old.log");
    let fresh_log = log_dir.join("fresh.log");
    let ignored = log_dir.join("old.txt");
    let now = SystemTime::now();
    write_log_with_mtime(&old_log, "old", now - Duration::from_secs(2 * 60 * 60));
    write_log_with_mtime(&fresh_log, "fresh", now - Duration::from_secs(5 * 60));
    write_log_with_mtime(&ignored, "ignored", now - Duration::from_secs(2 * 60 * 60));

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    sandbox.set_env("FKST_CODEX_LOG_MAX_AGE", "1h");
    sandbox.unset_env("FKST_CODEX_LOG_MAX_BYTES");
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let result: Table = spawn.call(lua_opts(&lua, "retention")).unwrap();
    let current_log = PathBuf::from(result.get::<String>("log_path").unwrap());

    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    assert!(!old_log.exists());
    assert!(fresh_log.exists());
    assert!(ignored.exists());
    assert!(current_log.exists());
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_prunes_oldest_codex_logs_to_size_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'ok'
"#,
    );

    let log_dir = tmp.path().join("runtime/codex");
    std::fs::create_dir_all(&log_dir).unwrap();
    let oldest = log_dir.join("oldest.log");
    let middle = log_dir.join("middle.log");
    let newest = log_dir.join("newest.log");
    let now = SystemTime::now();
    write_log_with_mtime(&oldest, "aaaaaa", now - Duration::from_secs(30));
    write_log_with_mtime(&middle, "bbbbbb", now - Duration::from_secs(20));
    write_log_with_mtime(&newest, "cccccc", now - Duration::from_secs(10));

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    sandbox.set_env("FKST_CODEX_LOG_MAX_AGE", "0");
    sandbox.set_env("FKST_CODEX_LOG_MAX_BYTES", "12");
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let result: Table = spawn.call(lua_opts(&lua, "retention")).unwrap();
    let current_log = PathBuf::from(result.get::<String>("log_path").unwrap());

    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    assert!(!oldest.exists());
    assert!(middle.exists());
    assert!(newest.exists());
    assert!(current_log.exists());
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_respects_age_override_when_pruning_codex_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'ok'
"#,
    );

    let log_dir = tmp.path().join("runtime/codex");
    std::fs::create_dir_all(&log_dir).unwrap();
    let retained_by_override = log_dir.join("retained.log");
    write_log_with_mtime(
        &retained_by_override,
        "fresh-enough-for-override",
        SystemTime::now() - Duration::from_secs(2 * 60 * 60),
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    sandbox.set_env("FKST_CODEX_LOG_MAX_AGE", "3h");
    sandbox.unset_env("FKST_CODEX_LOG_MAX_BYTES");
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let result: Table = spawn.call(lua_opts(&lua, "retention")).unwrap();

    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    assert!(retained_by_override.exists());
}

#[cfg(unix)]
#[test]
fn spawn_codex_returns_handle_before_child_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let release_fifo = tmp.path().join("release.fifo");
    let pgid_fifo = tmp.path().join("pgid.fifo");
    make_fifo(&release_fifo);
    make_fifo(&pgid_fifo);
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf '%s' "$$" > "$PGID_FIFO"
read _ < "$RELEASE_FIFO"
printf 'released'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("RELEASE_FIFO", release_fifo.to_string_lossy().into_owned());
    sandbox.set_env("PGID_FIFO", pgid_fifo.to_string_lossy().into_owned());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex").unwrap();
    let handle: AnyUserData = spawn.call(lua_opts(&lua, "hello")).unwrap();

    let child_pid = read_fifo(&pgid_fifo).trim().parse::<i32>().unwrap();
    let child_pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(child_pid)))
        .unwrap()
        .as_raw();
    let current_pgid = nix::unistd::getpgrp().as_raw();
    assert_ne!(child_pgid, current_pgid);

    write_fifo(&release_fifo, "go\n");
    let await_all: mlua::Function = lua.globals().get("await_all").unwrap();
    let handles = lua.create_sequence_from(vec![handle]).unwrap();
    let results: Table = await_all.call(handles).unwrap();
    let first: Table = results.get(1).unwrap();
    assert_eq!(first.get::<String>("stdout").unwrap(), "released");
}

#[cfg(unix)]
#[test]
fn await_all_preserves_input_order() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let release_slow = tmp.path().join("release-slow.fifo");
    make_fifo(&release_slow);
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
read prompt
if [ "$prompt" = "slow" ]; then
  read _ < "$RELEASE_SLOW"
fi
printf '%s' "$prompt"
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("RELEASE_SLOW", release_slow.to_string_lossy().into_owned());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex").unwrap();
    let slow: AnyUserData = spawn.call(lua_opts(&lua, "slow")).unwrap();
    let fast: AnyUserData = spawn.call(lua_opts(&lua, "fast")).unwrap();
    write_fifo(&release_slow, "go\n");

    let await_all: mlua::Function = lua.globals().get("await_all").unwrap();
    let handles = lua.create_sequence_from(vec![slow, fast]).unwrap();
    let results: Table = await_all.call(handles).unwrap();
    let first: Table = results.get(1).unwrap();
    let second: Table = results.get(2).unwrap();
    assert_eq!(first.get::<String>("stdout").unwrap(), "slow");
    assert_eq!(second.get::<String>("stdout").unwrap(), "fast");
}

#[cfg(unix)]
#[test]
fn await_all_returns_each_failure_table_without_dropping_siblings() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
read prompt
if [ "$prompt" = "bad" ]; then
  printf 'bad-stderr' >&2
  exit 7
fi
printf 'ok-%s' "$prompt"
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex").unwrap();
    let bad: AnyUserData = spawn.call(lua_opts(&lua, "bad")).unwrap();
    let good: AnyUserData = spawn.call(lua_opts(&lua, "good")).unwrap();

    let await_all: mlua::Function = lua.globals().get("await_all").unwrap();
    let handles = lua.create_sequence_from(vec![bad, good]).unwrap();
    let results: Table = await_all.call(handles).unwrap();
    let first: Table = results.get(1).unwrap();
    let second: Table = results.get(2).unwrap();
    assert_eq!(first.get::<i64>("exit_code").unwrap(), 7);
    assert_eq!(first.get::<String>("stderr").unwrap(), "bad-stderr");
    assert_eq!(second.get::<i64>("exit_code").unwrap(), 0);
    assert_eq!(second.get::<String>("stdout").unwrap(), "ok-good");
}

#[cfg(unix)]
#[test]
fn await_all_rejects_reused_or_foreign_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'ok'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex").unwrap();
    let await_all: mlua::Function = lua.globals().get("await_all").unwrap();
    let handle: AnyUserData = spawn.call(lua_opts(&lua, "one")).unwrap();

    let duplicate = lua
        .create_sequence_from(vec![handle.clone(), handle.clone()])
        .unwrap();
    let duplicate_err = await_all.call::<Table>(duplicate).unwrap_err();
    assert!(duplicate_err.to_string().contains("reused"));

    let first = lua.create_sequence_from(vec![handle]).unwrap();
    let results: Table = await_all.call(first).unwrap();
    let result: Table = results.get(1).unwrap();
    assert_eq!(result.get::<String>("stdout").unwrap(), "ok");

    let reused_handle: AnyUserData = spawn.call(lua_opts(&lua, "two")).unwrap();
    let consumed_once = lua
        .create_sequence_from(vec![reused_handle.clone()])
        .unwrap();
    let _: Table = await_all.call(consumed_once).unwrap();
    let consumed_again = lua.create_sequence_from(vec![reused_handle]).unwrap();
    let consumed_err = await_all.call::<Table>(consumed_again).unwrap_err();
    assert!(consumed_err.to_string().contains("already consumed"));

    let foreign = lua
        .create_userdata(CodexTaskHandle {
            owner_id: u64::MAX,
            task_id: 99,
            join: Arc::new(Mutex::new(Some(std::thread::spawn(|| {
                CodexResult::success(String::new(), String::new(), 0, String::new())
            })))),
        })
        .unwrap();
    let foreign_table = lua.create_sequence_from(vec![foreign]).unwrap();
    let foreign_err = await_all.call::<Table>(foreign_table).unwrap_err();
    assert!(foreign_err.to_string().contains("different pipeline"));
}

#[cfg(unix)]
#[test]
fn spawn_codex_holds_permit_until_child_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let release_fifo = tmp.path().join("release.fifo");
    let started_fifo = tmp.path().join("started.fifo");
    make_fifo(&release_fifo);
    make_fifo(&started_fifo);
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf 'started' > "$STARTED_FIFO"
read _ < "$RELEASE_FIFO"
printf 'done'
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("RELEASE_FIFO", release_fifo.to_string_lossy().into_owned());
    sandbox.set_env("STARTED_FIFO", started_fifo.to_string_lossy().into_owned());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    sandbox.set_env(CODEX_PERMIT_SLOTS_ENV, "2");
    let (_lock, _guard) = sandbox.enter();
    ensure_pool().unwrap();
    let held_permits = (1..2)
        .map(|i| {
            let path = tmp
                .path()
                .join(format!(".fkst/runtime/codex-permits/permit-{i}"));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock).unwrap();
            file
        })
        .collect::<Vec<_>>();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex").unwrap();
    let handle: AnyUserData = spawn.call(lua_opts(&lua, "permit")).unwrap();
    assert_eq!(read_fifo(&started_fifo), "started");
    let permit_zero = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(tmp.path().join(".fkst/runtime/codex-permits/permit-0"))
        .unwrap();
    assert!(flock(permit_zero.as_raw_fd(), FlockArg::LockExclusiveNonblock).is_err());

    write_fifo(&release_fifo, "go\n");
    let await_all: mlua::Function = lua.globals().get("await_all").unwrap();
    let handles = lua.create_sequence_from(vec![handle]).unwrap();
    let results: Table = await_all.call(handles).unwrap();
    let result: Table = results.get(1).unwrap();
    assert_eq!(result.get::<String>("stdout").unwrap(), "done");

    assert!(flock(permit_zero.as_raw_fd(), FlockArg::LockExclusiveNonblock).is_ok());
    drop(held_permits);
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_stdout_activity_does_not_extend_timeout() {
    let result = run_timeout_activity_test(TestStream::Stdout);
    assert_eq!(result.exit_code, 124);
    assert_eq!(result.error_kind, "timeout");
    assert!(result.error.contains("timed out"));
    assert!(result.stdout.contains("tick:"));
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_stderr_activity_does_not_extend_timeout() {
    let result = run_timeout_activity_test(TestStream::Stderr);
    assert_eq!(result.exit_code, 124);
    assert_eq!(result.error_kind, "timeout");
    assert!(result.error.contains("timed out"));
    assert!(result.stderr.contains("tick:"));
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_stdin_write_is_bounded_by_overall_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
while :; do :; done
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua_opts(&lua, &"prompt ".repeat(1024 * 1024));
    opts.set("timeout", 1).unwrap();

    let result: Table = spawn.call(opts).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 124);
    assert_eq!(result.get::<String>("error_kind").unwrap(), "timeout");
    assert!(result.get::<String>("error").unwrap().contains("timed out"));
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_silent_child_exits_before_timeout_successfully() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
sleep 1
exit 0
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua_opts(&lua, "silent");
    opts.set("timeout", 5).unwrap();

    let result: Table = spawn.call(opts).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 0);
    assert!(result.get::<String>("stderr").unwrap().is_empty());
    assert!(result.get::<String>("error_kind").is_err());
    let log_path = PathBuf::from(result.get::<String>("log_path").unwrap());
    let log_body = std::fs::read_to_string(log_path).unwrap();
    assert!(log_body.contains("EXIT=0\n"));
    assert!(log_body.contains("TIMEOUT_SECONDS=5\n"));
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_silent_child_returns_timeout_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
while :; do :; done
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua_opts(&lua, "timed out");
    opts.set("timeout", 1).unwrap();

    let result: Table = spawn.call(opts).unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 124);
    assert_eq!(result.get::<String>("error_kind").unwrap(), "timeout");
    assert!(result
        .get::<String>("stderr")
        .unwrap()
        .contains("timed out"));
    let log_path = PathBuf::from(result.get::<String>("log_path").unwrap());
    let log_body = std::fs::read_to_string(log_path).unwrap();
    assert!(log_body.contains("TIMEOUT_SECONDS=1\n"));
}

#[cfg(unix)]
#[test]
fn spawn_codex_sync_kills_timed_out_child_process_group() {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let pid_fifo = tmp.path().join("pid.fifo");
    let timeout_fifo = tmp.path().join("timeout.fifo");
    make_fifo(&pid_fifo);
    make_fifo(&timeout_fifo);
    install_codex_script(
        &bin_dir,
        r#"#!/bin/sh
cat >/dev/null
printf '%s' "$$" > "$PID_FIFO"
read _ < "$TIMEOUT_FIFO"
"#,
    );

    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(tmp.path()).runtime_root(".fkst/runtime");
    sandbox.prepend_path(&bin_dir);
    sandbox.set_env("PID_FIFO", pid_fifo.to_string_lossy().into_owned());
    sandbox.set_env("TIMEOUT_FIFO", timeout_fifo.to_string_lossy().into_owned());
    sandbox.runtime_log_dir(tmp.path().join("runtime"));
    let (_lock, _guard) = sandbox.enter();

    let pid_reader = std::thread::spawn({
        let pid_fifo = pid_fifo.clone();
        move || read_fifo(&pid_fifo).trim().parse::<i32>().unwrap()
    });

    let lua = Lua::new();
    register(&lua).unwrap();
    let spawn: mlua::Function = lua.globals().get("spawn_codex_sync").unwrap();
    let opts = lua_opts(&lua, "timed out");
    opts.set("timeout", 2).unwrap();

    let result: Table = spawn.call(opts).unwrap();
    let child_pid = pid_reader.join().unwrap();
    assert_eq!(result.get::<i64>("exit_code").unwrap(), 124);
    assert_eq!(result.get::<String>("error_kind").unwrap(), "timeout");
    assert!(result.get::<String>("error").unwrap().contains("timed out"));
    assert!(result
        .get::<String>("error")
        .unwrap()
        .contains("SIGKILL to process group"));
    assert!(kill(Pid::from_raw(child_pid), None).is_err());
}

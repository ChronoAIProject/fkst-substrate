// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/boundary_resource.rs"]
mod boundary_resource;
#[path = "../src/config_registry.rs"]
mod config_registry;
#[path = "../src/external_command.rs"]
mod external_command;
#[path = "../src/rate_pool.rs"]
mod rate_pool;
#[path = "../src/rate_shim.rs"]
mod rate_shim;
#[path = "../src/runtime_context.rs"]
mod runtime_context;
#[path = "../src/sdk_codex.rs"]
mod sdk_codex;
mod support;

use mlua::{AnyUserData, Lua, Table};
use nix::fcntl::{flock, FlockArg};
use sdk_codex::{
    acquire_permit, ensure_pool, CodexResult, CodexTaskHandle, CODEX_PERMIT_SLOTS_ENV,
};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::process_sandbox::ProcessSandbox;

const DEFAULT_CODEX_PERMIT_SLOTS: usize = 20;

fn register(lua: &Lua) -> mlua::Result<()> {
    let host_root = std::env::current_dir().map_err(mlua::Error::external)?;
    let config = config_registry::ConfigContext::from_host_root(&host_root)
        .map_err(mlua::Error::external)?;
    sdk_codex::register(lua, &host_root, config)
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
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|err| panic!("timed out reading FIFO {display}: {err}"));
    result.unwrap_or_else(|err| panic!("failed reading FIFO {display}: {err}"))
}

fn recv_result<T>(rx: &std::sync::mpsc::Receiver<T>, label: &str) -> T {
    rx.recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|err| panic!("timed out waiting for {label}: {err}"))
}

#[cfg(unix)]
fn continuous_output_codex_script(stream_redirect: &'static str) -> String {
    format!(
        r#"#!/bin/sh
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
        opts.set("timeout", 2).unwrap();
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

    let result = recv_result(&result_rx, "codex activity result");
    spawn_thread.join().unwrap();
    result
}

fn lua_opts(lua: &Lua, prompt: &str) -> Table {
    let opts = lua.create_table().unwrap();
    opts.set("prompt", prompt).unwrap();
    opts
}

#[test]
fn fixed_surface_does_not_register_await_any_await_or_sleep() {
    let lua = Lua::new();
    register(&lua).unwrap();

    lua.load(
        r#"
        assert(type(spawn_codex) == "function")
        assert(type(await_all) == "function")
        assert(await_any == nil)
        assert(await == nil)
        assert(sleep == nil)
        "#,
    )
    .exec()
    .unwrap();
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

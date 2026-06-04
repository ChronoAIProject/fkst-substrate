// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/runtime_context.rs"]
mod runtime_context;
#[path = "../src/sdk_log.rs"]
mod sdk_log;
#[path = "../src/sdk_mark.rs"]
mod sdk_mark;
mod support;

use mlua::Lua;
use std::path::Path;
use support::process_sandbox::ProcessSandbox;
use tempfile::tempdir;

fn in_sandbox<T>(
    dir: &Path,
    configure: impl FnOnce(&mut ProcessSandbox),
    f: impl FnOnce() -> T,
) -> T {
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(dir);
    configure(&mut sandbox);
    sandbox.run(f)
}

fn register_for_host(lua: &Lua, host_root: &Path) {
    sdk_mark::register(lua, host_root).unwrap();
}

#[test]
fn once_runs_first_time_and_writes_marker() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let (ran, count): (bool, i64) = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                local count = 0
                local ran = once("k", function() count = count + 1 end)
                return ran, count
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert!(ran);
    assert_eq!(count, 1);
    let marker = std::fs::read_to_string(runtime.path().join("marks/6b")).unwrap();
    assert!(marker.starts_with("key=k\nmarked_at="), "{marker}");
    assert!(marker.ends_with("Z\n"), "{marker}");
    assert!(runtime.path().join("locks/once-6b").exists());
}

#[test]
fn once_skips_second_time_without_running_callback() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let (first, second, count): (bool, bool, i64) = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                local count = 0
                local first = once("k", function() count = count + 1 end)
                local second = once("k", function() count = count + 1 end)
                return first, second, count
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert!(first);
    assert!(!second);
    assert_eq!(count, 1);
}

#[test]
fn once_error_does_not_write_marker_and_subsequent_call_retries() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let err = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                return once("k", function()
                    error("intentional once failure")
                end)
                "#,
            )
            .eval::<bool>()
            .unwrap_err()
        },
    );

    assert!(err.to_string().contains("intentional once failure"));
    assert!(!runtime.path().join("marks/6b").exists());

    let (ran, count): (bool, i64) = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                local count = 0
                local ran = once("k", function() count = count + 1 end)
                return ran, count
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert!(ran);
    assert_eq!(count, 1);
    let marker = std::fs::read_to_string(runtime.path().join("marks/6b")).unwrap();
    assert!(marker.starts_with("key=k\nmarked_at="), "{marker}");
    assert!(marker.ends_with("Z\n"), "{marker}");
}

#[test]
fn once_rejects_empty_key() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let err = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(r#"return once("", function() end)"#)
                .eval::<bool>()
                .unwrap_err()
        },
    );

    assert!(err.to_string().contains("once key must not be empty"));
}

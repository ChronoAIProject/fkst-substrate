// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/runtime_context.rs"]
mod runtime_context;
#[path = "../src/sdk_cache.rs"]
mod sdk_cache;
mod support;

use mlua::Lua;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
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
    sdk_cache::register(lua, host_root).unwrap();
}

fn register_for_host_with_clock(lua: &Lua, host_root: &Path, now: Arc<AtomicU64>) {
    sdk_cache::register_with_clock(
        lua,
        host_root,
        Arc::new(move || u128::from(now.load(Ordering::SeqCst))),
    )
    .unwrap();
}

fn cache_path(runtime: &Path, key: &str) -> std::path::PathBuf {
    runtime.join("cache").join(key).join("=value")
}

fn assert_cache_file_value(path: &Path, expected: &str) {
    assert_cache_file_bytes(path, expected.as_bytes());
}

fn assert_cache_file_bytes(path: &Path, expected: &[u8]) {
    let raw = std::fs::read(path).unwrap();
    assert!(
        raw.starts_with(b"fkst-cache-v1 expires_at="),
        "cache entry missing header: {raw:?}"
    );
    assert!(
        raw.ends_with(expected),
        "cache entry did not preserve value: {raw:?}"
    );
}

#[test]
fn cache_set_then_get_roundtrips_value() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let value: String = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                cache_set("github-proxy/issue/owner/repo/42", "value")
                return cache_get("github-proxy/issue/owner/repo/42")
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert_eq!(value, "value");
    assert_cache_file_value(
        &cache_path(runtime.path(), "github-proxy/issue/owner/repo/42"),
        "value",
    );
}

#[test]
fn cache_set_then_get_uses_readable_hierarchical_path() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let value: String = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                cache_set("github-proxy/issue/owner/repo/42", "payload")
                return cache_get("github-proxy/issue/owner/repo/42")
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    let path = runtime
        .path()
        .join("cache/github-proxy/issue/owner/repo/42/=value");
    assert_eq!(value, "payload");
    assert_cache_file_value(&path, "payload");
}

#[test]
fn cache_allows_prefix_extended_keys() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let (first, second): (String, String) = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                cache_set("a/b/c", "first")
                cache_set("a/b/c/d", "second")
                return cache_get("a/b/c"), cache_get("a/b/c/d")
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert_eq!(first, "first");
    assert_eq!(second, "second");
    let first_path = cache_path(runtime.path(), "a/b/c");
    let second_path = cache_path(runtime.path(), "a/b/c/d");
    assert_ne!(first_path, second_path);
    assert_cache_file_value(&first_path, "first");
    assert_cache_file_value(&second_path, "second");
}

#[test]
fn cache_get_missing_key_returns_nil() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let value: Option<String> = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || lua.load(r#"return cache_get("missing")"#).eval().unwrap(),
    );

    assert_eq!(value, None);
}

#[test]
fn cache_get_rejects_empty_key() {
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
            lua.load(r#"return cache_get("")"#)
                .eval::<()>()
                .unwrap_err()
        },
    );

    assert!(err.to_string().contains("runtime key must not be empty"));
}

#[test]
fn cache_set_overwrites_existing_value() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let value: String = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                cache_set("github-proxy/issue/owner/repo/42", "old")
                cache_set("github-proxy/issue/owner/repo/42", "new")
                return cache_get("github-proxy/issue/owner/repo/42")
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert_eq!(value, "new");
    assert_cache_file_value(
        &cache_path(runtime.path(), "github-proxy/issue/owner/repo/42"),
        "new",
    );
}

#[test]
fn cache_set_then_get_roundtrips_special_content() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let value: String = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                local value = "line one\nline=two"
                cache_set("content/special", value)
                return cache_get("content/special")
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert_eq!(value, "line one\nline=two");
}

#[test]
fn cache_set_then_get_roundtrips_binary_value() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    let value: mlua::String = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                local value = string.char(0, 255, 10, 128, 65, 0) .. "\ntrail"
                cache_set("content/binary", value)
                return cache_get("content/binary")
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    let expected = [0, 255, 10, 128, 65, 0, 10, b't', b'r', b'a', b'i', b'l'];
    assert_eq!(value.as_bytes().as_ref(), expected);
    assert_cache_file_bytes(&cache_path(runtime.path(), "content/binary"), &expected);
}

#[test]
fn cache_set_with_ttl_gets_before_deadline() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    let now = Arc::new(AtomicU64::new(1_000_000_000_000));
    register_for_host_with_clock(&lua, host.path(), now);

    let value: String = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                cache_set("ttl/live", "payload", 2)
                return cache_get("ttl/live")
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert_eq!(value, "payload");
    let raw = std::fs::read_to_string(cache_path(runtime.path(), "ttl/live")).unwrap();
    assert!(
        raw.starts_with("fkst-cache-v1 expires_at=1002000000000\n"),
        "{raw}"
    );
}

#[test]
fn cache_get_after_ttl_deadline_misses_and_evicts_file() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    let path = cache_path(runtime.path(), "ttl/expired");
    let now = Arc::new(AtomicU64::new(1_000_000_000_000));
    register_for_host_with_clock(&lua, host.path(), now.clone());

    in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(r#"cache_set("ttl/expired", "payload", 1)"#)
                .exec()
                .unwrap()
        },
    );
    assert!(path.exists());

    now.store(3_000_000_000_000, Ordering::SeqCst);
    let value: Option<String> = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(r#"return cache_get("ttl/expired")"#)
                .eval()
                .unwrap()
        },
    );

    assert_eq!(value, None);
    assert!(!path.exists());
}

#[test]
fn cache_clock_before_unix_epoch_saturates() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    let before_epoch = UNIX_EPOCH - Duration::from_nanos(1);
    sdk_cache::register_with_clock(
        &lua,
        host.path(),
        Arc::new(move || sdk_cache::unix_nanos_for_system_time(before_epoch)),
    )
    .unwrap();

    let value: String = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                cache_set("ttl/pre-epoch", "payload", 1)
                return cache_get("ttl/pre-epoch")
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert_eq!(value, "payload");
    let raw = std::fs::read_to_string(cache_path(runtime.path(), "ttl/pre-epoch")).unwrap();
    assert!(
        raw.starts_with("fkst-cache-v1 expires_at=1000000000\n"),
        "{raw}"
    );
}

#[test]
fn cache_expire_removes_live_key_and_allows_missing_key() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    let path = cache_path(runtime.path(), "ttl/live");
    register_for_host(&lua, host.path());

    let value: Option<String> = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(
                r#"
                cache_set("ttl/live", "payload")
                cache_expire("ttl/live")
                cache_expire("ttl/live")
                return cache_get("ttl/live")
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert_eq!(value, None);
    assert!(!path.exists());
}

#[test]
fn cache_get_malformed_file_returns_nil_without_error() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    let path = cache_path(runtime.path(), "bad/entry");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not a cache entry").unwrap();
    register_for_host(&lua, host.path());

    let value: Option<String> = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || lua.load(r#"return cache_get("bad/entry")"#).eval().unwrap(),
    );

    assert_eq!(value, None);
    assert!(path.exists());
}

#[test]
fn cache_set_concurrent_writers_leave_complete_entry() {
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    let path = cache_path(runtime.path(), "concurrent/key");
    let host_root = host.path().to_path_buf();

    in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            let mut threads = Vec::new();
            for idx in 0..16 {
                let host_root = host_root.clone();
                threads.push(std::thread::spawn(move || {
                    let lua = Lua::new();
                    register_for_host(&lua, &host_root);
                    lua.load(format!(r#"cache_set("concurrent/key", "value-{idx}")"#))
                        .exec()
                        .unwrap()
                }));
            }
            for thread in threads {
                thread.join().unwrap();
            }
        },
    );

    let lua = Lua::new();
    register_for_host(&lua, host.path());
    let value: String = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(r#"return cache_get("concurrent/key")"#)
                .eval()
                .unwrap()
        },
    );

    assert!(value.starts_with("value-"), "{value}");
    assert_cache_file_value(&path, &value);
}

#[test]
fn cache_set_rejects_empty_key() {
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
            lua.load(r#"return cache_set("", "value")"#)
                .eval::<()>()
                .unwrap_err()
        },
    );

    assert!(err.to_string().contains("runtime key must not be empty"));
}

#[test]
fn cache_rejects_invalid_path_keys() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let runtime = tempdir().unwrap();
    register_for_host(&lua, host.path());

    for key in [
        "..", "a/../b", "/abs", "a//b", "a/", "", "bad key", "bad:key",
    ] {
        let set_err = in_sandbox(
            host.path(),
            |sandbox| {
                sandbox.runtime_root(runtime.path());
            },
            || {
                lua.load(format!("return cache_set({key:?}, \"value\")"))
                    .eval::<()>()
                    .unwrap_err()
            },
        );
        assert!(
            set_err.to_string().contains("runtime key"),
            "{key:?}: {set_err}"
        );

        let get_err = in_sandbox(
            host.path(),
            |sandbox| {
                sandbox.runtime_root(runtime.path());
            },
            || {
                lua.load(format!("return cache_get({key:?})"))
                    .eval::<()>()
                    .unwrap_err()
            },
        );
        assert!(
            get_err.to_string().contains("runtime key"),
            "{key:?}: {get_err}"
        );

        let expire_err = in_sandbox(
            host.path(),
            |sandbox| {
                sandbox.runtime_root(runtime.path());
            },
            || {
                lua.load(format!("return cache_expire({key:?})"))
                    .eval::<()>()
                    .unwrap_err()
            },
        );
        assert!(
            expire_err.to_string().contains("runtime key"),
            "{key:?}: {expire_err}"
        );
    }
}

// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/runtime_context.rs"]
mod runtime_context;
#[path = "../src/sdk_cache.rs"]
mod sdk_cache;
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
    sdk_cache::register(lua, host_root).unwrap();
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
    assert_eq!(
        std::fs::read_to_string(
            runtime
                .path()
                .join("cache/github-proxy/issue/owner/repo/42")
        )
        .unwrap(),
        "value"
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
        .join("cache/github-proxy/issue/owner/repo/42");
    assert_eq!(value, "payload");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "payload");
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
    assert_eq!(
        std::fs::read_to_string(
            runtime
                .path()
                .join("cache/github-proxy/issue/owner/repo/42")
        )
        .unwrap(),
        "new"
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
    }
}

//! Initialize a Lua 5.4 state, expose SDK globals, load + run a lua file.

use anyhow::{Context, Result};
use mlua::{Lua, LuaSerdeExt, Value as LuaValue};
use serde_json::Value as JsonValue;
use std::path::Path;

use crate::config_registry::ConfigContext;
use crate::external_command::MockCommandState;
use crate::path_resolver::{package_root_path, NameResolver, PackageRoots};
use crate::raise::RaiseBuffer;

/// Create a Lua state with stdlib enabled.
pub fn new_lua() -> Lua {
    Lua::new()
}

/// Register the framework SDK globals in the same order for every entry point.
pub fn register_framework_sdk(
    lua: &Lua,
    raise_buf: RaiseBuffer,
    host_root: &Path,
    owner_root: &Path,
    dept: Option<String>,
    resolver: NameResolver,
    owner_namespace: String,
    graph_roots: Option<PackageRoots>,
    graph_json_authorized: bool,
) -> mlua::Result<()> {
    let config = ConfigContext::from_host_root(host_root).map_err(mlua::Error::external)?;
    crate::rate_pool::RatePoolRegistry::from_config(&config).map_err(mlua::Error::external)?;
    crate::sdk_log::register(lua)?;
    crate::sdk_i18n::register(lua, owner_root)?;
    crate::sdk_basic::register_with_runner(lua, config.clone(), None)?;
    crate::sdk_strings::register(lua)?;
    crate::sdk_graph::register(lua, graph_roots, graph_json_authorized)?;
    crate::sdk_fs::register(lua)?;
    crate::sdk_json::register(lua)?;
    crate::sdk_git::register(lua, host_root, config.clone())?;
    crate::sdk_mark::register(lua, host_root)?;
    crate::sdk_cache::register(lua, host_root)?;
    crate::sdk_codex::register(lua, host_root, config, dept)?;
    crate::raise::register(lua, raise_buf, resolver, owner_namespace)?;
    Ok(())
}

pub(crate) fn register_framework_sdk_with_runner(
    lua: &Lua,
    raise_buf: RaiseBuffer,
    host_root: &Path,
    owner_root: &Path,
    dept: Option<String>,
    resolver: NameResolver,
    owner_namespace: String,
    runner: Option<MockCommandState>,
    graph_roots: Option<PackageRoots>,
    graph_json_authorized: bool,
) -> mlua::Result<()> {
    let config = ConfigContext::from_host_root(host_root).map_err(mlua::Error::external)?;
    crate::rate_pool::RatePoolRegistry::from_config(&config).map_err(mlua::Error::external)?;
    crate::sdk_log::register(lua)?;
    crate::sdk_i18n::register(lua, owner_root)?;
    crate::sdk_basic::register_with_runner(lua, config.clone(), runner.clone())?;
    crate::sdk_strings::register(lua)?;
    crate::sdk_graph::register(lua, graph_roots, graph_json_authorized)?;
    crate::sdk_fs::register(lua)?;
    crate::sdk_json::register(lua)?;
    crate::sdk_git::register_with_runner(lua, host_root, config.clone(), runner.clone())?;
    crate::sdk_mark::register(lua, host_root)?;
    crate::sdk_cache::register(lua, host_root)?;
    crate::sdk_codex::register_with_runner(lua, host_root, config, dept, runner)?;
    crate::raise::register(lua, raise_buf, resolver, owner_namespace)?;
    Ok(())
}

/// Convert serde_json::Value to mlua::Value via LuaSerdeExt.
pub fn json_to_lua(lua: &Lua, v: &JsonValue) -> mlua::Result<LuaValue> {
    lua.to_value(v)
}

/// Build the Lua search path for fixed graph roots in lookup order.
pub(crate) fn package_roots_path<'a>(roots: impl IntoIterator<Item = &'a Path>) -> String {
    roots
        .into_iter()
        .map(package_root_path)
        .collect::<Vec<_>>()
        .join(";")
}

pub(crate) fn set_package_roots_path<'a>(
    lua: &Lua,
    package_roots: impl IntoIterator<Item = &'a Path>,
) -> mlua::Result<()> {
    let roots_path = package_roots_path(package_roots);
    lua.load(format!(
        "package.path = {:?}; package.cpath = \"\"",
        roots_path
    ))
    .exec()
}

/// Load the lua file at `path`, execute its top-level chunk, then call `pipeline(event)`.
/// `event` is a serde_json::Value that we expose as a Lua table under the global `event`
/// (and also pass as the argument to `pipeline`).
///
/// Returns Ok on success; errors propagate (script errors, missing pipeline fn, etc.).
#[cfg(test)]
fn run_dept_with_package_root(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    package_root: &Path,
) -> Result<()> {
    run_dept_with_require_roots(lua, lua_path, event, [package_root])
}

pub fn run_dept_with_require_roots<'a>(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    package_roots: impl IntoIterator<Item = &'a Path>,
) -> Result<()> {
    let package_roots = package_roots.into_iter().collect::<Vec<_>>();
    let roots_label = package_roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(";");
    set_package_roots_path(lua, package_roots.iter().copied())
        .with_context(|| format!("set package.path for {}", roots_label))?;

    let src = std::fs::read_to_string(lua_path)
        .with_context(|| format!("read {}", lua_path.display()))?;
    let chunk = lua.load(&src).set_name(lua_path.to_string_lossy());
    chunk
        .exec()
        .with_context(|| format!("exec {}", lua_path.display()))?;

    let pipeline: mlua::Function = lua
        .globals()
        .get("pipeline")
        .context("lua file did not define global `pipeline` function")?;
    let event_lua = json_to_lua(lua, event).context("json -> lua event conversion")?;
    lua.globals()
        .set("event", event_lua.clone())
        .context("set global `event`")?;
    pipeline
        .call::<()>(event_lua)
        .context("pipeline(event) call")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn write_lua(s: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(s.as_bytes()).unwrap();
        f
    }

    #[test]
    fn run_dept_calls_pipeline() {
        let lua = new_lua();
        // Use a Lua global side channel to verify pipeline was called.
        let f = write_lua(
            r#"
            called = 0
            function pipeline(event)
                called = called + 1
                assert(event.foo == "bar", "expected foo=bar, got " .. tostring(event.foo))
            end
        "#,
        );
        let package_root = f.path().parent().unwrap();
        run_dept_with_package_root(
            &lua,
            f.path(),
            &serde_json::json!({"foo": "bar"}),
            package_root,
        )
        .unwrap();
        let called: i64 = lua.globals().get("called").unwrap();
        assert_eq!(called, 1);
    }

    #[test]
    fn missing_pipeline_returns_err() {
        let lua = new_lua();
        let f = write_lua("x = 1\n");
        let package_root = f.path().parent().unwrap();
        let err = run_dept_with_package_root(&lua, f.path(), &serde_json::json!({}), package_root)
            .unwrap_err();
        assert!(format!("{}", err).contains("pipeline"));
    }

    #[test]
    fn lua_syntax_error_returns_err() {
        let lua = new_lua();
        let f = write_lua("this is = not valid {{ lua");
        let package_root = f.path().parent().unwrap();
        let err = run_dept_with_package_root(&lua, f.path(), &serde_json::json!({}), package_root)
            .unwrap_err();
        assert!(format!("{}", err).contains("exec"));
    }

    #[test]
    fn run_dept_loads_package_root_modules() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("departments/demo")).unwrap();
        std::fs::create_dir_all(dir.path().join("fkst")).unwrap();
        std::fs::write(
            dir.path().join("fkst/example.lua"),
            r#"return { value = function() return "ok" end }"#,
        )
        .unwrap();
        let main = dir.path().join("departments/demo/main.lua");
        std::fs::write(
            &main,
            r#"
            local example = require("fkst.example")
            function pipeline(event)
                called = example.value()
            end
        "#,
        )
        .unwrap();

        let lua = new_lua();
        run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path()).unwrap();
        let called: String = lua.globals().get("called").unwrap();
        assert_eq!(called, "ok");
    }

    #[test]
    fn set_package_root_path_replaces_existing_search_path() {
        let dir = TempDir::new().unwrap();
        let lua = new_lua();
        lua.load(r#"package.path = "prior/?.lua""#).exec().unwrap();

        set_package_roots_path(&lua, [dir.path()]).unwrap();

        let package: mlua::Table = lua.globals().get("package").unwrap();
        let path: String = package.get("path").unwrap();
        let cpath: String = package.get("cpath").unwrap();
        assert_eq!(path, package_root_path(dir.path()));
        assert_eq!(cpath, "");
    }

    #[test]
    fn package_root_path_resolves_module_main_lua() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("departments/demo")).unwrap();
        std::fs::create_dir_all(dir.path().join("core")).unwrap();
        std::fs::write(
            dir.path().join("core/main.lua"),
            r#"return { marker = "from-main" }"#,
        )
        .unwrap();
        let main = dir.path().join("departments/demo/main.lua");
        std::fs::write(
            &main,
            r#"
            local core = require("core")
            function pipeline(event)
                called = core.marker
            end
        "#,
        )
        .unwrap();

        let lua = new_lua();
        run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path()).unwrap();
        let called: String = lua.globals().get("called").unwrap();
        assert_eq!(called, "from-main");
    }

    #[test]
    fn run_dept_does_not_fall_back_to_cwd_or_existing_search_path() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let owner = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        std::fs::create_dir_all(owner.path().join("departments/demo")).unwrap();
        std::fs::write(cwd.path().join("core.lua"), r#"return { value = "cwd" }"#).unwrap();
        let main = owner.path().join("departments/demo/main.lua");
        std::fs::write(
            &main,
            r#"
            local core = require("core")
            function pipeline(event)
                called = core.value
            end
        "#,
        )
        .unwrap();

        let prior_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd.path()).unwrap();
        let lua = new_lua();
        lua.load(format!(
            "package.path = {:?}",
            cwd.path().join("?.lua").display().to_string()
        ))
        .exec()
        .unwrap();
        let err = run_dept_with_package_root(&lua, &main, &serde_json::json!({}), owner.path())
            .unwrap_err();
        std::env::set_current_dir(prior_cwd).unwrap();

        let msg = format!("{err:#}");
        assert!(msg.contains("module 'core' not found"), "{msg}");
    }
}

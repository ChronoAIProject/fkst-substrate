//! Initialize a Lua 5.4 state, expose SDK globals, load + run a lua file.

use anyhow::{Context, Result};
use mlua::{Lua, LuaSerdeExt, Value as LuaValue};
use serde_json::Value as JsonValue;
use std::path::{Path, PathBuf};

use crate::config_registry::ConfigContext;
use crate::path_resolver::PackageRoots;
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
) -> mlua::Result<()> {
    let config = ConfigContext::from_host_root(host_root).map_err(mlua::Error::external)?;
    crate::sdk_log::register(lua)?;
    crate::sdk_basic::register(lua)?;
    crate::sdk_fs::register(lua)?;
    crate::sdk_json::register(lua)?;
    crate::sdk_git::register(lua, host_root, config.clone())?;
    crate::sdk_codex::register(lua, host_root, config)?;
    crate::raise::register(lua, raise_buf)?;
    Ok(())
}

/// Convert serde_json::Value to mlua::Value via LuaSerdeExt.
pub fn json_to_lua(lua: &Lua, v: &JsonValue) -> mlua::Result<LuaValue> {
    lua.to_value(v)
}

/// Build the Lua search path for one root.
pub(crate) fn package_root_path(package_root: &Path) -> String {
    let root = package_root.display();
    format!("{root}/?.lua;{root}/?/init.lua;{root}/?/main.lua")
}

/// Build the Lua search path for fixed graph roots in lookup order.
pub(crate) fn package_roots_path<'a>(roots: impl IntoIterator<Item = &'a Path>) -> String {
    roots
        .into_iter()
        .map(package_root_path)
        .collect::<Vec<_>>()
        .join(";")
}

/// Find the host root that owns a Lua entrypoint.
pub(crate) fn package_root_for_lua(lua_path: &Path) -> PathBuf {
    let Some(parent) = lua_path.parent() else {
        return PathBuf::from(".");
    };
    if parent.file_name().and_then(|s| s.to_str()) == Some("raisers") {
        return parent.parent().unwrap_or(parent).to_path_buf();
    }
    if parent.file_name().and_then(|s| s.to_str()) == Some("fkst") {
        return parent.parent().unwrap_or(parent).to_path_buf();
    }
    if parent.file_name().and_then(|s| s.to_str()) == Some("departments") {
        return parent.parent().unwrap_or(parent).to_path_buf();
    }
    match parent.parent() {
        Some(grandparent)
            if grandparent.file_name().and_then(|s| s.to_str()) == Some("departments") =>
        {
            grandparent.parent().unwrap_or(grandparent).to_path_buf()
        }
        _ => parent.to_path_buf(),
    }
}

pub(crate) fn set_package_roots_path<'a>(
    lua: &Lua,
    package_roots: impl IntoIterator<Item = &'a Path>,
) -> mlua::Result<()> {
    let package: mlua::Table = lua.globals().get("package")?;
    let existing: String = package.get("path")?;
    let roots_path = package_roots_path(package_roots);
    let next = if existing.is_empty() {
        roots_path
    } else {
        format!("{};{}", roots_path, existing)
    };
    lua.load(format!("package.path = {:?}", next)).exec()
}

/// Load the lua file at `path`, execute its top-level chunk, then call `pipeline(event)`.
/// `event` is a serde_json::Value that we expose as a Lua table under the global `event`
/// (and also pass as the argument to `pipeline`).
///
/// Returns Ok on success; errors propagate (script errors, missing pipeline fn, etc.).
pub fn run_dept_with_package_root(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    package_root: &Path,
) -> Result<()> {
    let inferred_root = package_root_for_lua(lua_path);
    let roots: Vec<&Path> = if inferred_root == package_root {
        vec![package_root]
    } else {
        vec![package_root, inferred_root.as_path()]
    };
    set_package_roots_path(lua, roots)
        .with_context(|| format!("set package.path for {}", package_root.display()))?;

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

pub fn run_dept_with_roots(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    roots: &PackageRoots,
) -> Result<()> {
    run_dept_with_package_root(lua, lua_path, event, roots.package_root())
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
        let package_root = package_root_for_lua(f.path());
        run_dept_with_package_root(
            &lua,
            f.path(),
            &serde_json::json!({"foo": "bar"}),
            &package_root,
        )
        .unwrap();
        let called: i64 = lua.globals().get("called").unwrap();
        assert_eq!(called, 1);
    }

    #[test]
    fn missing_pipeline_returns_err() {
        let lua = new_lua();
        let f = write_lua("x = 1\n");
        let package_root = package_root_for_lua(f.path());
        let err = run_dept_with_package_root(&lua, f.path(), &serde_json::json!({}), &package_root)
            .unwrap_err();
        assert!(format!("{}", err).contains("pipeline"));
    }

    #[test]
    fn lua_syntax_error_returns_err() {
        let lua = new_lua();
        let f = write_lua("this is = not valid {{ lua");
        let package_root = package_root_for_lua(f.path());
        let err = run_dept_with_package_root(&lua, f.path(), &serde_json::json!({}), &package_root)
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
    fn set_package_root_path_preserves_existing_search_path() {
        let dir = TempDir::new().unwrap();
        let lua = new_lua();
        lua.load(r#"package.path = "prior/?.lua""#).exec().unwrap();

        set_package_roots_path(&lua, [dir.path()]).unwrap();

        let package: mlua::Table = lua.globals().get("package").unwrap();
        let path: String = package.get("path").unwrap();
        assert!(path.starts_with(&package_root_path(dir.path())));
        assert!(path.ends_with(";prior/?.lua"));
    }
}

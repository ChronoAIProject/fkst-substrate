//! Scoped Lua `require` resolver for manifest units.

use crate::manifest::{ModuleEntry, UnitCatalog};
use mlua::{Lua, Table, Value as LuaValue};
use std::sync::Arc;

pub(crate) const CACHE_KEY_PREFIX: &str = "\x1ffkst:";
const ENV_REGISTRY_PREFIX: &str = "fkst.require.env.";

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResolveError {
    #[error("require.denied module `{module}` not declared/visible to unit `{caller_unit}`")]
    NotDeclaredVisible { caller_unit: String, module: String },
    #[error("require.ambiguous module `{module}` visible to unit `{caller_unit}`")]
    Ambiguous { caller_unit: String, module: String },
    #[error("require.unknown-unit unit `{0}` is not in the manifest catalog")]
    UnknownUnit(String),
}

pub(crate) fn resolve(
    catalog: &UnitCatalog,
    caller_unit: &str,
    module: &str,
) -> Result<ModuleEntry, ResolveError> {
    if !catalog.contains_unit(caller_unit) {
        return Err(ResolveError::UnknownUnit(caller_unit.to_string()));
    }
    let Some(index) = catalog.module_index_for_unit(caller_unit) else {
        return Err(ResolveError::UnknownUnit(caller_unit.to_string()));
    };
    let candidates = index
        .iter()
        .filter(|(logical, _)| logical.as_str() == module)
        .map(|(_, entry)| entry.clone())
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [] => Err(ResolveError::NotDeclaredVisible {
            caller_unit: caller_unit.to_string(),
            module: module.to_string(),
        }),
        [entry] => Ok(entry.clone()),
        _ => Err(ResolveError::Ambiguous {
            caller_unit: caller_unit.to_string(),
            module: module.to_string(),
        }),
    }
}

pub(crate) fn install_scoped_require(
    lua: &Lua,
    catalog: Arc<UnitCatalog>,
    unit_id: &str,
) -> mlua::Result<mlua::Table> {
    neutralize_package_searchers(lua)?;
    unit_environment(lua, catalog, unit_id)
}

pub(crate) fn unit_environment(
    lua: &Lua,
    catalog: Arc<UnitCatalog>,
    unit_id: &str,
) -> mlua::Result<mlua::Table> {
    if !catalog.contains_unit(unit_id) {
        return Err(mlua::Error::external(ResolveError::UnknownUnit(
            unit_id.to_string(),
        )));
    }
    let registry_key = env_registry_key(unit_id);
    if let Ok(env) = lua.named_registry_value::<Table>(&registry_key) {
        return Ok(env);
    }

    neutralize_package_searchers(lua)?;
    let env = lua.create_table()?;
    let require = scoped_require_for(lua, catalog, unit_id.to_string())?;
    env.set("require", require)?;

    let metatable = lua.create_table()?;
    let globals = lua.globals();
    metatable.set("__index", globals.clone())?;
    metatable.set("__newindex", globals)?;
    env.set_metatable(Some(metatable));
    lua.set_named_registry_value(&registry_key, env.clone())?;
    Ok(env)
}

pub(crate) fn load_unit_chunk(
    lua: &Lua,
    catalog: Arc<UnitCatalog>,
    unit_id: &str,
    path: &std::path::Path,
    chunk_name: impl Into<String>,
    module_name: Option<&str>,
) -> mlua::Result<LuaValue> {
    let source = std::fs::read_to_string(path).map_err(mlua::Error::external)?;
    let env = unit_environment(lua, catalog, unit_id)?;
    let function = lua
        .load(&source)
        .set_name(chunk_name.into())
        .set_environment(env)
        .into_function()?;
    match module_name {
        Some(name) => function.call::<LuaValue>(name.to_string()),
        None => function.call::<LuaValue>(()),
    }
}

fn scoped_require_for(
    lua: &Lua,
    catalog: Arc<UnitCatalog>,
    caller_unit: String,
) -> mlua::Result<mlua::Function> {
    lua.create_function(move |lua, module: String| {
        let entry = resolve(&catalog, &caller_unit, &module).map_err(mlua::Error::external)?;
        let cache_key = canonical_cache_key(&entry.provider_unit, &module);
        let package = package_table(lua)?;
        let loaded: Table = package.get("loaded")?;
        let cached: LuaValue = loaded.raw_get(cache_key.as_str())?;
        if !matches!(cached, LuaValue::Nil) {
            return Ok(cached);
        }

        let chunk_name = format!("@{}", entry.path.display());
        let value = load_unit_chunk(
            lua,
            catalog.clone(),
            &entry.provider_unit,
            &entry.path,
            chunk_name,
            Some(&module),
        )?;
        let cached = if matches!(value, LuaValue::Nil) {
            LuaValue::Boolean(true)
        } else {
            value
        };
        loaded.raw_set(cache_key.as_str(), cached.clone())?;
        Ok(cached)
    })
}

fn canonical_cache_key(provider_unit: &str, module: &str) -> String {
    format!("{CACHE_KEY_PREFIX}{provider_unit}:{module}")
}

fn env_registry_key(unit_id: &str) -> String {
    format!("{ENV_REGISTRY_PREFIX}{unit_id}")
}

fn neutralize_package_searchers(lua: &Lua) -> mlua::Result<()> {
    let package = package_table(lua)?;
    package.set("path", "")?;
    package.set("cpath", "")?;
    let searchers = lua.create_table()?;
    searchers.set(1, engine_preload_searcher(lua)?)?;
    searchers.set(2, exact_file_searcher(lua)?)?;
    package.set("searchers", searchers)
}

fn package_table(lua: &Lua) -> mlua::Result<Table> {
    lua.globals().get("package")
}

fn engine_preload_searcher(lua: &Lua) -> mlua::Result<mlua::Function> {
    lua.create_function(|lua, name: String| {
        let package = package_table(lua)?;
        let preload: Table = package.get("preload")?;
        let loader: LuaValue = preload.raw_get(name.as_str())?;
        if matches!(loader, LuaValue::Nil) {
            Ok(LuaValue::String(lua.create_string(format!(
                "\n\tno engine preload '{name}'"
            ))?))
        } else {
            Ok(loader)
        }
    })
}

fn exact_file_searcher(lua: &Lua) -> mlua::Result<mlua::Function> {
    lua.create_function(|lua, name: String| {
        Ok(LuaValue::String(lua.create_string(format!(
            "\n\tfkst scoped require denies global searcher fallback for '{name}'"
        ))?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn workspace() -> TempDir {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("packages")).unwrap();
        fs::create_dir_all(temp.path().join("libraries")).unwrap();
        write(
            &temp.path().join("fkst.workspace.toml"),
            r#"
[workspace]
units = ["packages/*", "libraries/*"]
"#,
        );
        temp
    }

    fn package(root: &Path, name: &str, libs: &[&str]) {
        let deps = libs
            .iter()
            .map(|dep| format!(r#""{dep}""#))
            .collect::<Vec<_>>()
            .join(", ");
        write(
            &root.join(format!("packages/{name}/fkst.toml")),
            &format!(
                r#"
kind = "package"
name = "{name}"

[code]
root = "."

[lib_deps]
libraries = [{deps}]
"#
            ),
        );
    }

    fn library(root: &Path, name: &str, libs: &[&str]) {
        let deps = libs
            .iter()
            .map(|dep| format!(r#""{dep}""#))
            .collect::<Vec<_>>()
            .join(", ");
        write(
            &root.join(format!("libraries/{name}/fkst.toml")),
            &format!(
                r#"
kind = "library"
name = "{name}"

[code]
root = "."

[lib_deps]
libraries = [{deps}]

[library]
name = "{name}"
stable_id = "{name}"
version = "0.1.0"
"#
            ),
        );
    }

    fn catalog(root: &Path) -> Arc<UnitCatalog> {
        Arc::new(UnitCatalog::discover(root).unwrap().unwrap())
    }

    #[test]
    fn resolves_own_and_direct_public_modules_only() {
        let temp = workspace();
        package(temp.path(), "app", &["std"]);
        write(
            &temp.path().join("packages/app/departments/probe/main.lua"),
            "return {}\n",
        );
        library(temp.path(), "std", &[]);
        write(
            &temp.path().join("libraries/std/public/visible.lua"),
            "return {}\n",
        );
        write(
            &temp.path().join("libraries/std/private/secret.lua"),
            "return {}\n",
        );
        let catalog = catalog(temp.path());

        let own = resolve(&catalog, "app", "departments.probe.main").unwrap();
        assert_eq!(own.provider_unit, "app");
        let public = resolve(&catalog, "app", "visible").unwrap();
        assert_eq!(public.provider_unit, "std");
        let denied = resolve(&catalog, "app", "secret").unwrap_err();
        assert!(denied.to_string().contains("require.denied"));
    }

    #[test]
    fn denies_undeclared_and_transitive_library_modules() {
        let temp = workspace();
        package(temp.path(), "app", &["a"]);
        library(temp.path(), "a", &["b"]);
        write(
            &temp.path().join("libraries/a/public/a_mod.lua"),
            "return {}\n",
        );
        library(temp.path(), "b", &[]);
        write(
            &temp.path().join("libraries/b/public/b_mod.lua"),
            "return {}\n",
        );
        let catalog = catalog(temp.path());

        assert!(resolve(&catalog, "app", "a_mod").is_ok());
        let denied = resolve(&catalog, "app", "b_mod").unwrap_err();
        assert!(denied.to_string().contains("not declared/visible"));
        assert_eq!(resolve(&catalog, "a", "b_mod").unwrap().provider_unit, "b");
    }

    #[test]
    fn executes_required_module_with_provider_unit_scope() {
        let temp = workspace();
        package(temp.path(), "app", &["std"]);
        write(
            &temp.path().join("packages/app/main.lua"),
            r#"
local tool = require("tool")
return tool.value()
"#,
        );
        library(temp.path(), "std", &[]);
        write(
            &temp.path().join("libraries/std/public/tool.lua"),
            r#"
local private = require("secret")
return { value = function() return private.value end }
"#,
        );
        write(
            &temp.path().join("libraries/std/private/secret.lua"),
            r#"return { value = "provider-private" }"#,
        );
        let catalog = catalog(temp.path());
        let lua = Lua::new();

        let value: String = match load_unit_chunk(
            &lua,
            catalog,
            "app",
            &temp.path().join("packages/app/main.lua"),
            "@main.lua",
            None,
        )
        .unwrap()
        {
            LuaValue::String(value) => value.to_str().unwrap().to_string(),
            other => panic!("expected string, got {}", other.type_name()),
        };

        assert_eq!(value, "provider-private");
    }

    #[test]
    fn lazy_require_uses_loader_lexical_unit_scope() {
        let temp = workspace();
        package(temp.path(), "app", &["std"]);
        write(
            &temp.path().join("packages/app/main.lua"),
            r#"
local tool = require("tool")
return tool.lazy()
"#,
        );
        library(temp.path(), "std", &[]);
        write(
            &temp.path().join("libraries/std/public/tool.lua"),
            r#"
return {
  lazy = function()
    return require("secret").value
  end,
}
"#,
        );
        write(
            &temp.path().join("libraries/std/private/secret.lua"),
            r#"return { value = "lazy-provider-private" }"#,
        );
        let catalog = catalog(temp.path());
        let lua = Lua::new();

        let value = load_unit_chunk(
            &lua,
            catalog,
            "app",
            &temp.path().join("packages/app/main.lua"),
            "@main.lua",
            None,
        )
        .unwrap();

        assert_eq!(
            value.as_string().unwrap().to_str().unwrap(),
            "lazy-provider-private"
        );
    }

    #[test]
    fn canonical_cache_key_is_provider_unit_not_caller() {
        let temp = workspace();
        package(temp.path(), "a", &[]);
        package(temp.path(), "b", &[]);
        write(
            &temp.path().join("packages/a/main.lua"),
            r#"local state = require("state"); state.n = state.n + 1; return state.n"#,
        );
        write(
            &temp.path().join("packages/a/state.lua"),
            r#"return { n = 0 }"#,
        );
        write(
            &temp.path().join("packages/b/main.lua"),
            r#"local state = require("state"); state.n = state.n + 10; return state.n"#,
        );
        write(
            &temp.path().join("packages/b/state.lua"),
            r#"return { n = 0 }"#,
        );
        let catalog = catalog(temp.path());
        let lua = Lua::new();

        let a_first = load_unit_chunk(
            &lua,
            catalog.clone(),
            "a",
            &temp.path().join("packages/a/main.lua"),
            "@a/main.lua",
            None,
        )
        .unwrap()
        .as_integer()
        .unwrap();
        let b_first = load_unit_chunk(
            &lua,
            catalog.clone(),
            "b",
            &temp.path().join("packages/b/main.lua"),
            "@b/main.lua",
            None,
        )
        .unwrap()
        .as_integer()
        .unwrap();
        let a_second = load_unit_chunk(
            &lua,
            catalog,
            "a",
            &temp.path().join("packages/a/main.lua"),
            "@a/main.lua",
            None,
        )
        .unwrap()
        .as_integer()
        .unwrap();

        assert_eq!(a_first, 1);
        assert_eq!(b_first, 10);
        assert_eq!(a_second, 2);
    }
}

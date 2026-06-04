//! SDK: best-effort host-local scratch key-value cache.

use fkst_common::{validate_runtime_key, RuntimeKind};
use mlua::{Lua, Result};
use std::path::Path;

use crate::runtime_context;

pub fn register(lua: &Lua, host_root: &Path) -> Result<()> {
    let set_host_root = host_root.to_path_buf();
    lua.globals().set(
        "cache_set",
        lua.create_function(move |_, (key, value): (String, String)| {
            cache_set(&set_host_root, key, value)
        })?,
    )?;

    let get_host_root = host_root.to_path_buf();
    lua.globals().set(
        "cache_get",
        lua.create_function(move |_, key: String| cache_get(&get_host_root, key))?,
    )?;
    Ok(())
}

fn cache_set(host_root: &Path, key: String, value: String) -> Result<()> {
    let key = validate_runtime_key(&key).map_err(mlua::Error::external)?;
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let cache = layout.runtime_dir(RuntimeKind::Cache);
    let target = cache.join(key);
    let parent = target.parent().ok_or_else(|| {
        mlua::Error::external(anyhow::anyhow!(
            "cache target '{}' has no parent",
            target.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(mlua::Error::external)?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            mlua::Error::external(anyhow::anyhow!(
                "cache target '{}' has no file name",
                target.display()
            ))
        })?;
    let temp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&temp, value).map_err(mlua::Error::external)?;
    std::fs::rename(temp, target).map_err(mlua::Error::external)?;
    Ok(())
}

fn cache_get(host_root: &Path, key: String) -> Result<Option<String>> {
    let key = validate_runtime_key(&key).map_err(mlua::Error::external)?;
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let target = layout.runtime_dir(RuntimeKind::Cache).join(key);
    match std::fs::read_to_string(target) {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(mlua::Error::external(err)),
    }
}

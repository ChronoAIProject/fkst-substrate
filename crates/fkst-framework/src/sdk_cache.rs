//! SDK: best-effort host-local scratch key-value cache.

use fkst_common::RuntimeKind;
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
    if key.is_empty() {
        return Err(mlua::Error::external(anyhow::anyhow!(
            "cache key must not be empty"
        )));
    }

    let encoded = hex_encode(key.as_bytes());
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let cache = layout.runtime_dir(RuntimeKind::Cache);
    std::fs::create_dir_all(&cache).map_err(mlua::Error::external)?;
    let target = cache.join(&encoded);
    let temp = cache.join(format!(".{encoded}.{}.tmp", std::process::id()));
    std::fs::write(&temp, value).map_err(mlua::Error::external)?;
    std::fs::rename(temp, target).map_err(mlua::Error::external)?;
    Ok(())
}

fn cache_get(host_root: &Path, key: String) -> Result<Option<String>> {
    if key.is_empty() {
        return Err(mlua::Error::external(anyhow::anyhow!(
            "cache key must not be empty"
        )));
    }

    let encoded = hex_encode(key.as_bytes());
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let target = layout.runtime_dir(RuntimeKind::Cache).join(encoded);
    match std::fs::read_to_string(target) {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(mlua::Error::external(err)),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::hex_encode;

    #[test]
    fn hex_encode_is_lowercase_byte_encoding() {
        assert_eq!(hex_encode(b"k"), "6b");
        assert_eq!(hex_encode("a/b".as_bytes()), "612f62");
    }
}

//! SDK: `once(key, fn) -> boolean` best-effort per-key debounce marker.

use fkst_common::RuntimeKind;
use mlua::{Function, Lua, Result};
use nix::fcntl::{flock, FlockArg};
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::runtime_context;

pub fn register(lua: &Lua, host_root: &Path) -> Result<()> {
    let host_root = host_root.to_path_buf();
    lua.globals().set(
        "once",
        lua.create_function(move |_, (key, f): (String, Function)| once(&host_root, key, f))?,
    )?;
    Ok(())
}

fn once(host_root: &Path, key: String, f: Function) -> Result<bool> {
    if key.is_empty() {
        return Err(mlua::Error::external(anyhow::anyhow!(
            "once key must not be empty"
        )));
    }

    let encoded = hex_encode(key.as_bytes());
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let locks = layout.runtime_dir(RuntimeKind::Locks);
    std::fs::create_dir_all(&locks).map_err(mlua::Error::external)?;
    let lock_path = locks.join(format!("once-{encoded}"));
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(mlua::Error::external)?;

    flock(lock_file.as_raw_fd(), FlockArg::LockExclusive).map_err(mlua::Error::external)?;

    let marker = layout.runtime_dir(RuntimeKind::Marks).join(&encoded);
    if marker.exists() {
        drop(lock_file);
        return Ok(false);
    }

    let result = f.call::<()>(());
    match result {
        Ok(()) => {
            let marks = layout.runtime_dir(RuntimeKind::Marks);
            std::fs::create_dir_all(&marks).map_err(mlua::Error::external)?;
            let temp_marker = marks.join(format!(".{encoded}.tmp"));
            std::fs::write(&temp_marker, format!("{key}\n")).map_err(mlua::Error::external)?;
            std::fs::rename(temp_marker, marker).map_err(mlua::Error::external)?;
            drop(lock_file);
            Ok(true)
        }
        Err(err) => {
            drop(lock_file);
            Err(err)
        }
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

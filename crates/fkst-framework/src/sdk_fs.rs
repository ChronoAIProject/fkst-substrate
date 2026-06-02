//! SDK: file system helpers. file.read, file.write, file.exists.

use mlua::{Lua, Result};

// expose filesystem helpers through the fixed `file.*` SDK table.
pub fn register(lua: &Lua) -> Result<()> {
    let file = lua.create_table()?;
    file.set(
        "read",
        lua.create_function(|_, path: String| {
            std::fs::read_to_string(&path).map_err(mlua::Error::external)
        })?,
    )?;
    file.set(
        "write",
        lua.create_function(|_, (path, content): (String, String)| {
            std::fs::write(&path, content).map_err(mlua::Error::external)?;
            Ok(())
        })?,
    )?;
    file.set(
        "exists",
        lua.create_function(|_, path: String| Ok(std::path::Path::new(&path).exists()))?,
    )?;
    lua.globals().set("file", file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tempfile::tempdir;

    #[test]
    fn file_table_roundtrip() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("x.txt").to_string_lossy().to_string();
        lua.load(format!(r#"file.write("{}", "hello\n")"#, p))
            .exec()
            .unwrap();
        let exists: bool = lua
            .load(format!(r#"return file.exists("{}")"#, p))
            .eval()
            .unwrap();
        assert!(exists);
        let content: String = lua
            .load(format!(r#"return file.read("{}")"#, p))
            .eval()
            .unwrap();
        assert_eq!(content, "hello\n");
    }

    #[test]
    fn path_exists_false_for_missing() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let exists: bool = lua
            .load(r#"return file.exists("/no/such/path/zxcv")"#)
            .eval()
            .unwrap();
        assert!(!exists);
    }

    #[test]
    fn top_level_fs_helpers_are_not_registered() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let absent: bool = lua
            .load(
                r#"
                return read_file == nil
                    and write_file == nil
                    and path_exists == nil
                "#,
            )
            .eval()
            .unwrap();
        assert!(absent);
    }
}

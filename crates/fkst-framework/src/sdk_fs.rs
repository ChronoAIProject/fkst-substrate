//! SDK: file system helpers. file.read, file.write, file.exists, file.list.

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
    // file.list(dir): recursively enumerate the FILES (not directories) under `dir`,
    // returned as a sorted array of absolute path strings. A path that is missing or not
    // a directory yields an empty array; an IO error during the walk propagates
    // (fail-closed). Sorting makes conformance scans deterministic. This is what lets a
    // package-owned conformance function do whole-package source scans without a
    // hardcoded, stale-prone file list.
    file.set(
        "list",
        lua.create_function(|lua, dir: String| {
            let mut out: Vec<String> = Vec::new();
            let root = std::path::Path::new(&dir);
            if root.is_dir() {
                let mut stack = vec![root.to_path_buf()];
                while let Some(d) = stack.pop() {
                    for entry in std::fs::read_dir(&d).map_err(mlua::Error::external)? {
                        let entry = entry.map_err(mlua::Error::external)?;
                        let ft = entry.file_type().map_err(mlua::Error::external)?;
                        let path = entry.path();
                        if ft.is_dir() {
                            stack.push(path);
                        } else if ft.is_file() {
                            out.push(path.to_string_lossy().to_string());
                        }
                    }
                }
            }
            out.sort();
            lua.create_sequence_from(out)
        })?,
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
    fn file_list_recursive_sorted_files_only() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("b.lua"), "x").unwrap();
        std::fs::write(tmp.path().join("a.lua"), "x").unwrap();
        std::fs::write(tmp.path().join("sub/c.lua"), "x").unwrap();
        let dir = tmp.path().to_string_lossy().to_string();
        let listed: Vec<String> = lua
            .load(format!(r#"return file.list("{}")"#, dir))
            .eval()
            .unwrap();
        // files only (no "sub" dir entry), sorted, recursive (sub/c.lua present)
        assert_eq!(listed.len(), 3);
        assert!(listed[0].ends_with("a.lua"));
        assert!(listed[1].ends_with("b.lua"));
        assert!(listed[2].ends_with("sub/c.lua"));
    }

    #[test]
    fn file_list_missing_or_nondir_is_empty() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let missing: Vec<String> = lua
            .load(r#"return file.list("/no/such/dir/zxcv")"#)
            .eval()
            .unwrap();
        assert!(missing.is_empty());
        let tmp = tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, "x").unwrap();
        let nondir: Vec<String> = lua
            .load(format!(r#"return file.list("{}")"#, f.to_string_lossy()))
            .eval()
            .unwrap();
        assert!(nondir.is_empty());
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

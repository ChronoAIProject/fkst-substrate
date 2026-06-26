//! SDK: file system helpers. file.read, file.write, file.exists, file.list.

use crate::capabilities::StatelessGeneratorPolicy;
use mlua::{Lua, Result};
use std::path::{Component, Path, PathBuf};

// expose filesystem helpers through the fixed `file.*` SDK table.
pub fn register(lua: &Lua) -> Result<()> {
    register_with_policy(lua, FilePolicy::Full)
}

pub(crate) fn register_confined(
    lua: &Lua,
    owner_root: &Path,
    policy: StatelessGeneratorPolicy,
) -> Result<()> {
    let owner_root = owner_root.canonicalize().map_err(mlua::Error::external)?;
    register_with_policy(
        lua,
        FilePolicy::Confined(ConfinedFilePolicy {
            owner_root,
            read_roots: read_roots(&policy),
            output_roots: policy.output_roots,
        }),
    )
}

#[derive(Clone)]
enum FilePolicy {
    Full,
    Confined(ConfinedFilePolicy),
}

#[derive(Clone)]
struct ConfinedFilePolicy {
    owner_root: PathBuf,
    read_roots: Vec<PathBuf>,
    output_roots: Vec<PathBuf>,
}

fn register_with_policy(lua: &Lua, policy: FilePolicy) -> Result<()> {
    let file = lua.create_table()?;
    let read_policy = policy.clone();
    file.set(
        "read",
        lua.create_function(move |_, path: String| {
            let path = readable_path(&read_policy, &path)?;
            std::fs::read_to_string(&path).map_err(mlua::Error::external)
        })?,
    )?;
    let write_policy = policy.clone();
    file.set(
        "write",
        lua.create_function(move |_, (path, content): (String, String)| {
            write_file_with_policy(&write_policy, &path, content).map_err(mlua::Error::external)?;
            Ok(())
        })?,
    )?;
    let exists_policy = policy.clone();
    file.set(
        "exists",
        lua.create_function(move |_, path: String| {
            let path = existing_or_parent_confined_path(&exists_policy, &path)?;
            Ok(path.exists())
        })?,
    )?;
    // file.list(dir): recursively enumerate the FILES (not directories) under `dir`,
    // returned as a sorted array of absolute path strings. A path that is missing or not
    // a directory yields an empty array; an IO error during the walk propagates
    // (fail-closed). Sorting makes conformance scans deterministic. This is what lets a
    // package-owned conformance function do whole-package source scans without a
    // hardcoded, stale-prone file list.
    let list_policy = policy;
    file.set(
        "list",
        lua.create_function(move |lua, dir: String| {
            let mut out: Vec<String> = Vec::new();
            let root = existing_or_parent_confined_path(&list_policy, &dir)?;
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

fn read_roots(policy: &StatelessGeneratorPolicy) -> Vec<PathBuf> {
    let mut roots = policy.input_roots.clone();
    for output_root in &policy.output_roots {
        if !roots.iter().any(|root| root == output_root) {
            roots.push(output_root.clone());
        }
    }
    roots
}

fn readable_path(policy: &FilePolicy, raw: &str) -> Result<PathBuf> {
    match policy {
        FilePolicy::Full => Ok(PathBuf::from(raw)),
        FilePolicy::Confined(policy) => confined_existing_path(policy, raw, &policy.read_roots),
    }
}

fn writable_path(policy: &FilePolicy, raw: &str) -> Result<PathBuf> {
    match policy {
        FilePolicy::Full => Ok(PathBuf::from(raw)),
        FilePolicy::Confined(policy) => confined_write_path(policy, raw),
    }
}

fn existing_or_parent_confined_path(policy: &FilePolicy, raw: &str) -> Result<PathBuf> {
    match policy {
        FilePolicy::Full => Ok(PathBuf::from(raw)),
        FilePolicy::Confined(policy) => {
            let path = resolve_confined_input(policy, raw)?;
            match path.canonicalize() {
                Ok(canonical) => {
                    ensure_under_any_root(&canonical, &policy.read_roots, raw)?;
                    Ok(canonical)
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let parent = path.parent().ok_or_else(|| {
                        mlua::Error::external(format!("file path `{raw}` has no parent directory"))
                    })?;
                    let parent = parent.canonicalize().map_err(mlua::Error::external)?;
                    ensure_under_any_root(&parent, &policy.read_roots, raw)?;
                    Ok(path)
                }
                Err(err) => Err(mlua::Error::external(err)),
            }
        }
    }
}

fn confined_existing_path(
    policy: &ConfinedFilePolicy,
    raw: &str,
    roots: &[PathBuf],
) -> Result<PathBuf> {
    let path = resolve_confined_input(policy, raw)?;
    let canonical = path.canonicalize().map_err(mlua::Error::external)?;
    ensure_under_any_root(&canonical, roots, raw)?;
    Ok(canonical)
}

fn confined_write_path(policy: &ConfinedFilePolicy, raw: &str) -> Result<PathBuf> {
    let path = resolve_confined_input(policy, raw)?;
    let parent = path.parent().ok_or_else(|| {
        mlua::Error::external(format!("file.write target `{raw}` has no parent directory"))
    })?;
    let existing_parent = nearest_existing_parent(parent)?;
    ensure_under_any_root(&existing_parent, &policy.output_roots, raw)?;
    let file_name = path.file_name().ok_or_else(|| {
        mlua::Error::external(format!("file.write target `{raw}` has no file name"))
    })?;
    Ok(parent.join(file_name))
}

fn write_file_with_policy(policy: &FilePolicy, raw: &str, content: String) -> std::io::Result<()> {
    match policy {
        FilePolicy::Full => std::fs::write(raw, content),
        FilePolicy::Confined(_) => {
            let path = writable_path(policy, raw).map_err(std::io::Error::other)?;
            write_file_atomic(&path, content)
        }
    }
}

fn resolve_confined_input(policy: &ConfinedFilePolicy, raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw);
    reject_parent_components(raw, &path)?;
    let resolved = if path.is_absolute() {
        path
    } else {
        policy.owner_root.join(path)
    };
    Ok(resolved)
}

fn reject_parent_components(raw: &str, path: &Path) -> Result<()> {
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir
        )
    }) && !path.is_absolute()
    {
        return Err(mlua::Error::external(format!(
            "file path `{raw}` must not contain `..`"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(mlua::Error::external(format!(
            "file path `{raw}` must not contain `..`"
        )));
    }
    Ok(())
}

fn ensure_under_any_root(path: &Path, roots: &[PathBuf], raw: &str) -> Result<()> {
    if roots.iter().any(|root| path.starts_with(root)) {
        return Ok(());
    }
    Err(mlua::Error::external(format!(
        "file path `{raw}` is outside stateless_generator roots"
    )))
}

fn nearest_existing_parent(path: &Path) -> Result<PathBuf> {
    let mut current = path;
    loop {
        match current.canonicalize() {
            Ok(canonical) => return Ok(canonical),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                current = current.parent().ok_or_else(|| {
                    mlua::Error::external(format!(
                        "file.write parent `{}` has no existing ancestor",
                        path.display()
                    ))
                })?;
            }
            Err(err) => return Err(mlua::Error::external(err)),
        }
    }
}

fn write_file_atomic(path: &Path, content: String) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        ulid::Ulid::new()
    ));
    std::fs::write(&temp, content)?;
    std::fs::rename(temp, path)?;
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

    #[test]
    fn confined_file_write_allows_only_output_roots() {
        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("input")).unwrap();
        let policy = StatelessGeneratorPolicy {
            input_roots: vec![tmp.path().join("input").canonicalize().unwrap()],
            output_roots: vec![tmp.path().join("dist").canonicalize().unwrap_or_else(|_| {
                std::fs::create_dir(tmp.path().join("dist")).unwrap();
                tmp.path().join("dist").canonicalize().unwrap()
            })],
        };
        register_confined(&lua, tmp.path(), policy).unwrap();

        lua.load(r#"file.write("dist/nested/out.txt", "ok")"#)
            .exec()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("dist/nested/out.txt")).unwrap(),
            "ok"
        );

        let err = lua
            .load(r#"return file.write("main.lua", "bad")"#)
            .eval::<()>()
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("outside stateless_generator roots"),
            "{err}"
        );
    }

    #[test]
    fn confined_file_read_exists_and_list_are_read_root_limited() {
        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("input")).unwrap();
        std::fs::create_dir(tmp.path().join("dist")).unwrap();
        std::fs::write(tmp.path().join("input/source.txt"), "source").unwrap();
        std::fs::write(tmp.path().join("outside.txt"), "outside").unwrap();
        let policy = StatelessGeneratorPolicy {
            input_roots: vec![tmp.path().join("input").canonicalize().unwrap()],
            output_roots: vec![tmp.path().join("dist").canonicalize().unwrap()],
        };
        register_confined(&lua, tmp.path(), policy).unwrap();

        let content: String = lua
            .load(r#"return file.read("input/source.txt")"#)
            .eval()
            .unwrap();
        assert_eq!(content, "source");
        let exists: bool = lua
            .load(r#"return file.exists("input/missing.txt")"#)
            .eval()
            .unwrap();
        assert!(!exists);
        let listed: Vec<String> = lua.load(r#"return file.list("input")"#).eval().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].ends_with("source.txt"));

        let err = lua
            .load(r#"return file.read("outside.txt")"#)
            .eval::<String>()
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("outside stateless_generator roots"),
            "{err}"
        );
    }

    #[test]
    fn confined_file_paths_reject_parent_segments() {
        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dist")).unwrap();
        let policy = StatelessGeneratorPolicy {
            input_roots: Vec::new(),
            output_roots: vec![tmp.path().join("dist").canonicalize().unwrap()],
        };
        register_confined(&lua, tmp.path(), policy).unwrap();

        let err = lua
            .load(r#"return file.write("dist/../x.txt", "bad")"#)
            .eval::<()>()
            .unwrap_err();
        assert!(err.to_string().contains("must not contain `..`"), "{err}");
    }
}

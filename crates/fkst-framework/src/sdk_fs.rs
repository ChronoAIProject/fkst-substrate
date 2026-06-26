//! SDK: file system helpers. file.read, file.write, file.exists, file.list, file.mkdir.

use crate::capabilities::StatelessGeneratorPolicy;
use mlua::{Lua, Result};
use std::path::{Component, Path, PathBuf};

// expose filesystem helpers through the fixed `file.*` SDK table.
pub fn register(lua: &Lua) -> Result<()> {
    register_with_policy(lua, FilePolicy::Full)
}

pub(crate) fn register_confined(
    lua: &Lua,
    read_base_root: &Path,
    write_base_root: &Path,
    policy: StatelessGeneratorPolicy,
) -> Result<()> {
    let read_base_root = read_base_root
        .canonicalize()
        .map_err(mlua::Error::external)?;
    let write_base_root = write_base_root
        .canonicalize()
        .map_err(mlua::Error::external)?;
    register_with_policy(
        lua,
        FilePolicy::Confined(ConfinedFilePolicy {
            read_base_root,
            write_base_root,
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
    read_base_root: PathBuf,
    write_base_root: PathBuf,
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
    let mkdir_policy = policy.clone();
    file.set(
        "mkdir",
        lua.create_function(move |_, path: String| {
            mkdir_with_policy(&mkdir_policy, &path).map_err(mlua::Error::external)?;
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
                        let (path, ft) = authorize_list_child(&list_policy, entry)?;
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
        FilePolicy::Confined(policy) => {
            confined_existing_path(raw, &policy.read_base_root, &policy.read_roots)
        }
    }
}

fn writable_path(policy: &FilePolicy, raw: &str) -> Result<PathBuf> {
    match policy {
        FilePolicy::Full => Ok(PathBuf::from(raw)),
        FilePolicy::Confined(policy) => confined_write_path(policy, raw),
    }
}

fn mkdir_path(policy: &FilePolicy, raw: &str) -> Result<PathBuf> {
    match policy {
        FilePolicy::Full => Ok(PathBuf::from(raw)),
        FilePolicy::Confined(policy) => confined_mkdir_path(policy, raw),
    }
}

fn existing_or_parent_confined_path(policy: &FilePolicy, raw: &str) -> Result<PathBuf> {
    match policy {
        FilePolicy::Full => Ok(PathBuf::from(raw)),
        FilePolicy::Confined(policy) => {
            let path = resolve_confined_input(&policy.read_base_root, raw)?;
            match path.canonicalize() {
                Ok(canonical) => {
                    ensure_under_any_root(
                        &canonical,
                        &policy.read_roots,
                        raw,
                        "stateless_generator_fs_read_denied",
                    )?;
                    Ok(canonical)
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let parent = path.parent().ok_or_else(|| {
                        mlua::Error::external(format!("file path `{raw}` has no parent directory"))
                    })?;
                    let parent = parent.canonicalize().map_err(|err| {
                        mlua::Error::external(format!(
                            "stateless_generator_fs_read_denied: canonicalize parent {}: {err}",
                            parent.display()
                        ))
                    })?;
                    ensure_under_any_root(
                        &parent,
                        &policy.read_roots,
                        raw,
                        "stateless_generator_fs_read_denied",
                    )?;
                    Ok(path)
                }
                Err(err) => Err(mlua::Error::external(err)),
            }
        }
    }
}

fn authorize_list_child(
    policy: &FilePolicy,
    entry: std::fs::DirEntry,
) -> Result<(PathBuf, std::fs::FileType)> {
    match policy {
        FilePolicy::Full => {
            let ft = entry.file_type().map_err(mlua::Error::external)?;
            Ok((entry.path(), ft))
        }
        FilePolicy::Confined(policy) => {
            let path = entry.path();
            let raw = path.to_string_lossy().to_string();
            let canonical = path.canonicalize().map_err(mlua::Error::external)?;
            ensure_under_any_root(
                &canonical,
                &policy.read_roots,
                &raw,
                "stateless_generator_fs_read_denied",
            )?;
            let ft = std::fs::metadata(&canonical)
                .map_err(mlua::Error::external)?
                .file_type();
            Ok((canonical, ft))
        }
    }
}

fn confined_existing_path(raw: &str, base_root: &Path, roots: &[PathBuf]) -> Result<PathBuf> {
    let path = resolve_confined_input(base_root, raw)?;
    let canonical = path.canonicalize().map_err(mlua::Error::external)?;
    ensure_under_any_root(&canonical, roots, raw, "stateless_generator_fs_read_denied")?;
    Ok(canonical)
}

fn confined_write_path(policy: &ConfinedFilePolicy, raw: &str) -> Result<PathBuf> {
    let path = resolve_confined_input(&policy.write_base_root, raw)?;
    let parent = path.parent().ok_or_else(|| {
        mlua::Error::external(format!("file.write target `{raw}` has no parent directory"))
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        mlua::Error::external(format!("file.write target `{raw}` has no file name"))
    })?;
    let canonical_parent = ensure_creatable_parent_under(
        parent,
        &policy.output_roots,
        raw,
        "stateless_generator_fs_write_denied",
    )?;
    let target = canonical_parent.join(file_name);
    reject_final_symlink(&target, "stateless_generator_fs_write_denied")?;
    Ok(target)
}

fn confined_mkdir_path(policy: &ConfinedFilePolicy, raw: &str) -> Result<PathBuf> {
    let path = resolve_confined_input(&policy.write_base_root, raw)?;
    if let Ok(canonical) = path.canonicalize() {
        ensure_under_any_root(
            &canonical,
            &policy.output_roots,
            raw,
            "stateless_generator_fs_write_denied",
        )?;
        reject_final_symlink(&path, "stateless_generator_fs_write_denied")?;
        return Ok(canonical);
    }
    let parent = path.parent().ok_or_else(|| {
        mlua::Error::external(format!("file.mkdir target `{raw}` has no parent directory"))
    })?;
    let final_component = path.file_name().ok_or_else(|| {
        mlua::Error::external(format!("file.mkdir target `{raw}` has no final component"))
    })?;
    let canonical_parent = ensure_creatable_parent_under(
        parent,
        &policy.output_roots,
        raw,
        "stateless_generator_fs_write_denied",
    )?;
    let target = canonical_parent.join(final_component);
    reject_final_symlink(&target, "stateless_generator_fs_write_denied")?;
    Ok(target)
}

fn write_file_with_policy(policy: &FilePolicy, raw: &str, content: String) -> std::io::Result<()> {
    match policy {
        FilePolicy::Full => std::fs::write(raw, content),
        FilePolicy::Confined(_) => {
            let path = writable_path(policy, raw).map_err(std::io::Error::other)?;
            write_file_atomic_no_follow(&path, &content)
        }
    }
}

fn mkdir_with_policy(policy: &FilePolicy, raw: &str) -> std::io::Result<()> {
    match policy {
        FilePolicy::Full => std::fs::create_dir_all(raw),
        FilePolicy::Confined(_) => {
            let path = mkdir_path(policy, raw).map_err(std::io::Error::other)?;
            create_final_dir_no_symlink(&path, "stateless_generator_fs_write_denied")
                .map_err(std::io::Error::other)
        }
    }
}

fn resolve_confined_input(base_root: &Path, raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw);
    reject_parent_components(raw, &path)?;
    let resolved = if path.is_absolute() {
        path
    } else {
        base_root.join(path)
    };
    Ok(resolved)
}

fn reject_parent_components(raw: &str, path: &Path) -> Result<()> {
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

fn ensure_under_any_root(
    path: &Path,
    roots: &[PathBuf],
    raw: &str,
    error_class: &str,
) -> Result<()> {
    if roots.iter().any(|root| path.starts_with(root)) {
        return Ok(());
    }
    Err(mlua::Error::external(format!(
        "{error_class}: file path `{raw}` is outside stateless_generator roots"
    )))
}

fn ensure_creatable_parent_under(
    parent: &Path,
    roots: &[PathBuf],
    raw: &str,
    error_class: &str,
) -> Result<PathBuf> {
    let anchor = nearest_existing_parent(parent)?;
    ensure_under_any_root(&anchor, roots, raw, error_class)?;
    let mut current = anchor.clone();
    let missing = parent.strip_prefix(&anchor).map_err(|err| {
        mlua::Error::external(format!(
            "{error_class}: resolve missing path under {}: {err}",
            anchor.display()
        ))
    })?;
    for component in missing.components() {
        match component {
            Component::Normal(name) => {
                current.push(name);
                create_dir_component_no_symlink(&current, error_class)?;
            }
            Component::CurDir => {}
            _ => {
                return Err(mlua::Error::external(format!(
                    "{error_class}: invalid path component in {}",
                    parent.display()
                )));
            }
        }
    }

    let canonical_parent = current.canonicalize().map_err(|err| {
        mlua::Error::external(format!(
            "{error_class}: canonicalize parent {}: {err}",
            current.display()
        ))
    })?;
    ensure_under_any_root(&canonical_parent, roots, raw, error_class)?;
    Ok(canonical_parent)
}

fn nearest_existing_parent(path: &Path) -> Result<PathBuf> {
    let mut current = path;
    loop {
        match current.canonicalize() {
            Ok(canonical) => return Ok(canonical),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                current = current.parent().ok_or_else(|| {
                    mlua::Error::external(format!(
                        "file path `{}` has no existing ancestor",
                        path.display()
                    ))
                })?;
            }
            Err(err) => return Err(mlua::Error::external(err)),
        }
    }
}

fn create_dir_component_no_symlink(path: &Path, error_class: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(mlua::Error::external(format!(
            "{error_class}: symlink path component denied: {}",
            path.display()
        ))),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(mlua::Error::external(format!(
            "{error_class}: path component is not a directory: {}",
            path.display()
        ))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => match std::fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                create_dir_component_no_symlink(path, error_class)
            }
            Err(err) => Err(mlua::Error::external(format!(
                "{error_class}: create directory {}: {err}",
                path.display()
            ))),
        },
        Err(err) => Err(mlua::Error::external(format!(
            "{error_class}: inspect directory {}: {err}",
            path.display()
        ))),
    }
}

fn create_final_dir_no_symlink(path: &Path, error_class: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(mlua::Error::external(format!(
            "{error_class}: final path symlink denied: {}",
            path.display()
        ))),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(mlua::Error::external(format!(
            "{error_class}: final path is not a directory: {}",
            path.display()
        ))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => match std::fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                create_final_dir_no_symlink(path, error_class)
            }
            Err(err) => Err(mlua::Error::external(format!(
                "{error_class}: create directory {}: {err}",
                path.display()
            ))),
        },
        Err(err) => Err(mlua::Error::external(format!(
            "{error_class}: inspect path {}: {err}",
            path.display()
        ))),
    }
}

fn reject_final_symlink(path: &Path, error_class: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(mlua::Error::external(format!(
            "{error_class}: final path symlink denied: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(mlua::Error::external(format!(
            "{error_class}: inspect path {}: {err}",
            path.display()
        ))),
    }
}

fn write_file_atomic_no_follow(path: &Path, content: &str) -> std::io::Result<()> {
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
    let write_result =
        write_file_no_follow(&temp, content.as_bytes()).and_then(|()| std::fs::rename(&temp, path));
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    write_result
}

fn write_file_no_follow(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(content)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tempfile::tempdir;

    fn confined_policy(root: &Path, outputs: &[&str], inputs: &[&str]) -> StatelessGeneratorPolicy {
        StatelessGeneratorPolicy {
            input_roots: inputs
                .iter()
                .map(|path| root.join(path).canonicalize().unwrap())
                .collect(),
            output_roots: outputs
                .iter()
                .map(|path| root.join(path).canonicalize().unwrap())
                .collect(),
        }
    }

    #[test]
    fn file_table_roundtrip() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("x.txt").to_string_lossy().to_string();
        lua.load(format!(r#"file.write("{}", "hello\n")"#, p))
            .exec()
            .unwrap();
        let dir = tmp.path().join("nested").to_string_lossy().to_string();
        lua.load(format!(r#"file.mkdir("{}")"#, dir))
            .exec()
            .unwrap();
        assert!(tmp.path().join("nested").is_dir());
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
        std::fs::create_dir(tmp.path().join("dist")).unwrap();
        let policy = confined_policy(tmp.path(), &["dist"], &["input"]);
        register_confined(&lua, tmp.path(), tmp.path(), policy).unwrap();

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
        let msg = err.to_string();
        assert!(msg.contains("stateless_generator_fs_write_denied"), "{msg}");
        assert!(msg.contains("outside stateless_generator roots"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn confined_file_write_rejects_final_symlink_escape() {
        use std::os::unix::fs::symlink;

        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("dist")).unwrap();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        symlink(
            tmp.path().join("outside/escaped.txt"),
            tmp.path().join("dist/out.txt"),
        )
        .unwrap();
        let policy = confined_policy(tmp.path(), &["dist"], &[]);
        register_confined(&lua, tmp.path(), tmp.path(), policy).unwrap();

        let err = lua
            .load(r#"file.write("dist/out.txt", "escape")"#)
            .exec()
            .unwrap_err()
            .to_string();

        assert!(err.contains("stateless_generator_fs_write_denied"), "{err}");
        assert!(!tmp.path().join("outside/escaped.txt").exists());
        assert!(std::fs::symlink_metadata(tmp.path().join("dist/out.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn confined_file_mkdir_is_limited_to_output_root() {
        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("dist")).unwrap();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        let policy = confined_policy(tmp.path(), &["dist"], &[]);
        register_confined(&lua, tmp.path(), tmp.path(), policy).unwrap();

        lua.load(r#"file.mkdir("dist/assets/css")"#).exec().unwrap();
        assert!(tmp.path().join("dist/assets/css").is_dir());

        let err = lua
            .load(r#"file.mkdir("outside/assets")"#)
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains("stateless_generator_fs_write_denied"), "{err}");
    }

    #[test]
    fn confined_file_read_exists_and_list_are_read_root_limited() {
        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("input")).unwrap();
        std::fs::create_dir(tmp.path().join("dist")).unwrap();
        std::fs::write(tmp.path().join("input/source.txt"), "source").unwrap();
        std::fs::write(tmp.path().join("outside.txt"), "outside").unwrap();
        let policy = confined_policy(tmp.path(), &["dist"], &["input"]);
        register_confined(&lua, tmp.path(), tmp.path(), policy).unwrap();

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

    #[cfg(unix)]
    #[test]
    fn confined_file_list_rejects_symlinked_child_outside_read_roots() {
        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("input")).unwrap();
        std::fs::create_dir(tmp.path().join("dist")).unwrap();
        std::fs::create_dir(tmp.path().join("outside")).unwrap();
        std::fs::write(tmp.path().join("input/source.txt"), "source").unwrap();
        std::fs::write(tmp.path().join("outside/secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("outside"),
            tmp.path().join("input/outside-link"),
        )
        .unwrap();
        let policy = confined_policy(tmp.path(), &["dist"], &["input"]);
        register_confined(&lua, tmp.path(), tmp.path(), policy).unwrap();

        let err = lua
            .load(r#"return file.list("input")"#)
            .eval::<Vec<String>>()
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
        let policy = confined_policy(tmp.path(), &["dist"], &[]);
        register_confined(&lua, tmp.path(), tmp.path(), policy).unwrap();

        let err = lua
            .load(r#"return file.write("dist/../x.txt", "bad")"#)
            .eval::<()>()
            .unwrap_err();
        assert!(err.to_string().contains("must not contain `..`"), "{err}");
    }
}

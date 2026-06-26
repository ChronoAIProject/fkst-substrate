//! SDK: file system helpers. file.read, file.write, file.exists, file.list, file.mkdir.

use mlua::{Lua, Result};
use std::path::{Component, Path, PathBuf};

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
        "mkdir",
        lua.create_function(|_, path: String| {
            std::fs::create_dir_all(&path).map_err(mlua::Error::external)?;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FsPolicy {
    owner_root: PathBuf,
    output_roots: Vec<PathBuf>,
    input_roots: Vec<PathBuf>,
}

impl FsPolicy {
    pub(crate) fn new(
        owner_root: &Path,
        output_roots: Vec<PathBuf>,
        input_roots: Vec<PathBuf>,
    ) -> Result<Self> {
        let owner_root = owner_root.canonicalize().map_err(mlua::Error::external)?;
        let output_roots = canonical_policy_roots(&owner_root, output_roots, "output_roots")?;
        let input_roots = canonical_policy_roots(&owner_root, input_roots, "input_roots")?;
        Ok(Self {
            owner_root,
            output_roots,
            input_roots,
        })
    }

    fn read_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.input_roots.clone();
        roots.extend(self.output_roots.clone());
        roots
    }
}

pub(crate) fn register_with_policy(lua: &Lua, policy: FsPolicy) -> Result<()> {
    let file = lua.create_table()?;

    let read_policy = policy.clone();
    file.set(
        "read",
        lua.create_function(move |_, path: String| {
            let path = read_policy.require_read_file_path(&path)?;
            std::fs::read_to_string(&path).map_err(mlua::Error::external)
        })?,
    )?;

    let write_policy = policy.clone();
    file.set(
        "write",
        lua.create_function(move |_, (path, content): (String, String)| {
            let path = write_policy.require_write_path(&path)?;
            write_file_no_follow(&path, content.as_bytes()).map_err(|err| {
                mlua::Error::external(format!(
                    "stateless_generator_fs_write_denied: write {}: {err}",
                    path.display()
                ))
            })?;
            Ok(())
        })?,
    )?;

    let mkdir_policy = policy.clone();
    file.set(
        "mkdir",
        lua.create_function(move |_, path: String| {
            let path = mkdir_policy.require_mkdir_path(&path)?;
            create_final_dir_no_symlink(&path, "stateless_generator_fs_write_denied")?;
            Ok(())
        })?,
    )?;

    let exists_policy = policy.clone();
    file.set(
        "exists",
        lua.create_function(move |_, path: String| {
            let path = exists_policy.require_existing_or_parent_path(&path)?;
            Ok(path.exists())
        })?,
    )?;

    let list_policy = policy;
    file.set(
        "list",
        lua.create_function(move |lua, dir: String| {
            let root = list_policy.require_list_path(&dir)?;
            let mut out: Vec<String> = Vec::new();
            if root.is_dir() {
                let mut stack = vec![root];
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

impl FsPolicy {
    fn require_read_file_path(&self, raw: &str) -> Result<PathBuf> {
        self.require_existing_or_parent_path_with_class(raw, "stateless_generator_fs_read_denied")
    }

    fn require_existing_or_parent_path(&self, raw: &str) -> Result<PathBuf> {
        self.require_existing_or_parent_path_with_class(raw, "stateless_generator_fs_read_denied")
    }

    fn require_existing_or_parent_path_with_class(
        &self,
        raw: &str,
        error_class: &str,
    ) -> Result<PathBuf> {
        let roots = self.read_roots();
        self.require_existing_or_parent_path_under(raw, &roots, error_class)
    }

    fn require_list_path(&self, raw: &str) -> Result<PathBuf> {
        let roots = self.read_roots();
        let path = resolve_policy_path(&self.owner_root, raw)?;
        reject_parent_component(&path, "stateless_generator_fs_read_denied")?;
        if let Ok(canonical) = path.canonicalize() {
            if roots.iter().any(|root| canonical.starts_with(root)) {
                return Ok(path);
            }
            return Err(mlua::Error::external(format!(
                "stateless_generator_fs_read_denied: {} is outside configured generator roots",
                path.display()
            )));
        }
        self.require_existing_or_parent_path_under(
            raw,
            &roots,
            "stateless_generator_fs_read_denied",
        )
    }

    fn require_write_path(&self, raw: &str) -> Result<PathBuf> {
        self.require_creatable_path_parent_under(
            raw,
            &self.output_roots,
            "stateless_generator_fs_write_denied",
        )
    }

    fn require_mkdir_path(&self, raw: &str) -> Result<PathBuf> {
        self.require_creatable_path_parent_under(
            raw,
            &self.output_roots,
            "stateless_generator_fs_write_denied",
        )
    }

    fn require_existing_or_parent_path_under(
        &self,
        raw: &str,
        roots: &[PathBuf],
        error_class: &str,
    ) -> Result<PathBuf> {
        let path = resolve_policy_path(&self.owner_root, raw)?;
        reject_parent_component(&path, error_class)?;
        if let Ok(canonical) = path.canonicalize() {
            if roots.iter().any(|root| canonical.starts_with(root)) {
                return Ok(path);
            }
            return Err(mlua::Error::external(format!(
                "{error_class}: {} is outside configured generator roots",
                path.display()
            )));
        }
        self.require_existing_parent_path_under(raw, roots, error_class)
    }

    fn require_existing_parent_path_under(
        &self,
        raw: &str,
        roots: &[PathBuf],
        error_class: &str,
    ) -> Result<PathBuf> {
        let path = resolve_policy_path(&self.owner_root, raw)?;
        reject_parent_component(&path, error_class)?;
        let parent = path.parent().ok_or_else(|| {
            mlua::Error::external(format!(
                "{error_class}: path has no parent: {}",
                path.display()
            ))
        })?;
        let canonical_parent = parent.canonicalize().map_err(|err| {
            mlua::Error::external(format!(
                "{error_class}: canonicalize parent {}: {err}",
                parent.display()
            ))
        })?;
        if roots.iter().any(|root| canonical_parent.starts_with(root)) {
            Ok(path)
        } else {
            Err(mlua::Error::external(format!(
                "{error_class}: {} is outside configured generator roots",
                path.display()
            )))
        }
    }

    fn require_creatable_path_parent_under(
        &self,
        raw: &str,
        roots: &[PathBuf],
        error_class: &str,
    ) -> Result<PathBuf> {
        let path = resolve_policy_path(&self.owner_root, raw)?;
        reject_parent_component(&path, error_class)?;
        let Some(parent) = path.parent() else {
            return Err(mlua::Error::external(format!(
                "{error_class}: path has no parent: {}",
                path.display()
            )));
        };
        let Some(final_component) = path.file_name() else {
            return Err(mlua::Error::external(format!(
                "{error_class}: path has no final component: {}",
                path.display()
            )));
        };
        let canonical_parent = self.ensure_creatable_parent_under(parent, roots, error_class)?;
        let target = canonical_parent.join(final_component);
        reject_final_symlink(&target, error_class)?;
        Ok(target)
    }

    fn ensure_creatable_parent_under(
        &self,
        parent: &Path,
        roots: &[PathBuf],
        error_class: &str,
    ) -> Result<PathBuf> {
        let anchor = existing_ancestor(parent);
        let canonical_anchor = anchor.canonicalize().map_err(|err| {
            mlua::Error::external(format!(
                "{error_class}: canonicalize existing ancestor for {}: {err}",
                parent.display()
            ))
        })?;
        if !roots.iter().any(|root| canonical_anchor.starts_with(root)) {
            return Err(mlua::Error::external(format!(
                "{error_class}: {} is outside configured generator roots",
                parent.display()
            )));
        }

        let mut current = canonical_anchor;
        let missing = parent.strip_prefix(anchor).map_err(|err| {
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
        if roots.iter().any(|root| canonical_parent.starts_with(root)) {
            Ok(canonical_parent)
        } else {
            Err(mlua::Error::external(format!(
                "{error_class}: {} is outside configured generator roots",
                parent.display()
            )))
        }
    }
}

fn existing_ancestor(path: &Path) -> &Path {
    let mut current = path;
    while !current.exists() {
        match current.parent() {
            Some(parent) => current = parent,
            None => return current,
        }
    }
    current
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

fn write_file_no_follow(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
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

fn canonical_policy_roots(
    owner_root: &Path,
    roots: Vec<PathBuf>,
    label: &str,
) -> Result<Vec<PathBuf>> {
    roots
        .into_iter()
        .map(|root| {
            let resolved = resolve_policy_path(owner_root, &root.to_string_lossy())?;
            reject_parent_component(
                &resolved,
                &format!("stateless_generator_fs_policy_invalid_{label}"),
            )?;
            resolved.canonicalize().map_err(|err| {
                mlua::Error::external(format!(
                    "stateless_generator_fs_policy_invalid_{label}: canonicalize {}: {err}",
                    resolved.display()
                ))
            })
        })
        .collect()
}

fn resolve_policy_path(owner_root: &Path, raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        owner_root.join(path)
    })
}

fn reject_parent_component(path: &Path, error_class: &str) -> Result<()> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(mlua::Error::external(format!(
            "{error_class}: path must not contain `..`: {}",
            path.display()
        )));
    }
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
    fn confined_file_write_creates_parents_atomically_under_output_root() {
        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("generated")).unwrap();
        let policy =
            FsPolicy::new(tmp.path(), vec![PathBuf::from("generated")], Vec::new()).unwrap();
        register_with_policy(&lua, policy).unwrap();

        lua.load(r#"file.write("generated/deep/site/index.html", "ok")"#)
            .exec()
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("generated/deep/site/index.html")).unwrap(),
            "ok"
        );
    }

    #[cfg(unix)]
    #[test]
    fn confined_file_write_rejects_final_symlink_escape() {
        use std::os::unix::fs::symlink;

        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("generated")).unwrap();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        symlink(
            tmp.path().join("outside/escaped.txt"),
            tmp.path().join("generated/out.txt"),
        )
        .unwrap();
        let policy =
            FsPolicy::new(tmp.path(), vec![PathBuf::from("generated")], Vec::new()).unwrap();
        register_with_policy(&lua, policy).unwrap();

        let err = lua
            .load(r#"file.write("generated/out.txt", "escape")"#)
            .exec()
            .unwrap_err()
            .to_string();

        assert!(err.contains("stateless_generator_fs_write_denied"), "{err}");
        assert!(!tmp.path().join("outside/escaped.txt").exists());
        assert!(
            std::fs::symlink_metadata(tmp.path().join("generated/out.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn confined_file_mkdir_is_limited_to_output_root() {
        let lua = Lua::new();
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("generated")).unwrap();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        let policy =
            FsPolicy::new(tmp.path(), vec![PathBuf::from("generated")], Vec::new()).unwrap();
        register_with_policy(&lua, policy).unwrap();

        lua.load(r#"file.mkdir("generated/assets/css")"#)
            .exec()
            .unwrap();
        assert!(tmp.path().join("generated/assets/css").is_dir());

        let err = lua
            .load(r#"file.mkdir("outside/assets")"#)
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains("stateless_generator_fs_write_denied"), "{err}");
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

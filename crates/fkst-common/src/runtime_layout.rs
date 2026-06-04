//! Runtime path resolver for host-safe fkst state.

use anyhow::{anyhow, Result};
use std::path::{Component, Path, PathBuf};

pub const RUNTIME_ROOT_ENV: &str = "FKST_RUNTIME_ROOT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// runtime path categories are explicit and bounded before path construction.
pub enum RuntimeKind {
    Worktrees,
    CodexPermits,
    Locks,
    Logs,
    Marks,
    Cache,
}

#[derive(Debug, Clone, PartialEq, Eq)]
// runtime writers share one validated root object.
pub struct RuntimeLayout {
    root: PathBuf,
}

impl RuntimeKind {
    fn dir_name(self) -> &'static str {
        match self {
            Self::Worktrees => "worktrees",
            Self::CodexPermits => "codex-permits",
            Self::Locks => "locks",
            Self::Logs => "logs",
            Self::Marks => "marks",
            Self::Cache => "cache",
        }
    }
}

impl RuntimeLayout {
    // RuntimeLayout consumes a process contract produced before Rust starts.
    pub fn from_env() -> Result<Self> {
        let root = std::env::var_os(RUNTIME_ROOT_ENV)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("{RUNTIME_ROOT_ENV} must be set"))?;
        Self::new(root)
    }

    // each root is validated before it can construct runtime paths.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        reject_traversal(&root)?;
        Ok(Self { root })
    }

    // callers read the canonical base from RuntimeLayout.
    pub fn runtime_root(&self) -> &Path {
        &self.root
    }

    // RuntimeLayout owns kind-to-directory mapping.
    pub fn runtime_dir(&self, kind: RuntimeKind) -> PathBuf {
        self.root.join(kind.dir_name())
    }
}

fn reject_traversal(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(anyhow!("runtime root must not be empty"));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(anyhow!("runtime root must not contain parent traversal"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old }
        }

        fn unset(key: &'static str) -> Self {
            let old = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(old) = self.old.take() {
                std::env::set_var(self.key, old);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn missing_runtime_root_fails_closed() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _root = EnvGuard::unset(RUNTIME_ROOT_ENV);
        let err = RuntimeLayout::from_env().unwrap_err();
        assert!(format!("{err:#}").contains("FKST_RUNTIME_ROOT must be set"));
    }

    #[test]
    fn empty_runtime_root_fails_closed() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _root = EnvGuard::set(RUNTIME_ROOT_ENV, "");
        let err = RuntimeLayout::from_env().unwrap_err();
        assert!(format!("{err:#}").contains("FKST_RUNTIME_ROOT must be set"));
    }

    #[test]
    fn explicit_in_repo_root_is_allowed() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _root = EnvGuard::set(RUNTIME_ROOT_ENV, ".fkst/custom-runtime");
        let layout = RuntimeLayout::from_env().unwrap();
        assert_eq!(
            layout.runtime_dir(RuntimeKind::Logs),
            PathBuf::from(".fkst/custom-runtime/logs")
        );
    }

    #[test]
    fn explicit_out_of_tree_root_is_allowed() {
        let layout = RuntimeLayout::new("/tmp/fkst-runtime/repo-a").unwrap();
        assert_eq!(
            layout.runtime_dir(RuntimeKind::Worktrees),
            PathBuf::from("/tmp/fkst-runtime/repo-a/worktrees")
        );
    }

    #[test]
    fn traversal_is_rejected() {
        assert!(RuntimeLayout::new("../runtime").is_err());
    }
}

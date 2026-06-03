//! Runtime path resolver for host-safe fkst state.

use anyhow::{anyhow, Result};
use std::path::{Component, Path, PathBuf};

pub const RUNTIME_ROOT_ENV: &str = "FKST_RUNTIME_ROOT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// runtime path categories are explicit and bounded before path construction.
pub enum RuntimeKind {
    Pipeline,
    Mailbox,
    Worktrees,
    CodexPermits,
    Locks,
    Logs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
// runtime writers share one validated root object.
pub struct RuntimeLayout {
    root: PathBuf,
}

impl RuntimeKind {
    // external kind strings enter through one checked parser.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "pipeline" => Ok(Self::Pipeline),
            "mailbox" => Ok(Self::Mailbox),
            "worktrees" => Ok(Self::Worktrees),
            "codex_permits" | "codex-permits" => Ok(Self::CodexPermits),
            "locks" => Ok(Self::Locks),
            "logs" => Ok(Self::Logs),
            _ => Err(anyhow!("unknown runtime kind: {value}")),
        }
    }

    fn dir_name(self) -> &'static str {
        match self {
            Self::Pipeline => "pipeline",
            Self::Mailbox => "mailbox",
            Self::Worktrees => "worktrees",
            Self::CodexPermits => "codex-permits",
            Self::Locks => "locks",
            Self::Logs => "logs",
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

    // child paths are rejected if they escape the selected runtime kind.
    pub fn runtime_path(&self, kind: RuntimeKind, relative: impl AsRef<Path>) -> Result<PathBuf> {
        let relative = relative.as_ref();
        reject_relative_fragment(relative)?;
        Ok(self.runtime_dir(kind).join(relative))
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

fn reject_relative_fragment(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(anyhow!(
                    "runtime relative path must stay below its kind directory"
                ))
            }
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
            layout
                .runtime_path(RuntimeKind::Mailbox, "threads/a")
                .unwrap(),
            PathBuf::from(".fkst/custom-runtime/mailbox/threads/a")
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
        let layout = RuntimeLayout::new(".fkst/runtime").unwrap();
        assert!(layout.runtime_path(RuntimeKind::Pipeline, "../x").is_err());
        assert!(layout
            .runtime_path(RuntimeKind::Pipeline, "/tmp/x")
            .is_err());
    }

    #[test]
    fn unknown_kind_is_rejected() {
        assert!(RuntimeKind::parse("unknown").is_err());
    }
}

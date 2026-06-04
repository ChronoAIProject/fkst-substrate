//! Durable delivery path resolver.

use anyhow::{anyhow, Result};
use std::path::{Component, Path, PathBuf};

pub const DURABLE_ROOT_ENV: &str = "FKST_DURABLE_ROOT";

#[derive(Debug, Clone, PartialEq, Eq)]
// durable writers share one validated root object.
pub struct DurableLayout {
    root: PathBuf,
}

impl DurableLayout {
    // DurableLayout is only required when reliable delivery is enabled.
    pub fn from_env() -> Result<Self> {
        let root = std::env::var_os(DURABLE_ROOT_ENV)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("{DURABLE_ROOT_ENV} must be set"))?;
        Self::new(root)
    }

    // each root is validated before it can construct durable paths.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        reject_traversal(&root)?;
        Ok(Self { root })
    }

    // callers read the canonical base from DurableLayout.
    pub fn durable_root(&self) -> &Path {
        &self.root
    }

    // DurableLayout owns the delivery database location.
    pub fn delivery_db_path(&self) -> PathBuf {
        self.root.join("delivery.redb")
    }
}

fn reject_traversal(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(anyhow!("durable root must not be empty"));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(anyhow!("durable root must not contain parent traversal"));
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
    fn missing_durable_root_fails_closed() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _root = EnvGuard::unset(DURABLE_ROOT_ENV);
        let err = DurableLayout::from_env().unwrap_err();
        assert!(format!("{err:#}").contains("FKST_DURABLE_ROOT must be set"));
    }

    #[test]
    fn empty_durable_root_fails_closed() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _root = EnvGuard::set(DURABLE_ROOT_ENV, "");
        let err = DurableLayout::from_env().unwrap_err();
        assert!(format!("{err:#}").contains("FKST_DURABLE_ROOT must be set"));
    }

    #[test]
    fn delivery_database_path_is_under_durable_root() {
        let layout = DurableLayout::new("/tmp/fkst-durable/repo-a").unwrap();
        assert_eq!(
            layout.delivery_db_path(),
            PathBuf::from("/tmp/fkst-durable/repo-a/delivery.redb")
        );
    }

    #[test]
    fn traversal_is_rejected() {
        assert!(DurableLayout::new("../durable").is_err());
    }
}

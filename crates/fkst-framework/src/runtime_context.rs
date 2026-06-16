use anyhow::Result;
use fkst_common::RuntimeLayout;
use std::path::Path;

pub(crate) fn layout_from_host_root(host_root: &Path) -> Result<RuntimeLayout> {
    let layout = RuntimeLayout::from_env()?;
    if layout.runtime_root().is_absolute() {
        return Ok(layout);
    }
    RuntimeLayout::new(stable_host_root(host_root).join(layout.runtime_root()))
}

fn stable_host_root(host_root: &Path) -> &Path {
    host_root
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .unwrap_or(host_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn relative_runtime_root_anchors_to_enclosing_git_root() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let package = repo.join("packages/pkg");
        fs::create_dir_all(&package).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();

        assert_eq!(stable_host_root(&package), repo.as_path());
    }

    #[test]
    fn relative_runtime_root_falls_back_to_host_root_without_git_root() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("host");
        fs::create_dir_all(&host).unwrap();

        assert_eq!(stable_host_root(&host), host.as_path());
    }
}

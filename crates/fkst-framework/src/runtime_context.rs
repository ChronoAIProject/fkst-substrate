use anyhow::Result;
use fkst_common::RuntimeLayout;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub(crate) fn layout_from_host_root(host_root: &Path) -> Result<RuntimeLayout> {
    let layout = RuntimeLayout::from_env()?;
    if layout.runtime_root().is_absolute() {
        return Ok(layout);
    }
    RuntimeLayout::new(stable_host_root(host_root).join(layout.runtime_root()))
}

fn stable_host_root(host_root: &Path) -> PathBuf {
    git_toplevel(host_root).unwrap_or_else(|| host_root.to_path_buf())
}

fn git_toplevel(host_root: &Path) -> Option<PathBuf> {
    git_stdout(host_root, ["rev-parse", "--show-toplevel"])?;
    let prefix = git_stdout(host_root, ["rev-parse", "--show-prefix"])?;
    if prefix.is_empty() {
        return Some(host_root.to_path_buf());
    }
    strip_clean_suffix(host_root, Path::new(&prefix))
}

fn strip_clean_suffix(path: &Path, suffix: &Path) -> Option<PathBuf> {
    let mut path = path.to_path_buf();
    for component in suffix.components() {
        match component {
            Component::Normal(_) => {
                if !path.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::ParentDir => return None,
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(path)
}

fn git_stdout<const N: usize>(host_root: &Path, args: [&str; N]) -> Option<String> {
    Command::new(git_program())
        .arg("-C")
        .arg(host_root)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| stdout.trim().to_string())
}

#[cfg(not(test))]
fn git_program() -> PathBuf {
    PathBuf::from("git")
}

#[cfg(test)]
fn git_program() -> PathBuf {
    static GIT_BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    GIT_BIN
        .get_or_init(|| {
            [
                "/usr/bin/git",
                "/opt/homebrew/bin/git",
                "/usr/local/bin/git",
            ]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .expect("test host must provide git at a standard absolute path")
        })
        .clone()
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
        init_git_repo(&repo);

        assert_eq!(stable_host_root(&package), repo);
    }

    #[test]
    fn relative_runtime_root_anchors_to_git_worktree_toplevel() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let worktree = root.path().join("linked-worktree");
        init_git_repo(&repo);
        git(
            &repo,
            ["worktree", "add", "--detach", worktree.to_str().unwrap()],
        );
        let package = worktree.join("packages/pkg");
        fs::create_dir_all(&package).unwrap();

        assert_eq!(stable_host_root(&package), worktree);
        assert!(package
            .ancestors()
            .all(|ancestor| !ancestor.join(".git").is_dir()));
    }

    #[test]
    fn relative_runtime_root_anchors_to_git_submodule_toplevel() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let submodule_source = root.path().join("submodule-source");
        let submodule = repo.join("vendor/submodule");
        init_git_repo(&repo);
        init_git_repo(&submodule_source);
        git(
            &repo,
            [
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                fs::canonicalize(&submodule_source)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "vendor/submodule",
            ],
        );
        let package = submodule.join("packages/pkg");
        fs::create_dir_all(&package).unwrap();

        assert_eq!(
            fs::canonicalize(stable_host_root(&package)).unwrap(),
            fs::canonicalize(&submodule).unwrap()
        );
        assert!(submodule.join(".git").is_file());
        assert!(!submodule.join(".git").is_dir());
    }

    #[test]
    fn relative_runtime_root_falls_back_to_host_root_without_git_root() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("host");
        fs::create_dir_all(&host).unwrap();

        assert_eq!(stable_host_root(&host), host);
    }

    fn init_git_repo(repo: &Path) {
        fs::create_dir_all(repo).unwrap();
        git(repo, ["init"]);
        git(repo, ["config", "user.email", "test@example.com"]);
        git(repo, ["config", "user.name", "Test User"]);
        fs::write(repo.join("README.md"), "test\n").unwrap();
        git(repo, ["add", "README.md"]);
        git(repo, ["commit", "-m", "initial"]);
    }

    fn git<const N: usize>(repo: &Path, args: [&str; N]) {
        let output = Command::new(git_program())
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

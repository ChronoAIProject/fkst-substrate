//! Engine and graph-root provenance stamps for logs and command audit records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{OnceLock, RwLock};

const HOST_NAMESPACE: &str = "host";
static STATE: OnceLock<RwLock<ProvenanceState>> = OnceLock::new();

#[derive(Clone, Debug)]
struct ProvenanceState {
    engine_ver: &'static str,
    pkg_versions: BTreeMap<String, String>,
    pkg_ver: String,
}

impl Default for ProvenanceState {
    fn default() -> Self {
        Self {
            engine_ver: engine_version(),
            pkg_versions: BTreeMap::new(),
            pkg_ver: "unknown".to_string(),
        }
    }
}

pub fn engine_version() -> &'static str {
    env!("FKST_ENGINE_GIT_VERSION")
}

pub fn read_git_version(root: &Path) -> String {
    let Some(head) = git_stdout(root, ["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".to_string();
    };
    let dirty = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("status")
        .arg("--porcelain")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        format!("{head}-dirty")
    } else {
        head
    }
}

pub(crate) fn install_supervise(package_roots: &[PathBuf], host_root: &Path) {
    let mut versions = BTreeMap::new();
    for root in package_roots {
        versions.insert(root_namespace(root), read_git_version(root));
    }
    versions.insert(HOST_NAMESPACE.to_string(), read_git_version(host_root));
    let pkg_ver = summarize_versions(&versions);
    install_state(ProvenanceState {
        engine_ver: engine_version(),
        pkg_versions: versions,
        pkg_ver,
    });
}

pub(crate) fn install_run(owner_root: &Path, owner_namespace: &str) {
    let pkg_ver = read_git_version(owner_root);
    let versions = BTreeMap::from([(owner_namespace.to_string(), pkg_ver.clone())]);
    install_state(ProvenanceState {
        engine_ver: engine_version(),
        pkg_versions: versions,
        pkg_ver,
    });
}

pub(crate) fn current_engine_ver() -> String {
    state()
        .read()
        .map(|state| state.engine_ver.to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

pub(crate) fn current_pkg_ver() -> String {
    state()
        .read()
        .map(|state| state.pkg_ver.clone())
        .unwrap_or_else(|_| "unknown".to_string())
}

pub(crate) fn current_pkg_versions() -> BTreeMap<String, String> {
    state()
        .read()
        .map(|state| state.pkg_versions.clone())
        .unwrap_or_default()
}

pub(crate) fn current_pkg_versions_summary() -> String {
    summarize_versions(&current_pkg_versions())
}

pub(crate) fn pkg_ver_for_namespace(namespace: &str) -> String {
    state()
        .read()
        .ok()
        .and_then(|state| state.pkg_versions.get(namespace).cloned())
        .unwrap_or_else(|| current_pkg_ver())
}

fn install_state(next: ProvenanceState) {
    set_env(&next);
    let mut state = state()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *state = next;
}

fn set_env(state: &ProvenanceState) {
    std::env::set_var("FKST_ENGINE_VER", state.engine_ver);
    std::env::set_var("FKST_PKG_VER", &state.pkg_ver);
    std::env::set_var("FKST_PKG_VERS", summarize_versions(&state.pkg_versions));
}

fn state() -> &'static RwLock<ProvenanceState> {
    STATE.get_or_init(|| RwLock::new(ProvenanceState::default()))
}

fn summarize_versions(versions: &BTreeMap<String, String>) -> String {
    if versions.is_empty() {
        return "unknown".to_string();
    }
    versions
        .iter()
        .map(|(namespace, version)| format!("{namespace}@{version}"))
        .collect::<Vec<_>>()
        .join(";")
}

fn root_namespace(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn git_stdout<const N: usize>(root: &Path, args: [&str; N]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| stdout.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_before_install() {
        assert!(!current_engine_ver().is_empty());
        assert!(!current_pkg_ver().is_empty());
        assert!(
            current_pkg_versions_summary() == "unknown"
                || current_pkg_versions_summary().contains('@')
        );
    }
}

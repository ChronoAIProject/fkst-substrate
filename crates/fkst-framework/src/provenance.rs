//! Engine and graph-root provenance stamps for logs and command audit records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HOST_NAMESPACE: &str = "host";
static STATE: OnceLock<RwLock<ProvenanceState>> = OnceLock::new();

#[derive(Clone, Debug)]
struct ProvenanceState {
    engine_ver: String,
    pkg_versions: BTreeMap<String, String>,
    pkg_ver: String,
}

impl Default for ProvenanceState {
    fn default() -> Self {
        Self::from_env()
    }
}

impl ProvenanceState {
    fn from_env() -> Self {
        let engine_ver = std::env::var("FKST_ENGINE_VER")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| engine_version().to_string());
        let pkg_versions = std::env::var("FKST_PKG_VERS")
            .ok()
            .map(|value| parse_versions_summary(&value))
            .unwrap_or_default();
        let pkg_ver = std::env::var("FKST_PKG_VER")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| summarize_versions(&pkg_versions));
        Self {
            engine_ver,
            pkg_versions,
            pkg_ver,
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
        engine_ver: engine_version().to_string(),
        pkg_versions: versions,
        pkg_ver,
    });
}

pub(crate) fn install_run(owner_root: &Path, owner_namespace: &str) {
    let pkg_ver = read_git_version(owner_root);
    let versions = BTreeMap::from([(owner_namespace.to_string(), pkg_ver.clone())]);
    install_state(ProvenanceState {
        engine_ver: engine_version().to_string(),
        pkg_versions: versions,
        pkg_ver,
    });
}

pub(crate) fn current_engine_ver() -> String {
    state()
        .read()
        .map(|state| state.engine_ver.clone())
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

pub(crate) fn emit_code_provenance_line() {
    eprintln!("{}", code_provenance_line());
}

fn code_provenance_line() -> String {
    format!(
        "TIMESTAMP={} LEVEL=info EVENT=code_provenance ENGINE_VER={} PKG_VERS={}",
        rfc3339_utc_now(),
        escape_value(&current_engine_ver()),
        escape_value(&current_pkg_versions_summary())
    )
}

fn install_state(next: ProvenanceState) {
    set_env(&next);
    let mut state = state()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *state = next;
}

fn set_env(state: &ProvenanceState) {
    std::env::set_var("FKST_ENGINE_VER", &state.engine_ver);
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

fn parse_versions_summary(summary: &str) -> BTreeMap<String, String> {
    summary
        .split(';')
        .filter_map(|entry| {
            let (namespace, version) = entry.split_once('@')?;
            if namespace.is_empty() || version.is_empty() {
                return None;
            }
            Some((namespace.to_string(), version.to_string()))
        })
        .collect()
}

fn root_namespace(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn escape_value(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ' ' => escaped.push_str("\\s"),
            '=' => escaped.push_str("\\="),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn rfc3339_utc_now() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    rfc3339_from_unix(elapsed.as_secs())
}

fn rfc3339_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let seconds_of_day = secs % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
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

    #[test]
    fn parses_inherited_package_versions() {
        let parsed = parse_versions_summary("host@abc;pkg@def");
        assert_eq!(parsed.get("host"), Some(&"abc".to_string()));
        assert_eq!(parsed.get("pkg"), Some(&"def".to_string()));
    }

    #[test]
    fn code_provenance_line_records_engine_and_package_versions_once() {
        let line = code_provenance_line();
        assert!(line.starts_with("TIMESTAMP="), "{line}");
        assert!(
            line.contains(" LEVEL=info EVENT=code_provenance "),
            "{line}"
        );
        assert!(line.contains(" ENGINE_VER="), "{line}");
        assert!(line.contains(" PKG_VERS="), "{line}");
        assert!(!line.contains(" PKG_VER="), "{line}");
    }
}

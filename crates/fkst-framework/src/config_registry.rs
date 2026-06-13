//! Source-owned engine operation registry and resolver.
//!
//! This is an audit list, not a configuration framework. It has no dynamic
//! registration, write path, manifest, DSL, plugin, or tunables fallback.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigValueType {
    Usize,
    DurationString,
    String,
}

impl ConfigValueType {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Usize => "usize",
            Self::DurationString => "duration-string",
            Self::String => "string",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigKind {
    Operational { default: &'static str },
    HostFact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigKey {
    QueueCapacity,
    DepartmentDefaultStallWindow,
    CodexPermitSlots,
    DepartmentChildProcessLimit,
    DepartmentChildProcessLimitPerDept,
    RatePoolRoot,
    RetryDefaultMaxAttempts,
    RetryDefaultBase,
    RetryDefaultCap,
    CandidatePrefix,
    CandidateFromSep,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ConfigEntry {
    pub(crate) key: ConfigKey,
    pub(crate) name: &'static str,
    pub(crate) env_key: &'static str,
    pub(crate) kind: ConfigKind,
    pub(crate) value_type: ConfigValueType,
    pub(crate) doc: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Resolved {
    pub(crate) value: String,
    pub(crate) source: ConfigSource,
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigContext {
    host_root: PathBuf,
    process_env: HashMap<String, String>,
    fkst_env: HashMap<String, String>,
}

impl ConfigContext {
    pub(crate) fn from_host_root(host_root: &Path) -> Result<Self> {
        Ok(Self {
            host_root: host_root.to_path_buf(),
            process_env: process_env_for_registry()?,
            fkst_env: read_fkst_env(host_root)?,
        })
    }

    pub(crate) fn resolve(&self, key: ConfigKey) -> Result<Resolved> {
        resolve(key, &self.process_env, &self.fkst_env)
    }

    pub(crate) fn resolved_positive_usize(&self, key: ConfigKey) -> Result<usize> {
        let resolved = self.resolve(key)?;
        parse_positive_usize(entry(key), &resolved.value)
    }

    pub(crate) fn resolved_duration_string(&self, key: ConfigKey) -> Result<String> {
        let resolved = self.resolve(key)?;
        parse_duration_string(entry(key), &resolved.value)
    }

    pub(crate) fn resolved_string(&self, key: ConfigKey) -> Result<String> {
        self.resolve(key).map(|resolved| resolved.value)
    }

    pub(crate) fn host_root(&self) -> &Path {
        &self.host_root
    }

    pub(crate) fn rate_pool_env(&self) -> BTreeMap<String, String> {
        let mut values = self
            .fkst_env
            .iter()
            .filter(|(key, _)| {
                key.starts_with("FKST_RATE_POOL_") && key.as_str() != "FKST_RATE_POOL_ROOT"
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        values.extend(
            self.process_env
                .iter()
                .filter(|(key, _)| {
                    key.starts_with("FKST_RATE_POOL_") && key.as_str() != "FKST_RATE_POOL_ROOT"
                })
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        values
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigSource {
    Env,
    FkstEnv,
    Default,
}

impl ConfigSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::FkstEnv => "fkst.env",
            Self::Default => "default",
        }
    }
}

impl ConfigKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Operational { .. } => "Operational",
            Self::HostFact => "HostFact",
        }
    }

    pub(crate) fn default(self) -> Option<&'static str> {
        match self {
            Self::Operational { default } => Some(default),
            Self::HostFact => None,
        }
    }
}

pub(crate) static CONFIG_REGISTRY: &[ConfigEntry] = &[
    ConfigEntry {
        key: ConfigKey::QueueCapacity,
        name: "queue_capacity",
        env_key: "FKST_QUEUE_CAPACITY",
        kind: ConfigKind::Operational { default: "16" },
        value_type: ConfigValueType::Usize,
        doc: "Default capacity for derived event queues.",
    },
    ConfigEntry {
        key: ConfigKey::DepartmentDefaultStallWindow,
        name: "department_default_stall_window",
        env_key: "FKST_DEPARTMENT_DEFAULT_STALL_WINDOW",
        kind: ConfigKind::Operational { default: "30s" },
        value_type: ConfigValueType::DurationString,
        doc: "Default Department delivery lease window used when M.spec.stall_window is empty.",
    },
    ConfigEntry {
        key: ConfigKey::CodexPermitSlots,
        name: "codex_permit_slots",
        env_key: "FKST_CODEX_PERMIT_SLOTS",
        kind: ConfigKind::Operational { default: "20" },
        value_type: ConfigValueType::Usize,
        doc: "Global Codex process permit pool slot count.",
    },
    ConfigEntry {
        key: ConfigKey::DepartmentChildProcessLimit,
        name: "department_child_process_limit",
        env_key: "FKST_DEPARTMENT_CHILD_PROCESS_LIMIT",
        kind: ConfigKind::Operational { default: "16" },
        value_type: ConfigValueType::Usize,
        doc: "Global Department child process max-in-flight count.",
    },
    ConfigEntry {
        key: ConfigKey::DepartmentChildProcessLimitPerDept,
        name: "department_child_process_limit_per_dept",
        env_key: "FKST_DEPARTMENT_CHILD_PROCESS_LIMIT_PER_DEPT",
        kind: ConfigKind::Operational { default: "4" },
        value_type: ConfigValueType::Usize,
        doc: "Per-Department child process max-in-flight count.",
    },
    ConfigEntry {
        key: ConfigKey::RatePoolRoot,
        name: "rate_pool_root",
        env_key: "FKST_RATE_POOL_ROOT",
        kind: ConfigKind::Operational {
            default: "~/.fkst/rate-pools",
        },
        value_type: ConfigValueType::String,
        doc: "Host-stable root shared by named external-command token bucket pools.",
    },
    ConfigEntry {
        key: ConfigKey::RetryDefaultMaxAttempts,
        name: "retry_default_max_attempts",
        env_key: "FKST_RETRY_DEFAULT_MAX_ATTEMPTS",
        kind: ConfigKind::Operational { default: "5" },
        value_type: ConfigValueType::Usize,
        doc: "Default reliable retry max attempts for Departments without a custom value.",
    },
    ConfigEntry {
        key: ConfigKey::RetryDefaultBase,
        name: "retry_default_base",
        env_key: "FKST_RETRY_DEFAULT_BASE",
        kind: ConfigKind::Operational { default: "60s" },
        value_type: ConfigValueType::DurationString,
        doc: "Default reliable retry base backoff duration.",
    },
    ConfigEntry {
        key: ConfigKey::RetryDefaultCap,
        name: "retry_default_cap",
        env_key: "FKST_RETRY_DEFAULT_CAP",
        kind: ConfigKind::Operational { default: "30m" },
        value_type: ConfigValueType::DurationString,
        doc: "Default reliable retry capped backoff duration.",
    },
    ConfigEntry {
        key: ConfigKey::CandidatePrefix,
        name: "candidate_prefix",
        env_key: "FKST_CANDIDATE_PREFIX",
        kind: ConfigKind::HostFact,
        value_type: ConfigValueType::String,
        doc: "Host-owned prefix used for generated candidate branch names.",
    },
    ConfigEntry {
        key: ConfigKey::CandidateFromSep,
        name: "candidate_from_sep",
        env_key: "FKST_CANDIDATE_FROM_SEP",
        kind: ConfigKind::HostFact,
        value_type: ConfigValueType::String,
        doc: "Host-owned separator before the parent ref slug in candidate branch names.",
    },
];

pub(crate) fn entry(key: ConfigKey) -> &'static ConfigEntry {
    CONFIG_REGISTRY
        .iter()
        .find(|entry| entry.key == key)
        .expect("registry key must exist")
}

pub(crate) fn resolve(
    key: ConfigKey,
    process_env: &HashMap<String, String>,
    fkst_env: &HashMap<String, String>,
) -> Result<Resolved> {
    let entry = entry(key);
    if let Some(value) = nonempty(process_env.get(entry.env_key).cloned()) {
        return Ok(Resolved {
            value,
            source: ConfigSource::Env,
        });
    }
    if let Some(value) = nonempty(fkst_env.get(entry.env_key).cloned()) {
        return Ok(Resolved {
            value,
            source: ConfigSource::FkstEnv,
        });
    }
    if let ConfigKind::Operational { default } = entry.kind {
        return Ok(Resolved {
            value: default.to_string(),
            source: ConfigSource::Default,
        });
    }
    bail!("{} missing; set process env or fkst.env", entry.env_key)
}

pub(crate) fn process_env_for_registry() -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    for entry in CONFIG_REGISTRY {
        match std::env::var(entry.env_key) {
            Ok(value) => {
                values.insert(entry.env_key.to_string(), value);
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("{} must be valid UTF-8", entry.env_key);
            }
        }
    }
    for (key, value) in std::env::vars_os() {
        let Some(key) = key.to_str() else {
            continue;
        };
        if key.starts_with("FKST_RATE_POOL_") && key != "FKST_RATE_POOL_ROOT" {
            let Some(value) = value.to_str() else {
                bail!("{key} must be valid UTF-8");
            };
            values.insert(key.to_string(), value.to_string());
        }
    }
    Ok(values)
}

pub(crate) fn read_fkst_env(root: &Path) -> Result<HashMap<String, String>> {
    let path = root.join("fkst.env");
    if !path.is_file() {
        return Ok(HashMap::new());
    }

    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut values = HashMap::new();
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            bail!("{}:{} must use KEY=value", path.display(), idx + 1);
        };
        if key.trim() != key || key.is_empty() {
            bail!("{}:{} has invalid key", path.display(), idx + 1);
        }
        values.insert(key.to_string(), value.to_string());
    }
    Ok(values)
}

pub(crate) fn parse_positive_usize(entry: &ConfigEntry, raw: &str) -> Result<usize> {
    let value = raw
        .trim()
        .parse::<usize>()
        .with_context(|| format!("{} must be a positive integer, got {raw:?}", entry.env_key))?;
    if value == 0 {
        bail!("{} must be > 0", entry.env_key);
    }
    Ok(value)
}

pub(crate) fn parse_duration_string(entry: &ConfigEntry, raw: &str) -> Result<String> {
    let value = raw.trim().to_string();
    if value.is_empty() || !(value.ends_with('s') || value.ends_with('m') || value.ends_with('h')) {
        bail!(
            "{} must be a Department delivery lease duration ending with s/m/h, got {raw:?}",
            entry.env_key
        );
    }
    Ok(value)
}

fn nonempty(value: Option<String>) -> Option<String> {
    let value = value?.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn resolver_prefers_env_then_fkst_env_then_operational_default() {
        let fkst_env = env(&[("FKST_QUEUE_CAPACITY", "24")]);
        let resolved = resolve(ConfigKey::QueueCapacity, &HashMap::new(), &fkst_env).unwrap();
        assert_eq!(resolved.value, "24");
        assert_eq!(resolved.source, ConfigSource::FkstEnv);

        let process_env = env(&[("FKST_QUEUE_CAPACITY", "32")]);
        let resolved = resolve(ConfigKey::QueueCapacity, &process_env, &fkst_env).unwrap();
        assert_eq!(resolved.value, "32");
        assert_eq!(resolved.source, ConfigSource::Env);

        let resolved = resolve(ConfigKey::QueueCapacity, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(resolved.value, "16");
        assert_eq!(resolved.source, ConfigSource::Default);

        let resolved = resolve(
            ConfigKey::RetryDefaultMaxAttempts,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(resolved.value, "5");
        assert_eq!(resolved.source, ConfigSource::Default);

        let resolved = resolve(
            ConfigKey::RetryDefaultBase,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(resolved.value, "60s");

        let resolved =
            resolve(ConfigKey::RetryDefaultCap, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(resolved.value, "30m");
    }

    #[test]
    fn host_fact_missing_bails() {
        let err =
            resolve(ConfigKey::CandidatePrefix, &HashMap::new(), &HashMap::new()).unwrap_err();
        assert!(format!("{err:#}").contains("FKST_CANDIDATE_PREFIX missing"));
    }
}

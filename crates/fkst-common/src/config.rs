//! In-memory startup graph declarations. Validation lives in `validation.rs`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Config {
    // BTreeMap keeps queue/raiser/department iteration order deterministic
    // (startup validation, logs, spawn order) instead of HashMap random order.
    #[serde(default)]
    pub package: BTreeMap<String, PackageDecl>,
    #[serde(default)]
    pub queue: BTreeMap<String, QueueDecl>,
    #[serde(default)]
    pub raiser: BTreeMap<String, RaiserDecl>,
    #[serde(default)]
    pub department: BTreeMap<String, DepartmentDecl>,
    pub limits: LimitsDecl,
}

// Queue declarations carry the static delivery contract; queues remain
// single-consumer unless Department M.spec declares fanout.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct QueueDecl {
    pub capacity: usize,
    #[serde(default)]
    pub fanout: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
// `RaiserDecl` is limited to
// implemented source kinds, so unsupported types fail at parse time.
pub enum RaiserDecl {
    Cron { interval: String, produces: String },
    FileWatch { glob: String, produces: String },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct DepartmentDecl {
    pub lua: PathBuf,
    pub owner_root: PathBuf,
    pub owner_namespace: String,
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub produces: Vec<String>,
    #[serde(default)]
    pub ephemeral: Vec<String>,
    pub stall_window: String,
    #[serde(default)]
    pub graph_json: bool,
    #[serde(default)]
    pub retry: Option<RetryDecl>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RetryDecl {
    pub max_attempts: u64,
    pub base: String,
    pub cap: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LimitsDecl {
    pub global_codex_processes: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PackageDecl {
    pub persistence_class: PersistenceClass,
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default)]
    pub terminal_states: Vec<String>,
    #[serde(default)]
    pub transitions: Vec<SagaTransitionDecl>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceClass {
    Saga,
    StatelessAdapter,
    JudgmentPipeline,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SagaTransitionDecl {
    pub from: String,
    pub to: String,
    pub queue: String,
    pub dedup: String,
    #[serde(default)]
    pub payload_fields: BTreeMap<String, String>,
    #[serde(default)]
    pub required_facts: Vec<SagaRequiredFactDecl>,
    pub effect: SagaEffectDecl,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SagaRequiredFactDecl {
    pub name: String,
    pub freshness: SagaFactFreshness,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SagaFactFreshness {
    Current,
    Snapshot,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SagaEffectDecl {
    pub intent: String,
    pub completeness: String,
}

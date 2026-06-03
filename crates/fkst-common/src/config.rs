//! In-memory startup graph declarations. Validation lives in `validation.rs`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Config {
    // BTreeMap keeps queue/raiser/department iteration order deterministic
    // (startup validation, logs, spawn order) instead of HashMap random order.
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
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub produces: Vec<String>,
    pub stall_window: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LimitsDecl {
    pub global_codex_processes: usize,
}

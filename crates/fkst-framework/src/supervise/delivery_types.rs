//! Durable delivery records stored by supervise.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::time::Duration;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct SourceRef {
    pub kind: SourceKind,
    pub reference: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) enum SourceKind {
    File,
    Cron,
    Git,
    External,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct DeliveryRecord {
    pub delivery_id: String,
    pub queue: String,
    pub dept: String,
    pub payload: JsonValue,
    pub source: Option<SourceRef>,
    pub cron_payload: Option<JsonValue>,
    pub observed_at_ms: u64,
    pub attempt: u64,
    #[serde(default)]
    pub redrive_count: u64,
    #[serde(default)]
    pub collapse_by_dedup_id: bool,
    /// A stable keyed duplicate arrived while this delivery was leased.
    #[serde(default)]
    pub pending_dirty: bool,
    #[serde(default)]
    pub subscriber_absent_since_ms: Option<u64>,
    pub lease_generation: u64,
    pub lease_until_ms: Option<u64>,
    pub not_before_ms: u64,
    pub last_error_excerpt: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct DeadRecord {
    pub delivery_id: String,
    pub queue: String,
    pub dept: String,
    pub source: Option<SourceRef>,
    pub observed_at_ms: u64,
    pub not_before_ms: u64,
    pub dead_at_ms: u64,
    pub attempts: u64,
    #[serde(default)]
    pub redrive_count: u64,
    #[serde(default)]
    pub replayable: bool,
    #[serde(default = "default_dead_permanent")]
    pub permanent: bool,
    pub error_excerpt: Option<String>,
    #[serde(default)]
    pub record: Option<DeliveryRecord>,
}

#[derive(Clone, Debug)]
pub(crate) struct RetryPolicy {
    pub max_attempts: u64,
    pub base: Duration,
    pub cap: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct RedrivePolicy {
    pub max_redrives: u64,
    pub cooldown: Duration,
}

fn default_dead_permanent() -> bool {
    true
}

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
    pub dead_at_ms: u64,
    pub attempts: u64,
    pub error_excerpt: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct RetryPolicy {
    pub max_attempts: u64,
    pub base: Duration,
    pub cap: Duration,
}

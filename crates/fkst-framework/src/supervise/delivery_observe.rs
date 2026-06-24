use super::delivery_store::{DeliveryScanRecord, DeliveryStore};
use super::delivery_types::{DeadRecord, DeliveryRecord, SourceRef};
use crate::manifest_hash::sha256_hex;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeliveryObserveOptions {
    pub(crate) now_ms: u64,
    pub(crate) limit: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct DeliveryObserveSnapshot {
    pub(crate) schema_version: u32,
    pub(crate) generated_at_ms: u64,
    pub(crate) source: DeliveryObserveSource,
    pub(crate) limits: DeliveryObserveLimits,
    pub(crate) truncated: DeliveryObserveTruncated,
    pub(crate) queues: Vec<QueueObserveState>,
    pub(crate) deliveries: Vec<DeliveryObserveEntry>,
    pub(crate) dead_letters: Vec<DeadLetterObserveEntry>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct DeliveryObserveSource {
    pub(crate) durable_root: String,
    pub(crate) database: String,
    pub(crate) read_semantics: String,
    pub(crate) history_semantics: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct DeliveryObserveLimits {
    pub(crate) max_deliveries: usize,
    pub(crate) max_dead_letters: usize,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct DeliveryObserveTruncated {
    pub(crate) deliveries: bool,
    pub(crate) dead_letters: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct QueueObserveState {
    pub(crate) queue: String,
    pub(crate) depth: usize,
    pub(crate) pending: usize,
    pub(crate) in_flight: usize,
    pub(crate) retrying: usize,
    pub(crate) oldest_pending_age_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct DeliveryObserveEntry {
    pub(crate) delivery_id: String,
    pub(crate) queue: String,
    pub(crate) dept: String,
    pub(crate) source: Option<SourceRef>,
    pub(crate) status: DeliveryObserveStatus,
    pub(crate) observed_at_ms: u64,
    pub(crate) not_before_ms: u64,
    pub(crate) attempt: u64,
    pub(crate) redrive_count: u64,
    pub(crate) lease_generation: u64,
    pub(crate) lease_until_ms: Option<u64>,
    pub(crate) fence_token: String,
    pub(crate) payload: PayloadObserveSummary,
    pub(crate) last_error_excerpt: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct DeadLetterObserveEntry {
    pub(crate) delivery_id: String,
    pub(crate) queue: String,
    pub(crate) dept: String,
    pub(crate) source: Option<SourceRef>,
    pub(crate) observed_at_ms: u64,
    pub(crate) not_before_ms: u64,
    pub(crate) dead_at_ms: u64,
    pub(crate) attempts: u64,
    pub(crate) redrive_count: u64,
    pub(crate) replayable: bool,
    pub(crate) permanent: bool,
    pub(crate) payload: PayloadObserveSummary,
    pub(crate) error_excerpt: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DeliveryObserveStatus {
    Pending,
    InFlight,
    Retrying,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct PayloadObserveSummary {
    pub(crate) schema: Option<String>,
    pub(crate) dedup_key: Option<String>,
    pub(crate) digest: String,
    pub(crate) bytes: usize,
}

pub(crate) fn observe_snapshot(
    store: &DeliveryStore,
    durable_root: &Path,
    database: &Path,
    options: &DeliveryObserveOptions,
) -> Result<DeliveryObserveSnapshot> {
    let mut deliveries = Vec::new();
    let mut dead_letters = Vec::new();
    let mut queues = BTreeMap::<String, QueueAccumulator>::new();
    let mut truncated = DeliveryObserveTruncated::default();

    store.scan_records(|record| {
        match record {
            DeliveryScanRecord::Delivery(record) => {
                queues
                    .entry(record.queue.clone())
                    .or_default()
                    .observe_delivery(&record, options.now_ms);
                if deliveries.len() < options.limit {
                    deliveries.push(delivery_observe_entry(record, options.now_ms)?);
                } else {
                    truncated.deliveries = true;
                }
            }
            DeliveryScanRecord::Dead(record) => {
                if dead_letters.len() < options.limit {
                    dead_letters.push(dead_letter_observe_entry(record)?);
                } else {
                    truncated.dead_letters = true;
                }
            }
        }
        Ok(())
    })?;

    deliveries.sort_by(|left, right| {
        left.queue
            .cmp(&right.queue)
            .then_with(|| left.dept.cmp(&right.dept))
            .then_with(|| left.not_before_ms.cmp(&right.not_before_ms))
            .then_with(|| left.delivery_id.cmp(&right.delivery_id))
    });
    dead_letters.sort_by(|left, right| {
        left.dead_at_ms
            .cmp(&right.dead_at_ms)
            .then_with(|| left.delivery_id.cmp(&right.delivery_id))
    });

    Ok(DeliveryObserveSnapshot {
        schema_version: 1,
        generated_at_ms: options.now_ms,
        source: DeliveryObserveSource {
            durable_root: durable_root.display().to_string(),
            database: database.display().to_string(),
            read_semantics:
                "single read transaction over the owner redb handle for live supervise snapshots or over an offline database open"
                    .to_string(),
            history_semantics:
                "delivery queue snapshot only; acked deliveries are removed and historical timelines require a journal"
                    .to_string(),
        },
        limits: DeliveryObserveLimits {
            max_deliveries: options.limit,
            max_dead_letters: options.limit,
        },
        truncated,
        queues: queues
            .into_iter()
            .map(|(queue, accumulator)| accumulator.finish(queue, options.now_ms))
            .collect(),
        deliveries,
        dead_letters,
    })
}

#[derive(Clone, Debug, Default)]
struct QueueAccumulator {
    pending: usize,
    in_flight: usize,
    retrying: usize,
    oldest_pending_ms: Option<u64>,
}

impl QueueAccumulator {
    fn observe_delivery(&mut self, record: &DeliveryRecord, now_ms: u64) {
        match delivery_status(record, now_ms) {
            DeliveryObserveStatus::Pending => {
                self.pending += 1;
                self.oldest_pending_ms = Some(
                    self.oldest_pending_ms
                        .map(|oldest| oldest.min(record.not_before_ms))
                        .unwrap_or(record.not_before_ms),
                );
            }
            DeliveryObserveStatus::InFlight => self.in_flight += 1,
            DeliveryObserveStatus::Retrying => self.retrying += 1,
        }
    }

    fn finish(self, queue: String, now_ms: u64) -> QueueObserveState {
        QueueObserveState {
            queue,
            depth: self
                .pending
                .saturating_add(self.in_flight)
                .saturating_add(self.retrying),
            pending: self.pending,
            in_flight: self.in_flight,
            retrying: self.retrying,
            oldest_pending_age_ms: self
                .oldest_pending_ms
                .map(|observed| now_ms.saturating_sub(observed)),
        }
    }
}

fn delivery_observe_entry(record: DeliveryRecord, now_ms: u64) -> Result<DeliveryObserveEntry> {
    let payload = payload_summary(&record.payload)?;
    let status = delivery_status(&record, now_ms);
    Ok(DeliveryObserveEntry {
        fence_token: fence_token(&record.delivery_id, record.lease_generation),
        delivery_id: record.delivery_id,
        queue: record.queue,
        dept: record.dept,
        source: record.source,
        status,
        observed_at_ms: record.observed_at_ms,
        not_before_ms: record.not_before_ms,
        attempt: record.attempt,
        redrive_count: record.redrive_count,
        lease_generation: record.lease_generation,
        lease_until_ms: record.lease_until_ms,
        payload,
        last_error_excerpt: record.last_error_excerpt,
    })
}

fn dead_letter_observe_entry(record: DeadRecord) -> Result<DeadLetterObserveEntry> {
    let payload = match record.record.as_ref() {
        Some(delivery) => payload_summary(&delivery.payload)?,
        None => PayloadObserveSummary::empty(),
    };
    Ok(DeadLetterObserveEntry {
        delivery_id: record.delivery_id,
        queue: record.queue,
        dept: record.dept,
        source: record.source,
        observed_at_ms: record.observed_at_ms,
        not_before_ms: record.not_before_ms,
        dead_at_ms: record.dead_at_ms,
        attempts: record.attempts,
        redrive_count: record.redrive_count,
        replayable: record.replayable,
        permanent: record.permanent,
        payload,
        error_excerpt: record.error_excerpt,
    })
}

fn delivery_status(record: &DeliveryRecord, now_ms: u64) -> DeliveryObserveStatus {
    if record
        .lease_until_ms
        .is_some_and(|lease_until| lease_until > now_ms)
    {
        DeliveryObserveStatus::InFlight
    } else if record.attempt > 0 && record.not_before_ms > now_ms {
        DeliveryObserveStatus::Retrying
    } else {
        DeliveryObserveStatus::Pending
    }
}

fn fence_token(delivery_id: &str, lease_generation: u64) -> String {
    format!("{delivery_id}#{lease_generation}")
}

impl PayloadObserveSummary {
    fn empty() -> Self {
        Self {
            schema: None,
            dedup_key: None,
            digest: stable_json_digest(&JsonValue::Null),
            bytes: 4,
        }
    }
}

fn payload_summary(payload: &JsonValue) -> Result<PayloadObserveSummary> {
    let bytes = serde_json::to_vec(payload)?;
    Ok(PayloadObserveSummary {
        schema: payload_string_field(payload, "schema"),
        dedup_key: payload_string_field(payload, "dedup_key"),
        digest: sha256_hex(&bytes),
        bytes: bytes.len(),
    })
}

fn payload_string_field(payload: &JsonValue, key: &str) -> Option<String> {
    payload
        .as_object()
        .and_then(|object| object.get(key))
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

fn stable_json_digest(value: &JsonValue) -> String {
    sha256_hex(&serde_json::to_vec(value).expect("serialize JSON value"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::delivery_store::{DeliveryStore, RetryFailure};
    use crate::supervise::delivery_types::{DeliveryRecord, RetryPolicy};
    use std::time::Duration;
    use tempfile::TempDir;

    fn store(root: &TempDir) -> DeliveryStore {
        DeliveryStore::open(root.path().join("delivery.redb")).unwrap()
    }

    fn record(id: &str, not_before_ms: u64) -> DeliveryRecord {
        DeliveryRecord {
            delivery_id: id.to_string(),
            queue: "input".to_string(),
            dept: "worker".to_string(),
            payload: serde_json::json!({"n": 1}),
            source: None,
            cron_payload: None,
            observed_at_ms: 10,
            attempt: 0,
            redrive_count: 0,
            collapse_by_dedup_id: false,
            lease_generation: 0,
            lease_until_ms: None,
            not_before_ms,
            last_error_excerpt: None,
        }
    }

    fn policy(max_attempts: u64) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base: Duration::from_millis(100),
            cap: Duration::from_millis(1_000),
        }
    }

    fn failure(message: &str, replayable: bool) -> RetryFailure {
        RetryFailure {
            message: message.to_string(),
            replayable,
        }
    }

    #[test]
    fn observe_snapshot_reports_queue_state_without_payload_body() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let mut pending = record("pending", 100);
        pending.payload = serde_json::json!({
            "schema": "github.issue",
            "dedup_key": "issue-81",
            "body": "secret body must not be emitted"
        });
        let retrying = record("retrying", 250);
        store.enqueue(&pending).unwrap();
        store.enqueue(&retrying).unwrap();
        store.lease(100, 1, Duration::from_millis(100)).unwrap();

        let snapshot = observe_snapshot(
            &store,
            temp.path(),
            &temp.path().join("delivery.redb"),
            &DeliveryObserveOptions {
                now_ms: 150,
                limit: 10,
            },
        )
        .unwrap();
        let queue = snapshot
            .queues
            .iter()
            .find(|entry| entry.queue == "input")
            .unwrap();

        assert_eq!(queue.depth, 2);
        assert_eq!(queue.pending, 1);
        assert_eq!(queue.in_flight, 1);
        assert_eq!(queue.retrying, 0);
        assert_eq!(queue.oldest_pending_age_ms, Some(0));
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"schema\":\"github.issue\""));
        assert!(json.contains("\"dedup_key\":\"issue-81\""));
        assert!(!json.contains("secret body must not be emitted"));
    }

    #[test]
    fn observe_snapshot_reports_retrying_and_dead_letters() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        let leased = store
            .lease(100, 1, Duration::from_millis(50))
            .unwrap()
            .remove(0);
        store
            .retry(
                &leased.delivery_id,
                leased.lease_generation,
                &failure("temporary", false),
                &policy(3),
                120,
            )
            .unwrap();
        store.enqueue(&record("dead", 100)).unwrap();
        let dead = store
            .lease(100, 1, Duration::from_millis(50))
            .unwrap()
            .remove(0);
        store
            .retry(
                &dead.delivery_id,
                dead.lease_generation,
                &failure("final", false),
                &policy(1),
                130,
            )
            .unwrap();

        let snapshot = observe_snapshot(
            &store,
            temp.path(),
            &temp.path().join("delivery.redb"),
            &DeliveryObserveOptions {
                now_ms: 121,
                limit: 10,
            },
        )
        .unwrap();
        let queue = snapshot
            .queues
            .iter()
            .find(|entry| entry.queue == "input")
            .unwrap();

        assert_eq!(queue.retrying, 1);
        assert_eq!(snapshot.dead_letters.len(), 1);
        assert_eq!(snapshot.dead_letters[0].delivery_id, "dead");
        assert_eq!(snapshot.dead_letters[0].payload.bytes, 4);
    }

    #[test]
    fn observe_snapshot_marks_truncation() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.enqueue(&record("two", 100)).unwrap();

        let snapshot = observe_snapshot(
            &store,
            temp.path(),
            &temp.path().join("delivery.redb"),
            &DeliveryObserveOptions {
                now_ms: 100,
                limit: 1,
            },
        )
        .unwrap();

        assert_eq!(snapshot.deliveries.len(), 1);
        assert!(snapshot.truncated.deliveries);
    }
}

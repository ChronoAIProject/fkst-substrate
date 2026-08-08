use super::delivery_store::{DeliveryObservationProjection, DeliveryStore};
use super::delivery_types::{DeadRecord, DeliveryRecord, SourceRef};
use crate::manifest_hash::sha256_hex;
use crate::observe::{DeadLetterPageCursor, DeadLetterPageOptions};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeliveryObserveOptions {
    pub(crate) now_ms: u64,
    pub(crate) limit: usize,
    pub(crate) since: Option<String>,
    pub(crate) dead_letter_page: Option<DeadLetterPageOptions>,
    pub(crate) current_subscriber_queues: Option<BTreeSet<String>>,
    pub(crate) projection: DeliveryObservationProjection,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) page: Option<DeliveryObservePage>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct LineageObserveResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) live_delivery: Option<DeliveryObserveEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_dead_letter: Option<DeadLetterObserveEntry>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct DeliveryObservePage {
    pub(crate) section: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next: Option<String>,
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
    pub(crate) subscriber_status: QueueSubscriberStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) has_current_subscriber: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueSubscriberStatus {
    Current,
    Absent,
    Unknown,
}

impl std::fmt::Display for QueueSubscriberStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Current => "current",
            Self::Absent => "absent",
            Self::Unknown => "unknown",
        })
    }
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
    pub(crate) subscriber_absent_since_ms: Option<u64>,
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
    if options.since.as_deref().is_some_and(str::is_empty) {
        anyhow::bail!("observe since cursor must not be empty");
    }
    if options.dead_letter_page.is_some() && options.since.is_some() {
        anyhow::bail!("fkst.observe page cannot be combined with since");
    }
    let mut queues = BTreeMap::<String, QueueAccumulator>::new();
    let mut projection = options.projection;
    if options.dead_letter_page.is_some() {
        projection.dead_letters = true;
    }
    let dead_after = options
        .dead_letter_page
        .as_ref()
        .and_then(|page| page.after.as_ref())
        .map(|cursor| (cursor.dead_at_ms, cursor.delivery_id.as_str()));
    let records = store.observe_records(
        projection,
        options.since.as_deref(),
        dead_after,
        options.limit,
        |record| {
            queues
                .entry(record.queue.clone())
                .or_default()
                .observe_delivery(&record, options.now_ms);
            Ok(())
        },
    )?;
    let mut delivery_records = records.deliveries;
    let mut dead_records = records.dead_letters;
    let truncated = DeliveryObserveTruncated {
        deliveries: apply_limit(&mut delivery_records, options.limit),
        dead_letters: apply_limit(&mut dead_records, options.limit),
    };
    let page = options
        .dead_letter_page
        .as_ref()
        .map(|_| {
            let next = if truncated.dead_letters {
                dead_records
                    .last()
                    .map(|record| {
                        crate::observe::encode_dead_letter_cursor(&DeadLetterPageCursor {
                            dead_at_ms: record.dead_at_ms,
                            delivery_id: record.delivery_id.clone(),
                        })
                    })
                    .transpose()?
            } else {
                None
            };
            Ok::<DeliveryObservePage, anyhow::Error>(DeliveryObservePage {
                section: "dead_letters".to_string(),
                next,
            })
        })
        .transpose()?;
    let deliveries = delivery_records
        .into_iter()
        .map(|record| delivery_observe_entry(record, options.now_ms))
        .collect::<Result<Vec<_>>>()?;
    let dead_letters = dead_records
        .into_iter()
        .map(dead_letter_observe_entry)
        .collect::<Result<Vec<_>>>()?;

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
            .map(|(queue, accumulator)| {
                accumulator.finish(
                    queue,
                    options.now_ms,
                    options.current_subscriber_queues.as_ref(),
                )
            })
            .collect(),
        deliveries,
        dead_letters,
        page,
    })
}

pub(crate) fn observe_lineage(
    store: &DeliveryStore,
    queue: &str,
    dept: &str,
    source: &SourceRef,
    now_ms: u64,
) -> Result<LineageObserveResult> {
    let lineage = store.lookup_lineage(queue, dept, source)?;
    Ok(LineageObserveResult {
        live_delivery: lineage
            .live_delivery
            .map(|record| delivery_observe_entry(record, now_ms))
            .transpose()?,
        terminal_dead_letter: lineage
            .terminal_dead_letter
            .map(dead_letter_observe_entry)
            .transpose()?,
    })
}

fn apply_limit<T>(entries: &mut Vec<T>, limit: usize) -> bool {
    if entries.len() <= limit {
        return false;
    }
    entries.truncate(limit);
    true
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

    fn finish(
        self,
        queue: String,
        now_ms: u64,
        current_subscriber_queues: Option<&BTreeSet<String>>,
    ) -> QueueObserveState {
        let subscriber_status = match current_subscriber_queues {
            Some(subscribers) if subscribers.contains(&queue) => QueueSubscriberStatus::Current,
            Some(_) => QueueSubscriberStatus::Absent,
            None => QueueSubscriberStatus::Unknown,
        };
        let has_current_subscriber = match subscriber_status {
            QueueSubscriberStatus::Current => Some(true),
            QueueSubscriberStatus::Absent => Some(false),
            QueueSubscriberStatus::Unknown => None,
        };
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
            subscriber_status,
            has_current_subscriber,
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
        subscriber_absent_since_ms: record.subscriber_absent_since_ms,
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
            pending_dirty: false,
            subscriber_absent_since_ms: None,
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
    fn non_page_snapshot_serialization_is_unchanged() {
        let snapshot = DeliveryObserveSnapshot {
            schema_version: 1,
            generated_at_ms: 1,
            source: DeliveryObserveSource {
                durable_root: "/durable".to_string(),
                database: "/durable/delivery.redb".to_string(),
                read_semantics: "one read".to_string(),
                history_semantics: "mutable".to_string(),
            },
            limits: DeliveryObserveLimits {
                max_deliveries: 2,
                max_dead_letters: 2,
            },
            truncated: DeliveryObserveTruncated::default(),
            queues: Vec::new(),
            deliveries: Vec::new(),
            dead_letters: Vec::new(),
            page: None,
        };

        assert_eq!(
            serde_json::to_string(&snapshot).unwrap(),
            r#"{"schema_version":1,"generated_at_ms":1,"source":{"durable_root":"/durable","database":"/durable/delivery.redb","read_semantics":"one read","history_semantics":"mutable"},"limits":{"max_deliveries":2,"max_dead_letters":2},"truncated":{"deliveries":false,"dead_letters":false},"queues":[],"deliveries":[],"dead_letters":[]}"#
        );
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
                since: None,
                dead_letter_page: None,
                current_subscriber_queues: None,
                projection: DeliveryObservationProjection::default(),
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
        assert_eq!(queue.subscriber_status, QueueSubscriberStatus::Unknown);
        assert_eq!(queue.has_current_subscriber, None);
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"schema\":\"github.issue\""));
        assert!(json.contains("\"dedup_key\":\"issue-81\""));
        assert!(json.contains("\"subscriber_status\":\"unknown\""));
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
                since: None,
                dead_letter_page: None,
                current_subscriber_queues: None,
                projection: DeliveryObservationProjection::default(),
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
                since: None,
                dead_letter_page: None,
                current_subscriber_queues: None,
                projection: DeliveryObservationProjection::default(),
            },
        )
        .unwrap();

        assert_eq!(snapshot.deliveries.len(), 1);
        assert!(snapshot.truncated.deliveries);
    }

    #[test]
    fn observe_snapshot_pushes_error_only_projection_into_storage() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        for (delivery_id, dead_at_ms) in [("dead-b", 200), ("dead-a", 100)] {
            store.enqueue(&record(delivery_id, 100)).unwrap();
            let leased = store
                .lease(100, 1, Duration::from_millis(50))
                .unwrap()
                .remove(0);
            store
                .retry(
                    &leased.delivery_id,
                    leased.lease_generation,
                    &failure("final", false),
                    &policy(1),
                    dead_at_ms,
                )
                .unwrap();
        }
        for index in 0..32 {
            store
                .enqueue(&record(&format!("delivery-{index:02}"), 100))
                .unwrap();
        }

        DeliveryStore::reset_observation_record_read_counts();
        let snapshot = observe_snapshot(
            &store,
            temp.path(),
            &temp.path().join("delivery.redb"),
            &DeliveryObserveOptions {
                now_ms: 300,
                limit: 1,
                since: None,
                dead_letter_page: None,
                current_subscriber_queues: None,
                projection: DeliveryObservationProjection {
                    queue_aggregates: false,
                    deliveries: false,
                    dead_letters: true,
                },
            },
        )
        .unwrap();

        assert!(snapshot.queues.is_empty());
        assert!(snapshot.deliveries.is_empty());
        assert_eq!(snapshot.dead_letters[0].delivery_id, "dead-a");
        assert!(snapshot.truncated.dead_letters);
        assert_eq!(DeliveryStore::observation_record_read_counts(), (0, 0, 2));
    }

    #[test]
    fn observe_snapshot_applies_since_before_limit() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("delivery-001", 100)).unwrap();
        store.enqueue(&record("delivery-002", 100)).unwrap();
        store.enqueue(&record("delivery-003", 100)).unwrap();

        let snapshot = observe_snapshot(
            &store,
            temp.path(),
            &temp.path().join("delivery.redb"),
            &DeliveryObserveOptions {
                now_ms: 100,
                limit: 1,
                since: Some("delivery-001".to_string()),
                dead_letter_page: None,
                current_subscriber_queues: None,
                projection: DeliveryObservationProjection::default(),
            },
        )
        .unwrap();

        assert_eq!(snapshot.deliveries.len(), 1);
        assert_eq!(snapshot.deliveries[0].delivery_id, "delivery-002");
        assert!(snapshot.truncated.deliveries);
    }

    #[test]
    fn observe_snapshot_marks_subscriber_status_when_graph_is_available() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("subscribed", 100)).unwrap();
        let mut orphan = record("orphan", 100);
        orphan.queue = "orphan".to_string();
        orphan.dept = "removed_worker".to_string();
        store.enqueue(&orphan).unwrap();

        let snapshot = observe_snapshot(
            &store,
            temp.path(),
            &temp.path().join("delivery.redb"),
            &DeliveryObserveOptions {
                now_ms: 100,
                limit: 10,
                since: None,
                dead_letter_page: None,
                current_subscriber_queues: Some(BTreeSet::from(["input".to_string()])),
                projection: DeliveryObservationProjection::default(),
            },
        )
        .unwrap();

        let input = snapshot
            .queues
            .iter()
            .find(|entry| entry.queue == "input")
            .unwrap();
        let orphan = snapshot
            .queues
            .iter()
            .find(|entry| entry.queue == "orphan")
            .unwrap();
        assert_eq!(input.subscriber_status, QueueSubscriberStatus::Current);
        assert_eq!(orphan.subscriber_status, QueueSubscriberStatus::Absent);
        assert_eq!(input.has_current_subscriber, Some(true));
        assert_eq!(orphan.has_current_subscriber, Some(false));
    }
}

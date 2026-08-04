//! Structured engine failure facts emitted by supervise.

use super::delivery_retry::error_excerpt;
use super::delivery_types::{DeadRecord, DeliveryRecord, SourceRef};
pub(crate) use fkst_common::built_in_provider::FAILURE_FACT_SCHEMA;
use fkst_common::Event;
use serde_json::{json, Value as JsonValue};

pub(crate) const FAILURE_FACT_QUEUE: &str = "fkst.failure_fact";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ErrorClass {
    DeliveryFailure,
    FrameworkChildNonzero,
    FrameworkChildSpawn,
    LockBusy,
    RaisedPublish,
    SpawnArgs,
    StoreOpWatchdog,
    SchemaValidation,
}

impl ErrorClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DeliveryFailure => "delivery_failure",
            Self::FrameworkChildNonzero => "framework_child_nonzero",
            Self::FrameworkChildSpawn => "framework_child_spawn",
            Self::LockBusy => "lock_busy",
            Self::RaisedPublish => "raised_publish",
            Self::SpawnArgs => "spawn_args",
            Self::StoreOpWatchdog => "store_op_watchdog",
            Self::SchemaValidation => "schema_validation",
        }
    }
}

pub(crate) fn lock_busy_failure_fact(
    origin_dept: &str,
    origin_queue: &str,
    record: Option<&DeliveryRecord>,
    lock: Option<&str>,
) -> Event {
    let error_class = ErrorClass::LockBusy;
    let lock_label = lock.unwrap_or("unknown");
    let error = format!("lock busy lock={lock_label}");
    failure_fact_event(json!({
        "schema": FAILURE_FACT_SCHEMA,
        "error_class": error_class.as_str(),
        "fingerprint": fingerprint(error_class, lock_label),
        "error": error,
        "lock": lock,
        "attempt": record.map(|record| record.attempt),
        "delivery_id": record.map(|record| record.delivery_id.as_str()),
        "origin_queue": origin_queue,
        "origin_dept": origin_dept,
        "source_ref": source_ref_json(record.and_then(|record| record.source.as_ref())),
        "observed_at_ms": record.map(|record| record.observed_at_ms),
    }))
}

pub(crate) fn classify_delivery_error(error: &str) -> ErrorClass {
    if error.starts_with("exit=") {
        ErrorClass::FrameworkChildNonzero
    } else if error.starts_with("spawn error:") {
        ErrorClass::FrameworkChildSpawn
    } else if error.starts_with("raised publish error:") {
        ErrorClass::RaisedPublish
    } else if error.starts_with("build spawn args:") {
        ErrorClass::SpawnArgs
    } else {
        ErrorClass::DeliveryFailure
    }
}

pub(crate) fn delivery_failure_fact(record: &DeliveryRecord, error: &str) -> Event {
    let error_class = classify_delivery_error(error);
    failure_fact_event(json!({
        "schema": FAILURE_FACT_SCHEMA,
        "error_class": error_class.as_str(),
        "fingerprint": fingerprint(error_class, error),
        "error": error_excerpt(error),
        "attempt": record.attempt.saturating_add(1),
        "delivery_id": record.delivery_id,
        "origin_queue": record.queue,
        "origin_dept": record.dept,
        "source_ref": source_ref_json(record.source.as_ref()),
        "observed_at_ms": record.observed_at_ms,
    }))
}

pub(crate) fn dead_letter_payload(record: &DeliveryRecord, error: &str) -> JsonValue {
    let error_class = classify_delivery_error(error);
    json!({
        "delivery_id": record.delivery_id,
        "dedup_id": record.delivery_id,
        "queue": record.queue,
        "dept": record.dept,
        "origin_queue": record.queue,
        "origin_dept": record.dept,
        "attempt": record.attempt.saturating_add(1),
        "error": error_excerpt(error),
        "error_class": error_class.as_str(),
        "fingerprint": fingerprint(error_class, error),
        "source_ref": source_ref_json(record.source.as_ref()),
    })
}

pub(crate) fn dead_record_payload(dead: &DeadRecord) -> JsonValue {
    let error = dead
        .error_excerpt
        .as_deref()
        .unwrap_or("unknown delivery failure");
    let error_class = classify_delivery_error(error);
    json!({
        "delivery_id": dead.delivery_id,
        "dedup_id": dead.delivery_id,
        "queue": dead.queue,
        "dept": dead.dept,
        "origin_queue": dead.queue,
        "origin_dept": dead.dept,
        "attempt": dead.attempts,
        "redrive_count": dead.redrive_count,
        "error": error_excerpt(error),
        "error_class": error_class.as_str(),
        "fingerprint": fingerprint(error_class, error),
        "source_ref": source_ref_json(dead.source.as_ref()),
    })
}

pub(crate) fn store_watchdog_failure_fact(op: &str, dept: &str, elapsed_ms: u64) -> Event {
    let error_class = ErrorClass::StoreOpWatchdog;
    let message = format!(
        "durable delivery store operation exceeded watchdog threshold op={op} dept={dept} elapsed_ms={elapsed_ms}"
    );
    failure_fact_event(json!({
        "schema": FAILURE_FACT_SCHEMA,
        "error_class": error_class.as_str(),
        "fingerprint": fingerprint(error_class, &message),
        "error": message,
        "origin_dept": dept,
        "store_op": op,
        "elapsed_ms": elapsed_ms,
    }))
}

pub(crate) fn schema_validation_failure_fact(error: &str) -> Event {
    let error_class = ErrorClass::SchemaValidation;
    failure_fact_event(json!({
        "schema": FAILURE_FACT_SCHEMA,
        "error_class": error_class.as_str(),
        "fingerprint": fingerprint(error_class, error),
        "error": error_excerpt(error),
    }))
}

fn failure_fact_event(payload: JsonValue) -> Event {
    Event::new(FAILURE_FACT_QUEUE, payload)
}

fn source_ref_json(source: Option<&SourceRef>) -> JsonValue {
    match source {
        Some(source) => json!({
            "kind": source_kind_name(source),
            "reference": source.reference,
        }),
        None => JsonValue::Null,
    }
}

fn source_kind_name(source: &SourceRef) -> &'static str {
    match source.kind {
        super::delivery_types::SourceKind::File => "file",
        super::delivery_types::SourceKind::Cron => "cron",
        super::delivery_types::SourceKind::Git => "git",
        super::delivery_types::SourceKind::External => "external",
    }
}

pub(crate) fn fingerprint(error_class: ErrorClass, raw: &str) -> String {
    let normalized = normalize_for_fingerprint(raw);
    format!("{}:{:016x}", error_class.as_str(), fnv1a64(&normalized))
}

fn normalize_for_fingerprint(raw: &str) -> String {
    raw.split_whitespace()
        .map(normalize_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_token(token: &str) -> String {
    let trimmed = token.trim_matches(|ch: char| ch.is_ascii_punctuation());
    if looks_like_rfc3339_timestamp(trimmed) {
        return token.replace(trimmed, "<timestamp>");
    }
    if looks_like_sha(trimmed) {
        return token.replace(trimmed, "<sha>");
    }
    if looks_like_epoch_number(trimmed) {
        return token.replace(trimmed, "<number>");
    }
    token.to_string()
}

fn looks_like_rfc3339_timestamp(token: &str) -> bool {
    token.len() >= 20
        && token.as_bytes().get(4) == Some(&b'-')
        && token.as_bytes().get(7) == Some(&b'-')
        && matches!(
            token.as_bytes().get(10),
            Some(b'T') | Some(b't') | Some(b' ')
        )
}

fn looks_like_sha(token: &str) -> bool {
    (7..=64).contains(&token.len())
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
        && token.bytes().any(|byte| byte.is_ascii_alphabetic())
}

fn looks_like_epoch_number(token: &str) -> bool {
    token.len() >= 10 && token.bytes().all(|byte| byte.is_ascii_digit())
}

fn fnv1a64(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_ignores_timestamps_and_shas() {
        let left = fingerprint(
            ErrorClass::FrameworkChildNonzero,
            "exit=1 at 2026-06-11T09:52:37Z commit abcdef1234567890",
        );
        let right = fingerprint(
            ErrorClass::FrameworkChildNonzero,
            "exit=1 at 2027-01-01T00:00:00Z commit ffffff1234567890",
        );

        assert_eq!(left, right);
    }

    #[test]
    fn dead_letter_payload_carries_stable_failure_fields() {
        let record = DeliveryRecord {
            delivery_id: "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick".to_string(),
            queue: "jobs".to_string(),
            dept: "worker".to_string(),
            payload: json!({"n": 1}),
            source: Some(SourceRef {
                kind: super::super::delivery_types::SourceKind::Cron,
                reference: "tick".to_string(),
            }),
            cron_payload: None,
            observed_at_ms: 10,
            attempt: 2,
            redrive_count: 0,
            collapse_by_dedup_id: false,
            pending_dirty: false,
            subscriber_absent_since_ms: None,
            lease_generation: 1,
            lease_until_ms: Some(20),
            not_before_ms: 0,
            last_error_excerpt: None,
        };

        let payload = dead_letter_payload(&record, "exit=7 stderr=bad");

        assert_eq!(payload["error_class"], "framework_child_nonzero");
        assert_eq!(payload["attempt"], 3);
        assert_eq!(payload["origin_queue"], "jobs");
        assert_eq!(payload["origin_dept"], "worker");
        assert_eq!(payload["dedup_id"], record.delivery_id);
        assert_eq!(payload["source_ref"]["kind"], "cron");
        assert!(payload["fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("framework_child_nonzero:"));
    }

    #[test]
    fn delivery_failure_fact_does_not_own_queue_liveness_contract() {
        let record = DeliveryRecord {
            delivery_id:
                "delivery/v3/raised/queue/merge-ready/dept/pkg.implementer/dedup/issue-70-pr-82"
                    .to_string(),
            queue: "merge-ready".to_string(),
            dept: "pkg.implementer".to_string(),
            payload: json!({
                "queue_liveness": {
                    "item_id": "caller-supplied",
                    "linked_item_id": "caller-supplied",
                    "owner": "caller-supplied",
                    "state": "caller-supplied",
                    "order": 99,
                    "age_seconds": 1,
                    "slo_seconds": 9,
                    "breached": false,
                    "next_action": "caller-supplied",
                },
            }),
            source: Some(SourceRef {
                kind: super::super::delivery_types::SourceKind::External,
                reference: "observability-sample/not-queue-starvation".to_string(),
            }),
            cron_payload: None,
            observed_at_ms: 10_000,
            attempt: 2,
            redrive_count: 0,
            collapse_by_dedup_id: true,
            pending_dirty: false,
            subscriber_absent_since_ms: None,
            lease_generation: 3,
            lease_until_ms: Some(2),
            not_before_ms: 70,
            last_error_excerpt: None,
        };

        let event = delivery_failure_fact(&record, "exit=13 stderr=permission denied");

        assert!(event.payload.get("queue_liveness").is_none());
        assert_eq!(event.payload["origin_queue"], "merge-ready");
        assert_eq!(event.payload["origin_dept"], "pkg.implementer");
    }
}

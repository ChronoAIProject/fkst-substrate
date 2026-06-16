//! Queue-starvation source contract normalization.

use super::delivery_types::SourceRef;
use fkst_common::Event;
use serde_json::{json, Value as JsonValue};

const NEXT_ACTION: &str = "route through normal intake consensus implementation review pipeline";

pub(crate) fn canonicalize_event(event: &mut Event, source: Option<&SourceRef>) {
    let Some(liveness) = queue_liveness_json(event, source) else {
        return;
    };
    event.payload["queue_liveness"] = liveness;
}

fn queue_liveness_json(event: &Event, source: Option<&SourceRef>) -> Option<JsonValue> {
    let source = queue_starvation_source(source?)?;
    let age_seconds = payload_seconds(
        &event.payload,
        &["queue_head_age_seconds", "age_seconds"],
        &["queue_head_age_minutes", "age_minutes"],
    )?;
    let slo_seconds = payload_seconds(
        &event.payload,
        &["threshold_seconds", "slo_seconds"],
        &["threshold_minutes", "slo_minutes"],
    )?;
    let order = payload_u64(&event.payload, &["queue_order", "order"]).unwrap_or(1);
    Some(json!({
        "item_id": source.item_id(),
        "linked_item_id": source.linked_item_id(),
        "queue": event.queue,
        "owner": source.owner,
        "state": source.state,
        "order": order,
        "age_seconds": age_seconds,
        "slo_seconds": slo_seconds,
        "breached": age_seconds >= slo_seconds,
        "next_action": NEXT_ACTION,
    }))
}

struct QueueStarvationSource<'a> {
    repo_owner: &'a str,
    repo_name: &'a str,
    state: &'a str,
    pr_number: &'a str,
    owner: &'a str,
    issue_repo_owner: &'a str,
    issue_repo_name: &'a str,
    issue_number: &'a str,
}

impl QueueStarvationSource<'_> {
    fn item_id(&self) -> String {
        format!(
            "{}/issue/{}/{}/{}",
            self.owner, self.issue_repo_owner, self.issue_repo_name, self.issue_number
        )
    }

    fn linked_item_id(&self) -> String {
        format!(
            "{}/pr/{}/{}/{}",
            self.owner, self.repo_owner, self.repo_name, self.pr_number
        )
    }

    fn has_nonempty_segments(&self) -> bool {
        [
            self.repo_owner,
            self.repo_name,
            self.state,
            self.pr_number,
            self.owner,
            self.issue_repo_owner,
            self.issue_repo_name,
            self.issue_number,
        ]
        .into_iter()
        .all(|segment| !segment.is_empty())
    }
}

fn queue_starvation_source(source: &SourceRef) -> Option<QueueStarvationSource<'_>> {
    let segments = source.reference.split('/').collect::<Vec<_>>();
    let detector = segments
        .iter()
        .position(|segment| *segment == "queue-starvation")?;
    let rest = segments.get(detector + 1..)?;
    if rest.len() < 11 || rest[3] != "pr" || rest[5] != "proposal" || rest[7] != "issue" {
        return None;
    }
    let parsed = QueueStarvationSource {
        repo_owner: rest[0],
        repo_name: rest[1],
        state: rest[2],
        pr_number: rest[4],
        owner: rest[6],
        issue_repo_owner: rest[8],
        issue_repo_name: rest[9],
        issue_number: rest[10],
    };
    parsed.has_nonempty_segments().then_some(parsed)
}

fn payload_seconds(payload: &JsonValue, second_keys: &[&str], minute_keys: &[&str]) -> Option<u64> {
    if let Some(seconds) = payload_u64(payload, second_keys) {
        return Some(seconds);
    }
    payload_u64(payload, minute_keys).map(|minutes| minutes.saturating_mul(60))
}

fn payload_u64(payload: &JsonValue, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        payload
            .get(*key)
            .or_else(|| {
                payload
                    .get("watchdog")
                    .and_then(|watchdog| watchdog.get(*key))
            })
            .and_then(JsonValue::as_u64)
    })
}

#[cfg(test)]
mod tests {
    use super::super::delivery_types::SourceKind;
    use super::*;

    fn queue_starvation_source_ref() -> SourceRef {
        SourceRef {
            kind: SourceKind::External,
            reference: "observability-sample/queue-starvation/ChronoAIProject/fkst-substrate/merge-ready/pr/82/proposal/github-devloop/issue/ChronoAIProject/fkst-substrate/70/version/ready-consensus-github-devloo-0129659072".to_string(),
        }
    }

    #[test]
    fn queue_starvation_source_owns_canonical_liveness_contract() {
        let mut event = Event::new(
            "merge-ready",
            json!({
                "queue_head_age_minutes": 3898_u64,
                "threshold_minutes": 60_u64,
                "queue_order": 1,
                "queue_liveness": {
                    "item_id": "caller-supplied",
                    "linked_item_id": "caller-supplied",
                    "owner": "caller-supplied",
                    "state": "caller-supplied",
                    "order": 99,
                    "age_seconds": 1,
                    "slo_seconds": 9,
                    "breached": false,
                    "next_action": "caller-supplied"
                }
            }),
        );

        canonicalize_event(&mut event, Some(&queue_starvation_source_ref()));

        let liveness = &event.payload["queue_liveness"];
        assert_eq!(
            liveness["item_id"],
            "github-devloop/issue/ChronoAIProject/fkst-substrate/70"
        );
        assert_eq!(
            liveness["linked_item_id"],
            "github-devloop/pr/ChronoAIProject/fkst-substrate/82"
        );
        assert_eq!(liveness["queue"], "merge-ready");
        assert_eq!(liveness["owner"], "github-devloop");
        assert_eq!(liveness["state"], "merge-ready");
        assert_eq!(liveness["order"], 1);
        assert_eq!(liveness["age_seconds"], 3898_u64 * 60);
        assert_eq!(liveness["slo_seconds"], 60_u64 * 60);
        assert_eq!(liveness["breached"], true);
        assert_eq!(liveness["next_action"], NEXT_ACTION);
    }

    #[test]
    fn non_queue_starvation_source_does_not_accept_caller_liveness() {
        let mut event = Event::new(
            "merge-ready",
            json!({
                "queue_liveness": {
                    "item_id": "caller-supplied"
                }
            }),
        );
        let source = SourceRef {
            kind: SourceKind::External,
            reference: "observability-sample/not-queue-starvation".to_string(),
        };

        canonicalize_event(&mut event, Some(&source));

        assert_eq!(
            event.payload["queue_liveness"]["item_id"],
            "caller-supplied"
        );
    }
}

//! Durable/ephemeral delivery split for supervise producers.

use super::delivery_store::DeliveryStore;
use super::delivery_types::{DeliveryRecord, SourceKind, SourceRef};
use super::event_fanout::Fanout;
use anyhow::{anyhow, bail, Context, Result};
use fkst_common::config::Config;
use fkst_common::validate_runtime_key;
use fkst_common::Event;
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::warn;

const MAX_DURABLE_PAYLOAD_BYTES: usize = 64 * 1024;
const DELIVERY_ID_PART_CHUNK_BYTES: usize = 200;

#[derive(Clone)]
pub(crate) struct DeliveryRouter {
    fanout: Fanout,
    store: Option<Arc<DeliveryStore>>,
    subscriptions: Arc<BTreeMap<String, Vec<Subscription>>>,
    reliable_wakes: Arc<Mutex<BTreeMap<String, mpsc::Sender<()>>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Subscription {
    pub dept: String,
    pub reliable: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PublishEnvelope {
    pub event: Event,
    pub source: Option<SourceRef>,
    pub cron_payload: Option<JsonValue>,
    pub derived: Option<DerivedDelivery>,
}

#[derive(Clone, Debug)]
pub(crate) struct DerivedDelivery {
    pub parent_delivery_id: String,
    pub ordinal: usize,
}

impl DeliveryRouter {
    pub(crate) fn new(cfg: &Config, fanout: Fanout, store: Option<Arc<DeliveryStore>>) -> Self {
        Self {
            fanout,
            store,
            subscriptions: Arc::new(subscriptions(cfg)),
            reliable_wakes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub(crate) fn has_reliable_subscriptions(cfg: &Config) -> bool {
        cfg.department.values().any(|dept| {
            dept.consumes
                .iter()
                .any(|queue| !dept.ephemeral.iter().any(|ephemeral| ephemeral == queue))
        })
    }

    pub(crate) fn register_reliable_wake(&self, dept: &str, wake: mpsc::Sender<()>) {
        match self.reliable_wakes.lock() {
            Ok(mut wakes) => {
                wakes.insert(dept.to_string(), wake);
            }
            Err(err) => {
                warn!(dept = %dept, error = %err, "reliable wake registry lock failed");
            }
        }
    }

    pub(crate) fn publish(&self, envelope: PublishEnvelope) -> Result<()> {
        let queue = envelope.event.queue.clone();
        let subscribers = self
            .subscriptions
            .get(&queue)
            .ok_or_else(|| anyhow!("queue `{}` has no delivery subscriptions", queue))?;
        let mut sent_ephemeral = false;
        for sub in subscribers {
            if sub.reliable {
                let source = envelope.source.clone().ok_or_else(|| {
                    anyhow!(
                        "reliable delivery to queue `{}` for dept `{}` requires source_ref",
                        queue,
                        sub.dept
                    )
                })?;
                let store = self
                    .store
                    .as_ref()
                    .ok_or_else(|| anyhow!("reliable delivery store is not open"))?;
                ensure_payload_within_bound(&envelope.event.payload)?;
                let record = DeliveryRecord {
                    delivery_id: derive_delivery_id(
                        &queue,
                        &sub.dept,
                        &source,
                        envelope.derived.as_ref(),
                    ),
                    queue: queue.clone(),
                    dept: sub.dept.clone(),
                    payload: envelope.event.payload.clone(),
                    source: Some(source),
                    cron_payload: envelope.cron_payload.clone(),
                    observed_at_ms: envelope.event.ts,
                    attempt: 0,
                    lease_generation: 0,
                    lease_until_ms: None,
                    not_before_ms: now_unix_millis(),
                    last_error_excerpt: None,
                };
                store
                    .enqueue(&record)
                    .with_context(|| format!("enqueue delivery `{}`", record.delivery_id))?;
                self.notify_reliable(&sub.dept);
            } else {
                sent_ephemeral = true;
            }
        }
        if sent_ephemeral {
            self.fanout.send(&queue, envelope.event.clone())?;
        }
        Ok(())
    }

    fn notify_reliable(&self, dept: &str) {
        let wake = match self.reliable_wakes.lock() {
            Ok(wakes) => wakes.get(dept).cloned(),
            Err(err) => {
                warn!(dept = %dept, error = %err, "reliable wake registry lock failed");
                None
            }
        };
        let Some(wake) = wake else {
            warn!(dept = %dept, "reliable wake receiver not registered");
            return;
        };
        if let Err(err) = wake.try_send(()) {
            warn!(dept = %dept, error = %err, "reliable wake notify failed");
        }
    }
}

fn subscriptions(cfg: &Config) -> BTreeMap<String, Vec<Subscription>> {
    let mut by_queue: BTreeMap<String, Vec<Subscription>> = BTreeMap::new();
    for (dept_name, dept) in &cfg.department {
        let ephemeral: BTreeSet<&String> = dept.ephemeral.iter().collect();
        for queue in &dept.consumes {
            by_queue
                .entry(queue.clone())
                .or_default()
                .push(Subscription {
                    dept: dept_name.clone(),
                    reliable: !ephemeral.contains(queue),
                });
        }
    }
    for subs in by_queue.values_mut() {
        subs.sort_by(|left, right| left.dept.cmp(&right.dept));
    }
    by_queue
}

pub(crate) fn derive_delivery_id(
    queue: &str,
    dept: &str,
    source: &SourceRef,
    derived: Option<&DerivedDelivery>,
) -> String {
    let key = if let Some(derived) = derived {
        runtime_key([
            "delivery",
            "v1",
            "raised",
            "queue",
            queue,
            "dept",
            dept,
            "parent",
            &derived.parent_delivery_id,
            "ordinal",
            &derived.ordinal.to_string(),
        ])
    } else {
        runtime_key([
            "delivery",
            "v1",
            "source",
            source_kind_key(&source.kind),
            "queue",
            queue,
            "dept",
            dept,
            "ref",
            &source.reference,
        ])
    };
    validate_runtime_key(&key).expect("delivery id should be a runtime-safe key");
    key
}

fn runtime_key<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut segments = Vec::new();
    for part in parts {
        segments.extend(runtime_key_part_segments(part));
    }
    segments.join("/")
}

fn runtime_key_part_segments(part: &str) -> Vec<String> {
    let encoded = encode_runtime_key_part(part);
    encoded
        .as_bytes()
        .chunks(DELIVERY_ID_PART_CHUNK_BYTES)
        .map(|chunk| std::str::from_utf8(chunk).expect("runtime key part should be ASCII"))
        .map(|chunk| {
            if chunk.bytes().all(|byte| byte == b'.') {
                format!("_x{chunk}")
            } else {
                chunk.to_string()
            }
        })
        .collect()
}

fn encode_runtime_key_part(part: &str) -> String {
    if part.is_empty() {
        return "_".to_string();
    }
    let mut encoded = String::new();
    for byte in part.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' => encoded.push(byte as char),
            b'_' => encoded.push_str("__"),
            _ => encoded.push_str(&format!("_x{byte:02X}")),
        }
    }
    if encoded.bytes().all(|byte| byte == b'.') {
        format!("_x{}", encoded)
    } else {
        encoded
    }
}

fn source_kind_key(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::File => "file_watch",
        SourceKind::Cron => "cron",
        SourceKind::Git => "git",
        SourceKind::External => "external",
    }
}

fn ensure_payload_within_bound(payload: &JsonValue) -> Result<()> {
    let bytes = serde_json::to_vec(payload)?;
    if bytes.len() > MAX_DURABLE_PAYLOAD_BYTES {
        bail!(
            "durable delivery payload exceeds {} bytes: {}",
            MAX_DURABLE_PAYLOAD_BYTES,
            bytes.len()
        );
    }
    Ok(())
}

pub(crate) fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::super::delivery_store::DeliveryStore;
    use super::*;
    use fkst_common::config::{Config, DepartmentDecl, LimitsDecl, QueueDecl};
    use std::time::Duration;
    use tempfile::TempDir;

    fn config(ephemeral: bool) -> Config {
        let mut queue = BTreeMap::new();
        queue.insert(
            "jobs".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let mut department = BTreeMap::new();
        department.insert(
            "worker".to_string(),
            DepartmentDecl {
                lua: "departments/worker/main.lua".into(),
                consumes: vec!["jobs".to_string()],
                produces: Vec::new(),
                ephemeral: if ephemeral {
                    vec!["jobs".to_string()]
                } else {
                    Vec::new()
                },
                stall_window: "30s".to_string(),
                retry: None,
            },
        );
        Config {
            queue,
            raiser: BTreeMap::new(),
            department,
            limits: LimitsDecl {
                global_codex_processes: 1,
            },
        }
    }

    #[test]
    fn reliable_publish_requires_source_ref() {
        let cfg = config(false);
        let router = DeliveryRouter::new(&cfg, Fanout::new(), None);

        let err = router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"n": 1})),
                source: None,
                cron_payload: None,
                derived: None,
            })
            .unwrap_err();

        assert!(err.to_string().contains("requires source_ref"), "{err}");
    }

    #[tokio::test]
    async fn ephemeral_publish_uses_fanout_without_store() {
        let cfg = config(true);
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("jobs", 8).await;
        let router = DeliveryRouter::new(&cfg, fanout, None);

        router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"n": 1})),
                source: None,
                cron_payload: None,
                derived: None,
            })
            .unwrap();

        let got = rx.recv().await.unwrap();
        assert_eq!(got.queue, "jobs");
        assert_eq!(got.payload, serde_json::json!({"n": 1}));
    }

    #[test]
    fn reliable_publish_enqueues_durable_record() {
        let cfg = config(false);
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()));

        router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"n": 1})),
                source: Some(SourceRef {
                    kind: SourceKind::Cron,
                    reference: "tick".to_string(),
                }),
                cron_payload: Some(serde_json::json!({"raiser": "tick"})),
                derived: None,
            })
            .unwrap();

        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_secs(30))
            .unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].queue, "jobs");
        assert_eq!(leased[0].dept, "worker");
        assert_eq!(leased[0].payload, serde_json::json!({"n": 1}));
        assert_eq!(leased[0].source.as_ref().unwrap().reference, "tick");
        assert_eq!(
            leased[0].delivery_id,
            "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick"
        );
    }

    #[test]
    fn source_delivery_id_ignores_process_observation_time() {
        let source = SourceRef {
            kind: SourceKind::File,
            reference: "/tmp/input.txt/len/4/mtime/1000".to_string(),
        };
        let first = derive_delivery_id("jobs", "worker", &source, None);
        let second = derive_delivery_id("jobs", "worker", &source, None);

        assert_eq!(first, second);
        assert_eq!(
            first,
            "delivery/v1/source/file__watch/queue/jobs/dept/worker/ref/_x2Ftmp_x2Finput.txt_x2Flen_x2F4_x2Fmtime_x2F1000"
        );
    }

    #[test]
    fn raised_delivery_id_uses_parent_ordinal_queue_and_dept() {
        let source = SourceRef {
            kind: SourceKind::Cron,
            reference: "tick/slot/1000".to_string(),
        };
        let derived = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick"
                .to_string(),
            ordinal: 2,
        };

        let id = derive_delivery_id("next", "next_worker", &source, Some(&derived));

        assert_eq!(
            id,
            "delivery/v1/raised/queue/next/dept/next__worker/parent/delivery_x2Fv1_x2Fsource_x2Fcron_x2Fqueue_x2Fjobs_x2Fdept_x2Fworker_x2Fref_x2Ftick/ordinal/2"
        );
    }

    #[tokio::test]
    async fn reliable_publish_does_not_send_fanout_event() {
        let cfg = config(false);
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("jobs", 8).await;
        let router = DeliveryRouter::new(&cfg, fanout, Some(store));

        router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"n": 1})),
                source: Some(SourceRef {
                    kind: SourceKind::Cron,
                    reference: "tick".to_string(),
                }),
                cron_payload: None,
                derived: None,
            })
            .unwrap();

        assert!(tokio::time::timeout(Duration::from_millis(50), rx.recv())
            .await
            .is_err());
    }

    #[test]
    fn reliable_publish_rejects_oversized_payload() {
        let cfg = config(false);
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store));

        let err = router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"blob": "x".repeat(70 * 1024)})),
                source: Some(SourceRef {
                    kind: SourceKind::Cron,
                    reference: "tick".to_string(),
                }),
                cron_payload: None,
                derived: None,
            })
            .unwrap_err();

        assert!(err.to_string().contains("durable delivery payload exceeds"));
    }
}

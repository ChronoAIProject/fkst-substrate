//! Durable/ephemeral delivery split for supervise producers.

use super::delivery_store::DeliveryStore;
use super::delivery_types::{DeliveryRecord, SourceKind, SourceRef};
use super::event_fanout::Fanout;
use super::failure_fact::{FAILURE_FACT_QUEUE, FAILURE_FACT_SCHEMA};
use super::journal::SupervisorJournal;
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
    journal: SupervisorJournal,
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
    pub derived: bool,
}

impl DeliveryRouter {
    pub(crate) fn new(
        cfg: &Config,
        fanout: Fanout,
        store: Option<Arc<DeliveryStore>>,
        journal: Option<SupervisorJournal>,
    ) -> Self {
        Self {
            fanout,
            store,
            subscriptions: Arc::new(subscriptions(cfg)),
            reliable_wakes: Arc::new(Mutex::new(BTreeMap::new())),
            journal: journal.unwrap_or_else(SupervisorJournal::disabled),
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
        self.journal.event(
            "published",
            [
                ("queue", queue.clone()),
                ("subscriber_count", subscribers.len().to_string()),
                ("reason", publish_reason(&envelope).to_string()),
            ],
        );
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
                    delivery_id: derive_delivery_id(&queue, &sub.dept, &source, &envelope)?,
                    queue: queue.clone(),
                    dept: sub.dept.clone(),
                    payload: envelope.event.payload.clone(),
                    source: Some(source),
                    cron_payload: envelope.cron_payload.clone(),
                    observed_at_ms: envelope.event.ts,
                    attempt: 0,
                    redrive_count: 0,
                    lease_generation: 0,
                    lease_until_ms: None,
                    not_before_ms: now_unix_millis(),
                    last_error_excerpt: None,
                };
                store
                    .enqueue(&record)
                    .with_context(|| format!("enqueue delivery `{}`", record.delivery_id))?;
                self.journal.event(
                    "delivered",
                    [
                        ("queue", queue.clone()),
                        ("dept", sub.dept.clone()),
                        ("delivery_id", record.delivery_id.clone()),
                        ("reason", "durable_enqueue".to_string()),
                    ],
                );
                self.notify_reliable(&sub.dept);
            } else {
                self.journal.event(
                    "delivered",
                    [
                        ("queue", queue.clone()),
                        ("dept", sub.dept.clone()),
                        ("delivery_id", "ephemeral".to_string()),
                        ("reason", "ephemeral_fanout".to_string()),
                    ],
                );
                sent_ephemeral = true;
            }
        }
        if sent_ephemeral {
            self.fanout.send(&queue, envelope.event.clone())?;
        }
        Ok(())
    }

    pub(crate) fn publish_failure_fact(&self, event: Event) -> Result<()> {
        if event.queue != FAILURE_FACT_QUEUE {
            bail!("failure fact queue must be `{}`", FAILURE_FACT_QUEUE);
        }
        let Some(subscribers) = self.subscriptions.get(FAILURE_FACT_QUEUE) else {
            return Ok(());
        };
        if subscribers.is_empty() {
            return Ok(());
        }
        let source = failure_fact_source(&event);
        self.publish(PublishEnvelope {
            event,
            source: Some(source),
            cron_payload: None,
            derived: false,
        })
    }

    pub(crate) fn failure_fact_publisher(&self) -> FailureFactPublisher {
        FailureFactPublisher {
            router: self.clone(),
        }
    }

    pub(crate) fn notify_reliable_public(&self, dept: &str) {
        self.notify_reliable(dept);
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

fn publish_reason(envelope: &PublishEnvelope) -> &'static str {
    if envelope.derived {
        "raised"
    } else if envelope.cron_payload.is_some() {
        "cron"
    } else if envelope.source.is_some() {
        "source"
    } else {
        "ephemeral"
    }
}

fn failure_fact_source(event: &Event) -> SourceRef {
    let fingerprint = event
        .payload
        .get("fingerprint")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    SourceRef {
        kind: SourceKind::External,
        reference: format!("{FAILURE_FACT_SCHEMA}/{fingerprint}/{}", event.ts),
    }
}

#[derive(Clone)]
pub(crate) struct FailureFactPublisher {
    router: DeliveryRouter,
}

impl FailureFactPublisher {
    pub(crate) fn publish(&self, event: Event) -> Result<()> {
        self.router.publish_failure_fact(event)
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
    envelope: &PublishEnvelope,
) -> Result<String> {
    let key = if envelope.derived {
        if let Some(dedup_key) = payload_dedup_key(&envelope.event.payload) {
            runtime_key([
                "delivery", "v3", "raised", "queue", queue, "dept", dept, "dedup", &dedup_key,
            ])
        } else {
            bail!(
                "reliable raised delivery to queue `{}` for dept `{}` requires payload.dedup_key",
                queue,
                dept
            );
        }
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
    Ok(key)
}

fn payload_dedup_key(payload: &JsonValue) -> Option<String> {
    payload
        .get("dedup_key")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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

    fn source_envelope(queue: &str, payload: JsonValue) -> PublishEnvelope {
        PublishEnvelope {
            event: Event::new(queue, payload),
            source: None,
            cron_payload: None,
            derived: false,
        }
    }

    fn raised_envelope(queue: &str, payload: JsonValue) -> PublishEnvelope {
        PublishEnvelope {
            event: Event::new(queue, payload),
            source: None,
            cron_payload: None,
            derived: true,
        }
    }

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
                owner_root: std::path::PathBuf::from("."),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["jobs".to_string()],
                produces: Vec::new(),
                ephemeral: if ephemeral {
                    vec!["jobs".to_string()]
                } else {
                    Vec::new()
                },
                stall_window: "30s".to_string(),
                graph_json: false,
                retry: None,
            },
        );
        Config {
            queue,
            raiser: BTreeMap::new(),
            department,
            limits: LimitsDecl {
                global_codex_processes: 1,
                global_department_child_processes: 16,
                department_child_processes_per_dept: 4,
            },
        }
    }

    fn namespaced_config() -> Config {
        let mut queue = BTreeMap::new();
        for name in ["pkg.jobs", "other.jobs"] {
            queue.insert(
                name.to_string(),
                QueueDecl {
                    capacity: 8,
                    fanout: false,
                },
            );
        }
        let mut department = BTreeMap::new();
        for (dept, queue_name, owner_namespace) in [
            ("pkg.worker", "pkg.jobs", "pkg"),
            ("other.worker", "other.jobs", "other"),
        ] {
            department.insert(
                dept.to_string(),
                DepartmentDecl {
                    lua: format!("departments/{dept}/main.lua").into(),
                    owner_root: std::path::PathBuf::from("."),
                    owner_namespace: owner_namespace.to_string(),
                    consumes: vec![queue_name.to_string()],
                    produces: Vec::new(),
                    ephemeral: Vec::new(),
                    stall_window: "30s".to_string(),
                    graph_json: false,
                    retry: None,
                },
            );
        }
        Config {
            queue,
            raiser: BTreeMap::new(),
            department,
            limits: LimitsDecl {
                global_codex_processes: 1,
                global_department_child_processes: 16,
                department_child_processes_per_dept: 4,
            },
        }
    }

    #[test]
    fn reliable_publish_requires_source_ref() {
        let cfg = config(false);
        let router = DeliveryRouter::new(&cfg, Fanout::new(), None, None);

        let err = router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"n": 1})),
                source: None,
                cron_payload: None,
                derived: false,
            })
            .unwrap_err();

        assert!(err.to_string().contains("requires source_ref"), "{err}");
    }

    #[tokio::test]
    async fn ephemeral_publish_uses_fanout_without_store() {
        let cfg = config(true);
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("jobs", 8).await;
        let router = DeliveryRouter::new(&cfg, fanout, None, None);

        router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"n": 1})),
                source: None,
                cron_payload: None,
                derived: false,
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
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);

        router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"n": 1})),
                source: Some(SourceRef {
                    kind: SourceKind::Cron,
                    reference: "tick".to_string(),
                }),
                cron_payload: Some(serde_json::json!({"raiser": "tick"})),
                derived: false,
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
    fn reliable_publish_uses_namespaced_queue_and_dept_in_delivery_id() {
        let cfg = namespaced_config();
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);
        let source = SourceRef {
            kind: SourceKind::Cron,
            reference: "pkg.tick".to_string(),
        };

        for queue in ["pkg.jobs", "other.jobs"] {
            router
                .publish(PublishEnvelope {
                    event: Event::new(queue, serde_json::json!({"queue": queue})),
                    source: Some(source.clone()),
                    cron_payload: None,
                    derived: false,
                })
                .unwrap();
        }

        let pkg = store
            .lease_for_dept("pkg.worker", now_unix_millis(), 8, Duration::from_secs(30))
            .unwrap();
        let other = store
            .lease_for_dept(
                "other.worker",
                now_unix_millis(),
                8,
                Duration::from_secs(30),
            )
            .unwrap();

        assert_eq!(pkg.len(), 1);
        assert_eq!(pkg[0].queue, "pkg.jobs");
        assert_eq!(pkg[0].dept, "pkg.worker");
        assert_eq!(pkg[0].source.as_ref().unwrap().reference, "pkg.tick");
        assert_eq!(
            pkg[0].delivery_id,
            "delivery/v1/source/cron/queue/pkg.jobs/dept/pkg.worker/ref/pkg.tick"
        );

        assert_eq!(other.len(), 1);
        assert_eq!(other[0].queue, "other.jobs");
        assert_eq!(other[0].dept, "other.worker");
        assert_eq!(other[0].source.as_ref().unwrap().reference, "pkg.tick");
        assert_eq!(
            other[0].delivery_id,
            "delivery/v1/source/cron/queue/other.jobs/dept/other.worker/ref/pkg.tick"
        );
        assert_ne!(pkg[0].delivery_id, other[0].delivery_id);
    }

    #[test]
    fn source_delivery_id_ignores_process_observation_time() {
        let source = SourceRef {
            kind: SourceKind::File,
            reference: "/tmp/input.txt/len/4/mtime/1000".to_string(),
        };
        let first = derive_delivery_id(
            "jobs",
            "worker",
            &source,
            &source_envelope("jobs", serde_json::json!({"n": 1})),
        )
        .unwrap();
        let second = derive_delivery_id(
            "jobs",
            "worker",
            &source,
            &source_envelope("jobs", serde_json::json!({"n": 2})),
        )
        .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first,
            "delivery/v1/source/file__watch/queue/jobs/dept/worker/ref/_x2Ftmp_x2Finput.txt_x2Flen_x2F4_x2Fmtime_x2F1000"
        );
    }

    #[test]
    fn raised_delivery_id_uses_payload_dedup_key_when_present() {
        let source = SourceRef {
            kind: SourceKind::Cron,
            reference: "tick/slot/1000".to_string(),
        };
        let first = raised_envelope("next", serde_json::json!({"dedup_key": "issue/67", "n": 1}));
        let second = raised_envelope("next", serde_json::json!({"dedup_key": "issue/67", "n": 1}));

        let first_id = derive_delivery_id("next", "next_worker", &source, &first).unwrap();
        let second_id = derive_delivery_id("next", "next_worker", &source, &second).unwrap();

        assert_eq!(
            first_id,
            "delivery/v3/raised/queue/next/dept/next__worker/dedup/issue_x2F67"
        );
        assert_eq!(first_id, second_id);
    }

    #[test]
    fn reliable_raised_publish_collapses_same_dedup_key_across_parent_ticks() {
        let mut queue = BTreeMap::new();
        queue.insert(
            "next".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let mut department = BTreeMap::new();
        department.insert(
            "next_worker".to_string(),
            DepartmentDecl {
                lua: "departments/next_worker/main.lua".into(),
                owner_root: std::path::PathBuf::from("."),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["next".to_string()],
                produces: Vec::new(),
                ephemeral: Vec::new(),
                stall_window: "30s".to_string(),
                graph_json: false,
                retry: None,
            },
        );
        let cfg = Config {
            queue,
            raiser: BTreeMap::new(),
            department,
            limits: LimitsDecl {
                global_codex_processes: 1,
                global_department_child_processes: 16,
                department_child_processes_per_dept: 4,
            },
        };
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);
        let source = SourceRef {
            kind: SourceKind::Cron,
            reference: "tick".to_string(),
        };

        for _parent in ["tick-a", "tick-b"] {
            router
                .publish(PublishEnvelope {
                    event: Event::new("next", serde_json::json!({"dedup_key": "issue/67"})),
                    source: Some(source.clone()),
                    cron_payload: None,
                    derived: true,
                })
                .unwrap();
        }

        let leased = store
            .lease_for_dept("next_worker", now_unix_millis(), 8, Duration::from_secs(30))
            .unwrap();

        assert_eq!(leased.len(), 1);
        assert_eq!(
            leased[0].delivery_id,
            "delivery/v3/raised/queue/next/dept/next__worker/dedup/issue_x2F67"
        );
        assert_eq!(
            leased[0].payload,
            serde_json::json!({"dedup_key": "issue/67"})
        );
    }

    #[test]
    fn reliable_raised_delivery_id_without_dedup_key_is_rejected() {
        let source = SourceRef {
            kind: SourceKind::Cron,
            reference: "tick/slot/1000".to_string(),
        };
        let envelope = raised_envelope("next", serde_json::json!({"n": 1}));

        let err = derive_delivery_id("next", "next_worker", &source, &envelope).unwrap_err();
        let message = err.to_string();

        assert!(
            message.contains(
                "reliable raised delivery to queue `next` for dept `next_worker` requires payload.dedup_key"
            ),
            "{message}"
        );
    }

    #[test]
    fn reliable_raised_delivery_id_with_empty_dedup_key_is_rejected() {
        let source = SourceRef {
            kind: SourceKind::Cron,
            reference: "tick/slot/1000".to_string(),
        };
        let envelope = raised_envelope("next", serde_json::json!({"dedup_key": ""}));

        let err = derive_delivery_id("next", "next_worker", &source, &envelope).unwrap_err();
        let message = err.to_string();

        assert!(
            message.contains(
                "reliable raised delivery to queue `next` for dept `next_worker` requires payload.dedup_key"
            ),
            "{message}"
        );
    }

    #[tokio::test]
    async fn reliable_publish_does_not_send_fanout_event() {
        let cfg = config(false);
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("jobs", 8).await;
        let router = DeliveryRouter::new(&cfg, fanout, Some(store), None);

        router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"n": 1})),
                source: Some(SourceRef {
                    kind: SourceKind::Cron,
                    reference: "tick".to_string(),
                }),
                cron_payload: None,
                derived: false,
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
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store), None);

        let err = router
            .publish(PublishEnvelope {
                event: Event::new("jobs", serde_json::json!({"blob": "x".repeat(70 * 1024)})),
                source: Some(SourceRef {
                    kind: SourceKind::Cron,
                    reference: "tick".to_string(),
                }),
                cron_payload: None,
                derived: false,
            })
            .unwrap_err();

        assert!(err.to_string().contains("durable delivery payload exceeds"));
    }
}

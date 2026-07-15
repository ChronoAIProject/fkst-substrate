//! Durable/ephemeral delivery split for supervise producers.

use super::delivery_store::DeliveryStore;
use super::delivery_types::{DeliveryRecord, SourceKind, SourceRef};
use super::event_fanout::Fanout;
use super::failure_fact::{FAILURE_FACT_QUEUE, FAILURE_FACT_SCHEMA};
use super::journal::SupervisorJournal;
use super::queue_starvation;
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
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
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
    pub(crate) fn new(
        cfg: &Config,
        fanout: Fanout,
        store: Option<Arc<DeliveryStore>>,
        journal: Option<SupervisorJournal>,
    ) -> Self {
        Self::new_with_clock(cfg, fanout, store, journal, Arc::new(now_unix_millis))
    }

    pub(crate) fn new_with_clock(
        cfg: &Config,
        fanout: Fanout,
        store: Option<Arc<DeliveryStore>>,
        journal: Option<SupervisorJournal>,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            fanout,
            store,
            subscriptions: Arc::new(subscriptions(cfg)),
            reliable_wakes: Arc::new(Mutex::new(BTreeMap::new())),
            journal: journal.unwrap_or_else(SupervisorJournal::disabled),
            clock,
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

    pub(crate) fn publish(&self, mut envelope: PublishEnvelope) -> Result<()> {
        queue_starvation::canonicalize_event(&mut envelope.event, envelope.source.as_ref());
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
                let raised_dedup_key = envelope
                    .event
                    .payload
                    .get("dedup_key")
                    .and_then(JsonValue::as_str)
                    .filter(|key| !key.is_empty());
                let raised_source_ref = payload_source_ref(&envelope.event.payload);
                let delivery_identity = derive_delivery_identity(
                    &queue,
                    &sub.dept,
                    Some(&source),
                    envelope.derived.as_ref(),
                    raised_dedup_key,
                    raised_source_ref.as_ref(),
                );
                let record = DeliveryRecord {
                    delivery_id: delivery_identity.delivery_id,
                    queue: queue.clone(),
                    dept: sub.dept.clone(),
                    payload: envelope.event.payload.clone(),
                    source: Some(source),
                    cron_payload: envelope.cron_payload.clone(),
                    observed_at_ms: envelope.event.ts,
                    attempt: 0,
                    redrive_count: 0,
                    collapse_by_dedup_id: delivery_identity.collapse_by_dedup_id,
                    subscriber_absent_since_ms: None,
                    lease_generation: 0,
                    lease_until_ms: None,
                    not_before_ms: (self.clock)(),
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
            derived: None,
        })
    }

    pub(crate) fn failure_fact_publisher(&self) -> FailureFactPublisher {
        FailureFactPublisher {
            router: self.clone(),
        }
    }

    pub(crate) fn route_subscriptions_for_test(cfg: &Config, queue: &str) -> Vec<Subscription> {
        subscriptions(cfg).remove(queue).unwrap_or_default()
    }

    pub(crate) fn current_subscriber_queues(&self) -> BTreeSet<String> {
        self.subscriptions.keys().cloned().collect()
    }

    pub(crate) fn reliable_subscriber_queues(&self) -> BTreeSet<String> {
        self.subscriptions
            .iter()
            .filter(|(_, subscriptions)| {
                subscriptions
                    .iter()
                    .any(|subscription| subscription.reliable)
            })
            .map(|(queue, _)| queue.clone())
            .collect()
    }

    pub(crate) fn single_reliable_subscribers_by_queue(&self) -> BTreeMap<String, String> {
        self.subscriptions
            .iter()
            .filter_map(|(queue, subscriptions)| {
                let reliable = subscriptions
                    .iter()
                    .filter(|subscription| subscription.reliable)
                    .collect::<Vec<_>>();
                if reliable.len() == 1 {
                    Some((queue.clone(), reliable[0].dept.clone()))
                } else {
                    None
                }
            })
            .collect()
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
    if envelope.derived.is_some() {
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
    source: Option<&SourceRef>,
    derived: Option<&DerivedDelivery>,
    raised_dedup_key: Option<&str>,
    raised_source_ref: Option<&SourceRef>,
) -> String {
    derive_delivery_identity(
        queue,
        dept,
        source,
        derived,
        raised_dedup_key,
        raised_source_ref,
    )
    .delivery_id
}

struct DeliveryIdentity {
    delivery_id: String,
    collapse_by_dedup_id: bool,
}

fn derive_delivery_identity(
    queue: &str,
    dept: &str,
    source: Option<&SourceRef>,
    derived: Option<&DerivedDelivery>,
    raised_dedup_key: Option<&str>,
    raised_source_ref: Option<&SourceRef>,
) -> DeliveryIdentity {
    let mut collapse_by_dedup_id = false;
    let key = if let Some(derived) = derived {
        if let Some(dedup_key) = raised_dedup_key.filter(|key| !key.is_empty()) {
            collapse_by_dedup_id = true;
            // dedup_key is an engine-visible raised payload control field like source_ref;
            // durable collapse depends on producers keeping it stable per logical signal.
            runtime_key_v3([
                RuntimeKeyPart::Raw("delivery"),
                RuntimeKeyPart::Raw("v3"),
                RuntimeKeyPart::Raw("raised"),
                RuntimeKeyPart::Raw("queue"),
                RuntimeKeyPart::Raw(queue),
                RuntimeKeyPart::Raw("dept"),
                RuntimeKeyPart::Raw(dept),
                RuntimeKeyPart::Raw("dedup"),
                RuntimeKeyPart::Escaped(dedup_key),
            ])
        } else if let Some(source_ref) = raised_source_ref.or(source) {
            runtime_key_v3([
                RuntimeKeyPart::Raw("delivery"),
                RuntimeKeyPart::Raw("v3"),
                RuntimeKeyPart::Raw("raised-src"),
                RuntimeKeyPart::Raw("queue"),
                RuntimeKeyPart::Raw(queue),
                RuntimeKeyPart::Raw("dept"),
                RuntimeKeyPart::Raw(dept),
                RuntimeKeyPart::Raw("kind"),
                RuntimeKeyPart::Raw(source_kind_key(&source_ref.kind)),
                RuntimeKeyPart::Raw("ref"),
                RuntimeKeyPart::Escaped(&source_ref.reference),
            ])
        } else {
            let parent_hash = stable_hex_hash(&derived.parent_delivery_id);
            runtime_key([
                "delivery",
                "v2",
                "raised",
                "queue",
                queue,
                "dept",
                dept,
                "parent_hash",
                &parent_hash,
                "ordinal",
                &derived.ordinal.to_string(),
            ])
        }
    } else {
        let source = source.expect("source delivery id requires source_ref");
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
    DeliveryIdentity {
        delivery_id: key,
        collapse_by_dedup_id,
    }
}

fn payload_source_ref(payload: &JsonValue) -> Option<SourceRef> {
    let source = payload.get("source_ref")?;
    let kind = source.get("kind").and_then(JsonValue::as_str)?;
    let reference = source
        .get("reference")
        .and_then(JsonValue::as_str)
        .filter(|reference| !reference.is_empty())?;
    let kind = payload_source_kind(kind)?;
    Some(SourceRef {
        kind,
        reference: reference.to_string(),
    })
}

fn payload_source_kind(kind: &str) -> Option<SourceKind> {
    match kind {
        "file" | "file_watch" => Some(SourceKind::File),
        "cron" => Some(SourceKind::Cron),
        "git" => Some(SourceKind::Git),
        "external" => Some(SourceKind::External),
        _ => None,
    }
}

fn stable_hex_hash(value: &str) -> String {
    let digest = sha256(value.as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h = H0;
    let mut message = input.to_vec();
    let bit_len = (message.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0_u32; 64];
        for (idx, word) in w.iter_mut().take(16).enumerate() {
            let offset = idx * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for idx in 16..64 {
            let s0 =
                w[idx - 15].rotate_right(7) ^ w[idx - 15].rotate_right(18) ^ (w[idx - 15] >> 3);
            let s1 = w[idx - 2].rotate_right(17) ^ w[idx - 2].rotate_right(19) ^ (w[idx - 2] >> 10);
            w[idx] = w[idx - 16]
                .wrapping_add(s0)
                .wrapping_add(w[idx - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for idx in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[idx])
                .wrapping_add(w[idx]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0_u8; 32];
    for (idx, word) in h.iter().enumerate() {
        out[idx * 4..idx * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
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

enum RuntimeKeyPart<'a> {
    Raw(&'a str),
    Escaped(&'a str),
}

fn runtime_key_v3<'a>(parts: impl IntoIterator<Item = RuntimeKeyPart<'a>>) -> String {
    let mut segments = Vec::new();
    for part in parts {
        match part {
            RuntimeKeyPart::Raw(part) => segments.push(part.to_string()),
            RuntimeKeyPart::Escaped(part) => segments.extend(runtime_key_part_segments_v3(part)),
        }
    }
    segments.join("/")
}

fn runtime_key_part_segments_v3(part: &str) -> Vec<String> {
    let encoded = encode_runtime_key_part_v3(part);
    encoded
        .as_bytes()
        .chunks(DELIVERY_ID_PART_CHUNK_BYTES)
        .map(|chunk| std::str::from_utf8(chunk).expect("runtime key part should be ASCII"))
        .map(str::to_string)
        .collect()
}

fn encode_runtime_key_part_v3(part: &str) -> String {
    if part.is_empty() {
        return "_".to_string();
    }
    let mut encoded = String::new();
    for byte in part.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("_{byte:02X}_")),
        }
    }
    encoded
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
                owner_root: std::path::PathBuf::from("."),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["jobs".to_string()],
                produces: Vec::new(),
                published_seam: Vec::new(),
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
                    published_seam: Vec::new(),
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
            },
        }
    }

    fn queue_starvation_config(ephemeral: bool) -> Config {
        let mut queue = BTreeMap::new();
        queue.insert(
            "merge-ready".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let mut department = BTreeMap::new();
        department.insert(
            "pkg.implementer".to_string(),
            DepartmentDecl {
                lua: "departments/implementer/main.lua".into(),
                owner_root: std::path::PathBuf::from("."),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["merge-ready".to_string()],
                produces: Vec::new(),
                published_seam: Vec::new(),
                ephemeral: if ephemeral {
                    vec!["merge-ready".to_string()]
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
            },
        }
    }

    fn cron_source(reference: &str) -> SourceRef {
        SourceRef {
            kind: SourceKind::Cron,
            reference: reference.to_string(),
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
                derived: None,
            })
            .unwrap_err();

        assert!(err.to_string().contains("requires source_ref"), "{err}");
    }

    #[test]
    fn current_subscriber_queues_are_derived_from_router_subscriptions() {
        let cfg = namespaced_config();
        let router = DeliveryRouter::new(&cfg, Fanout::new(), None, None);

        assert_eq!(
            router.current_subscriber_queues(),
            BTreeSet::from(["other.jobs".to_string(), "pkg.jobs".to_string()])
        );
    }

    #[test]
    fn reliable_subscriber_queues_exclude_ephemeral_only_subscriptions() {
        let cfg = config(true);
        let router = DeliveryRouter::new(&cfg, Fanout::new(), None, None);

        assert_eq!(
            router.current_subscriber_queues(),
            BTreeSet::from(["jobs".to_string()])
        );
        assert_eq!(router.reliable_subscriber_queues(), BTreeSet::new());
    }

    #[test]
    fn single_reliable_subscribers_are_derived_from_router_subscriptions() {
        let cfg = namespaced_config();
        let router = DeliveryRouter::new(&cfg, Fanout::new(), None, None);

        assert_eq!(
            router.single_reliable_subscribers_by_queue(),
            BTreeMap::from([
                ("other.jobs".to_string(), "other.worker".to_string()),
                ("pkg.jobs".to_string(), "pkg.worker".to_string())
            ])
        );
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
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);

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
                    derived: None,
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
    fn queue_starvation_publish_path_stores_canonical_liveness_fact() {
        let cfg = queue_starvation_config(false);
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);
        let mut event = Event::new(
            "merge-ready",
            serde_json::json!({
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
        event.ts = 10_000;

        router
            .publish(PublishEnvelope {
                event,
                source: Some(SourceRef {
                    kind: SourceKind::External,
                    reference: "observability-sample/queue-starvation/ChronoAIProject/fkst-substrate/merge-ready/pr/82/proposal/github-devloop/issue/ChronoAIProject/fkst-substrate/70/version/ready-consensus-github-devloo-0129659072".to_string(),
                }),
                cron_payload: None,
                derived: None,
            })
            .unwrap();

        let leased = store
            .lease_for_dept(
                "pkg.implementer",
                now_unix_millis(),
                8,
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(leased.len(), 1);

        let liveness = &leased[0].payload["queue_liveness"];
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
        assert_eq!(
            liveness["next_action"],
            "route through normal intake consensus implementation review pipeline"
        );
    }

    #[tokio::test]
    async fn queue_starvation_ephemeral_publish_path_emits_canonical_liveness_fact() {
        let cfg = queue_starvation_config(true);
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("merge-ready", 8).await;
        let router = DeliveryRouter::new(&cfg, fanout, None, None);

        router
            .publish(PublishEnvelope {
                event: Event::new(
                    "merge-ready",
                    serde_json::json!({
                        "watchdog": {
                            "age_seconds": 233_880_u64,
                            "slo_seconds": 3_600_u64,
                            "order": 1
                        },
                        "queue_liveness": {
                            "item_id": "caller-supplied"
                        }
                    }),
                ),
                source: Some(SourceRef {
                    kind: SourceKind::External,
                    reference: "observability-sample/queue-starvation/ChronoAIProject/fkst-substrate/merge-ready/pr/82/proposal/github-devloop/issue/ChronoAIProject/fkst-substrate/70/version/ready-consensus-github-devloo-0129659072".to_string(),
                }),
                cron_payload: None,
                derived: None,
            })
            .unwrap();

        let got = rx.recv().await.unwrap();
        let liveness = &got.payload["queue_liveness"];
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
        assert_eq!(liveness["age_seconds"], 233_880_u64);
        assert_eq!(liveness["slo_seconds"], 3_600_u64);
        assert_eq!(liveness["breached"], true);
        assert_eq!(
            liveness["next_action"],
            "route through normal intake consensus implementation review pipeline"
        );
    }

    #[test]
    fn source_delivery_id_ignores_process_observation_time() {
        let source = SourceRef {
            kind: SourceKind::File,
            reference: "/tmp/input.txt/len/4/mtime/1000".to_string(),
        };
        let first = derive_delivery_id("jobs", "worker", Some(&source), None, None, None);
        let second = derive_delivery_id("jobs", "worker", Some(&source), None, None, None);

        assert_eq!(first, second);
        assert_eq!(
            first,
            "delivery/v1/source/file__watch/queue/jobs/dept/worker/ref/_x2Ftmp_x2Finput.txt_x2Flen_x2F4_x2Fmtime_x2F1000"
        );
    }

    #[test]
    fn raised_delivery_id_collapses_same_dedup_key_across_parents() {
        let first_parent = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick-1"
                .to_string(),
            ordinal: 0,
        };
        let second_parent = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick-2"
                .to_string(),
            ordinal: 7,
        };

        let first = derive_delivery_id(
            "next",
            "next_worker",
            Some(&cron_source("tick/slot/1000")),
            Some(&first_parent),
            Some("issue/512/proposal/abc"),
            None,
        );
        let second = derive_delivery_id(
            "next",
            "next_worker",
            Some(&cron_source("tick/slot/2000")),
            Some(&second_parent),
            Some("issue/512/proposal/abc"),
            None,
        );

        assert_eq!(first, second);
        assert_eq!(
            first,
            "delivery/v3/raised/queue/next/dept/next_worker/dedup/issue_2F_512_2F_proposal_2F_abc"
        );
    }

    #[test]
    fn raised_delivery_id_keeps_distinct_dedup_keys_for_same_source() {
        let source = cron_source("issue/512");
        let derived = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick"
                .to_string(),
            ordinal: 0,
        };

        let first = derive_delivery_id(
            "comments",
            "comment_worker",
            Some(&source),
            Some(&derived),
            Some("proposal-a/comment/approve/v1"),
            None,
        );
        let second = derive_delivery_id(
            "comments",
            "comment_worker",
            Some(&source),
            Some(&derived),
            Some("proposal-b/comment/approve/v1"),
            None,
        );

        assert_ne!(first, second);
    }

    #[test]
    fn raised_delivery_id_dedup_key_wins_over_own_source_ref() {
        let parent_source = cron_source("parent/tick");
        let own_source = SourceRef {
            kind: SourceKind::External,
            reference: "child/entity:512".to_string(),
        };
        let derived = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick"
                .to_string(),
            ordinal: 1,
        };

        let id = derive_delivery_id(
            "next",
            "next_worker",
            Some(&parent_source),
            Some(&derived),
            Some("issue-512"),
            Some(&own_source),
        );

        assert_eq!(
            id,
            "delivery/v3/raised/queue/next/dept/next_worker/dedup/issue-512"
        );
        assert!(!id.contains("child"));
    }

    #[test]
    fn reliable_raised_dedup_key_collapses_across_parent_source_and_ts() {
        let cfg = config(false);
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);
        let first_parent = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/source/dept/producer/ref/tick-1"
                .to_string(),
            ordinal: 0,
        };
        let second_parent = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/source/dept/producer/ref/tick-2"
                .to_string(),
            ordinal: 4,
        };
        let mut first_event = Event::new(
            "jobs",
            serde_json::json!({
                "entity": "issue-512",
                "dedup_key": "issue-512/proposal-a"
            }),
        );
        first_event.ts = 1_000;
        let mut second_event = Event::new(
            "jobs",
            serde_json::json!({
                "entity": "issue-512",
                "dedup_key": "issue-512/proposal-a",
                "minor": "later"
            }),
        );
        second_event.ts = 2_000;

        router
            .publish(PublishEnvelope {
                event: first_event,
                source: Some(cron_source("tick/slot/1000")),
                cron_payload: None,
                derived: Some(first_parent),
            })
            .unwrap();
        router
            .publish(PublishEnvelope {
                event: second_event,
                source: Some(cron_source("tick/slot/2000")),
                cron_payload: None,
                derived: Some(second_parent),
            })
            .unwrap();

        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_secs(30))
            .unwrap();

        assert_eq!(leased.len(), 1);
        assert_eq!(
            leased[0].delivery_id,
            "delivery/v3/raised/queue/jobs/dept/worker/dedup/issue-512_2F_proposal-a"
        );
        assert_eq!(
            leased[0].source.as_ref().unwrap().reference,
            "tick/slot/1000"
        );
        assert_eq!(leased[0].observed_at_ms, 1_000);
        assert_eq!(
            leased[0].payload,
            serde_json::json!({"entity": "issue-512", "dedup_key": "issue-512/proposal-a"})
        );
    }

    #[test]
    fn reliable_raised_same_entity_with_different_dedup_keys_stores_two_records() {
        let cfg = config(false);
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);
        let source = cron_source("tick/slot/1000");

        for (ordinal, dedup_key) in ["issue-512/proposal-a", "issue-512/proposal-b"]
            .into_iter()
            .enumerate()
        {
            router
                .publish(PublishEnvelope {
                    event: Event::new(
                        "jobs",
                        serde_json::json!({
                            "entity": "issue-512",
                            "dedup_key": dedup_key
                        }),
                    ),
                    source: Some(source.clone()),
                    cron_payload: None,
                    derived: Some(DerivedDelivery {
                        parent_delivery_id:
                            "delivery/v1/source/cron/queue/source/dept/producer/ref/tick"
                                .to_string(),
                        ordinal,
                    }),
                })
                .unwrap();
        }

        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_secs(30))
            .unwrap();

        assert_eq!(leased.len(), 2);
    }

    #[test]
    fn raised_delivery_id_falls_back_to_own_source_ref_without_dedup_key() {
        let parent_source = cron_source("parent/tick");
        let own_source = SourceRef {
            kind: SourceKind::External,
            reference: "child/entity:512".to_string(),
        };
        let derived = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick"
                .to_string(),
            ordinal: 1,
        };

        let id = derive_delivery_id(
            "next",
            "next_worker",
            Some(&parent_source),
            Some(&derived),
            None,
            Some(&own_source),
        );

        assert_eq!(
            id,
            "delivery/v3/raised-src/queue/next/dept/next_worker/kind/external/ref/child_2F_entity_3A_512"
        );
    }

    #[test]
    fn raised_delivery_id_last_resort_uses_parent_ordinal_queue_and_dept() {
        let derived = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick"
                .to_string(),
            ordinal: 2,
        };

        let id = derive_delivery_id("next", "next_worker", None, Some(&derived), None, None);

        assert_eq!(
            id,
            "delivery/v2/raised/queue/next/dept/next__worker/parent__hash/1137428a8a684af06e4ef49a79a0d5e8/ordinal/2"
        );
    }

    #[test]
    fn raised_delivery_id_escaping_is_injective_and_runtime_safe() {
        let source = cron_source("tick");
        let derived = DerivedDelivery {
            parent_delivery_id: "delivery/v1/source/cron/queue/jobs/dept/worker/ref/tick"
                .to_string(),
            ordinal: 0,
        };

        let slash = derive_delivery_id(
            "next",
            "next_worker",
            Some(&source),
            Some(&derived),
            Some("a/b"),
            None,
        );
        let colon = derive_delivery_id(
            "next",
            "next_worker",
            Some(&source),
            Some(&derived),
            Some("a:b"),
            None,
        );

        assert_ne!(slash, colon);
        assert!(slash.ends_with("/dedup/a_2F_b"));
        assert!(colon.ends_with("/dedup/a_3A_b"));
        validate_runtime_key(&slash).unwrap();
        validate_runtime_key(&colon).unwrap();

        let only_dots = derive_delivery_id(
            "next",
            "next_worker",
            Some(&source),
            Some(&derived),
            Some("..."),
            None,
        );
        let long_dot_tail = derive_delivery_id(
            "next",
            "next_worker",
            Some(&source),
            Some(&derived),
            Some(&format!("a{}", ".".repeat(220))),
            None,
        );

        assert!(only_dots.ends_with("/dedup/_2E__2E__2E_"));
        validate_runtime_key(&only_dots).unwrap();
        validate_runtime_key(&long_dot_tail).unwrap();
    }

    #[test]
    fn raised_delivery_id_chain_stays_bounded() {
        let source = cron_source("tick");
        let mut parent = derive_delivery_id("jobs", "worker", Some(&source), None, None, None);

        for hop in 0..20 {
            let derived = DerivedDelivery {
                parent_delivery_id: parent.clone(),
                ordinal: hop,
            };
            let id = derive_delivery_id("next", "next_worker", None, Some(&derived), None, None);

            assert!(
                id.len() < 512,
                "hop {hop} delivery id exceeded bound: {} bytes",
                id.len()
            );
            assert!(id.contains("/parent__hash/"));
            assert!(!id.contains("/parent/"));
            parent = id;
        }
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
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store), None);

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

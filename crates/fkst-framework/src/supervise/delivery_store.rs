//! Durable delivery redb state machine.
#![allow(dead_code)]

use super::delivery_types::{DeadRecord, DeliveryRecord, RetryPolicy};
use anyhow::{bail, Result};
use redb::{Database, ReadableTable, TableDefinition};
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::warn;

const SCHEMA_VERSION: &str = "1";
const ERROR_EXCERPT_LIMIT: usize = 500;
const STORE_OP_WARN_AFTER: Duration = Duration::from_millis(250);
const JITTER_DIVISOR: u128 = 4;

const DELIVERY_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("delivery_by_id");
const READY_BY_DUE: TableDefinition<(u64, &str), ()> = TableDefinition::new("ready_by_due");
const LEASED_BY_UNTIL: TableDefinition<(u64, &str), ()> = TableDefinition::new("leased_by_until");
const DEAD_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("dead_by_id");
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RetryOutcome {
    Scheduled,
    Dead,
    Stale,
    Missing,
}

pub(crate) struct DeliveryStore {
    db: Database,
}

impl DeliveryStore {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        let write = db.begin_write()?;
        {
            write.open_table(DELIVERY_BY_ID)?;
            write.open_table(READY_BY_DUE)?;
            write.open_table(LEASED_BY_UNTIL)?;
            write.open_table(DEAD_BY_ID)?;
            let mut meta = write.open_table(META)?;
            meta.insert("schema_version", SCHEMA_VERSION)?;
        }
        write.commit()?;
        Ok(Self { db })
    }

    pub(crate) fn enqueue(&self, record: &DeliveryRecord) -> Result<()> {
        let _op = StoreOpWatch::new("enqueue", &record.dept);
        let write = self.db.begin_write()?;
        {
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            if let Some(current) = read_delivery_table(&delivery, record.delivery_id.as_str())? {
                if same_delivery_content(&current, record) {
                    drop(delivery);
                    write.commit()?;
                    return Ok(());
                }
                bail!("conflicting duplicate delivery_id: {}", record.delivery_id);
            }
            let bytes = serde_json::to_vec(record)?;
            delivery.insert(record.delivery_id.as_str(), bytes.as_slice())?;
            let mut ready = write.open_table(READY_BY_DUE)?;
            ready.insert((record.not_before_ms, record.delivery_id.as_str()), &())?;
        }
        write.commit()?;
        Ok(())
    }

    pub(crate) fn renew_lease(
        &self,
        delivery_id: &str,
        lease_generation: u64,
        lease_until_ms: u64,
    ) -> Result<bool> {
        let mut op = StoreOpWatch::new("renew", "<unknown>");
        let write = self.db.begin_write()?;
        let applied = {
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            if let Some(mut current) = read_delivery_table(&delivery, delivery_id)? {
                op.set_dept(&current.dept);
                if let Some(old_lease_until) = current.lease_until_ms {
                    if current.lease_generation == lease_generation {
                        let mut lease_index = write.open_table(LEASED_BY_UNTIL)?;
                        lease_index.remove((old_lease_until, delivery_id))?;
                        current.lease_until_ms = Some(lease_until_ms);
                        let bytes = serde_json::to_vec(&current)?;
                        delivery.insert(delivery_id, bytes.as_slice())?;
                        lease_index.insert((lease_until_ms, delivery_id), &())?;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        };
        write.commit()?;
        Ok(applied)
    }

    pub(crate) fn lease(
        &self,
        now_ms: u64,
        batch_limit: usize,
        lease_dur: Duration,
    ) -> Result<Vec<DeliveryRecord>> {
        self.lease_matching(now_ms, batch_limit, lease_dur, None, &BTreeSet::new())
    }

    pub(crate) fn lease_for_dept(
        &self,
        dept: &str,
        now_ms: u64,
        batch_limit: usize,
        lease_dur: Duration,
    ) -> Result<Vec<DeliveryRecord>> {
        self.lease_for_dept_excluding(dept, now_ms, batch_limit, lease_dur, &BTreeSet::new())
    }

    pub(crate) fn lease_for_dept_excluding(
        &self,
        dept: &str,
        now_ms: u64,
        batch_limit: usize,
        lease_dur: Duration,
        excluded: &BTreeSet<String>,
    ) -> Result<Vec<DeliveryRecord>> {
        self.lease_matching(now_ms, batch_limit, lease_dur, Some(dept), excluded)
    }

    fn lease_matching(
        &self,
        now_ms: u64,
        batch_limit: usize,
        lease_dur: Duration,
        dept: Option<&str>,
        excluded: &BTreeSet<String>,
    ) -> Result<Vec<DeliveryRecord>> {
        let _op = StoreOpWatch::new("lease", dept.unwrap_or("<any>"));
        if batch_limit == 0 {
            return Ok(Vec::new());
        }
        let lease_until = now_ms.saturating_add(duration_millis(lease_dur));
        let write = self.db.begin_write()?;
        let mut leased = Vec::new();
        {
            let expired_quota = expired_lease_quota(batch_limit);
            let scan_budget = scan_budget(batch_limit, excluded.len());
            let expired_keys =
                collect_due_keys(&write.open_table(LEASED_BY_UNTIL)?, now_ms, scan_budget)?;
            let ready_keys =
                collect_due_keys(&write.open_table(READY_BY_DUE)?, now_ms, scan_budget)?;
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            let mut ready = write.open_table(READY_BY_DUE)?;
            let mut lease_index = write.open_table(LEASED_BY_UNTIL)?;

            for key in expired_keys {
                if leased.len() >= expired_quota {
                    break;
                }
                if let Some(record) = lease_key(
                    key,
                    now_ms,
                    lease_until,
                    dept,
                    excluded,
                    &mut delivery,
                    &mut ready,
                    &mut lease_index,
                    true,
                )? {
                    leased.push(record);
                }
            }
            for key in ready_keys {
                if leased.len() >= batch_limit {
                    break;
                }
                if let Some(record) = lease_key(
                    key,
                    now_ms,
                    lease_until,
                    dept,
                    excluded,
                    &mut delivery,
                    &mut ready,
                    &mut lease_index,
                    false,
                )? {
                    leased.push(record);
                }
            }
        }
        write.commit()?;
        Ok(leased)
    }

    pub(crate) fn ack(&self, delivery_id: &str, lease_generation: u64) -> Result<bool> {
        let mut op = StoreOpWatch::new("ack", "<unknown>");
        let write = self.db.begin_write()?;
        let applied = {
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            if let Some(current) = read_delivery_table(&delivery, delivery_id)? {
                op.set_dept(&current.dept);
                if current.lease_generation == lease_generation {
                    if let Some(lease_until) = current.lease_until_ms {
                        let mut lease_index = write.open_table(LEASED_BY_UNTIL)?;
                        lease_index.remove((lease_until, delivery_id))?;
                    } else {
                        return Ok(false);
                    }
                    delivery.remove(delivery_id)?;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        write.commit()?;
        Ok(applied)
    }

    pub(crate) fn retry(
        &self,
        delivery_id: &str,
        lease_generation: u64,
        error: &str,
        policy: &RetryPolicy,
        now_ms: u64,
    ) -> Result<RetryOutcome> {
        let mut op = StoreOpWatch::new("retry", "<unknown>");
        let write = self.db.begin_write()?;
        let outcome = {
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            if let Some(mut current) = read_delivery_table(&delivery, delivery_id)? {
                op.set_dept(&current.dept);
                let Some(lease_until) = current.lease_until_ms else {
                    return Ok(RetryOutcome::Stale);
                };
                if current.lease_generation != lease_generation {
                    RetryOutcome::Stale
                } else {
                    let mut lease_index = write.open_table(LEASED_BY_UNTIL)?;
                    lease_index.remove((lease_until, delivery_id))?;

                    let attempt = current.attempt.saturating_add(1);
                    current.attempt = attempt;
                    current.last_error_excerpt = Some(error_excerpt(error));
                    current.lease_until_ms = None;

                    if attempt >= policy.max_attempts {
                        let dead = DeadRecord {
                            delivery_id: current.delivery_id.clone(),
                            queue: current.queue.clone(),
                            dept: current.dept.clone(),
                            source: current.source.clone(),
                            dead_at_ms: now_ms,
                            attempts: attempt,
                            error_excerpt: current.last_error_excerpt.clone(),
                        };
                        let bytes = serde_json::to_vec(&dead)?;
                        let mut dead_table = write.open_table(DEAD_BY_ID)?;
                        dead_table.insert(delivery_id, bytes.as_slice())?;
                        delivery.remove(delivery_id)?;
                        RetryOutcome::Dead
                    } else {
                        let delay = backoff_delay(policy.base, policy.cap, attempt);
                        let jitter = bounded_jitter(delivery_id, attempt, policy.base);
                        current.not_before_ms = now_ms
                            .saturating_add(duration_millis(delay))
                            .saturating_add(duration_millis(jitter));
                        let bytes = serde_json::to_vec(&current)?;
                        delivery.insert(delivery_id, bytes.as_slice())?;
                        let mut ready = write.open_table(READY_BY_DUE)?;
                        ready.insert((current.not_before_ms, delivery_id), &())?;
                        RetryOutcome::Scheduled
                    }
                }
            } else {
                RetryOutcome::Missing
            }
        };
        write.commit()?;
        Ok(outcome)
    }

    #[cfg(test)]
    pub(crate) fn get(&self, delivery_id: &str) -> Result<Option<DeliveryRecord>> {
        let read = self.db.begin_read()?;
        let delivery = read.open_table(DELIVERY_BY_ID)?;
        read_delivery_read_only(&delivery, delivery_id)
    }

    #[cfg(test)]
    pub(crate) fn get_dead(&self, delivery_id: &str) -> Result<Option<DeadRecord>> {
        let read = self.db.begin_read()?;
        let dead = read.open_table(DEAD_BY_ID)?;
        let Some(bytes) = dead.get(delivery_id)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(bytes.value())?))
    }

    #[cfg(test)]
    fn ready_index_len(&self) -> Result<usize> {
        let read = self.db.begin_read()?;
        let ready = read.open_table(READY_BY_DUE)?;
        count_index_entries(&ready)
    }

    #[cfg(test)]
    fn leased_index_len(&self) -> Result<usize> {
        let read = self.db.begin_read()?;
        let leased = read.open_table(LEASED_BY_UNTIL)?;
        count_index_entries(&leased)
    }
}

fn same_delivery_content(left: &DeliveryRecord, right: &DeliveryRecord) -> bool {
    left.delivery_id == right.delivery_id
        && left.queue == right.queue
        && left.dept == right.dept
        && left.payload == right.payload
        && left.source == right.source
        && left.cron_payload == right.cron_payload
}

fn expired_lease_quota(batch_limit: usize) -> usize {
    if batch_limit == 0 {
        0
    } else {
        (batch_limit / 2).max(1)
    }
}

fn scan_budget(batch_limit: usize, excluded_len: usize) -> usize {
    batch_limit.saturating_mul(8).saturating_add(excluded_len)
}

fn collect_due_keys(
    table: &redb::Table<'_, (u64, &str), ()>,
    now_ms: u64,
    limit: usize,
) -> Result<Vec<(u64, String)>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut keys = Vec::new();
    for entry in table.range::<(u64, &str)>((0, "")..=(now_ms, "\u{10ffff}"))? {
        let (key, _) = entry?;
        let (due, delivery_id) = key.value();
        keys.push((due, delivery_id.to_string()));
        if keys.len() >= limit {
            break;
        }
    }
    Ok(keys)
}

struct StoreOpWatch<'a> {
    op: &'static str,
    dept: String,
    started: Instant,
    _marker: std::marker::PhantomData<&'a str>,
}

impl<'a> StoreOpWatch<'a> {
    fn new(op: &'static str, dept: &'a str) -> Self {
        Self {
            op,
            dept: dept.to_string(),
            started: Instant::now(),
            _marker: std::marker::PhantomData,
        }
    }

    fn set_dept(&mut self, dept: &str) {
        self.dept.clear();
        self.dept.push_str(dept);
    }
}

impl Drop for StoreOpWatch<'_> {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed > STORE_OP_WARN_AFTER {
            warn!(
                op = self.op,
                dept = %self.dept,
                elapsed_ms = elapsed.as_millis() as u64,
                "durable delivery store operation exceeded watchdog threshold"
            );
        }
    }
}

fn lease_key(
    key: (u64, String),
    now_ms: u64,
    lease_until: u64,
    dept: Option<&str>,
    excluded: &BTreeSet<String>,
    delivery: &mut redb::Table<'_, &str, &[u8]>,
    ready: &mut redb::Table<'_, (u64, &str), ()>,
    lease_index: &mut redb::Table<'_, (u64, &str), ()>,
    from_expired_lease: bool,
) -> Result<Option<DeliveryRecord>> {
    let (indexed_at, delivery_id) = key;
    if excluded.contains(&delivery_id) {
        return Ok(None);
    }
    let Some(mut record) = read_delivery_table(delivery, &delivery_id)? else {
        if from_expired_lease {
            lease_index.remove((indexed_at, delivery_id.as_str()))?;
        } else {
            ready.remove((indexed_at, delivery_id.as_str()))?;
        }
        return Ok(None);
    };
    if dept.is_some_and(|wanted| record.dept != wanted) {
        return Ok(None);
    }
    if from_expired_lease {
        lease_index.remove((indexed_at, delivery_id.as_str()))?;
        if record.lease_until_ms != Some(indexed_at) {
            return Ok(None);
        }
    } else {
        ready.remove((indexed_at, delivery_id.as_str()))?;
        if record.lease_until_ms.is_some() {
            return Ok(None);
        }
    }
    if record.not_before_ms > now_ms && !from_expired_lease {
        ready.insert((record.not_before_ms, delivery_id.as_str()), &())?;
        return Ok(None);
    }
    if let Some(old_lease_until) = record.lease_until_ms {
        lease_index.remove((old_lease_until, delivery_id.as_str()))?;
    }
    record.lease_generation = record.lease_generation.saturating_add(1);
    record.lease_until_ms = Some(lease_until);
    let bytes = serde_json::to_vec(&record)?;
    delivery.insert(delivery_id.as_str(), bytes.as_slice())?;
    lease_index.insert((lease_until, delivery_id.as_str()), &())?;
    Ok(Some(record))
}

fn read_delivery_table(
    table: &redb::Table<'_, &str, &[u8]>,
    delivery_id: &str,
) -> Result<Option<DeliveryRecord>> {
    let Some(bytes) = table.get(delivery_id)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(bytes.value())?))
}

fn read_delivery_read_only(
    table: &redb::ReadOnlyTable<&str, &[u8]>,
    delivery_id: &str,
) -> Result<Option<DeliveryRecord>> {
    let Some(bytes) = table.get(delivery_id)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(bytes.value())?))
}

pub(crate) fn backoff_delay(base: Duration, cap: Duration, attempt: u64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(63);
    let multiplier = 1_u128 << exponent;
    let millis = base.as_millis().saturating_mul(multiplier);
    let capped = millis.min(cap.as_millis());
    Duration::from_millis(capped.min(u64::MAX as u128) as u64)
}

pub(crate) fn bounded_jitter(delivery_id: &str, attempt: u64, base: Duration) -> Duration {
    let max = (base.as_millis() / JITTER_DIVISOR).min(u64::MAX as u128) as u64;
    if max == 0 {
        return Duration::ZERO;
    }
    let mut hasher = DefaultHasher::new();
    delivery_id.hash(&mut hasher);
    attempt.hash(&mut hasher);
    Duration::from_millis(hasher.finish() % (max + 1))
}

pub(crate) fn error_excerpt(error: &str) -> String {
    let mut excerpt = error.replace('\r', "\\r").replace('\n', "\\n");
    if excerpt.len() > ERROR_EXCERPT_LIMIT {
        let boundary = excerpt
            .char_indices()
            .map(|(idx, _)| idx)
            .take_while(|idx| *idx <= ERROR_EXCERPT_LIMIT)
            .last()
            .unwrap_or(0);
        excerpt.truncate(boundary);
    }
    excerpt
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
fn count_index_entries(table: &redb::ReadOnlyTable<(u64, &str), ()>) -> Result<usize> {
    let mut count = 0;
    for entry in table.iter()? {
        let _ = entry?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn enqueue_then_lease_returns_due_record() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();

        let leased = store.lease(100, 10, Duration::from_millis(50)).unwrap();

        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].delivery_id, "one");
    }

    #[test]
    fn duplicate_enqueue_same_content_is_noop() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let original = record("one", 100);

        store.enqueue(&original).unwrap();
        store.enqueue(&original).unwrap();
        let current = store.get("one").unwrap().unwrap();

        assert_eq!(current.queue, "input");
        assert_eq!(current.not_before_ms, 100);
        assert_eq!(store.ready_index_len().unwrap(), 1);
        assert_eq!(
            store
                .lease(100, 10, Duration::from_millis(50))
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .lease(100, 10, Duration::from_millis(50))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn duplicate_enqueue_allows_new_observation_time_for_same_delivery() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let original = record("one", 100);
        let mut duplicate = original.clone();
        duplicate.observed_at_ms = original.observed_at_ms + 1;

        store.enqueue(&original).unwrap();
        store.enqueue(&duplicate).unwrap();
        let current = store.get("one").unwrap().unwrap();

        assert_eq!(current.observed_at_ms, original.observed_at_ms);
        assert_eq!(store.ready_index_len().unwrap(), 1);
    }

    #[test]
    fn duplicate_enqueue_conflicting_content_is_rejected_without_overwriting_record() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        let mut duplicate = record("one", 50);
        duplicate.queue = "other".to_string();

        let error = store.enqueue(&duplicate).unwrap_err();
        let current = store.get("one").unwrap().unwrap();

        assert!(error
            .to_string()
            .contains("conflicting duplicate delivery_id"));
        assert_eq!(current.queue, "input");
        assert_eq!(current.not_before_ms, 100);
        assert_eq!(store.ready_index_len().unwrap(), 1);
    }

    #[test]
    fn renew_lease_extends_matching_generation() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();

        assert!(store.renew_lease("one", 1, 250).unwrap());

        let current = store.get("one").unwrap().unwrap();
        assert_eq!(current.lease_until_ms, Some(250));
        assert!(store
            .lease(151, 10, Duration::from_millis(50))
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .lease(251, 10, Duration::from_millis(50))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn future_record_is_not_leased() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 200)).unwrap();

        let leased = store.lease(100, 10, Duration::from_millis(50)).unwrap();

        assert!(leased.is_empty());
    }

    #[test]
    fn lease_increments_generation_and_sets_lease_until() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();

        let leased = store.lease(100, 10, Duration::from_millis(50)).unwrap();
        let current = store.get("one").unwrap().unwrap();

        assert_eq!(leased[0].lease_generation, 1);
        assert_eq!(leased[0].lease_until_ms, Some(150));
        assert_eq!(current.lease_generation, 1);
        assert_eq!(current.lease_until_ms, Some(150));
        assert!(store
            .lease(120, 10, Duration::from_millis(50))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn lease_for_dept_excluding_skips_running_delivery() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();

        let first = store
            .lease_for_dept("worker", 100, 10, Duration::from_millis(50))
            .unwrap();
        let excluded = BTreeSet::from([first[0].delivery_id.clone()]);
        let second = store
            .lease_for_dept_excluding("worker", 151, 10, Duration::from_millis(50), &excluded)
            .unwrap();

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
    }

    #[test]
    fn ack_deletes_only_matching_generation() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();

        assert!(!store.ack("one", 0).unwrap());
        assert!(store.get("one").unwrap().is_some());
        assert!(store.ack("one", 1).unwrap());
        assert!(store.get("one").unwrap().is_none());
    }

    #[test]
    fn ack_ready_record_is_rejected_without_changing_indexes() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();

        assert!(!store.ack("one", 0).unwrap());

        let current = store.get("one").unwrap().unwrap();
        assert_eq!(current.lease_generation, 0);
        assert_eq!(current.lease_until_ms, None);
        assert_eq!(store.ready_index_len().unwrap(), 1);
        assert_eq!(store.leased_index_len().unwrap(), 0);
        assert_eq!(
            store
                .lease(100, 10, Duration::from_millis(50))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn retry_matching_generation_returns_to_ready() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();
        let expected_not_before = 120
            + duration_millis(backoff_delay(policy(3).base, policy(3).cap, 1))
            + duration_millis(bounded_jitter("one", 1, policy(3).base));

        let outcome = store
            .retry("one", 1, "temporary\nfailure", &policy(3), 120)
            .unwrap();
        let current = store.get("one").unwrap().unwrap();

        assert_eq!(outcome, RetryOutcome::Scheduled);
        assert_eq!(current.attempt, 1);
        assert_eq!(current.lease_until_ms, None);
        assert_eq!(current.not_before_ms, expected_not_before);
        assert_eq!(
            current.last_error_excerpt.as_deref(),
            Some("temporary\\nfailure")
        );
        assert_eq!(
            store
                .lease(expected_not_before, 10, Duration::from_millis(50))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn retry_stale_generation_is_noop() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();

        let outcome = store.retry("one", 0, "temporary", &policy(3), 120).unwrap();
        let current = store.get("one").unwrap().unwrap();

        assert_eq!(outcome, RetryOutcome::Stale);
        assert_eq!(current.attempt, 0);
        assert_eq!(current.lease_until_ms, Some(150));
    }

    #[test]
    fn retry_ready_record_is_rejected_without_changing_indexes() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();

        let outcome = store.retry("one", 0, "temporary", &policy(3), 120).unwrap();
        let current = store.get("one").unwrap().unwrap();

        assert_eq!(outcome, RetryOutcome::Stale);
        assert_eq!(current.attempt, 0);
        assert_eq!(current.lease_generation, 0);
        assert_eq!(current.lease_until_ms, None);
        assert_eq!(store.ready_index_len().unwrap(), 1);
        assert_eq!(store.leased_index_len().unwrap(), 0);
        assert_eq!(
            store
                .lease(100, 10, Duration::from_millis(50))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn retry_at_max_moves_to_dead() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let mut original = record("one", 100);
        original.payload = serde_json::json!({"blob": "x".repeat(10_000)});
        store.enqueue(&original).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();

        let outcome = store
            .retry(
                "one",
                1,
                &format!("final {}", "x".repeat(800)),
                &policy(1),
                120,
            )
            .unwrap();

        assert_eq!(outcome, RetryOutcome::Dead);
        assert!(store.get("one").unwrap().is_none());
        let dead = store.get_dead("one").unwrap().unwrap();
        assert_eq!(dead.delivery_id, "one");
        assert_eq!(dead.queue, "input");
        assert_eq!(dead.dept, "worker");
        assert_eq!(dead.source, None);
        assert_eq!(dead.dead_at_ms, 120);
        assert_eq!(dead.attempts, 1);
        assert!(dead.error_excerpt.as_ref().unwrap().len() <= ERROR_EXCERPT_LIMIT);
        let dead_json = serde_json::to_value(&dead).unwrap();
        assert!(dead_json.get("payload").is_none());
        assert!(dead_json.get("record").is_none());
    }

    #[test]
    fn acked_record_leaves_no_leaseable_index_entry() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();

        assert!(store.ack("one", 1).unwrap());

        assert_eq!(store.ready_index_len().unwrap(), 0);
        assert_eq!(store.leased_index_len().unwrap(), 0);
        assert!(store
            .lease(151, 10, Duration::from_millis(50))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn retried_record_leaves_only_one_ready_index_entry() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();
        let outcome = store.retry("one", 1, "temporary", &policy(3), 120).unwrap();
        let next_due = store.get("one").unwrap().unwrap().not_before_ms;

        assert_eq!(outcome, RetryOutcome::Scheduled);
        assert_eq!(store.ready_index_len().unwrap(), 1);
        assert_eq!(store.leased_index_len().unwrap(), 0);
        assert_eq!(
            store
                .lease(next_due, 10, Duration::from_millis(50))
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .lease(next_due, 10, Duration::from_millis(50))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn dead_record_leaves_no_leaseable_index_entry() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();

        let outcome = store.retry("one", 1, "final", &policy(1), 120).unwrap();

        assert_eq!(outcome, RetryOutcome::Dead);
        assert_eq!(store.ready_index_len().unwrap(), 0);
        assert_eq!(store.leased_index_len().unwrap(), 0);
        assert!(store
            .lease(151, 10, Duration::from_millis(50))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn expired_lease_can_be_leased_again() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();

        let leased = store.lease(151, 10, Duration::from_millis(50)).unwrap();

        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].lease_generation, 2);
        assert_eq!(leased[0].lease_until_ms, Some(201));
    }

    #[test]
    fn lease_respects_batch_limit_and_empty_store_returns_empty() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);

        assert!(store
            .lease(100, 10, Duration::from_millis(50))
            .unwrap()
            .is_empty());

        store.enqueue(&record("one", 100)).unwrap();
        store.enqueue(&record("two", 100)).unwrap();

        assert_eq!(
            store
                .lease(100, 1, Duration::from_millis(50))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.leased_index_len().unwrap(), 1);
        assert_eq!(store.ready_index_len().unwrap(), 1);
    }

    #[test]
    fn scan_budget_is_derived_from_batch_limit_and_exclusions() {
        assert_eq!(scan_budget(1, 0), 8);
        assert_eq!(scan_budget(2, 3), 19);
    }

    #[test]
    fn lease_for_dept_scans_only_budgeted_due_records() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        for index in 0..10_000 {
            let mut other = record(&format!("other-{index:05}"), 100);
            other.dept = "other".to_string();
            store.enqueue(&other).unwrap();
        }
        store.enqueue(&record("worker-record", 100)).unwrap();

        let leased = store
            .lease_for_dept("worker", 100, 1, Duration::from_millis(50))
            .unwrap();

        assert!(leased.is_empty());
        assert_eq!(store.ready_index_len().unwrap(), 10_001);
        assert_eq!(store.leased_index_len().unwrap(), 0);
    }

    #[test]
    fn expired_lease_is_not_starved_by_ready_backlog() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("expired", 100)).unwrap();
        store.lease(100, 1, Duration::from_millis(50)).unwrap();
        for index in 0..10 {
            store
                .enqueue(&record(&format!("ready-{index}"), 151))
                .unwrap();
        }

        let leased = store.lease(151, 1, Duration::from_millis(50)).unwrap();

        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].delivery_id, "expired");
        assert_eq!(leased[0].lease_generation, 2);
    }

    #[test]
    fn reopen_preserves_pending_delivery() {
        let temp = TempDir::new().unwrap();
        {
            let store = store(&temp);
            store.enqueue(&record("one", 100)).unwrap();
        }
        let store = store(&temp);

        let leased = store.lease(100, 10, Duration::from_millis(50)).unwrap();

        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].delivery_id, "one");
    }

    #[test]
    fn backoff_delay_is_capped() {
        assert_eq!(
            backoff_delay(Duration::from_millis(100), Duration::from_millis(250), 4),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn bounded_jitter_is_deterministic_and_bounded() {
        let base = Duration::from_millis(100);
        let first = bounded_jitter("one", 2, base);
        let second = bounded_jitter("one", 2, base);

        assert_eq!(first, second);
        assert!(first <= Duration::from_millis(25));
    }

    #[test]
    fn error_excerpt_truncates_on_utf8_boundary() {
        let excerpt = error_excerpt(&"é".repeat(300));

        assert!(excerpt.len() <= ERROR_EXCERPT_LIMIT);
        assert!(excerpt.is_char_boundary(excerpt.len()));
    }
}

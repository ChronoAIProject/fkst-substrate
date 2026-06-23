//! Durable delivery redb state machine.
#![allow(dead_code)]

#[cfg(test)]
use super::delivery_index::count_index_entries;
use super::delivery_index::{
    collect_due_keys, make_dead_due_index_key, make_index_key, parse_dead_due_index_key,
    DEAD_BY_DEPT_DUE, LEASED_BY_DEPT_UNTIL, READY_BY_DEPT_DUE,
};
#[cfg(test)]
use super::delivery_retry::ERROR_EXCERPT_LIMIT;
use super::delivery_retry::{backoff_delay, bounded_jitter, error_excerpt};
use super::delivery_transition::{lease_key, read_delivery_table, rebuild_due_indexes};
use super::delivery_types::{DeadRecord, DeliveryRecord, RedrivePolicy, RetryPolicy, SourceRef};
use super::delivery_watch::StoreOpWatch;
use anyhow::{bail, Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

const SCHEMA_VERSION: &str = "9";
const TERMINAL_DEAD_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;
const TERMINAL_DEAD_COMPACTION_LIMIT: usize = 1_024;
const TERMINAL_SUPPRESSION_SLOTS: u64 = 16_777_216;

const DELIVERY_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("delivery_by_id");
const DEAD_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("dead_by_id");
const TERMINAL_DEAD_BY_TIME: TableDefinition<&str, ()> =
    TableDefinition::new("terminal_dead_by_time");
const TERMINAL_SUPPRESSED_BY_ID: TableDefinition<&str, ()> =
    TableDefinition::new("terminal_suppressed_by_id");
const TERMINAL_SUPPRESSION_BY_SLOT: TableDefinition<&str, &str> =
    TableDefinition::new("terminal_suppression_by_slot");
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");
const OLD_READY_BY_DUE: TableDefinition<(u64, &str), ()> = TableDefinition::new("ready_by_due");
const OLD_LEASED_BY_UNTIL: TableDefinition<(u64, &str), ()> =
    TableDefinition::new("leased_by_until");

#[cfg(test)]
thread_local! {
    static WRITE_TXN_COUNT: Cell<usize> = const { Cell::new(0) };
    static WRITE_COMMIT_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RetryOutcome {
    Scheduled,
    DeadPendingRedrive,
    PermanentDead,
    Stale,
    Missing,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RetryFailure {
    pub message: String,
    pub replayable: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) struct RedriveResult {
    pub redriven: Vec<DeliveryRecord>,
    pub permanent: Vec<DeadRecord>,
}

pub(crate) struct DeliveryStore {
    db: Database,
}

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

impl DeliveryStore {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        let write = db.begin_write()?;
        {
            write.open_table(DELIVERY_BY_ID)?;
            write.open_table(READY_BY_DEPT_DUE)?;
            write.open_table(LEASED_BY_DEPT_UNTIL)?;
            write.open_table(DEAD_BY_DEPT_DUE)?;
            let meta = write.open_table(META)?;
            let current_version = meta
                .get("schema_version")?
                .map(|value| value.value().to_string());
            drop(meta);
            if current_version.as_deref() == Some("1") {
                write.delete_table(DEAD_BY_ID)?;
            }
            write.open_table(DEAD_BY_ID)?;
            write.open_table(TERMINAL_DEAD_BY_TIME)?;
            write.open_table(TERMINAL_SUPPRESSION_BY_SLOT)?;
            if current_version.as_deref() != Some(SCHEMA_VERSION) {
                delete_old_global_indexes(&write)?;
                rebuild_due_indexes(&write, DELIVERY_BY_ID)?;
                if current_version.as_deref() != Some("3") {
                    mark_existing_dead_records_permanent(&write)?;
                }
                rebuild_dead_due_index(&write)?;
                rebuild_terminal_dead_tables(&write)?;
                import_legacy_terminal_suppressed_ids(&write)?;
                drop_legacy_terminal_suppressed_table(&write)?;
                drop_legacy_terminal_suppression_filter(&write)?;
            }
            let mut meta = write.open_table(META)?;
            meta.insert("schema_version", SCHEMA_VERSION)?;
        }
        write.commit()?;
        Ok(Self { db })
    }

    pub(crate) fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::open(path.as_ref()).with_context(|| {
            format!(
                "open existing durable delivery database `{}`",
                path.as_ref().display()
            )
        })?;
        Ok(Self { db })
    }

    pub(crate) fn enqueue(&self, record: &DeliveryRecord) -> Result<()> {
        let _op = StoreOpWatch::new("enqueue", &record.dept);
        let write = self.begin_write()?;
        {
            let delivery = write.open_table(DELIVERY_BY_ID)?;
            if let Some(current) = read_delivery_table(&delivery, record.delivery_id.as_str())? {
                if current.collapse_by_dedup_id || record.collapse_by_dedup_id {
                    drop(delivery);
                    commit_write(write)?;
                    return Ok(());
                }
                if same_delivery_content(&current, record) {
                    drop(delivery);
                    commit_write(write)?;
                    return Ok(());
                }
                bail!("conflicting duplicate delivery_id: {}", record.delivery_id);
            }
            drop(delivery);
            compact_terminal_dead_records(&write, record.observed_at_ms)?;
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            if terminal_delivery_is_suppressed(&write, record.delivery_id.as_str())? {
                drop(delivery);
                commit_write(write)?;
                return Ok(());
            }
            let bytes = serde_json::to_vec(record)?;
            delivery.insert(record.delivery_id.as_str(), bytes.as_slice())?;
            let mut ready = write.open_table(READY_BY_DEPT_DUE)?;
            ready.insert(
                make_index_key(&record.dept, record.not_before_ms, &record.delivery_id).as_str(),
                &(),
            )?;
        }
        commit_write(write)?;
        Ok(())
    }

    pub(crate) fn renew_leases(
        &self,
        renewals: &[(String, u64)],
        lease_until_ms: u64,
    ) -> Result<BTreeMap<String, DeliveryRecord>> {
        if renewals.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut op = StoreOpWatch::new("renew", "<batch>");
        let write = self.begin_write()?;
        let mut applied = BTreeMap::new();
        {
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            let mut lease_index = write.open_table(LEASED_BY_DEPT_UNTIL)?;
            for (delivery_id, lease_generation) in renewals {
                let Some(mut current) = read_delivery_table(&delivery, delivery_id)? else {
                    continue;
                };
                op.set_dept(&current.dept);
                let Some(old_lease_until) = current.lease_until_ms else {
                    continue;
                };
                if current.lease_generation != *lease_generation {
                    continue;
                }
                lease_index
                    .remove(make_index_key(&current.dept, old_lease_until, delivery_id).as_str())?;
                current.lease_until_ms = Some(lease_until_ms);
                let bytes = serde_json::to_vec(&current)?;
                delivery.insert(delivery_id.as_str(), bytes.as_slice())?;
                lease_index.insert(
                    make_index_key(&current.dept, lease_until_ms, delivery_id).as_str(),
                    &(),
                )?;
                applied.insert(delivery_id.clone(), current);
            }
        }
        if applied.is_empty() {
            return Ok(applied);
        }
        commit_write(write)?;
        Ok(applied)
    }

    pub(crate) fn renew_lease(
        &self,
        delivery_id: &str,
        lease_generation: u64,
        lease_until_ms: u64,
    ) -> Result<bool> {
        Ok(!self
            .renew_leases(
                &[(delivery_id.to_string(), lease_generation)],
                lease_until_ms,
            )?
            .is_empty())
    }

    pub(crate) fn lease(
        &self,
        now_ms: u64,
        batch_limit: usize,
        lease_dur: Duration,
    ) -> Result<Vec<DeliveryRecord>> {
        self.lease_matching(now_ms, batch_limit, lease_dur, None, &BTreeSet::new())
    }

    pub(crate) fn next_due_deterministic(
        &self,
        now_ms: u64,
        lease_dur: Duration,
    ) -> Result<Option<DeliveryRecord>> {
        let mut due = self.due_ready_records(now_ms)?;
        // TestRuntime dispatches one delivery at a time with a stable key.
        // Production consumers keep their normal per-department scheduler.
        due.sort_by(|left, right| {
            left.not_before_ms
                .cmp(&right.not_before_ms)
                .then_with(|| left.queue.cmp(&right.queue))
                .then_with(|| left.dept.cmp(&right.dept))
                .then_with(|| left.delivery_id.cmp(&right.delivery_id))
        });
        let Some(next) = due.into_iter().next() else {
            return Ok(None);
        };
        self.lease_delivery(&next.delivery_id, now_ms, lease_dur)
    }

    fn due_ready_records(&self, now_ms: u64) -> Result<Vec<DeliveryRecord>> {
        let read = self.db.begin_read()?;
        let ready = read.open_table(READY_BY_DEPT_DUE)?;
        let delivery = read.open_table(DELIVERY_BY_ID)?;
        let mut records = Vec::new();
        for key in collect_due_keys(&ready, None, now_ms, usize::MAX)? {
            if let Some(record) = read_delivery_read_only(&delivery, &key.delivery_id)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    fn lease_delivery(
        &self,
        delivery_id: &str,
        now_ms: u64,
        lease_dur: Duration,
    ) -> Result<Option<DeliveryRecord>> {
        let lease_until = now_ms.saturating_add(duration_millis(lease_dur));
        let write = self.begin_write()?;
        let outcome = {
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            let mut ready = write.open_table(READY_BY_DEPT_DUE)?;
            let mut lease_index = write.open_table(LEASED_BY_DEPT_UNTIL)?;
            let Some(mut record) = read_delivery_table(&delivery, delivery_id)? else {
                return Ok(None);
            };
            if record.lease_until_ms.is_some() || record.not_before_ms > now_ms {
                return Ok(None);
            }
            ready
                .remove(make_index_key(&record.dept, record.not_before_ms, delivery_id).as_str())?;
            record.lease_generation = record.lease_generation.saturating_add(1);
            record.lease_until_ms = Some(lease_until);
            let bytes = serde_json::to_vec(&record)?;
            delivery.insert(delivery_id, bytes.as_slice())?;
            lease_index.insert(
                make_index_key(&record.dept, lease_until, delivery_id).as_str(),
                &(),
            )?;
            record
        };
        commit_write(write)?;
        Ok(Some(outcome))
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
        let (expired_keys, ready_keys) =
            self.collect_leaseable_keys(now_ms, batch_limit, dept, excluded)?;
        if expired_keys.is_empty() && ready_keys.is_empty() {
            return Ok(Vec::new());
        }
        let write = self.begin_write()?;
        let mut leased = Vec::new();
        let mut mutated = false;
        {
            let expired_quota = expired_lease_quota(batch_limit);
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            let mut ready = write.open_table(READY_BY_DEPT_DUE)?;
            let mut lease_index = write.open_table(LEASED_BY_DEPT_UNTIL)?;

            for key in expired_keys {
                if leased.len() >= expired_quota {
                    break;
                }
                let outcome = lease_key(
                    key,
                    now_ms,
                    lease_until,
                    dept,
                    excluded,
                    &mut delivery,
                    &mut ready,
                    &mut lease_index,
                    true,
                )?;
                mutated |= outcome.mutated;
                if let Some(record) = outcome.record {
                    leased.push(record);
                }
            }
            for key in ready_keys {
                if leased.len() >= batch_limit {
                    break;
                }
                let outcome = lease_key(
                    key,
                    now_ms,
                    lease_until,
                    dept,
                    excluded,
                    &mut delivery,
                    &mut ready,
                    &mut lease_index,
                    false,
                )?;
                mutated |= outcome.mutated;
                if let Some(record) = outcome.record {
                    leased.push(record);
                }
            }
        }
        if !mutated {
            return Ok(leased);
        }
        commit_write(write)?;
        Ok(leased)
    }

    fn collect_leaseable_keys(
        &self,
        now_ms: u64,
        batch_limit: usize,
        dept: Option<&str>,
        excluded: &BTreeSet<String>,
    ) -> Result<(
        Vec<super::delivery_index::DueIndexKey>,
        Vec<super::delivery_index::DueIndexKey>,
    )> {
        let read = self.db.begin_read()?;
        let expired_quota = expired_lease_quota(batch_limit);
        let scan_budget = scan_budget(batch_limit, excluded.len());
        let expired_keys = collect_due_keys(
            &read.open_table(LEASED_BY_DEPT_UNTIL)?,
            dept,
            now_ms,
            scan_budget,
        )?
        .into_iter()
        .take(expired_quota)
        .collect();
        let ready_keys = collect_due_keys(
            &read.open_table(READY_BY_DEPT_DUE)?,
            dept,
            now_ms,
            scan_budget,
        )?;
        Ok((expired_keys, ready_keys))
    }

    pub(crate) fn ack(&self, delivery_id: &str, lease_generation: u64) -> Result<bool> {
        let mut op = StoreOpWatch::new("ack", "<unknown>");
        let write = self.begin_write()?;
        let applied = {
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            if let Some(current) = read_delivery_table(&delivery, delivery_id)? {
                op.set_dept(&current.dept);
                if current.lease_generation == lease_generation {
                    if let Some(lease_until) = current.lease_until_ms {
                        let mut lease_index = write.open_table(LEASED_BY_DEPT_UNTIL)?;
                        lease_index.remove(
                            make_index_key(&current.dept, lease_until, delivery_id).as_str(),
                        )?;
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
        commit_write(write)?;
        Ok(applied)
    }

    pub(crate) fn retry(
        &self,
        delivery_id: &str,
        lease_generation: u64,
        failure: &RetryFailure,
        policy: &RetryPolicy,
        now_ms: u64,
    ) -> Result<RetryOutcome> {
        let mut op = StoreOpWatch::new("retry", "<unknown>");
        let write = self.begin_write()?;
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
                    let mut lease_index = write.open_table(LEASED_BY_DEPT_UNTIL)?;
                    lease_index
                        .remove(make_index_key(&current.dept, lease_until, delivery_id).as_str())?;

                    let attempt = current.attempt.saturating_add(1);
                    current.attempt = attempt;
                    current.last_error_excerpt = Some(error_excerpt(&failure.message));
                    current.lease_until_ms = None;

                    if attempt >= policy.max_attempts {
                        let dead = DeadRecord {
                            delivery_id: current.delivery_id.clone(),
                            queue: current.queue.clone(),
                            dept: current.dept.clone(),
                            source: current.source.clone(),
                            observed_at_ms: current.observed_at_ms,
                            not_before_ms: current.not_before_ms,
                            dead_at_ms: now_ms,
                            attempts: attempt,
                            redrive_count: current.redrive_count,
                            replayable: failure.replayable,
                            permanent: !failure.replayable,
                            error_excerpt: current.last_error_excerpt.clone(),
                            record: failure.replayable.then_some(current.clone()),
                        };
                        let bytes = serde_json::to_vec(&dead)?;
                        let mut dead_table = write.open_table(DEAD_BY_ID)?;
                        dead_table.insert(delivery_id, bytes.as_slice())?;
                        if failure.replayable {
                            let mut dead_index = write.open_table(DEAD_BY_DEPT_DUE)?;
                            dead_index.insert(
                                make_dead_due_index_key(&dead.dept, dead.dead_at_ms, delivery_id)
                                    .as_str(),
                                &(),
                            )?;
                        } else {
                            let mut terminal_index = write.open_table(TERMINAL_DEAD_BY_TIME)?;
                            terminal_index.insert(
                                make_dead_due_index_key(&dead.dept, dead.dead_at_ms, delivery_id)
                                    .as_str(),
                                &(),
                            )?;
                            suppress_terminal_delivery(&write, delivery_id)?;
                        }
                        delivery.remove(delivery_id)?;
                        if failure.replayable {
                            RetryOutcome::DeadPendingRedrive
                        } else {
                            RetryOutcome::PermanentDead
                        }
                    } else {
                        let delay = backoff_delay(policy.base, policy.cap, attempt);
                        let jitter = bounded_jitter(delivery_id, attempt, policy.base);
                        current.not_before_ms = now_ms
                            .saturating_add(duration_millis(delay))
                            .saturating_add(duration_millis(jitter));
                        let bytes = serde_json::to_vec(&current)?;
                        delivery.insert(delivery_id, bytes.as_slice())?;
                        let mut ready = write.open_table(READY_BY_DEPT_DUE)?;
                        ready.insert(
                            make_index_key(&current.dept, current.not_before_ms, delivery_id)
                                .as_str(),
                            &(),
                        )?;
                        RetryOutcome::Scheduled
                    }
                }
            } else {
                RetryOutcome::Missing
            }
        };
        compact_terminal_dead_records(&write, now_ms)?;
        commit_write(write)?;
        Ok(outcome)
    }

    pub(crate) fn redrive_due(
        &self,
        policy: &RedrivePolicy,
        now_ms: u64,
        batch_limit: usize,
    ) -> Result<RedriveResult> {
        let _op = StoreOpWatch::new("redrive", "<any>");
        let mut result = RedriveResult {
            redriven: Vec::new(),
            permanent: Vec::new(),
        };
        if batch_limit == 0 {
            return Ok(result);
        }
        let cooldown_ms = duration_millis(policy.cooldown);
        let due_keys = self.collect_due_dead_keys(policy, now_ms, batch_limit)?;
        if due_keys.is_empty() {
            return Ok(result);
        }
        let write = self.begin_write()?;
        let mut mutated = false;
        {
            let mut dead_table = write.open_table(DEAD_BY_ID)?;
            let mut delivery = write.open_table(DELIVERY_BY_ID)?;
            let mut ready = write.open_table(READY_BY_DEPT_DUE)?;
            let mut dead_index = write.open_table(DEAD_BY_DEPT_DUE)?;
            let mut terminal_index = write.open_table(TERMINAL_DEAD_BY_TIME)?;
            for key in due_keys {
                let delivery_id = key.delivery_id;
                let Some(mut dead) = read_dead_table(&dead_table, &delivery_id)? else {
                    dead_index.remove(key.key.as_str())?;
                    mutated = true;
                    continue;
                };
                if dead.dead_at_ms != key.due_ms {
                    dead_index.remove(key.key.as_str())?;
                    if should_index_dead_record(&dead) {
                        dead_index.insert(
                            make_dead_due_index_key(&dead.dept, dead.dead_at_ms, &delivery_id)
                                .as_str(),
                            &(),
                        )?;
                    }
                    mutated = true;
                    continue;
                }
                if !dead_redrive_cooldown_elapsed(dead.dead_at_ms, cooldown_ms, now_ms) {
                    continue;
                }
                if dead.permanent || !dead.replayable {
                    dead_index.remove(key.key.as_str())?;
                    mutated = true;
                    continue;
                }
                let Some(mut record) = dead.record.clone() else {
                    dead_index.remove(key.key.as_str())?;
                    dead.permanent = true;
                    dead.replayable = false;
                    let bytes = serde_json::to_vec(&dead)?;
                    dead_table.insert(delivery_id.as_str(), bytes.as_slice())?;
                    terminal_index.insert(
                        make_dead_due_index_key(&dead.dept, dead.dead_at_ms, delivery_id.as_str())
                            .as_str(),
                        &(),
                    )?;
                    suppress_terminal_delivery(&write, delivery_id.as_str())?;
                    result.permanent.push(dead);
                    mutated = true;
                    continue;
                };
                if dead.redrive_count >= policy.max_redrives {
                    dead_index.remove(key.key.as_str())?;
                    dead.permanent = true;
                    dead.replayable = false;
                    dead.record = None;
                    let bytes = serde_json::to_vec(&dead)?;
                    dead_table.insert(delivery_id.as_str(), bytes.as_slice())?;
                    terminal_index.insert(
                        make_dead_due_index_key(&dead.dept, dead.dead_at_ms, delivery_id.as_str())
                            .as_str(),
                        &(),
                    )?;
                    suppress_terminal_delivery(&write, delivery_id.as_str())?;
                    result.permanent.push(dead);
                    mutated = true;
                    continue;
                }
                if delivery.get(delivery_id.as_str())?.is_some() {
                    dead_index.remove(key.key.as_str())?;
                    dead.permanent = true;
                    dead.replayable = false;
                    dead.record = None;
                    dead.error_excerpt =
                        Some(error_excerpt("redrive collision with live delivery"));
                    let bytes = serde_json::to_vec(&dead)?;
                    dead_table.insert(delivery_id.as_str(), bytes.as_slice())?;
                    terminal_index.insert(
                        make_dead_due_index_key(&dead.dept, dead.dead_at_ms, delivery_id.as_str())
                            .as_str(),
                        &(),
                    )?;
                    suppress_terminal_delivery(&write, delivery_id.as_str())?;
                    result.permanent.push(dead);
                    mutated = true;
                    continue;
                }
                dead_index.remove(key.key.as_str())?;
                record.redrive_count = dead.redrive_count.saturating_add(1);
                record.attempt = 0;
                record.lease_until_ms = None;
                record.lease_generation = record.lease_generation.saturating_add(1);
                record.not_before_ms = now_ms;
                record.last_error_excerpt = None;
                let bytes = serde_json::to_vec(&record)?;
                delivery.insert(delivery_id.as_str(), bytes.as_slice())?;
                ready.insert(
                    make_index_key(&record.dept, record.not_before_ms, delivery_id.as_str())
                        .as_str(),
                    &(),
                )?;
                dead_table.remove(delivery_id.as_str())?;
                result.redriven.push(record);
                mutated = true;
            }
            drop(dead_table);
            drop(delivery);
            drop(ready);
            drop(dead_index);
            drop(terminal_index);
            mutated |= compact_terminal_dead_records(&write, now_ms)? > 0;
        }
        if mutated {
            commit_write(write)?;
        }
        Ok(result)
    }

    fn collect_due_dead_keys(
        &self,
        policy: &RedrivePolicy,
        now_ms: u64,
        batch_limit: usize,
    ) -> Result<Vec<super::delivery_index::DueIndexKey>> {
        let cooldown_ms = duration_millis(policy.cooldown);
        let read = self.db.begin_read()?;
        let dead_index = read.open_table(DEAD_BY_DEPT_DUE)?;
        let dead_table = read.open_table(DEAD_BY_ID)?;
        let start = format!("{:020}/", 0);
        let end = format!("{now_ms:020}/\u{10ffff}");
        let mut due = Vec::new();
        for entry in dead_index.range::<&str>(start.as_str()..=end.as_str())? {
            let (key, _) = entry?;
            let parsed = parse_dead_due_index_key(key.value())?;
            let Some(dead) = read_dead_table_read_only(&dead_table, &parsed.delivery_id)? else {
                due.push(parsed);
                if due.len() >= batch_limit {
                    break;
                }
                continue;
            };
            if dead.dead_at_ms != parsed.due_ms
                || dead_redrive_cooldown_elapsed(dead.dead_at_ms, cooldown_ms, now_ms)
            {
                due.push(parsed);
                if due.len() >= batch_limit {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(due)
    }

    #[cfg(test)]
    pub(crate) fn get(&self, delivery_id: &str) -> Result<Option<DeliveryRecord>> {
        let read = self.db.begin_read()?;
        let delivery = read.open_table(DELIVERY_BY_ID)?;
        read_delivery_read_only(&delivery, delivery_id)
    }

    pub(crate) fn get_dead(&self, delivery_id: &str) -> Result<Option<DeadRecord>> {
        let read = self.db.begin_read()?;
        let dead = read.open_table(DEAD_BY_ID)?;
        let Some(bytes) = dead.get(delivery_id)? else {
            return Ok(None);
        };
        match serde_json::from_slice(bytes.value()) {
            Ok(dead) => Ok(Some(dead)),
            Err(err) => {
                tracing::warn!(
                    delivery_id = %delivery_id,
                    error = %err,
                    "skipping undecodable dead delivery record"
                );
                Ok(None)
            }
        }
    }

    pub(crate) fn observe_snapshot(
        &self,
        durable_root: &Path,
        database: &Path,
        options: &DeliveryObserveOptions,
    ) -> Result<DeliveryObserveSnapshot> {
        let read = self.db.begin_read()?;
        let delivery = read.open_table(DELIVERY_BY_ID)?;
        let dead = read.open_table(DEAD_BY_ID)?;
        let mut deliveries = Vec::new();
        let mut dead_letters = Vec::new();
        let mut queues = BTreeMap::<String, QueueAccumulator>::new();
        let mut truncated = DeliveryObserveTruncated::default();

        for entry in delivery.iter()? {
            let (delivery_id, bytes) = entry?;
            let delivery_id = delivery_id.value().to_string();
            let record = match serde_json::from_slice::<DeliveryRecord>(bytes.value()) {
                Ok(record) => record,
                Err(err) => {
                    tracing::warn!(
                        delivery_id = %delivery_id,
                        error = %err,
                        "skipping undecodable delivery record"
                    );
                    continue;
                }
            };
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

        for entry in dead.iter()? {
            let (delivery_id, bytes) = entry?;
            let delivery_id = delivery_id.value().to_string();
            let Some(record) = decode_dead_record(&delivery_id, bytes.value()) else {
                continue;
            };
            if dead_letters.len() < options.limit {
                dead_letters.push(dead_letter_observe_entry(record)?);
            } else {
                truncated.dead_letters = true;
            }
        }

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

    #[cfg(test)]
    fn ready_index_len(&self) -> Result<usize> {
        let read = self.db.begin_read()?;
        let ready = read.open_table(READY_BY_DEPT_DUE)?;
        count_index_entries(&ready)
    }

    #[cfg(test)]
    fn leased_index_len(&self) -> Result<usize> {
        let read = self.db.begin_read()?;
        let leased = read.open_table(LEASED_BY_DEPT_UNTIL)?;
        count_index_entries(&leased)
    }

    #[cfg(test)]
    fn dead_due_index_len(&self) -> Result<usize> {
        let read = self.db.begin_read()?;
        let dead = read.open_table(DEAD_BY_DEPT_DUE)?;
        count_index_entries(&dead)
    }

    #[cfg(test)]
    fn terminal_dead_index_len(&self) -> Result<usize> {
        let read = self.db.begin_read()?;
        let terminal = read.open_table(TERMINAL_DEAD_BY_TIME)?;
        count_index_entries(&terminal)
    }

    #[cfg(test)]
    fn terminal_suppression_slot_len(&self) -> Result<usize> {
        let read = self.db.begin_read()?;
        let suppression = read.open_table(TERMINAL_SUPPRESSION_BY_SLOT)?;
        count_suppression_slots(&suppression)
    }

    #[cfg(test)]
    fn terminal_suppresses(&self, delivery_id: &str) -> Result<bool> {
        let read = self.db.begin_read()?;
        let suppression = read.open_table(TERMINAL_SUPPRESSION_BY_SLOT)?;
        terminal_delivery_is_suppressed_in_table(&suppression, delivery_id)
    }

    #[cfg(test)]
    fn reset_write_counts() {
        WRITE_TXN_COUNT.set(0);
        WRITE_COMMIT_COUNT.set(0);
    }

    #[cfg(test)]
    fn write_counts() -> (usize, usize) {
        (WRITE_TXN_COUNT.get(), WRITE_COMMIT_COUNT.get())
    }

    fn begin_write(&self) -> Result<redb::WriteTransaction> {
        begin_write(&self.db)
    }
}

fn begin_write(db: &Database) -> Result<redb::WriteTransaction> {
    #[cfg(test)]
    WRITE_TXN_COUNT.set(WRITE_TXN_COUNT.get() + 1);
    Ok(db.begin_write()?)
}

fn commit_write(write: redb::WriteTransaction) -> Result<()> {
    #[cfg(test)]
    WRITE_COMMIT_COUNT.set(WRITE_COMMIT_COUNT.get() + 1);
    Ok(write.commit()?)
}

fn same_delivery_content(left: &DeliveryRecord, right: &DeliveryRecord) -> bool {
    left.delivery_id == right.delivery_id
        && left.queue == right.queue
        && left.dept == right.dept
        && left.payload == right.payload
        && left.source == right.source
        && left.cron_payload == right.cron_payload
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
        digest: stable_digest(&bytes),
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
    stable_digest(&serde_json::to_vec(value).expect("serialize JSON value"))
}

fn stable_digest(bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to string should not fail");
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
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut data = input.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = H0;
    for chunk in data.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (i, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = words[i - 15].rotate_right(7)
                ^ words[i - 15].rotate_right(18)
                ^ (words[i - 15] >> 3);
            let s1 = words[i - 2].rotate_right(17)
                ^ words[i - 2].rotate_right(19)
                ^ (words[i - 2] >> 10);
            words[i] = words[i - 16]
                .wrapping_add(s0)
                .wrapping_add(words[i - 7])
                .wrapping_add(s1);
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(words[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut out = [0_u8; 32];
    for (idx, word) in state.iter().enumerate() {
        out[idx * 4..idx * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
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

fn delete_old_global_indexes(write: &redb::WriteTransaction) -> Result<()> {
    write.delete_table(OLD_READY_BY_DUE)?;
    write.delete_table(OLD_LEASED_BY_UNTIL)?;
    Ok(())
}

fn mark_existing_dead_records_permanent(write: &redb::WriteTransaction) -> Result<()> {
    let dead_rows = {
        let dead = write.open_table(DEAD_BY_ID)?;
        let mut rows = BTreeMap::new();
        for entry in dead.iter()? {
            let (key, bytes) = entry?;
            rows.insert(key.value().to_string(), bytes.value().to_vec());
        }
        rows
    };
    let mut dead = write.open_table(DEAD_BY_ID)?;
    for (delivery_id, bytes) in dead_rows {
        let Ok(mut record) = serde_json::from_slice::<DeadRecord>(&bytes) else {
            continue;
        };
        record.permanent = true;
        record.replayable = false;
        record.record = None;
        let bytes = serde_json::to_vec(&record)?;
        dead.insert(delivery_id.as_str(), bytes.as_slice())?;
    }
    Ok(())
}

fn rebuild_dead_due_index(write: &redb::WriteTransaction) -> Result<()> {
    write.delete_table(DEAD_BY_DEPT_DUE)?;
    let dead_rows = {
        let dead = write.open_table(DEAD_BY_ID)?;
        let mut rows = Vec::new();
        for entry in dead.iter()? {
            let (key, bytes) = entry?;
            rows.push((key.value().to_string(), bytes.value().to_vec()));
        }
        rows
    };
    let mut dead_index = write.open_table(DEAD_BY_DEPT_DUE)?;
    for (delivery_id, bytes) in dead_rows {
        let Some(record) = decode_dead_record(&delivery_id, &bytes) else {
            continue;
        };
        if should_index_dead_record(&record) {
            dead_index.insert(
                make_dead_due_index_key(&record.dept, record.dead_at_ms, &delivery_id).as_str(),
                &(),
            )?;
        }
    }
    Ok(())
}

fn rebuild_terminal_dead_tables(write: &redb::WriteTransaction) -> Result<()> {
    write.delete_table(TERMINAL_DEAD_BY_TIME)?;
    let dead_rows = {
        let dead = write.open_table(DEAD_BY_ID)?;
        let mut rows = Vec::new();
        for entry in dead.iter()? {
            let (key, bytes) = entry?;
            rows.push((key.value().to_string(), bytes.value().to_vec()));
        }
        rows
    };
    let mut terminal_index = write.open_table(TERMINAL_DEAD_BY_TIME)?;
    for (delivery_id, bytes) in dead_rows {
        let Some(record) = decode_dead_record(&delivery_id, &bytes) else {
            continue;
        };
        if is_terminal_dead_record(&record) {
            terminal_index.insert(
                make_dead_due_index_key(&record.dept, record.dead_at_ms, &delivery_id).as_str(),
                &(),
            )?;
            suppress_terminal_delivery(write, delivery_id.as_str())?;
        }
    }
    Ok(())
}

fn should_index_dead_record(record: &DeadRecord) -> bool {
    !record.permanent && record.replayable && record.record.is_some()
}

fn is_terminal_dead_record(record: &DeadRecord) -> bool {
    record.permanent || !record.replayable
}

fn suppress_terminal_delivery(write: &redb::WriteTransaction, delivery_id: &str) -> Result<()> {
    let mut suppression = write.open_table(TERMINAL_SUPPRESSION_BY_SLOT)?;
    suppress_terminal_delivery_in_table(&mut suppression, delivery_id)
}

fn suppress_terminal_delivery_in_table(
    suppression: &mut redb::Table<'_, &str, &str>,
    delivery_id: &str,
) -> Result<()> {
    for slot in terminal_suppression_probe_keys(delivery_id) {
        if let Some(current) = suppression.get(slot.as_str())? {
            if current.value() == delivery_id {
                return Ok(());
            }
            continue;
        }
        suppression.insert(slot.as_str(), delivery_id)?;
        return Ok(());
    }
    bail!(
        "terminal suppression table exhausted for delivery_id: {}",
        delivery_id
    );
}

fn terminal_delivery_is_suppressed(
    write: &redb::WriteTransaction,
    delivery_id: &str,
) -> Result<bool> {
    let suppression = write.open_table(TERMINAL_SUPPRESSION_BY_SLOT)?;
    terminal_delivery_is_suppressed_in_table(&suppression, delivery_id)
}

fn terminal_delivery_is_suppressed_in_table<T>(table: &T, delivery_id: &str) -> Result<bool>
where
    T: ReadableTable<&'static str, &'static str>,
{
    for slot in terminal_suppression_probe_keys(delivery_id) {
        let Some(current) = table.get(slot.as_str())? else {
            return Ok(false);
        };
        if current.value() == delivery_id {
            return Ok(true);
        }
    }
    Ok(false)
}

fn terminal_suppression_probe_keys(delivery_id: &str) -> impl Iterator<Item = String> {
    let start = terminal_suppression_hash(delivery_id) % TERMINAL_SUPPRESSION_SLOTS;
    (0..TERMINAL_SUPPRESSION_SLOTS).map(move |offset| {
        let slot = (start + offset) % TERMINAL_SUPPRESSION_SLOTS;
        terminal_suppression_slot_key(slot)
    })
}

fn terminal_suppression_slot_key(slot: u64) -> String {
    format!("{slot:05x}")
}

fn terminal_suppression_hash(delivery_id: &str) -> u64 {
    fnv1a64(0xcbf29ce484222325, delivery_id.as_bytes())
}

fn fnv1a64(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn drop_legacy_terminal_suppressed_table(write: &redb::WriteTransaction) -> Result<()> {
    match write.delete_table(TERMINAL_SUPPRESSED_BY_ID) {
        Ok(_) | Err(redb::TableError::TableDoesNotExist(_)) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn drop_legacy_terminal_suppression_filter(write: &redb::WriteTransaction) -> Result<()> {
    let legacy_filter: TableDefinition<&str, &[u8]> =
        TableDefinition::new("terminal_suppression_filter");
    match write.delete_table(legacy_filter) {
        Ok(_) | Err(redb::TableError::TableDoesNotExist(_)) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn read_legacy_terminal_suppressed_ids(write: &redb::WriteTransaction) -> Result<Vec<String>> {
    match write.open_table(TERMINAL_SUPPRESSED_BY_ID) {
        Ok(table) => {
            let mut ids = Vec::new();
            for entry in table.iter()? {
                let (key, _) = entry?;
                ids.push(key.value().to_string());
            }
            Ok(ids)
        }
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
fn count_suppression_slots(table: &redb::ReadOnlyTable<&str, &str>) -> Result<usize> {
    let mut count = 0;
    for entry in table.iter()? {
        let _ = entry?;
        count += 1;
    }
    Ok(count)
}

fn import_legacy_terminal_suppressed_ids(write: &redb::WriteTransaction) -> Result<()> {
    for delivery_id in read_legacy_terminal_suppressed_ids(write)? {
        suppress_terminal_delivery(write, delivery_id.as_str())?;
    }
    Ok(())
}

fn compact_terminal_dead_records(write: &redb::WriteTransaction, now_ms: u64) -> Result<usize> {
    let terminal_dead_cutoff_ms = now_ms.checked_sub(TERMINAL_DEAD_RETENTION_MS);
    let stale_keys = terminal_dead_cutoff_ms
        .map(|cutoff_ms| collect_compactable_terminal_dead_keys(write, cutoff_ms))
        .transpose()?
        .unwrap_or_default();
    if stale_keys.is_empty() {
        return Ok(0);
    }
    let mut compacted = 0;
    let mut dead = write.open_table(DEAD_BY_ID)?;
    let mut terminal_index = write.open_table(TERMINAL_DEAD_BY_TIME)?;
    let mut dead_due_index = write.open_table(DEAD_BY_DEPT_DUE)?;
    for key in stale_keys {
        let delivery_id = key.delivery_id;
        let Some(record) = read_dead_table(&dead, &delivery_id)? else {
            terminal_index.remove(key.key.as_str())?;
            compacted += 1;
            continue;
        };
        if record.dead_at_ms != key.due_ms || !is_terminal_dead_record(&record) {
            terminal_index.remove(key.key.as_str())?;
            if is_terminal_dead_record(&record) {
                terminal_index.insert(
                    make_dead_due_index_key(&record.dept, record.dead_at_ms, &delivery_id).as_str(),
                    &(),
                )?;
                suppress_terminal_delivery(write, delivery_id.as_str())?;
            }
            compacted += 1;
            continue;
        }
        let Some(cutoff_ms) = terminal_dead_cutoff_ms else {
            break;
        };
        if record.dead_at_ms > cutoff_ms {
            break;
        }
        dead.remove(delivery_id.as_str())?;
        terminal_index.remove(key.key.as_str())?;
        dead_due_index.remove(
            make_dead_due_index_key(&record.dept, record.dead_at_ms, &delivery_id).as_str(),
        )?;
        compacted += 1;
    }
    Ok(compacted)
}

fn collect_compactable_terminal_dead_keys(
    write: &redb::WriteTransaction,
    cutoff_ms: u64,
) -> Result<Vec<super::delivery_index::DueIndexKey>> {
    let terminal_index = write.open_table(TERMINAL_DEAD_BY_TIME)?;
    let start = format!("{:020}/", 0);
    let end = format!("{cutoff_ms:020}/\u{10ffff}");
    let mut keys = Vec::new();
    for entry in terminal_index.range::<&str>(start.as_str()..=end.as_str())? {
        let (key, _) = entry?;
        keys.push(parse_dead_due_index_key(key.value())?);
        if keys.len() >= TERMINAL_DEAD_COMPACTION_LIMIT {
            break;
        }
    }
    Ok(keys)
}

#[allow(dead_code)]
fn collect_due_dead_ids(
    write: &redb::WriteTransaction,
    policy: &RedrivePolicy,
    now_ms: u64,
    batch_limit: usize,
) -> Result<Vec<String>> {
    let cooldown_ms = duration_millis(policy.cooldown);
    let dead = write.open_table(DEAD_BY_ID)?;
    let mut due = Vec::new();
    for entry in dead.iter()? {
        let (key, bytes) = entry?;
        let Some(record) = decode_dead_record(key.value(), bytes.value()) else {
            continue;
        };
        if record.permanent || !record.replayable {
            continue;
        }
        if record.dead_at_ms.saturating_add(cooldown_ms) <= now_ms {
            due.push(record.delivery_id);
            if due.len() >= batch_limit {
                break;
            }
        }
    }
    Ok(due)
}

fn decode_dead_record(delivery_id: &str, bytes: &[u8]) -> Option<DeadRecord> {
    match serde_json::from_slice(bytes) {
        Ok(record) => Some(record),
        Err(err) => {
            tracing::warn!(
                delivery_id = %delivery_id,
                error = %err,
                "skipping undecodable dead delivery record"
            );
            None
        }
    }
}

fn read_dead_table(
    table: &redb::Table<'_, &str, &[u8]>,
    delivery_id: &str,
) -> Result<Option<DeadRecord>> {
    let Some(bytes) = table.get(delivery_id)? else {
        return Ok(None);
    };
    Ok(decode_dead_record(delivery_id, bytes.value()))
}

fn read_dead_table_read_only(
    table: &redb::ReadOnlyTable<&str, &[u8]>,
    delivery_id: &str,
) -> Result<Option<DeadRecord>> {
    let Some(bytes) = table.get(delivery_id)? else {
        return Ok(None);
    };
    Ok(decode_dead_record(delivery_id, bytes.value()))
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

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn dead_redrive_cooldown_elapsed(dead_at_ms: u64, cooldown_ms: u64, now_ms: u64) -> bool {
    dead_at_ms.saturating_add(cooldown_ms) <= now_ms
}

#[cfg(test)]
mod tests {
    use super::super::delivery_types::{SourceKind, SourceRef};
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

    fn insert_replayable_dead(store: &DeliveryStore, delivery_id: &str, dead_at_ms: u64) {
        let mut delivery = record(delivery_id, 100);
        delivery.delivery_id = delivery_id.to_string();
        let dead = DeadRecord {
            delivery_id: delivery_id.to_string(),
            queue: delivery.queue.clone(),
            dept: delivery.dept.clone(),
            source: delivery.source.clone(),
            observed_at_ms: delivery.observed_at_ms,
            not_before_ms: delivery.not_before_ms,
            dead_at_ms,
            attempts: 1,
            redrive_count: 0,
            replayable: true,
            permanent: false,
            error_excerpt: Some("timeout".to_string()),
            record: Some(delivery),
        };
        let write = store.db.begin_write().unwrap();
        {
            let mut dead_table = write.open_table(DEAD_BY_ID).unwrap();
            dead_table
                .insert(delivery_id, serde_json::to_vec(&dead).unwrap().as_slice())
                .unwrap();
            let mut dead_index = write.open_table(DEAD_BY_DEPT_DUE).unwrap();
            dead_index
                .insert(
                    make_dead_due_index_key(&dead.dept, dead.dead_at_ms, delivery_id).as_str(),
                    &(),
                )
                .unwrap();
        }
        write.commit().unwrap();
    }

    fn insert_permanent_dead(store: &DeliveryStore, delivery_id: &str, dead_at_ms: u64) {
        let dead = DeadRecord {
            delivery_id: delivery_id.to_string(),
            queue: "input".to_string(),
            dept: "worker".to_string(),
            source: None,
            observed_at_ms: 10,
            not_before_ms: 100,
            dead_at_ms,
            attempts: 1,
            redrive_count: 0,
            replayable: false,
            permanent: true,
            error_excerpt: Some("final".to_string()),
            record: None,
        };
        let write = store.db.begin_write().unwrap();
        {
            let mut dead_table = write.open_table(DEAD_BY_ID).unwrap();
            dead_table
                .insert(delivery_id, serde_json::to_vec(&dead).unwrap().as_slice())
                .unwrap();
            let mut terminal_index = write.open_table(TERMINAL_DEAD_BY_TIME).unwrap();
            terminal_index
                .insert(
                    make_dead_due_index_key(&dead.dept, dead.dead_at_ms, delivery_id).as_str(),
                    &(),
                )
                .unwrap();
            suppress_terminal_delivery(&write, delivery_id).unwrap();
        }
        write.commit().unwrap();
    }

    fn colliding_terminal_id(delivery_id: &str) -> String {
        let target_slot = terminal_suppression_hash(delivery_id) % TERMINAL_SUPPRESSION_SLOTS;
        for index in 0..TERMINAL_SUPPRESSION_SLOTS.saturating_mul(2) {
            let candidate = format!("{delivery_id}-slot-collision-{index}");
            if terminal_suppression_hash(&candidate) % TERMINAL_SUPPRESSION_SLOTS == target_slot {
                return candidate;
            }
        }
        panic!("failed to find terminal suppression slot collision");
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
    fn duplicate_enqueue_dedup_keyed_record_keeps_existing_record() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let mut original = record("one", 100);
        original.collapse_by_dedup_id = true;
        let mut duplicate = record("one", 50);
        duplicate.payload = serde_json::json!({"n": 2});
        duplicate.source = Some(SourceRef {
            kind: SourceKind::Cron,
            reference: "later-tick".to_string(),
        });

        store.enqueue(&original).unwrap();
        store.enqueue(&duplicate).unwrap();
        let current = store.get("one").unwrap().unwrap();

        assert_eq!(current.payload, serde_json::json!({"n": 1}));
        assert_eq!(current.source, original.source);
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
            .retry(
                "one",
                1,
                &failure("temporary\nfailure", false),
                &policy(3),
                120,
            )
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

        let outcome = store
            .retry("one", 0, &failure("temporary", false), &policy(3), 120)
            .unwrap();
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

        let outcome = store
            .retry("one", 0, &failure("temporary", false), &policy(3), 120)
            .unwrap();
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
                &failure(&format!("final {}", "x".repeat(800)), true),
                &policy(1),
                120,
            )
            .unwrap();

        assert_eq!(outcome, RetryOutcome::DeadPendingRedrive);
        assert!(store.get("one").unwrap().is_none());
        let dead = store.get_dead("one").unwrap().unwrap();
        assert_eq!(dead.delivery_id, "one");
        assert_eq!(dead.queue, "input");
        assert_eq!(dead.dept, "worker");
        assert_eq!(dead.source, None);
        assert_eq!(dead.observed_at_ms, 10);
        assert_eq!(dead.not_before_ms, 100);
        assert_eq!(dead.dead_at_ms, 120);
        assert_eq!(dead.attempts, 1);
        assert_eq!(dead.redrive_count, 0);
        assert!(dead.replayable);
        assert!(!dead.permanent);
        assert!(dead.error_excerpt.as_ref().unwrap().len() <= ERROR_EXCERPT_LIMIT);
        let dead_json = serde_json::to_value(&dead).unwrap();
        assert!(dead_json.get("payload").is_none());
        assert!(dead_json.get("record").is_some());
        assert_eq!(store.dead_due_index_len().unwrap(), 1);
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
        let outcome = store
            .retry("one", 1, &failure("temporary", false), &policy(3), 120)
            .unwrap();
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

        let outcome = store
            .retry("one", 1, &failure("final", false), &policy(1), 120)
            .unwrap();

        assert_eq!(outcome, RetryOutcome::PermanentDead);
        assert_eq!(store.ready_index_len().unwrap(), 0);
        assert_eq!(store.leased_index_len().unwrap(), 0);
        assert!(store
            .lease(151, 10, Duration::from_millis(50))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn transient_dead_record_redrives_after_cooldown() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();
        assert_eq!(
            store
                .retry(
                    "one",
                    1,
                    &failure("classified upstream timeout", true),
                    &policy(1),
                    120,
                )
                .unwrap(),
            RetryOutcome::DeadPendingRedrive
        );
        let policy = RedrivePolicy {
            max_redrives: 3,
            cooldown: Duration::from_millis(50),
        };

        let early = store.redrive_due(&policy, 169, 10).unwrap();
        assert!(early.redriven.is_empty());
        assert!(early.permanent.is_empty());
        assert_eq!(store.dead_due_index_len().unwrap(), 1);
        let due = store.redrive_due(&policy, 170, 10).unwrap();

        assert_eq!(due.redriven.len(), 1);
        assert!(due.permanent.is_empty());
        assert!(store.get_dead("one").unwrap().is_none());
        let current = store.get("one").unwrap().unwrap();
        assert_eq!(current.delivery_id, "one");
        assert_eq!(current.attempt, 0);
        assert_eq!(current.redrive_count, 1);
        assert_eq!(current.lease_until_ms, None);
        assert_eq!(current.not_before_ms, 170);
        assert_eq!(current.last_error_excerpt, None);
        let leased = store
            .lease_for_dept("worker", 170, 1, Duration::from_millis(50))
            .unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].redrive_count, 1);
        assert_eq!(store.dead_due_index_len().unwrap(), 0);
    }

    #[test]
    fn redrive_due_does_not_redrive_zero_dead_at_before_cooldown() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_replayable_dead(&store, "zero", 0);
        let policy = RedrivePolicy {
            max_redrives: 3,
            cooldown: Duration::from_millis(600),
        };
        DeliveryStore::reset_write_counts();

        let early = store.redrive_due(&policy, 0, 10).unwrap();

        assert!(early.redriven.is_empty());
        assert!(early.permanent.is_empty());
        assert!(store.get("zero").unwrap().is_none());
        assert!(store.get_dead("zero").unwrap().is_some());
        assert_eq!(store.dead_due_index_len().unwrap(), 1);
        assert_eq!(DeliveryStore::write_counts(), (0, 0));

        let due = store.redrive_due(&policy, 600, 10).unwrap();

        assert_eq!(due.redriven.len(), 1);
        assert_eq!(due.redriven[0].delivery_id, "zero");
        assert!(store.get_dead("zero").unwrap().is_none());
        assert_eq!(store.dead_due_index_len().unwrap(), 0);
    }

    #[test]
    fn redrive_due_handles_near_u64_max_saturating_cooldown() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_replayable_dead(&store, "near-max", u64::MAX - 40);

        let result = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::from_millis(50),
                },
                u64::MAX,
                10,
            )
            .unwrap();

        assert_eq!(result.redriven.len(), 1);
        assert_eq!(result.redriven[0].delivery_id, "near-max");
        assert_eq!(result.redriven[0].not_before_ms, u64::MAX);
        assert!(store.get_dead("near-max").unwrap().is_none());
        assert_eq!(store.dead_due_index_len().unwrap(), 0);
    }

    #[test]
    fn redrive_due_skips_cooldown_window_row_until_elapsed() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_replayable_dead(&store, "window", 80);
        let policy = RedrivePolicy {
            max_redrives: 3,
            cooldown: Duration::from_millis(50),
        };
        DeliveryStore::reset_write_counts();

        let early = store.redrive_due(&policy, 100, 10).unwrap();

        assert!(early.redriven.is_empty());
        assert!(early.permanent.is_empty());
        assert!(store.get("window").unwrap().is_none());
        assert!(store.get_dead("window").unwrap().is_some());
        assert_eq!(store.dead_due_index_len().unwrap(), 1);
        assert_eq!(DeliveryStore::write_counts(), (0, 0));

        let due = store.redrive_due(&policy, 130, 10).unwrap();

        assert_eq!(due.redriven.len(), 1);
        assert_eq!(due.redriven[0].delivery_id, "window");
        assert!(store.get_dead("window").unwrap().is_none());
        assert_eq!(store.dead_due_index_len().unwrap(), 0);
    }

    #[test]
    fn redrive_due_no_due_records_does_not_open_write_transaction() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();
        store
            .retry("one", 1, &failure("timeout", true), &policy(1), 120)
            .unwrap();
        DeliveryStore::reset_write_counts();

        let result = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::from_millis(50),
                },
                169,
                10,
            )
            .unwrap();

        assert!(result.redriven.is_empty());
        assert!(result.permanent.is_empty());
        assert_eq!(DeliveryStore::write_counts(), (0, 0));
    }

    #[test]
    fn global_redrive_due_is_ordered_by_dead_at_across_depts() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        for index in 0..8 {
            let mut next = record(&format!("aaa-{index}"), 100);
            next.dept = "aaa".to_string();
            store.enqueue(&next).unwrap();
            store
                .lease_for_dept("aaa", 100, 1, Duration::from_millis(50))
                .unwrap();
            store
                .retry(
                    &next.delivery_id,
                    1,
                    &failure("aaa timeout", true),
                    &policy(1),
                    200 + index,
                )
                .unwrap();
        }
        let mut zzz = record("zzz-oldest", 100);
        zzz.dept = "zzz".to_string();
        store.enqueue(&zzz).unwrap();
        store
            .lease_for_dept("zzz", 100, 1, Duration::from_millis(50))
            .unwrap();
        store
            .retry(
                "zzz-oldest",
                1,
                &failure("zzz timeout", true),
                &policy(1),
                150,
            )
            .unwrap();

        let due = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::ZERO,
                },
                300,
                2,
            )
            .unwrap();

        let ids = due
            .redriven
            .iter()
            .map(|record| record.delivery_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["zzz-oldest", "aaa-0"]);
    }

    #[test]
    fn redrive_due_removes_stale_dead_index_entry() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        {
            let write = store.db.begin_write().unwrap();
            {
                let mut dead_index = write.open_table(DEAD_BY_DEPT_DUE).unwrap();
                dead_index
                    .insert(
                        make_dead_due_index_key("worker", 10, "missing").as_str(),
                        &(),
                    )
                    .unwrap();
            }
            write.commit().unwrap();
        }

        let result = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::ZERO,
                },
                10,
                10,
            )
            .unwrap();

        assert!(result.redriven.is_empty());
        assert!(result.permanent.is_empty());
        assert_eq!(store.dead_due_index_len().unwrap(), 0);
    }

    #[test]
    fn redrive_due_marks_live_collision_permanent_and_removes_index() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let mut dead_record = record("one", 100);
        store.enqueue(&record("one", 100)).unwrap();
        dead_record.delivery_id = "dead-copy".to_string();
        let dead = DeadRecord {
            delivery_id: "one".to_string(),
            queue: dead_record.queue.clone(),
            dept: dead_record.dept.clone(),
            source: dead_record.source.clone(),
            observed_at_ms: dead_record.observed_at_ms,
            not_before_ms: dead_record.not_before_ms,
            dead_at_ms: 10,
            attempts: 1,
            redrive_count: 0,
            replayable: true,
            permanent: false,
            error_excerpt: None,
            record: Some(dead_record),
        };
        {
            let write = store.db.begin_write().unwrap();
            {
                let mut dead_table = write.open_table(DEAD_BY_ID).unwrap();
                dead_table
                    .insert("one", serde_json::to_vec(&dead).unwrap().as_slice())
                    .unwrap();
                let mut dead_index = write.open_table(DEAD_BY_DEPT_DUE).unwrap();
                dead_index
                    .insert(make_dead_due_index_key("worker", 10, "one").as_str(), &())
                    .unwrap();
            }
            write.commit().unwrap();
        }

        let result = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::ZERO,
                },
                10,
                10,
            )
            .unwrap();

        assert!(result.redriven.is_empty());
        assert_eq!(result.permanent.len(), 1);
        assert!(store.get_dead("one").unwrap().unwrap().permanent);
        assert_eq!(store.dead_due_index_len().unwrap(), 0);
    }

    #[test]
    fn redrive_count_survives_reopen_and_caps_to_permanent_dead() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("delivery.redb");
        {
            let store = DeliveryStore::open(&path).unwrap();
            store.enqueue(&record("one", 100)).unwrap();
            store.lease(100, 10, Duration::from_millis(50)).unwrap();
            assert_eq!(
                store
                    .retry(
                        "one",
                        1,
                        &failure("classified upstream timeout", true),
                        &policy(1),
                        120,
                    )
                    .unwrap(),
                RetryOutcome::DeadPendingRedrive
            );
            let redriven = store
                .redrive_due(
                    &RedrivePolicy {
                        max_redrives: 1,
                        cooldown: Duration::ZERO,
                    },
                    121,
                    10,
                )
                .unwrap();
            assert_eq!(redriven.redriven[0].redrive_count, 1);
        }
        let store = DeliveryStore::open(&path).unwrap();
        let record = store.get("one").unwrap().unwrap();
        assert_eq!(record.redrive_count, 1);
        let leased = store
            .lease_for_dept("worker", 121, 1, Duration::from_millis(50))
            .unwrap()
            .remove(0);
        assert_eq!(
            store
                .retry(
                    &leased.delivery_id,
                    leased.lease_generation,
                    &failure("classified upstream timeout", true),
                    &policy(1),
                    130,
                )
                .unwrap(),
            RetryOutcome::DeadPendingRedrive
        );

        let capped = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 1,
                    cooldown: Duration::ZERO,
                },
                131,
                10,
            )
            .unwrap();

        assert!(capped.redriven.is_empty());
        assert_eq!(capped.permanent.len(), 1);
        let dead = store.get_dead("one").unwrap().unwrap();
        assert!(dead.permanent);
        assert!(!dead.replayable);
        assert_eq!(dead.redrive_count, 1);
        assert!(dead.record.is_none());
        assert_eq!(store.dead_due_index_len().unwrap(), 0);
    }

    #[test]
    fn non_transient_dead_record_is_permanent_without_redrive_payload() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store.enqueue(&record("one", 100)).unwrap();
        store.lease(100, 10, Duration::from_millis(50)).unwrap();

        let outcome = store
            .retry(
                "one",
                1,
                &failure("timeout-shaped validation failed", false),
                &policy(1),
                120,
            )
            .unwrap();

        assert_eq!(outcome, RetryOutcome::PermanentDead);
        let dead = store.get_dead("one").unwrap().unwrap();
        assert!(dead.permanent);
        assert!(!dead.replayable);
        assert!(dead.record.is_none());
        assert!(store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::ZERO,
                },
                120,
                10,
            )
            .unwrap()
            .redriven
            .is_empty());
    }

    #[test]
    fn terminal_dead_records_are_compacted_after_retention() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_permanent_dead(&store, "old", 100);
        insert_permanent_dead(
            &store,
            "young",
            100_u64.saturating_add(TERMINAL_DEAD_RETENTION_MS),
        );

        let mut fresh = record(
            "fresh",
            100_u64
                .saturating_add(TERMINAL_DEAD_RETENTION_MS)
                .saturating_add(1),
        );
        fresh.observed_at_ms = fresh.not_before_ms;
        store.enqueue(&fresh).unwrap();

        assert!(store.get_dead("old").unwrap().is_none());
        assert!(store.get_dead("young").unwrap().is_some());
        assert_eq!(store.terminal_dead_index_len().unwrap(), 1);
        assert_eq!(store.terminal_suppression_slot_len().unwrap(), 2);
        assert!(store.terminal_suppresses("old").unwrap());
        assert!(store.terminal_suppresses("young").unwrap());
        let leased = store
            .lease_for_dept(
                "worker",
                100_u64
                    .saturating_add(TERMINAL_DEAD_RETENTION_MS)
                    .saturating_add(1),
                10,
                Duration::from_millis(50),
            )
            .unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].delivery_id, "fresh");
    }

    #[test]
    fn terminal_id_is_suppressed_within_retention() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_permanent_dead(&store, "old", 100);
        let mut duplicate = record(
            "old",
            100_u64
                .saturating_add(TERMINAL_DEAD_RETENTION_MS)
                .saturating_sub(1),
        );
        duplicate.observed_at_ms = duplicate.not_before_ms;

        store.enqueue(&duplicate).unwrap();

        assert!(store.get_dead("old").unwrap().is_some());
        assert_eq!(store.terminal_dead_index_len().unwrap(), 1);
        assert_eq!(store.terminal_suppression_slot_len().unwrap(), 1);
        assert!(store.terminal_suppresses("old").unwrap());
        assert!(store
            .lease_for_dept(
                "worker",
                duplicate.not_before_ms,
                10,
                Duration::from_millis(50),
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn terminal_suppression_slot_collision_does_not_suppress_fresh_delivery() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_permanent_dead(&store, "old", 100);
        let colliding = colliding_terminal_id("old");
        assert!(!store.terminal_suppresses(&colliding).unwrap());
        let mut fresh = record(&colliding, 100);
        fresh.observed_at_ms = fresh.not_before_ms;

        store.enqueue(&fresh).unwrap();

        let leased = store
            .lease_for_dept("worker", fresh.not_before_ms, 10, Duration::from_millis(50))
            .unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].delivery_id, colliding);
        assert_eq!(store.terminal_suppression_slot_len().unwrap(), 1);
    }

    #[test]
    fn terminal_suppression_slot_collision_terminalizes_and_suppresses_exact_ids() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_permanent_dead(&store, "old", 100);
        let colliding = colliding_terminal_id("old");
        store.enqueue(&record(&colliding, 100)).unwrap();
        let leased = store
            .lease_for_dept("worker", 100, 10, Duration::from_millis(50))
            .unwrap()
            .remove(0);

        let outcome = store
            .retry(
                &leased.delivery_id,
                leased.lease_generation,
                &failure("final", false),
                &policy(1),
                120,
            )
            .unwrap();

        assert_eq!(outcome, RetryOutcome::PermanentDead);
        assert_eq!(store.terminal_suppression_slot_len().unwrap(), 2);
        assert!(store.get(&colliding).unwrap().is_none());
        assert!(store.terminal_suppresses("old").unwrap());
        assert!(store.terminal_suppresses(&colliding).unwrap());

        let mut duplicate = record(&colliding, 130);
        duplicate.observed_at_ms = duplicate.not_before_ms;
        store.enqueue(&duplicate).unwrap();
        assert!(store
            .lease_for_dept(
                "worker",
                duplicate.not_before_ms,
                10,
                Duration::from_millis(50),
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn terminal_suppression_slots_stay_bounded_after_retention() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_permanent_dead(&store, "old", 100);
        let trigger_time = 100_u64
            .saturating_add(TERMINAL_DEAD_RETENTION_MS)
            .saturating_mul(100)
            .saturating_add(1);
        let mut trigger = record("trigger", trigger_time);
        trigger.dept = "maintenance".to_string();
        trigger.observed_at_ms = trigger_time;
        store.enqueue(&trigger).unwrap();

        assert!(store.get_dead("old").unwrap().is_none());
        assert_eq!(store.terminal_dead_index_len().unwrap(), 0);
        assert_eq!(store.terminal_suppression_slot_len().unwrap(), 1);
        assert!(store.terminal_suppresses("old").unwrap());

        let mut duplicate = record("old", trigger_time.saturating_add(1));
        duplicate.observed_at_ms = duplicate.not_before_ms;
        store.enqueue(&duplicate).unwrap();
        assert!(store
            .lease_for_dept(
                "worker",
                duplicate.not_before_ms,
                10,
                Duration::from_millis(50),
            )
            .unwrap()
            .is_empty());
        assert_eq!(store.terminal_suppression_slot_len().unwrap(), 1);
    }

    #[test]
    fn duplicate_terminal_id_remains_suppressed_after_payload_compaction() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_permanent_dead(&store, "old", 100);
        let mut duplicate = record(
            "old",
            100_u64
                .saturating_add(TERMINAL_DEAD_RETENTION_MS)
                .saturating_sub(1),
        );
        duplicate.observed_at_ms = duplicate.not_before_ms;

        store.enqueue(&duplicate).unwrap();
        let trigger_time = 100_u64
            .saturating_add(TERMINAL_DEAD_RETENTION_MS)
            .saturating_mul(100)
            .saturating_add(1);
        let mut trigger = record("trigger", trigger_time);
        trigger.dept = "trigger-worker".to_string();
        trigger.observed_at_ms = trigger_time;
        store.enqueue(&trigger).unwrap();

        assert_eq!(store.terminal_suppression_slot_len().unwrap(), 1);
        assert!(store.terminal_suppresses("old").unwrap());
        assert_eq!(
            store
                .lease_for_dept(
                    "worker",
                    duplicate.not_before_ms,
                    10,
                    Duration::from_millis(50),
                )
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn future_scheduled_enqueue_does_not_compact_before_observed_retention() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_permanent_dead(&store, "old", 100);
        let mut future = record(
            "future",
            100_u64
                .saturating_add(TERMINAL_DEAD_RETENTION_MS)
                .saturating_add(1),
        );
        future.observed_at_ms = 101;

        store.enqueue(&future).unwrap();

        assert!(store.get_dead("old").unwrap().is_some());
        assert_eq!(store.terminal_dead_index_len().unwrap(), 1);
    }

    #[test]
    fn observed_enqueue_after_retention_compacts_terminal_dead_records() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_permanent_dead(&store, "old", 100);
        let mut fresh = record("fresh", 100);
        fresh.observed_at_ms = 100_u64
            .saturating_add(TERMINAL_DEAD_RETENTION_MS)
            .saturating_add(1);

        store.enqueue(&fresh).unwrap();

        assert!(store.get_dead("old").unwrap().is_none());
        assert_eq!(store.terminal_dead_index_len().unwrap(), 0);
        assert_eq!(store.terminal_suppression_slot_len().unwrap(), 1);
        assert!(store.terminal_suppresses("old").unwrap());
    }

    #[test]
    fn replayable_dead_records_are_not_terminal_compaction_candidates() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        insert_replayable_dead(&store, "replayable", 100);

        store
            .enqueue(&record(
                "fresh",
                100_u64
                    .saturating_add(TERMINAL_DEAD_RETENTION_MS)
                    .saturating_add(1),
            ))
            .unwrap();

        assert!(store.get_dead("replayable").unwrap().is_some());
        assert_eq!(store.dead_due_index_len().unwrap(), 1);
        assert_eq!(store.terminal_dead_index_len().unwrap(), 0);
        let result = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::ZERO,
                },
                100_u64
                    .saturating_add(TERMINAL_DEAD_RETENTION_MS)
                    .saturating_add(1),
                10,
            )
            .unwrap();
        assert_eq!(result.redriven.len(), 1);
        assert_eq!(result.redriven[0].delivery_id, "replayable");
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

        let snapshot = store
            .observe_snapshot(
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

        let snapshot = store
            .observe_snapshot(
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

        let snapshot = store
            .observe_snapshot(
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
    fn lease_no_due_records_does_not_open_write_transaction() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        DeliveryStore::reset_write_counts();

        let leased = store.lease(100, 10, Duration::from_millis(50)).unwrap();

        assert!(leased.is_empty());
        assert_eq!(DeliveryStore::write_counts(), (0, 0));
    }

    #[test]
    fn scan_budget_is_derived_from_batch_limit_and_exclusions() {
        assert_eq!(scan_budget(1, 0), 8);
        assert_eq!(scan_budget(2, 3), 19);
    }

    #[test]
    fn lease_for_dept_is_not_starved_by_other_dept_backlog() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let other_backlog = scan_budget(1, 0) + 1;
        for index in 0..other_backlog {
            let mut other = record(&format!("other-{index:05}"), 100);
            other.dept = "other".to_string();
            store.enqueue(&other).unwrap();
        }
        store.enqueue(&record("worker-record", 100)).unwrap();

        let leased = store
            .lease_for_dept("worker", 100, 1, Duration::from_millis(50))
            .unwrap();

        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].delivery_id, "worker-record");
        assert_eq!(store.ready_index_len().unwrap(), other_backlog);
        assert_eq!(store.leased_index_len().unwrap(), 1);
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
    fn schema_v1_open_clears_dead_rows_and_rebuilds_dept_indexes() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("delivery.redb");
        {
            let db = Database::create(&path).unwrap();
            let write = db.begin_write().unwrap();
            {
                let mut delivery = write.open_table(DELIVERY_BY_ID).unwrap();
                let pending = record("pending", 100);
                delivery
                    .insert(
                        pending.delivery_id.as_str(),
                        serde_json::to_vec(&pending).unwrap().as_slice(),
                    )
                    .unwrap();
                let mut old_ready = write.open_table(OLD_READY_BY_DUE).unwrap();
                old_ready.insert((100, "pending"), &()).unwrap();
                let mut dead = write.open_table(DEAD_BY_ID).unwrap();
                dead.insert(
                    "legacy-dead",
                    br#"{"delivery_id":"legacy-dead","queue":"input","dept":"worker","source":null,"dead_at_ms":1,"attempts":1,"error_excerpt":null}"#
                        .as_slice(),
                )
                .unwrap();
                let mut meta = write.open_table(META).unwrap();
                meta.insert("schema_version", "1").unwrap();
            }
            write.commit().unwrap();
        }

        let store = DeliveryStore::open(&path).unwrap();

        assert!(store.get_dead("legacy-dead").unwrap().is_none());
        assert_eq!(store.ready_index_len().unwrap(), 1);
        let leased = store
            .lease_for_dept("worker", 100, 1, Duration::from_millis(50))
            .unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].delivery_id, "pending");
    }

    #[test]
    fn schema_v3_open_rebuilds_dead_due_index_for_replayable_nonpermanent_records_only() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("delivery.redb");
        let replayable = DeadRecord {
            delivery_id: "replayable".to_string(),
            queue: "input".to_string(),
            dept: "worker".to_string(),
            source: None,
            observed_at_ms: 10,
            not_before_ms: 100,
            dead_at_ms: 120,
            attempts: 1,
            redrive_count: 0,
            replayable: true,
            permanent: false,
            error_excerpt: Some("timeout".to_string()),
            record: Some(record("replayable", 100)),
        };
        let permanent = DeadRecord {
            delivery_id: "permanent".to_string(),
            queue: "input".to_string(),
            dept: "worker".to_string(),
            source: None,
            observed_at_ms: 10,
            not_before_ms: 100,
            dead_at_ms: 121,
            attempts: 1,
            redrive_count: 0,
            replayable: true,
            permanent: true,
            error_excerpt: Some("permanent".to_string()),
            record: Some(record("permanent", 100)),
        };
        let missing_record = DeadRecord {
            delivery_id: "missing-record".to_string(),
            queue: "input".to_string(),
            dept: "worker".to_string(),
            source: None,
            observed_at_ms: 10,
            not_before_ms: 100,
            dead_at_ms: 122,
            attempts: 1,
            redrive_count: 0,
            replayable: true,
            permanent: false,
            error_excerpt: Some("missing payload".to_string()),
            record: None,
        };
        {
            let db = Database::create(&path).unwrap();
            let write = db.begin_write().unwrap();
            {
                write.open_table(DELIVERY_BY_ID).unwrap();
                write.open_table(READY_BY_DEPT_DUE).unwrap();
                write.open_table(LEASED_BY_DEPT_UNTIL).unwrap();
                let mut old_dead_index = write.open_table(DEAD_BY_DEPT_DUE).unwrap();
                old_dead_index
                    .insert(make_index_key("worker", 1, "stale-old-shape").as_str(), &())
                    .unwrap();
                let mut dead = write.open_table(DEAD_BY_ID).unwrap();
                for record in [&replayable, &permanent, &missing_record] {
                    dead.insert(
                        record.delivery_id.as_str(),
                        serde_json::to_vec(record).unwrap().as_slice(),
                    )
                    .unwrap();
                }
                dead.insert("undecodable", b"{not-json".as_slice()).unwrap();
                let mut meta = write.open_table(META).unwrap();
                meta.insert("schema_version", "3").unwrap();
            }
            write.commit().unwrap();
        }

        let store = DeliveryStore::open(&path).unwrap();

        assert_eq!(store.dead_due_index_len().unwrap(), 1);
        let redriven = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::ZERO,
                },
                120,
                10,
            )
            .unwrap();
        assert_eq!(redriven.redriven.len(), 1);
        assert_eq!(redriven.redriven[0].delivery_id, "replayable");
        assert_eq!(store.dead_due_index_len().unwrap(), 0);
        assert!(store.get_dead("permanent").unwrap().unwrap().permanent);
        assert!(store
            .get_dead("missing-record")
            .unwrap()
            .unwrap()
            .record
            .is_none());
        assert!(store.get_dead("undecodable").unwrap().is_none());
    }

    #[test]
    fn schema_v5_open_rebuilds_terminal_dead_tables_for_compaction() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("delivery.redb");
        let terminal = DeadRecord {
            delivery_id: "terminal".to_string(),
            queue: "input".to_string(),
            dept: "worker".to_string(),
            source: None,
            observed_at_ms: 10,
            not_before_ms: 100,
            dead_at_ms: 100,
            attempts: 1,
            redrive_count: 0,
            replayable: false,
            permanent: true,
            error_excerpt: Some("final".to_string()),
            record: None,
        };
        {
            let db = Database::create(&path).unwrap();
            let write = db.begin_write().unwrap();
            {
                write.open_table(DELIVERY_BY_ID).unwrap();
                write.open_table(READY_BY_DEPT_DUE).unwrap();
                write.open_table(LEASED_BY_DEPT_UNTIL).unwrap();
                write.open_table(DEAD_BY_DEPT_DUE).unwrap();
                let mut dead = write.open_table(DEAD_BY_ID).unwrap();
                dead.insert(
                    terminal.delivery_id.as_str(),
                    serde_json::to_vec(&terminal).unwrap().as_slice(),
                )
                .unwrap();
                let mut meta = write.open_table(META).unwrap();
                meta.insert("schema_version", "5").unwrap();
            }
            write.commit().unwrap();
        }

        let store = DeliveryStore::open(&path).unwrap();

        assert_eq!(store.terminal_dead_index_len().unwrap(), 1);
        let mut fresh = record(
            "fresh",
            terminal
                .dead_at_ms
                .saturating_add(TERMINAL_DEAD_RETENTION_MS)
                .saturating_add(1),
        );
        fresh.observed_at_ms = fresh.not_before_ms;
        store.enqueue(&fresh).unwrap();
        assert!(store.get_dead("terminal").unwrap().is_none());
        assert_eq!(store.terminal_dead_index_len().unwrap(), 0);
        assert_eq!(store.terminal_suppression_slot_len().unwrap(), 1);
        assert!(store.terminal_suppresses("terminal").unwrap());
    }

    #[test]
    fn get_dead_skips_undecodable_rows() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        {
            let write = store.db.begin_write().unwrap();
            {
                let mut dead = write.open_table(DEAD_BY_ID).unwrap();
                dead.insert("bad", b"{not-json".as_slice()).unwrap();
            }
            write.commit().unwrap();
        }

        assert!(store.get_dead("bad").unwrap().is_none());
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

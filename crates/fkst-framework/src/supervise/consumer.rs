//! Per-department consumer task.

use super::delivery_router::{now_unix_millis, DeliveryRouter, DerivedDelivery, PublishEnvelope};
use super::delivery_store::{DeliveryStore, LockBusyDeferOutcome, RetryFailure, RetryOutcome};
use super::delivery_types::{
    DeadRecord, DeliveryRecord, RedrivePolicy, RetryPolicy, SourceKind, SourceRef,
};
use super::event_fanout::Fanout;
use super::failure_fact::{
    dead_record_payload, delivery_failure_fact, lock_busy_failure_fact, FAILURE_FACT_QUEUE,
};
use super::journal::{optional_path, SupervisorJournal};
use super::raised::{parse_authenticated_raised, parse_authenticated_raised_line};
use super::source_runner::parse_duration;
use super::spawner::{spawn_framework_with_stdout_observer, SpawnResult, StdoutLineObserver};
use crate::path_resolver::PackageRoots;
use crate::process_tree::ProcessGroupRegistry;
use fkst_common::config::{DepartmentDecl, RetryDecl};
use fkst_common::{Event, RuntimeKind};
use nix::errno::Errno;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

const DISPATCH_INTERVAL: Duration = Duration::from_secs(1);
const DISPATCH_BATCH: usize = 16;
const REDRIVE_COOLDOWN: Duration = Duration::from_secs(600);
const REDRIVE_MAX: u64 = 3;

#[allow(clippy::too_many_arguments)]
pub async fn spawn_consumer(
    name: String,
    decl: DepartmentDecl,
    project_root: PathBuf,
    roots: PackageRoots,
    framework_binary: PathBuf,
    fanout: Fanout,
    router: DeliveryRouter,
    store: Option<Arc<DeliveryStore>>,
    queue_capacity: usize,
    codex_permit_slots: usize,
    max_in_flight: usize,
    admission_burst: usize,
    lock_busy_default_backoff: RetryPolicy,
    process_groups: ProcessGroupRegistry,
    journal: SupervisorJournal,
) -> JoinHandle<()> {
    let reliable_queues: Vec<String> = decl
        .consumes
        .iter()
        .filter(|queue| !decl.ephemeral.iter().any(|ephemeral| ephemeral == *queue))
        .cloned()
        .collect();
    let ephemeral_queues: Vec<String> = decl
        .consumes
        .iter()
        .filter(|queue| decl.ephemeral.iter().any(|ephemeral| ephemeral == *queue))
        .cloned()
        .collect();

    let mut receivers: Vec<mpsc::Receiver<Event>> = Vec::new();
    for q in &ephemeral_queues {
        receivers.push(fanout.subscribe(q, queue_capacity).await);
    }

    tokio::spawn(async move {
        let stall_window =
            parse_duration(&decl.stall_window).expect("validation already accepted stall_window");
        let layout = crate::runtime_context::layout_from_host_root(&project_root)
            .expect("runtime layout should be valid");
        let framework_child_log_dir = layout
            .runtime_dir(RuntimeKind::Logs)
            .join("framework-child");
        let retry_policy = decl
            .retry
            .as_ref()
            .map(policy_from_decl)
            .transpose()
            .expect("validation already accepted retry");
        let lock_busy_backoff = retry_policy.clone().unwrap_or(lock_busy_default_backoff);

        let (ephemeral_tx, mut ephemeral_rx) = mpsc::channel::<Event>(queue_capacity);
        let mut ephemeral_open = !receivers.is_empty();
        for mut rx in receivers {
            let tx = ephemeral_tx.clone();
            let dept_name = name.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if tx.send(ev).await.is_err() {
                        warn!(dept = %dept_name, "consumer wake inbox closed");
                        return;
                    }
                }
                warn!(dept = %dept_name, "queue receiver disconnected");
            });
        }
        drop(ephemeral_tx);
        let mut reliable_wake_rx = if reliable_queues.is_empty() {
            None
        } else {
            let (tx, rx) = mpsc::channel::<()>(queue_capacity);
            router.register_reliable_wake(&name, tx);
            Some(rx)
        };
        let (complete_tx, mut complete_rx) = mpsc::channel::<CompletedDelivery>(queue_capacity);
        let mut running: BTreeMap<String, RunningDelivery> = BTreeMap::new();

        info!(
            dept = %name,
            reliable_queues = ?reliable_queues,
            ephemeral_queues = ?ephemeral_queues,
            "consumer started"
        );

        let mut tick = tokio::time::interval(DISPATCH_INTERVAL);
        tick.tick().await;
        loop {
            tokio::select! {
                maybe_ev = ephemeral_rx.recv(), if ephemeral_open => {
                    let Some(ev) = maybe_ev else {
                        match on_ephemeral_disconnect(reliable_queues.is_empty(), &mut ephemeral_open) {
                            ShouldExit::Yes => {
                                warn!(dept = %name, "consumer ephemeral inbox disconnected");
                                return;
                            }
                            ShouldExit::No => {
                                warn!(dept = %name, "consumer ephemeral inbox disconnected; disabling ephemeral arm");
                                continue;
                            }
                        }
                    };
                    if ephemeral_queues.iter().any(|queue| queue == &ev.queue) {
                        spawn_ephemeral(
                            &name,
                            &decl,
                            &project_root,
                            &roots,
                            &framework_binary,
                            &router,
                            &framework_child_log_dir,
                            ev,
                            stall_window,
                            codex_permit_slots,
                            process_groups.clone(),
                            journal.clone(),
                        );
                    }
                }
                maybe_wake = recv_reliable_wake(&mut reliable_wake_rx), if reliable_wake_rx.is_some() => {
                    if maybe_wake.is_none() {
                        warn!(dept = %name, "consumer reliable wake inbox disconnected");
                        on_wake_disconnect(&mut reliable_wake_rx);
                    }
                    dispatch_due(
                        &name,
                        &decl,
                        &project_root,
                        &roots,
                        &framework_binary,
                        &router,
                        store.clone(),
                        retry_policy.as_ref(),
                        &framework_child_log_dir,
                        stall_window,
                        codex_permit_slots,
                        max_in_flight,
                        admission_burst,
                        process_groups.clone(),
                        journal.clone(),
                        &complete_tx,
                        &mut running,
                    );
                }
                _ = tick.tick(), if !reliable_queues.is_empty() => {
                    renew_running(&name, store.as_deref(), stall_window, &mut running);
                    maintain_dead_letters(&name, store.as_deref(), &router);
                    dispatch_due(
                        &name,
                        &decl,
                        &project_root,
                        &roots,
                        &framework_binary,
                        &router,
                        store.clone(),
                        retry_policy.as_ref(),
                        &framework_child_log_dir,
                        stall_window,
                        codex_permit_slots,
                        max_in_flight,
                        admission_burst,
                        process_groups.clone(),
                        journal.clone(),
                        &complete_tx,
                        &mut running,
                    );
                }
                maybe_done = complete_rx.recv(), if !running.is_empty() => {
                    if let Some(done) = maybe_done {
                        running.remove(&done.record.delivery_id);
                        finish_durable_record(
                            &name,
                            store.as_deref(),
                            &router,
                            retry_policy.as_ref(),
                            &lock_busy_backoff,
                            done,
                            &journal,
                        );
                    }
                }
            }
        }
    })
}

async fn recv_reliable_wake(rx: &mut Option<mpsc::Receiver<()>>) -> Option<()> {
    match rx {
        Some(rx) => rx.recv().await,
        None => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShouldExit {
    Yes,
    No,
}

fn on_ephemeral_disconnect(reliable_queues_empty: bool, ephemeral_open: &mut bool) -> ShouldExit {
    if reliable_queues_empty {
        ShouldExit::Yes
    } else {
        *ephemeral_open = false;
        ShouldExit::No
    }
}

fn on_wake_disconnect(reliable_wake_rx: &mut Option<mpsc::Receiver<()>>) {
    *reliable_wake_rx = None;
}

fn durable_dispatch_capacity(
    running_len: usize,
    max_in_flight: usize,
    admission_burst: usize,
) -> usize {
    max_in_flight
        .saturating_sub(running_len)
        .min(admission_burst)
        .min(DISPATCH_BATCH)
}

#[allow(clippy::too_many_arguments)]
fn spawn_ephemeral(
    name: &str,
    decl: &DepartmentDecl,
    project_root: &std::path::Path,
    roots: &PackageRoots,
    framework_binary: &std::path::Path,
    router: &DeliveryRouter,
    log_dir: &std::path::Path,
    event: Event,
    stall_window: Duration,
    codex_permit_slots: usize,
    process_groups: ProcessGroupRegistry,
    journal: SupervisorJournal,
) {
    let origin_queue = event.queue.clone();
    let args = match spawn_args(
        decl,
        project_root,
        roots,
        framework_binary,
        log_dir,
        event,
        stall_window,
        codex_permit_slots,
        process_groups,
    ) {
        Ok(args) => args,
        Err(err) => {
            error!(dept = %name, error = %err, "build spawn args failed");
            journal.event(
                "dept_child_exit",
                [
                    ("dept", name.to_string()),
                    ("pid", "none".to_string()),
                    ("exit_code", "none".to_string()),
                    ("elapsed_ms", "0".to_string()),
                    ("log_path", "none".to_string()),
                    ("reason", format!("build_spawn_args:{err}")),
                ],
            );
            return;
        }
    };
    let dept_name = name.to_string();
    let router = router.clone();
    tokio::spawn(async move {
        let raised_observed = Arc::new(AtomicBool::new(false));
        let publish_error = Arc::new(std::sync::Mutex::new(None::<String>));
        let stdout_observer = args.raised_auth_token.clone().map(|token| {
            let router = router.clone();
            let raised_observed = Arc::clone(&raised_observed);
            let publish_error = Arc::clone(&publish_error);
            Arc::new(move |line: &str| -> anyhow::Result<()> {
                let raised_events = parse_authenticated_raised_line(line, &token);
                if !raised_events.is_empty() {
                    raised_observed.store(true, Ordering::Relaxed);
                }
                if let Err(err) = publish_ephemeral_raised_events(&router, raised_events) {
                    if let Ok(mut publish_error) = publish_error.lock() {
                        *publish_error = Some(err.to_string());
                    }
                }
                Ok(())
            }) as StdoutLineObserver
        });
        match spawn_and_report_with_stdout_observer(&dept_name, &args, stdout_observer, &journal)
            .await
        {
            Ok(result) => {
                if let Some(lock_busy) = lock_busy_from_spawn_result(&result) {
                    record_lock_busy_fact(
                        &router,
                        &journal,
                        &dept_name,
                        &origin_queue,
                        None,
                        lock_busy.lock.as_deref(),
                        "ephemeral_dropped",
                    );
                    return;
                }
                if result.exit_code != 0 {
                    return;
                }
                if let Ok(mut publish_error) = publish_error.lock() {
                    if let Some(err) = publish_error.take() {
                        error!(dept = %dept_name, error = %err, "publish raised failed");
                        return;
                    }
                }
                if !raised_observed.load(Ordering::Relaxed) {
                    if let Err(err) = publish_ephemeral_raised(
                        &router,
                        &result.stdout,
                        args.raised_auth_token.as_deref(),
                    ) {
                        error!(dept = %dept_name, error = %err, "publish raised failed");
                    }
                }
            }
            Err(err) => {
                error!(
                    dept = %dept_name,
                    log_dir = %args.log_dir.display(),
                    error = %err,
                    "framework spawn error"
                );
                journal.event(
                    "dept_child_exit",
                    [
                        ("dept", dept_name),
                        ("pid", "none".to_string()),
                        ("exit_code", "none".to_string()),
                        ("elapsed_ms", "0".to_string()),
                        ("log_path", optional_path(None)),
                        ("reason", format!("spawn_error:{err}")),
                    ],
                );
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn dispatch_due(
    name: &str,
    decl: &DepartmentDecl,
    project_root: &std::path::Path,
    roots: &PackageRoots,
    framework_binary: &std::path::Path,
    router: &DeliveryRouter,
    store: Option<Arc<DeliveryStore>>,
    retry_policy: Option<&RetryPolicy>,
    log_dir: &std::path::Path,
    stall_window: Duration,
    codex_permit_slots: usize,
    max_in_flight: usize,
    admission_burst: usize,
    process_groups: ProcessGroupRegistry,
    journal: SupervisorJournal,
    complete_tx: &mpsc::Sender<CompletedDelivery>,
    running: &mut BTreeMap<String, RunningDelivery>,
) {
    let Some(store) = store else {
        error!(dept = %name, "reliable consumer missing delivery store");
        return;
    };
    let capacity = durable_dispatch_capacity(running.len(), max_in_flight, admission_burst);
    if capacity == 0 {
        return;
    }
    let lease = retry_lease(stall_window);
    let excluded = running.keys().cloned().collect();
    let leased =
        match store.lease_for_dept_excluding(name, now_unix_millis(), capacity, lease, &excluded) {
            Ok(records) => records,
            Err(err) => {
                error!(dept = %name, error = %err, "delivery lease failed");
                return;
            }
        };
    for record in leased {
        if running.contains_key(&record.delivery_id) {
            continue;
        }
        let mut args = match spawn_args(
            decl,
            project_root,
            roots,
            framework_binary,
            log_dir,
            event_from_record(&record),
            stall_window,
            codex_permit_slots,
            process_groups.clone(),
        ) {
            Ok(args) => args,
            Err(err) => {
                retry_record(
                    &store,
                    router,
                    retry_policy,
                    &record,
                    DeliveryFailure::permanent(format!("build spawn args: {err}")),
                    &journal,
                );
                continue;
            }
        };
        args.replay_scratch_bypass = requires_replay_scratch_bypass(&record);
        let dept_name = name.to_string();
        let router = router.clone();
        let complete_tx = complete_tx.clone();
        let running_record = record.clone();
        let delivery_id = record.delivery_id.clone();
        let journal = journal.clone();
        let handle = tokio::spawn(async move {
            let result = run_durable_record(&dept_name, &router, record, args, &journal).await;
            if complete_tx.send(result).await.is_err() {
                warn!(dept = %dept_name, delivery_id = %delivery_id, "delivery completion receiver closed");
            }
        });
        running.insert(
            running_record.delivery_id.clone(),
            RunningDelivery {
                next_renew_at_ms: next_renew_at_ms(now_unix_millis(), stall_window),
                record: running_record,
                handle,
            },
        );
    }
}

async fn run_durable_record(
    dept_name: &str,
    router: &DeliveryRouter,
    record: DeliveryRecord,
    args: SpawnArgs,
    journal: &SupervisorJournal,
) -> CompletedDelivery {
    let next_ordinal = Arc::new(std::sync::Mutex::new(0_usize));
    let publish_error = Arc::new(std::sync::Mutex::new(None::<String>));
    let stdout_observer = args.raised_auth_token.clone().map(|token| {
        let router = router.clone();
        let record = record.clone();
        let next_ordinal = Arc::clone(&next_ordinal);
        let publish_error = Arc::clone(&publish_error);
        Arc::new(move |line: &str| -> anyhow::Result<()> {
            let raised_events = parse_authenticated_raised_line(line, &token);
            match next_ordinal.lock() {
                Ok(mut next_ordinal) => {
                    if let Err(err) =
                        publish_raised_events(&router, raised_events, &record, &mut next_ordinal)
                    {
                        if let Ok(mut publish_error) = publish_error.lock() {
                            *publish_error = Some(err.to_string());
                        }
                    }
                }
                Err(_) => {
                    if let Ok(mut publish_error) = publish_error.lock() {
                        *publish_error = Some("raised ordinal lock poisoned".to_string());
                    }
                }
            }
            Ok(())
        }) as StdoutLineObserver
    });
    let result =
        spawn_and_report_with_stdout_observer(dept_name, &args, stdout_observer, journal).await;
    if let Ok(result) = &result {
        if let Some(lock_busy) = lock_busy_from_spawn_result(result) {
            return CompletedDelivery {
                record,
                failure: None,
                lock_busy: Some(lock_busy),
            };
        }
    }
    let failure = match result {
        Ok(result) if result.exit_code == 0 => {
            let publish_error_message = match publish_error.lock() {
                Ok(mut publish_error) => publish_error.take(),
                Err(_) => Some("raised publish error lock poisoned".to_string()),
            };
            if let Some(err) = publish_error_message {
                return CompletedDelivery {
                    record,
                    failure: Some(DeliveryFailure::permanent(format!(
                        "raised publish error: {err}"
                    ))),
                    lock_busy: None,
                };
            }
            let next_ordinal = match next_ordinal.lock() {
                Ok(next_ordinal) => next_ordinal,
                Err(_) => {
                    return CompletedDelivery {
                        record,
                        failure: Some(DeliveryFailure::permanent("raised ordinal lock poisoned")),
                        lock_busy: None,
                    }
                }
            };
            if *next_ordinal == 0 {
                publish_raised(
                    &router,
                    &result.stdout,
                    args.raised_auth_token.as_deref(),
                    &record,
                )
            } else {
                Ok(())
            }
            .err()
            .map(|err| DeliveryFailure::permanent(format!("raised publish error: {err}")))
        }
        Ok(result) => Some(DeliveryFailure::from_spawn_result(&result)),
        Err(err) => {
            error!(
                dept = %dept_name,
                log_dir = %args.log_dir.display(),
                error = %err,
                "framework spawn error"
            );
            journal.event(
                "dept_child_exit",
                [
                    ("dept", dept_name.to_string()),
                    ("pid", "none".to_string()),
                    ("exit_code", "none".to_string()),
                    ("elapsed_ms", "0".to_string()),
                    ("log_path", optional_path(None)),
                    ("reason", format!("spawn_error:{err}")),
                ],
            );
            Some(DeliveryFailure::from_spawn_error(&err))
        }
    };

    CompletedDelivery {
        record,
        failure,
        lock_busy: None,
    }
}

fn finish_durable_record(
    dept_name: &str,
    store: Option<&DeliveryStore>,
    router: &DeliveryRouter,
    retry_policy: Option<&RetryPolicy>,
    lock_busy_backoff: &RetryPolicy,
    done: CompletedDelivery,
    journal: &SupervisorJournal,
) {
    let Some(store) = store else {
        error!(dept = %dept_name, "reliable consumer missing delivery store");
        return;
    };
    if let Some(lock_busy) = done.lock_busy {
        defer_lock_busy_record(
            store,
            router,
            &done.record,
            lock_busy.lock.as_deref(),
            lock_busy_backoff,
            journal,
        );
        return;
    }
    if let Some(error) = done.failure {
        retry_record(store, router, retry_policy, &done.record, error, journal);
        return;
    }

    match store.ack(&done.record.delivery_id, done.record.lease_generation) {
        Ok(true) => {
            journal.event(
                "acked",
                [
                    ("dept", dept_name.to_string()),
                    ("delivery_id", done.record.delivery_id.clone()),
                    ("reason", "completed".to_string()),
                ],
            );
            info!(
                dept = %dept_name,
                delivery_id = %done.record.delivery_id,
                generation = done.record.lease_generation,
                "delivery acked"
            );
        }
        Ok(false) => {
            journal.event(
                "acked",
                [
                    ("dept", dept_name.to_string()),
                    ("delivery_id", done.record.delivery_id.clone()),
                    ("reason", "stale_or_missing".to_string()),
                ],
            );
            warn!(
                dept = %dept_name,
                delivery_id = %done.record.delivery_id,
                generation = done.record.lease_generation,
                "delivery ack stale or missing"
            );
        }
        Err(err) => {
            journal.event(
                "acked",
                [
                    ("dept", dept_name.to_string()),
                    ("delivery_id", done.record.delivery_id.clone()),
                    ("reason", format!("failed:{err}")),
                ],
            );
            error!(
                dept = %dept_name,
                delivery_id = %done.record.delivery_id,
                generation = done.record.lease_generation,
                error = %err,
                "delivery ack failed"
            );
        }
    }
}

fn defer_lock_busy_record(
    store: &DeliveryStore,
    router: &DeliveryRouter,
    record: &DeliveryRecord,
    lock: Option<&str>,
    policy: &RetryPolicy,
    journal: &SupervisorJournal,
) {
    let lock_label = lock.unwrap_or("unknown");
    let (outcome, error_message) = match store.defer_lock_busy(
        &record.delivery_id,
        record.lease_generation,
        lock_label,
        policy,
        now_unix_millis(),
    ) {
        Ok(LockBusyDeferOutcome::Scheduled) => ("deferred", None),
        Ok(LockBusyDeferOutcome::Stale | LockBusyDeferOutcome::Missing) => {
            ("stale_or_missing", None)
        }
        Err(err) => ("defer_failed", Some(err.to_string())),
    };
    record_lock_busy_fact(
        router,
        journal,
        &record.dept,
        &record.queue,
        Some(record),
        lock,
        outcome,
    );
    if let Some(error_message) = error_message {
        error!(
            dept = %record.dept,
            delivery_id = %record.delivery_id,
            lock = %lock_label,
            error = %error_message,
            "lock busy delivery defer failed"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn record_lock_busy_fact(
    router: &DeliveryRouter,
    journal: &SupervisorJournal,
    dept: &str,
    queue: &str,
    record: Option<&DeliveryRecord>,
    lock: Option<&str>,
    outcome: &str,
) {
    journal.event(
        "lock_busy",
        [
            ("dept", dept.to_string()),
            (
                "delivery_id",
                record.map_or_else(
                    || "ephemeral".to_string(),
                    |record| record.delivery_id.clone(),
                ),
            ),
            ("origin_queue", queue.to_string()),
            ("error_class", "lock_busy".to_string()),
            ("lock", lock.unwrap_or("unknown").to_string()),
            ("outcome", outcome.to_string()),
        ],
    );
    publish_failure_fact(router, lock_busy_failure_fact(dept, queue, record, lock));
}

fn lock_busy_from_spawn_result(result: &SpawnResult) -> Option<LockBusy> {
    (result.exit_code == crate::sdk_git::LOCK_BUSY_EXIT_CODE).then(|| LockBusy {
        lock: crate::sdk_git::lock_busy_key_from_stderr(&result.stderr),
    })
}

fn renew_running(
    dept_name: &str,
    store: Option<&DeliveryStore>,
    stall_window: Duration,
    running: &mut BTreeMap<String, RunningDelivery>,
) {
    let Some(store) = store else {
        error!(dept = %dept_name, "reliable consumer missing delivery store");
        return;
    };
    let now = now_unix_millis();
    let lease_until = now.saturating_add(duration_millis(retry_lease(stall_window)));
    let renewals = running
        .values()
        .filter(|running_delivery| {
            !running_delivery.handle.is_finished() && running_delivery.next_renew_at_ms <= now
        })
        .map(|running_delivery| {
            (
                running_delivery.record.delivery_id.clone(),
                running_delivery.record.lease_generation,
            )
        })
        .collect::<Vec<_>>();
    if renewals.is_empty() {
        return;
    }
    let renewed = match store.renew_leases(&renewals, lease_until) {
        Ok(renewed) => renewed,
        Err(err) => {
            error!(
                dept = %dept_name,
                error = %err,
                "delivery lease renewal failed"
            );
            return;
        }
    };
    let next_renew = next_renew_at_ms(now, stall_window);
    for (delivery_id, lease_generation) in renewals {
        let Some(running_delivery) = running.get_mut(&delivery_id) else {
            continue;
        };
        if running_delivery.handle.is_finished() {
            continue;
        }
        if let Some(record) = renewed.get(&delivery_id) {
            running_delivery.record.lease_until_ms = record.lease_until_ms;
            running_delivery.record.lease_generation = record.lease_generation;
            running_delivery.next_renew_at_ms = next_renew;
        } else {
            warn!(
                dept = %dept_name,
                delivery_id = %delivery_id,
                generation = lease_generation,
                "delivery lease renewal stale or missing"
            );
        }
    }
}

fn next_renew_at_ms(now_ms: u64, stall_window: Duration) -> u64 {
    now_ms.saturating_add(duration_millis(renew_interval(stall_window)))
}

fn renew_interval(stall_window: Duration) -> Duration {
    let half_lease = retry_lease(stall_window) / 2;
    half_lease.max(Duration::from_secs(1))
}

fn retry_record(
    store: &DeliveryStore,
    router: &DeliveryRouter,
    retry_policy: Option<&RetryPolicy>,
    record: &DeliveryRecord,
    failure: DeliveryFailure,
    journal: &SupervisorJournal,
) {
    let Some(policy) = retry_policy else {
        if let Err(err) = store.ack(&record.delivery_id, record.lease_generation) {
            error!(
                delivery_id = %record.delivery_id,
                generation = record.lease_generation,
                error = %err,
                "delivery drop ack failed"
            );
        }
        journal.event(
            "acked",
            [
                ("dept", record.dept.clone()),
                ("delivery_id", record.delivery_id.clone()),
                ("reason", "dropped_no_retry_policy".to_string()),
            ],
        );
        return;
    };
    let failure_message = failure.message.clone();
    match store.retry(
        &record.delivery_id,
        record.lease_generation,
        &RetryFailure {
            message: failure.message,
            replayable: failure.replayable,
        },
        policy,
        now_unix_millis(),
    ) {
        Ok(RetryOutcome::DeadPendingRedrive) => {
            journal.event(
                "dead_lettered",
                [
                    ("dept", record.dept.clone()),
                    ("delivery_id", record.delivery_id.clone()),
                    ("reason", "dead_pending_redrive".to_string()),
                ],
            );
        }
        Ok(RetryOutcome::PermanentDead) => match store.get_dead(&record.delivery_id) {
            Ok(Some(dead)) => {
                journal.event(
                    "dead_lettered",
                    [
                        ("dept", record.dept.clone()),
                        ("delivery_id", record.delivery_id.clone()),
                        ("reason", "permanent_dead".to_string()),
                    ],
                );
                publish_failure_fact(router, delivery_failure_fact(record, &failure_message));
                publish_permanent_dead_letter(router, &dead);
            }
            Ok(None) => warn!(
                delivery_id = %record.delivery_id,
                "permanent dead delivery missing tombstone"
            ),
            Err(err) => error!(
                delivery_id = %record.delivery_id,
                error = %err,
                "permanent dead delivery lookup failed"
            ),
        },
        Ok(RetryOutcome::Scheduled) => {
            journal.event(
                "retried",
                [
                    ("dept", record.dept.clone()),
                    ("delivery_id", record.delivery_id.clone()),
                    ("reason", failure_message),
                ],
            );
        }
        Ok(RetryOutcome::Stale | RetryOutcome::Missing) => {
            journal.event(
                "retried",
                [
                    ("dept", record.dept.clone()),
                    ("delivery_id", record.delivery_id.clone()),
                    ("reason", "stale_or_missing".to_string()),
                ],
            );
        }
        Err(err) => {
            journal.event(
                "retried",
                [
                    ("dept", record.dept.clone()),
                    ("delivery_id", record.delivery_id.clone()),
                    ("reason", format!("failed:{err}")),
                ],
            );
            error!(
                delivery_id = %record.delivery_id,
                generation = record.lease_generation,
                error = %err,
                "delivery retry failed"
            );
        }
    }
}

fn maintain_dead_letters(dept_name: &str, store: Option<&DeliveryStore>, router: &DeliveryRouter) {
    let Some(store) = store else {
        error!(dept = %dept_name, "reliable consumer missing delivery store");
        return;
    };
    let policy = RedrivePolicy {
        max_redrives: REDRIVE_MAX,
        cooldown: REDRIVE_COOLDOWN,
    };
    match store.redrive_due_with_current_subscribers(
        &policy,
        &router.single_reliable_subscribers_by_queue(),
        now_unix_millis(),
        DISPATCH_BATCH,
    ) {
        Ok(result) => {
            for record in result.redriven {
                info!(
                    dept = %record.dept,
                    queue = %record.queue,
                    delivery_id = %record.delivery_id,
                    redrive_count = record.redrive_count,
                    "delivery redriven"
                );
                router.notify_reliable_public(&record.dept);
            }
            for dead in result.permanent {
                publish_permanent_dead_letter(router, &dead);
            }
        }
        Err(err) => {
            error!(
                dept = %dept_name,
                error = %err,
                "dead delivery redrive failed"
            );
        }
    }
}

fn publish_permanent_dead_letter(router: &DeliveryRouter, dead: &DeadRecord) {
    let Some((namespace, _)) = dead.dept.split_once('.') else {
        if dead.queue == "dead_letter" {
            return;
        }
        publish_dead_letter_to(router, dead, "dead_letter");
        return;
    };
    let dead_letter = format!("{namespace}.dead_letter");
    if dead.queue == dead_letter {
        return;
    }
    publish_dead_letter_to(router, dead, &dead_letter);
}

fn publish_dead_letter_to(router: &DeliveryRouter, dead: &DeadRecord, queue: &str) {
    let event = Event::new(queue, dead_record_payload(dead));
    if let Err(err) = router.publish(PublishEnvelope {
        event,
        source: Some(SourceRef {
            kind: SourceKind::External,
            reference: format!("dead/{}", dead.delivery_id),
        }),
        cron_payload: None,
        derived: None,
    }) {
        warn!(
            delivery_id = %dead.delivery_id,
            error = %err,
            "dead_letter publish failed"
        );
    }
}

fn publish_failure_fact(router: &DeliveryRouter, event: Event) {
    if let Err(err) = router.publish_failure_fact(event) {
        warn!(error = %err, "failure fact publish failed");
    }
}

fn publish_raised(
    router: &DeliveryRouter,
    stdout: &str,
    raised_auth_token: Option<&str>,
    parent: &DeliveryRecord,
) -> anyhow::Result<()> {
    let Some(raised_auth_token) = raised_auth_token else {
        return Ok(());
    };
    let mut next_ordinal = 0;
    publish_raised_events(
        router,
        parse_authenticated_raised(stdout, raised_auth_token),
        parent,
        &mut next_ordinal,
    )
}

fn publish_raised_events(
    router: &DeliveryRouter,
    raised_events: Vec<Event>,
    parent: &DeliveryRecord,
    next_ordinal: &mut usize,
) -> anyhow::Result<()> {
    for mut raised_ev in raised_events {
        reject_reserved_raised_queue(&raised_ev)?;
        raised_ev.ts = parent.observed_at_ms;
        let ordinal = *next_ordinal;
        *next_ordinal = next_ordinal.saturating_add(1);
        router.publish(PublishEnvelope {
            event: raised_ev,
            source: parent.source.clone(),
            cron_payload: None,
            derived: Some(DerivedDelivery {
                parent_delivery_id: parent.delivery_id.clone(),
                ordinal,
            }),
        })?;
    }
    Ok(())
}

fn publish_ephemeral_raised(
    router: &DeliveryRouter,
    stdout: &str,
    raised_auth_token: Option<&str>,
) -> anyhow::Result<()> {
    let Some(raised_auth_token) = raised_auth_token else {
        return Ok(());
    };
    publish_ephemeral_raised_events(
        router,
        parse_authenticated_raised(stdout, raised_auth_token),
    )
}

fn publish_ephemeral_raised_events(
    router: &DeliveryRouter,
    raised_events: Vec<Event>,
) -> anyhow::Result<()> {
    for raised_ev in raised_events {
        reject_reserved_raised_queue(&raised_ev)?;
        router.publish(PublishEnvelope {
            event: raised_ev,
            source: None,
            cron_payload: None,
            derived: None,
        })?;
    }
    Ok(())
}

fn reject_reserved_raised_queue(event: &Event) -> anyhow::Result<()> {
    if event.queue == FAILURE_FACT_QUEUE {
        anyhow::bail!(
            "reserved engine queue `{}` cannot be raised by department output",
            event.queue
        );
    }
    Ok(())
}

fn event_from_record(record: &DeliveryRecord) -> Event {
    Event {
        queue: record.queue.clone(),
        payload: record.payload.clone(),
        ts: record.observed_at_ms,
    }
}

fn requires_replay_scratch_bypass(record: &DeliveryRecord) -> bool {
    record.lease_generation > 1
}

#[allow(clippy::too_many_arguments)]
fn spawn_args(
    decl: &DepartmentDecl,
    project_root: &std::path::Path,
    roots: &PackageRoots,
    framework_binary: &std::path::Path,
    log_dir: &std::path::Path,
    event: Event,
    stall_window: Duration,
    codex_permit_slots: usize,
    process_groups: ProcessGroupRegistry,
) -> anyhow::Result<SpawnArgs> {
    Ok(SpawnArgs {
        framework_bin: framework_binary.to_path_buf(),
        lua_full: if decl.lua.is_absolute() {
            decl.lua.clone()
        } else {
            project_root.join(&decl.lua)
        },
        project_root: project_root.to_path_buf(),
        graph_package_roots: roots.package_roots().to_vec(),
        event_json: serde_json::to_string(&event)?,
        stall_window,
        codex_permit_slots,
        log_dir: log_dir.to_path_buf(),
        raised_auth_token: Some(ulid::Ulid::new().to_string()),
        replay_scratch_bypass: false,
        owner_namespace: decl.owner_namespace.clone(),
        process_groups,
    })
}

struct SpawnArgs {
    framework_bin: PathBuf,
    lua_full: PathBuf,
    project_root: PathBuf,
    graph_package_roots: Vec<PathBuf>,
    event_json: String,
    stall_window: Duration,
    codex_permit_slots: usize,
    log_dir: PathBuf,
    raised_auth_token: Option<String>,
    replay_scratch_bypass: bool,
    owner_namespace: String,
    process_groups: ProcessGroupRegistry,
}

async fn spawn_and_report(
    dept_name: &str,
    args: &SpawnArgs,
    journal: &SupervisorJournal,
) -> anyhow::Result<SpawnResult> {
    spawn_and_report_with_stdout_observer(dept_name, args, None, journal).await
}

async fn spawn_and_report_with_stdout_observer(
    dept_name: &str,
    args: &SpawnArgs,
    stdout_observer: Option<StdoutLineObserver>,
    journal: &SupervisorJournal,
) -> anyhow::Result<SpawnResult> {
    let result = spawn_framework_with_stdout_observer(
        &args.framework_bin,
        &args.lua_full,
        &args.project_root,
        &args.graph_package_roots,
        &args.owner_namespace,
        &args.event_json,
        args.codex_permit_slots,
        dept_name,
        &args.log_dir,
        args.process_groups.clone(),
        args.raised_auth_token.as_deref(),
        args.replay_scratch_bypass,
        stdout_observer,
    )
    .await?;

    journal.event(
        "dept_child_spawn",
        [
            ("dept", dept_name.to_string()),
            ("pid", result.pid.to_string()),
            ("exit_code", "pending".to_string()),
            ("elapsed_ms", "0".to_string()),
            ("log_path", optional_path(result.log_path.as_deref())),
            ("reason", "spawn_framework_run".to_string()),
        ],
    );
    journal.event(
        "dept_child_exit",
        [
            ("dept", dept_name.to_string()),
            ("pid", result.pid.to_string()),
            ("exit_code", result.exit_code.to_string()),
            ("elapsed_ms", result.elapsed_ms.to_string()),
            ("log_path", optional_path(result.log_path.as_deref())),
            ("reason", "natural_exit".to_string()),
        ],
    );
    if result.exit_code != 0 {
        warn!(dept = %dept_name, exit = result.exit_code,
              stall_window_ms = args.stall_window.as_millis(),
              elapsed_ms = result.elapsed_ms,
              log_path = ?result.log_path,
              stderr = %result.stderr,
              "framework failed");
    } else {
        info!(dept = %dept_name,
              stall_window_ms = args.stall_window.as_millis(),
              elapsed_ms = result.elapsed_ms,
              log_path = ?result.log_path,
              "framework ok");
    }
    Ok(result)
}

fn retry_lease(stall_window: Duration) -> Duration {
    stall_window
        .saturating_add(DISPATCH_INTERVAL)
        .saturating_add(Duration::from_secs(5))
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

struct RunningDelivery {
    next_renew_at_ms: u64,
    record: DeliveryRecord,
    handle: JoinHandle<()>,
}

struct LockBusy {
    lock: Option<String>,
}

struct CompletedDelivery {
    record: DeliveryRecord,
    failure: Option<DeliveryFailure>,
    lock_busy: Option<LockBusy>,
}

pub(crate) struct TestDurableCompletion {
    pub(crate) exit_code: i32,
    pub(crate) error: Option<String>,
    pub(crate) raises: Vec<Event>,
}

pub(crate) fn finish_test_durable_record(
    store: &DeliveryStore,
    router: &DeliveryRouter,
    retry_policy: Option<&RetryPolicy>,
    record: DeliveryRecord,
    completion: TestDurableCompletion,
    journal: &SupervisorJournal,
) -> anyhow::Result<()> {
    let done = if completion.exit_code == 0 {
        let failure = publish_test_raised_events(router, completion.raises, &record)
            .err()
            .map(|err| DeliveryFailure::permanent(format!("raised publish error: {err}")));
        CompletedDelivery {
            record,
            failure,
            lock_busy: None,
        }
    } else {
        CompletedDelivery {
            record,
            failure: Some(DeliveryFailure {
                message: completion
                    .error
                    .unwrap_or_else(|| format!("exit={}", completion.exit_code)),
                replayable: is_replayable_framework_exit_code(completion.exit_code),
            }),
            lock_busy: None,
        }
    };
    let dept = done.record.dept.clone();
    let default_lock_busy_backoff = RetryPolicy {
        max_attempts: 1,
        base: DISPATCH_INTERVAL,
        cap: DISPATCH_INTERVAL,
    };
    finish_durable_record(
        &dept,
        Some(store),
        router,
        retry_policy,
        retry_policy.unwrap_or(&default_lock_busy_backoff),
        done,
        journal,
    );
    Ok(())
}

fn publish_test_raised_events(
    router: &DeliveryRouter,
    raised_events: Vec<Event>,
    parent: &DeliveryRecord,
) -> anyhow::Result<()> {
    let mut next_ordinal = 0;
    publish_raised_events(router, raised_events, parent, &mut next_ordinal)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeliveryFailure {
    message: String,
    replayable: bool,
}

impl DeliveryFailure {
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            replayable: false,
        }
    }

    fn from_spawn_result(result: &SpawnResult) -> Self {
        Self {
            message: format!("exit={} stderr={}", result.exit_code, result.stderr),
            replayable: is_replayable_framework_exit_code(result.exit_code),
        }
    }

    fn from_spawn_error(error: &anyhow::Error) -> Self {
        Self {
            message: format!("spawn error: {error}"),
            replayable: is_replayable_spawn_error(error),
        }
    }
}

fn is_replayable_framework_exit_code(exit_code: i32) -> bool {
    exit_code == 124
}

fn is_replayable_spawn_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|source| source.downcast_ref::<std::io::Error>())
        .any(|source| {
            matches!(
                source.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::OutOfMemory
            ) || source.raw_os_error().is_some_and(|errno| {
                [
                    Errno::EMFILE as i32,
                    Errno::ENFILE as i32,
                    Errno::EAGAIN as i32,
                    Errno::EWOULDBLOCK as i32,
                    Errno::ENOMEM as i32,
                ]
                .contains(&errno)
            })
        })
}

fn policy_from_decl(decl: &RetryDecl) -> anyhow::Result<RetryPolicy> {
    Ok(RetryPolicy {
        max_attempts: decl.max_attempts,
        base: parse_duration(&decl.base)?,
        cap: parse_duration(&decl.cap)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::delivery_store::DeliveryStore;
    use base64::Engine;
    use fkst_common::config::{Config, LimitsDecl, QueueDecl};
    use nix::errno::Errno;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use tempfile::TempDir;
    use tokio::time::timeout;

    static RESOURCE_HEAVY_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct ResourceHeavyTestGuard {
        _process_lock: MutexGuard<'static, ()>,
        _file_lock: fs::File,
    }

    fn resource_heavy_test_lock() -> ResourceHeavyTestGuard {
        let process_lock = RESOURCE_HEAVY_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let path = std::env::temp_dir().join("fkst-framework-resource-heavy-tests.lock");
        let file_lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        nix::fcntl::flock(
            std::os::fd::AsRawFd::as_raw_fd(&file_lock),
            nix::fcntl::FlockArg::LockExclusive,
        )
        .unwrap();

        ResourceHeavyTestGuard {
            _process_lock: process_lock,
            _file_lock: file_lock,
        }
    }

    fn package_namespace(root: &Path) -> String {
        root.canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn record(id: &str) -> DeliveryRecord {
        DeliveryRecord {
            delivery_id: id.to_string(),
            queue: "jobs".to_string(),
            dept: "worker".to_string(),
            payload: serde_json::json!({"n": 1}),
            source: Some(SourceRef {
                kind: SourceKind::Cron,
                reference: "tick".to_string(),
            }),
            cron_payload: None,
            observed_at_ms: now_unix_millis(),
            attempt: 0,
            redrive_count: 0,
            collapse_by_dedup_id: false,
            pending_dirty: false,
            subscriber_absent_since_ms: None,
            lease_generation: 0,
            lease_until_ms: None,
            not_before_ms: 0,
            last_error_excerpt: None,
        }
    }

    fn policy(max_attempts: u64) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base: Duration::from_millis(1),
            cap: Duration::from_millis(1),
        }
    }

    #[test]
    fn replay_scratch_bypass_follows_engine_lease_generation() {
        let mut delivery = record("one");
        delivery.lease_generation = 1;
        assert!(!requires_replay_scratch_bypass(&delivery));

        delivery.lease_generation = 2;
        assert!(requires_replay_scratch_bypass(&delivery));
    }

    #[test]
    fn raised_publish_failure_schedules_replay_with_scratch_bypass() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("parent")).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 1, Duration::from_secs(30))
            .unwrap()
            .remove(0);
        assert_eq!(leased.lease_generation, 1);
        let router = router_with_dead_letter(store.clone());

        finish_test_durable_record(
            &store,
            &router,
            Some(&policy(3)),
            leased,
            TestDurableCompletion {
                exit_code: 0,
                error: None,
                raises: vec![Event::new("missing", serde_json::json!({"result": true}))],
            },
            &SupervisorJournal::disabled(),
        )
        .unwrap();

        let replay = store
            .lease_for_dept("worker", u64::MAX, 1, Duration::from_secs(30))
            .unwrap()
            .remove(0);
        assert_eq!(replay.attempt, 1);
        assert_eq!(replay.lease_generation, 2);
        assert!(requires_replay_scratch_bypass(&replay));
    }

    fn write_executable(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn built_framework_binary() -> PathBuf {
        let test_binary = std::env::current_exe().unwrap();
        let binary = test_binary
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("fkst-framework");
        assert!(binary.is_file(), "missing binary: {}", binary.display());
        binary
    }

    fn shell_quote_path(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    fn write_single_package_workspace(root: &Path) {
        write_package_manifest(root, &unit_name(root));
        write_workspace_manifest(root, &[root]);
    }

    fn write_composed_workspace(host: &Path, package_roots: &[&Path]) {
        write_package_manifest(host, "host");
        for package_root in package_roots {
            write_package_manifest(package_root, &unit_name(package_root));
        }
        let mut roots = Vec::with_capacity(package_roots.len() + 1);
        roots.push(host);
        roots.extend(
            package_roots
                .iter()
                .copied()
                .filter(|package_root| package_root.starts_with(host)),
        );
        write_workspace_manifest(host, &roots);
        for package_root in package_roots {
            write_workspace_manifest(package_root, &[*package_root]);
        }
    }

    fn write_package_manifest(root: &Path, name: &str) {
        fs::write(
            root.join("fkst.toml"),
            format!(
                r#"
kind = "package"
name = "{name}"

[code]
root = "."
"#
            ),
        )
        .unwrap();
    }

    fn write_workspace_manifest(workspace_root: &Path, unit_roots: &[&Path]) {
        let units = unit_roots
            .iter()
            .map(|root| {
                let relative = relative_unit_path(workspace_root, root);
                format!(r#""{}""#, relative.display())
            })
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            workspace_root.join("fkst.workspace.toml"),
            format!(
                r#"
[workspace]
units = [{units}]
"#
            ),
        )
        .unwrap();
    }

    fn relative_unit_path(workspace_root: &Path, unit_root: &Path) -> PathBuf {
        let workspace_root = workspace_root.canonicalize().unwrap();
        let unit_root = unit_root.canonicalize().unwrap();
        if unit_root == workspace_root {
            return PathBuf::from(".");
        }
        if let Ok(relative) = unit_root.strip_prefix(&workspace_root) {
            return relative.to_path_buf();
        }
        if let Some(parent) = workspace_root.parent() {
            if let Ok(sibling) = unit_root.strip_prefix(parent) {
                let mut relative = PathBuf::from("..");
                relative.push(sibling);
                return relative;
            }
        }
        panic!(
            "test unit root {} is not relative to workspace {}",
            unit_root.display(),
            workspace_root.display()
        );
    }

    fn unit_name(root: &Path) -> String {
        root.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("pkg")
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => byte as char,
                _ => '_',
            })
            .collect()
    }

    fn durable_decl(root: &Path, dept: &str, stall_window: &str) -> DepartmentDecl {
        DepartmentDecl {
            lua: format!("departments/{dept}/main.lua").into(),
            owner_root: root.canonicalize().unwrap(),
            owner_namespace: package_namespace(root),
            consumes: vec!["jobs".to_string()],
            produces: Vec::new(),
            published_seam: Vec::new(),
            ephemeral: Vec::new(),
            stall_window: stall_window.to_string(),
            graph_json: false,
            retry: None,
        }
    }

    fn durable_router(
        store: Arc<DeliveryStore>,
        departments: &[(&str, DepartmentDecl)],
    ) -> DeliveryRouter {
        let mut queue = BTreeMap::new();
        queue.insert(
            "jobs".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let department = departments
            .iter()
            .map(|(name, decl)| ((*name).to_string(), decl.clone()))
            .collect();
        let cfg = Config {
            queue,
            raiser: BTreeMap::new(),
            department,
            limits: LimitsDecl {
                global_codex_processes: 1,
            },
        };
        DeliveryRouter::new(&cfg, Fanout::new(), Some(store), None)
    }

    fn record_for_dept(id: &str, dept: &str) -> DeliveryRecord {
        let mut record = record(id);
        record.dept = dept.to_string();
        record
    }

    async fn wait_for_started_count(path: &Path, count: usize) {
        timeout(Duration::from_secs(5), async {
            loop {
                if fs::read_to_string(path)
                    .map(|text| text.lines().count())
                    .unwrap_or(0)
                    >= count
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("durable child should reach the expected start count");
    }

    #[test]
    fn ephemeral_disconnect_disables_arm_when_reliable_queues_remain() {
        let mut ephemeral_open = true;

        let should_exit = on_ephemeral_disconnect(false, &mut ephemeral_open);

        assert_eq!(should_exit, ShouldExit::No);
        assert!(!ephemeral_open);
    }

    #[test]
    fn ephemeral_disconnect_exits_when_reliable_queues_are_empty() {
        let mut ephemeral_open = true;

        let should_exit = on_ephemeral_disconnect(true, &mut ephemeral_open);

        assert_eq!(should_exit, ShouldExit::Yes);
        assert!(ephemeral_open);
    }

    #[test]
    fn wake_disconnect_clears_receiver() {
        let (_tx, rx) = mpsc::channel::<()>(1);
        let mut reliable_wake_rx = Some(rx);

        on_wake_disconnect(&mut reliable_wake_rx);

        assert!(reliable_wake_rx.is_none());
    }

    #[test]
    fn durable_dispatch_capacity_admits_one_per_pass_with_default_burst() {
        assert_eq!(durable_dispatch_capacity(0, 16, 1), 1);
        assert_eq!(durable_dispatch_capacity(1, 16, 1), 1);
        assert_eq!(durable_dispatch_capacity(16, 16, 1), 0);
    }

    #[test]
    fn durable_dispatch_capacity_honors_steady_state_ceiling() {
        assert_eq!(durable_dispatch_capacity(15, 16, 1), 1);
        assert_eq!(durable_dispatch_capacity(16, 16, 1), 0);
        assert_eq!(durable_dispatch_capacity(17, 16, 1), 0);
    }

    #[test]
    fn durable_dispatch_capacity_limits_restart_burst() {
        assert_eq!(durable_dispatch_capacity(0, 16, 1), 1);
        assert_eq!(durable_dispatch_capacity(0, 16, 4), 4);
    }

    #[test]
    fn durable_dispatch_capacity_clamps_to_burst_and_dispatch_batch() {
        assert_eq!(durable_dispatch_capacity(0, 16, 8), 8);
        assert_eq!(durable_dispatch_capacity(0, 64, 32), DISPATCH_BATCH);
    }

    #[test]
    fn renew_interval_is_before_retry_lease() {
        let stall_window = Duration::from_secs(30);

        let interval = renew_interval(stall_window);

        assert!(interval >= Duration::from_secs(1));
        assert!(interval < retry_lease(stall_window));
    }

    #[test]
    fn dispatch_renew_deadline_is_before_stored_lease_expiry() {
        for stall_window in [
            Duration::ZERO,
            Duration::from_millis(1),
            Duration::from_secs(1),
            Duration::from_secs(30),
        ] {
            let temp = TempDir::new().unwrap();
            let store = DeliveryStore::open(temp.path().join("delivery.redb")).unwrap();
            store.enqueue(&record("one")).unwrap();
            let now_ms = 1_000;
            let leased = store
                .lease_for_dept("worker", now_ms, 1, retry_lease(stall_window))
                .unwrap();
            let stored = store.get("one").unwrap().unwrap();
            let next_renew_at_ms = next_renew_at_ms(now_ms, stall_window);

            assert_eq!(leased.len(), 1);
            assert!(next_renew_at_ms < stored.lease_until_ms.unwrap());
        }
    }

    fn router_with_dead_letter(store: Arc<DeliveryStore>) -> DeliveryRouter {
        router_with_dead_letter_and_fanout(store, Fanout::new())
    }

    fn router_with_dead_letter_and_fanout(
        store: Arc<DeliveryStore>,
        fanout: Fanout,
    ) -> DeliveryRouter {
        let mut queue = BTreeMap::new();
        queue.insert(
            "dead_letter".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let mut department = BTreeMap::new();
        department.insert(
            "dlq".to_string(),
            DepartmentDecl {
                lua: "departments/dlq/main.lua".into(),
                owner_root: std::path::PathBuf::from("."),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["dead_letter".to_string()],
                produces: Vec::new(),
                published_seam: Vec::new(),
                ephemeral: vec!["dead_letter".to_string()],
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
            },
        };
        DeliveryRouter::new(&cfg, fanout, Some(store), None)
    }

    fn router_with_failure_fact_and_fanout(fanout: Fanout) -> DeliveryRouter {
        let mut queue = BTreeMap::new();
        queue.insert(
            "fkst.failure_fact".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let mut department = BTreeMap::new();
        department.insert(
            "triage".to_string(),
            DepartmentDecl {
                lua: "departments/triage/main.lua".into(),
                owner_root: std::path::PathBuf::from("."),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["fkst.failure_fact".to_string()],
                produces: Vec::new(),
                published_seam: Vec::new(),
                ephemeral: vec!["fkst.failure_fact".to_string()],
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
            },
        };
        DeliveryRouter::new(&cfg, fanout, None, None)
    }

    fn router_with_reliable_jobs_and_failure_fact(
        store: Arc<DeliveryStore>,
        fanout: Fanout,
    ) -> DeliveryRouter {
        let mut queue = BTreeMap::new();
        for name in ["jobs", "fkst.failure_fact"] {
            queue.insert(
                name.to_string(),
                QueueDecl {
                    capacity: 8,
                    fanout: false,
                },
            );
        }
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
                ephemeral: Vec::new(),
                stall_window: "30s".to_string(),
                graph_json: false,
                retry: None,
            },
        );
        department.insert(
            "triage".to_string(),
            DepartmentDecl {
                lua: "departments/triage/main.lua".into(),
                owner_root: std::path::PathBuf::from("."),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["fkst.failure_fact".to_string()],
                produces: Vec::new(),
                published_seam: Vec::new(),
                ephemeral: vec!["fkst.failure_fact".to_string()],
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
            },
        };
        DeliveryRouter::new(&cfg, fanout, Some(store), None)
    }

    fn authenticated_raised_stdout(token: &str, entries: serde_json::Value) -> String {
        let encoded =
            base64::engine::general_purpose::URL_SAFE.encode(serde_json::to_vec(&entries).unwrap());
        format!("RAISED-AUTH: {token} {encoded}\n")
    }

    #[test]
    fn spawn_args_passes_composed_package_roots_for_namespace_graph() {
        let temp = TempDir::new().unwrap();
        let host = temp.path().join("host");
        let github_devloop = temp.path().join("github-devloop");
        let consensus = temp.path().join("consensus");
        fs::create_dir_all(&host).unwrap();
        fs::create_dir_all(&github_devloop).unwrap();
        fs::create_dir_all(&consensus).unwrap();
        write_composed_workspace(&host, &[&github_devloop, &consensus]);
        let roots =
            PackageRoots::resolve(&host, vec![github_devloop.clone(), consensus.clone()]).unwrap();
        let decl = DepartmentDecl {
            lua: "departments/producer/main.lua".into(),
            owner_root: github_devloop.canonicalize().unwrap(),
            owner_namespace: "github-devloop".to_string(),
            consumes: vec!["github-devloop.tick".to_string()],
            produces: vec!["consensus.proposal".to_string()],
            published_seam: Vec::new(),
            ephemeral: vec!["github-devloop.tick".to_string()],
            stall_window: "30s".to_string(),
            graph_json: false,
            retry: None,
        };

        let args = spawn_args(
            &decl,
            &host,
            &roots,
            &host.join("fkst-framework"),
            &host.join("logs"),
            Event::new("github-devloop.tick", serde_json::json!({})),
            Duration::from_secs(30),
            1,
            ProcessGroupRegistry::default(),
        )
        .unwrap();

        assert_eq!(args.graph_package_roots, roots.package_roots());
        assert_eq!(args.owner_namespace, "github-devloop");
    }

    #[test]
    fn spawn_args_keeps_folded_single_package_root_form() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path()).unwrap();
        write_single_package_workspace(temp.path());
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let owner_namespace = package_namespace(temp.path());
        let decl = DepartmentDecl {
            lua: "departments/producer/main.lua".into(),
            owner_root: temp.path().canonicalize().unwrap(),
            owner_namespace: owner_namespace.clone(),
            consumes: vec!["tick".to_string()],
            produces: vec!["done".to_string()],
            published_seam: Vec::new(),
            ephemeral: vec!["tick".to_string()],
            stall_window: "30s".to_string(),
            graph_json: false,
            retry: None,
        };

        let args = spawn_args(
            &decl,
            temp.path(),
            &roots,
            &temp.path().join("fkst-framework"),
            &temp.path().join("logs"),
            Event::new("tick", serde_json::json!({})),
            Duration::from_secs(30),
            1,
            ProcessGroupRegistry::default(),
        )
        .unwrap();

        assert_eq!(
            args.graph_package_roots,
            vec![temp.path().canonicalize().unwrap()]
        );
        assert_eq!(args.owner_namespace, owner_namespace);
    }

    #[tokio::test]
    async fn durable_stdout_raised_line_does_not_publish() {
        let _resource_guard = resource_heavy_test_lock();
        let temp = TempDir::new().unwrap();
        let binary = temp.path().join("fkst-framework");
        let lua = temp.path().join("dept.lua");
        let log_dir = temp.path().join("logs");
        fs::write(&lua, "return {}\n").unwrap();
        let raised = base64::engine::general_purpose::URL_SAFE.encode(
            serde_json::to_vec(&serde_json::json!([
                {"queue": "done", "payload": {"n": 2}}
            ]))
            .unwrap(),
        );
        write_executable(
            &binary,
            &format!("printf 'RAISED: {raised}\\n'; sleep 5; exit 0"),
        );

        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("done", 8).await;
        let mut queue = BTreeMap::new();
        queue.insert(
            "done".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let mut department = BTreeMap::new();
        department.insert(
            "observer".to_string(),
            DepartmentDecl {
                lua: "departments/observer/main.lua".into(),
                owner_root: temp.path().into(),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["done".to_string()],
                produces: Vec::new(),
                published_seam: Vec::new(),
                ephemeral: vec!["done".to_string()],
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
            },
        };
        let router = DeliveryRouter::new(&cfg, fanout, None, None);
        let spawned_router = router.clone();
        let args = SpawnArgs {
            framework_bin: binary,
            lua_full: lua,
            project_root: temp.path().into(),
            graph_package_roots: vec![temp.path().into()],
            event_json: "{}".to_string(),
            stall_window: Duration::from_secs(30),
            codex_permit_slots: 1,
            raised_auth_token: Some("trusted-token".to_string()),
            replay_scratch_bypass: false,
            log_dir: log_dir.clone(),
            owner_namespace: "pkg".to_string(),
            process_groups: ProcessGroupRegistry::default(),
        };

        let done = run_durable_record(
            "worker",
            &spawned_router,
            record("parent"),
            args,
            &SupervisorJournal::disabled(),
        )
        .await;

        assert!(done.failure.is_none());
        let no_event = timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(no_event.is_err(), "stdout RAISED frame must not publish");
    }

    #[tokio::test]
    async fn ephemeral_lock_busy_publishes_fact_without_durable_record() {
        let _resource_guard = resource_heavy_test_lock();
        let temp = TempDir::new().unwrap();
        let binary = temp.path().join("fkst-framework");
        let lua = temp.path().join("departments/worker/main.lua");
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(lua.parent().unwrap()).unwrap();
        fs::write(&lua, "return {}\n").unwrap();
        write_executable(
            &binary,
            "printf 'event=with_lock_busy error_class=lock_busy lock=shared/work outcome=deferred\\n' >&2\nexit 75",
        );
        write_single_package_workspace(temp.path());
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let decl = DepartmentDecl {
            lua: "departments/worker/main.lua".into(),
            owner_root: temp.path().canonicalize().unwrap(),
            owner_namespace: package_namespace(temp.path()),
            consumes: vec!["jobs".to_string()],
            produces: Vec::new(),
            published_seam: Vec::new(),
            ephemeral: vec!["jobs".to_string()],
            stall_window: "30s".to_string(),
            graph_json: false,
            retry: None,
        };
        let fanout = Fanout::new();
        let mut failure_rx = fanout.subscribe("fkst.failure_fact", 8).await;
        let router = router_with_failure_fact_and_fanout(fanout);
        let journal_path = temp.path().join("supervisor.log");
        let journal = SupervisorJournal::open_at(&journal_path);

        spawn_ephemeral(
            "worker",
            &decl,
            temp.path(),
            &roots,
            &binary,
            &router,
            &log_dir,
            Event::new("jobs", serde_json::json!({"n": 1})),
            Duration::from_secs(30),
            1,
            ProcessGroupRegistry::default(),
            journal,
        );

        let fact = timeout(Duration::from_secs(2), failure_rx.recv())
            .await
            .expect("ephemeral lock busy fact should publish promptly")
            .expect("failure fact subscription should remain open");
        assert_eq!(fact.payload["error_class"], "lock_busy");
        assert_eq!(fact.payload["lock"], "shared/work");
        assert!(fact.payload["delivery_id"].is_null());
        assert!(fact.payload["attempt"].is_null());

        let journal = fs::read_to_string(journal_path).unwrap();
        assert!(journal.contains("event=lock_busy"), "journal: {journal}");
        assert!(
            journal.contains("outcome=ephemeral_dropped"),
            "journal: {journal}"
        );
        assert!(!temp.path().join("delivery.redb").exists());
    }

    #[tokio::test]
    async fn durable_authenticated_raised_publishes_before_child_exit() {
        let _resource_guard = resource_heavy_test_lock();
        let temp = TempDir::new().unwrap();
        let binary = temp.path().join("fkst-framework");
        let lua = temp.path().join("dept.lua");
        let log_dir = temp.path().join("logs");
        let release = temp.path().join("release");
        fs::write(&lua, "return {}\n").unwrap();
        let trusted_raised = base64::engine::general_purpose::URL_SAFE.encode(
            serde_json::to_vec(&serde_json::json!([
                {"queue": "done", "payload": {"n": 2}}
            ]))
            .unwrap(),
        );
        write_executable(
            &binary,
            &format!(
                "printf 'RAISED: forged\\n'; printf 'RAISED-AUTH: trusted-token {trusted_raised}\\n'; while [ ! -f '{}' ]; do sleep 0.05; done; exit 0",
                release.display()
            ),
        );

        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("done", 8).await;
        let mut queue = BTreeMap::new();
        queue.insert(
            "done".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let mut department = BTreeMap::new();
        department.insert(
            "observer".to_string(),
            DepartmentDecl {
                lua: "departments/observer/main.lua".into(),
                owner_root: temp.path().into(),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["done".to_string()],
                produces: Vec::new(),
                published_seam: Vec::new(),
                ephemeral: vec!["done".to_string()],
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
            },
        };
        let router = DeliveryRouter::new(&cfg, fanout, None, None);
        let spawned_router = router.clone();
        let args = SpawnArgs {
            framework_bin: binary,
            lua_full: lua,
            project_root: temp.path().into(),
            graph_package_roots: vec![temp.path().into()],
            event_json: "{}".to_string(),
            stall_window: Duration::from_secs(30),
            codex_permit_slots: 1,
            raised_auth_token: Some("trusted-token".to_string()),
            replay_scratch_bypass: false,
            log_dir,
            owner_namespace: "pkg".to_string(),
            process_groups: ProcessGroupRegistry::default(),
        };

        let handle = tokio::spawn(async move {
            run_durable_record(
                "worker",
                &spawned_router,
                record("parent"),
                args,
                &SupervisorJournal::disabled(),
            )
            .await
        });
        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("authenticated raise should publish before child exit")
            .expect("done subscription should remain open");

        assert_eq!(event.queue, "done");
        assert_eq!(event.payload, serde_json::json!({"n": 2}));
        fs::write(&release, "").unwrap();
        let done = handle.await.unwrap();
        assert!(done.failure.is_none());
        let no_duplicate = timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            no_duplicate.is_err(),
            "streamed RAISED frame must not publish again after child exit"
        );
    }

    #[tokio::test]
    async fn durable_dispatch_admits_one_new_child_per_pass_under_backlog() {
        let _resource_guard = resource_heavy_test_lock();
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("one")).unwrap();
        store.enqueue(&record("two")).unwrap();
        let started = temp.path().join("started");
        let release = temp.path().join("release");
        let binary = temp.path().join("fkst-framework");
        let lua = temp.path().join("departments/worker/main.lua");
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(lua.parent().unwrap()).unwrap();
        fs::write(&lua, "return {}\n").unwrap();
        write_executable(
            &binary,
            &format!(
                "printf 'started\\n' >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexit 0",
                started.display(),
                release.display()
            ),
        );
        write_single_package_workspace(temp.path());
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let decl = DepartmentDecl {
            lua: "departments/worker/main.lua".into(),
            owner_root: temp.path().canonicalize().unwrap(),
            owner_namespace: package_namespace(temp.path()),
            consumes: vec!["jobs".to_string()],
            produces: Vec::new(),
            published_seam: Vec::new(),
            ephemeral: Vec::new(),
            stall_window: "30s".to_string(),
            graph_json: false,
            retry: None,
        };
        let mut queue = BTreeMap::new();
        queue.insert(
            "jobs".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let mut department = BTreeMap::new();
        department.insert("worker".to_string(), decl.clone());
        let cfg = Config {
            queue,
            raiser: BTreeMap::new(),
            department,
            limits: LimitsDecl {
                global_codex_processes: 1,
            },
        };
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);
        let (complete_tx, mut complete_rx) = mpsc::channel::<CompletedDelivery>(8);
        let mut running = BTreeMap::new();

        dispatch_due(
            "worker",
            &decl,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_secs(30),
            1,
            16,
            1,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running,
        );
        timeout(Duration::from_secs(2), async {
            loop {
                if fs::read_to_string(&started)
                    .map(|text| text.lines().count())
                    .unwrap_or(0)
                    == 1
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first durable child should start");
        assert_eq!(running.len(), 1);

        dispatch_due(
            "worker",
            &decl,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_secs(30),
            1,
            16,
            1,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running,
        );
        wait_for_started_count(&started, 2).await;
        assert_eq!(running.len(), 2);

        dispatch_due(
            "worker",
            &decl,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_secs(30),
            1,
            16,
            1,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running,
        );
        assert_eq!(
            fs::read_to_string(&started).unwrap().lines().count(),
            2,
            "backlog admission should stay bounded by one new child per pass"
        );

        fs::write(&release, "").unwrap();
        for _ in 0..2 {
            let done = timeout(Duration::from_secs(2), complete_rx.recv())
                .await
                .expect("durable child should complete")
                .expect("completion channel should remain open");
            running.remove(&done.record.delivery_id);
            finish_durable_record(
                "worker",
                Some(store.as_ref()),
                &router,
                None,
                &policy(3),
                done,
                &SupervisorJournal::disabled(),
            );
        }

        assert!(store.get("one").unwrap().is_none());
        assert!(store.get("two").unwrap().is_none());
    }

    #[tokio::test]
    async fn durable_dispatch_receives_rebound_delivery_after_consumer_swap() {
        let _resource_guard = resource_heavy_test_lock();
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let mut stale = record_for_dept("swapped", "old_worker");
        stale.queue = "jobs".to_string();
        store.enqueue(&stale).unwrap();
        let started = temp.path().join("started");
        let release = temp.path().join("release");
        let binary = temp.path().join("fkst-framework");
        let lua = temp.path().join("departments/new_worker/main.lua");
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(lua.parent().unwrap()).unwrap();
        fs::write(&lua, "return {}\n").unwrap();
        write_executable(
            &binary,
            &format!(
                "printf 'started\\n' >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexit 0",
                started.display(),
                release.display()
            ),
        );
        write_single_package_workspace(temp.path());
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let decl = durable_decl(temp.path(), "new_worker", "30s");
        let router = durable_router(store.clone(), &[("new_worker", decl.clone())]);
        assert!(store
            .lease_for_dept("new_worker", now_unix_millis(), 1, Duration::from_secs(30))
            .unwrap()
            .is_empty());
        store
            .rebind_deliveries_to_current_subscribers(
                &router.single_reliable_subscribers_by_queue(),
            )
            .unwrap();
        let (complete_tx, mut complete_rx) = mpsc::channel::<CompletedDelivery>(8);
        let mut running = BTreeMap::new();

        dispatch_due(
            "new_worker",
            &decl,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_secs(30),
            1,
            16,
            1,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running,
        );
        wait_for_started_count(&started, 1).await;
        assert_eq!(running.len(), 1);
        let running_delivery = running.values().next().unwrap();
        assert_eq!(running_delivery.record.delivery_id, "swapped");
        assert_eq!(running_delivery.record.dept, "new_worker");

        fs::write(&release, "").unwrap();
        let done = timeout(Duration::from_secs(2), complete_rx.recv())
            .await
            .expect("rebound durable child should complete")
            .expect("completion channel should remain open");
        running.remove(&done.record.delivery_id);
        finish_durable_record(
            "new_worker",
            Some(store.as_ref()),
            &router,
            None,
            &policy(3),
            done,
            &SupervisorJournal::disabled(),
        );

        assert!(store.get("swapped").unwrap().is_none());
    }

    #[tokio::test]
    async fn durable_dispatch_admits_burst_children_in_one_pass() {
        let _resource_guard = resource_heavy_test_lock();
        // Guards the dispatch path itself (not just durable_dispatch_capacity arithmetic):
        // with admission_burst = 2 a single dispatch_due pass must lease+spawn exactly two
        // children, proving FKST_DURABLE_ADMISSION_BURST_PER_DEPT > 1 actually admits more
        // than one per pass and is not silently pinned to one-at-a-time downstream.
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("one")).unwrap();
        store.enqueue(&record("two")).unwrap();
        store.enqueue(&record("three")).unwrap();
        let started = temp.path().join("started");
        let release = temp.path().join("release");
        let binary = temp.path().join("fkst-framework");
        let lua = temp.path().join("departments/worker/main.lua");
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(lua.parent().unwrap()).unwrap();
        fs::write(&lua, "return {}\n").unwrap();
        write_executable(
            &binary,
            &format!(
                "printf 'started\\n' >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexit 0",
                started.display(),
                release.display()
            ),
        );
        write_single_package_workspace(temp.path());
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let decl = DepartmentDecl {
            lua: "departments/worker/main.lua".into(),
            owner_root: temp.path().canonicalize().unwrap(),
            owner_namespace: package_namespace(temp.path()),
            consumes: vec!["jobs".to_string()],
            produces: Vec::new(),
            published_seam: Vec::new(),
            ephemeral: Vec::new(),
            stall_window: "30s".to_string(),
            graph_json: false,
            retry: None,
        };
        let mut queue = BTreeMap::new();
        queue.insert(
            "jobs".to_string(),
            QueueDecl {
                capacity: 8,
                fanout: false,
            },
        );
        let mut department = BTreeMap::new();
        department.insert("worker".to_string(), decl.clone());
        let cfg = Config {
            queue,
            raiser: BTreeMap::new(),
            department,
            limits: LimitsDecl {
                global_codex_processes: 1,
            },
        };
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);
        let (complete_tx, mut complete_rx) = mpsc::channel::<CompletedDelivery>(8);
        let mut running = BTreeMap::new();

        dispatch_due(
            "worker",
            &decl,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_secs(30),
            1,
            16,
            2,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running,
        );
        wait_for_started_count(&started, 2).await;
        assert_eq!(
            running.len(),
            2,
            "admission_burst=2 should start exactly two children in one dispatch pass"
        );
        assert_eq!(
            fs::read_to_string(&started).unwrap().lines().count(),
            2,
            "burst bounds a single pass at two children even with three queued"
        );

        fs::write(&release, "").unwrap();
        for _ in 0..2 {
            let done = timeout(Duration::from_secs(2), complete_rx.recv())
                .await
                .expect("durable child should complete")
                .expect("completion channel should remain open");
            running.remove(&done.record.delivery_id);
            finish_durable_record(
                "worker",
                Some(store.as_ref()),
                &router,
                None,
                &policy(3),
                done,
                &SupervisorJournal::disabled(),
            );
        }
    }

    #[tokio::test]
    async fn durable_dispatch_cap_is_per_department_not_global() {
        let _resource_guard = resource_heavy_test_lock();
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store
            .enqueue(&record_for_dept("dept-a-one", "dept_a"))
            .unwrap();
        store
            .enqueue(&record_for_dept("dept-b-one", "dept_b"))
            .unwrap();
        let started = temp.path().join("started");
        let release = temp.path().join("release");
        let binary = temp.path().join("fkst-framework");
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(temp.path().join("departments/dept_a")).unwrap();
        fs::create_dir_all(temp.path().join("departments/dept_b")).unwrap();
        fs::write(
            temp.path().join("departments/dept_a/main.lua"),
            "return {}\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("departments/dept_b/main.lua"),
            "return {}\n",
        )
        .unwrap();
        write_executable(
            &binary,
            &format!(
                "printf 'started\\n' >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexit 0",
                started.display(),
                release.display()
            ),
        );
        write_single_package_workspace(temp.path());
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let decl_a = durable_decl(temp.path(), "dept_a", "30s");
        let decl_b = durable_decl(temp.path(), "dept_b", "30s");
        let router = durable_router(
            store.clone(),
            &[("dept_a", decl_a.clone()), ("dept_b", decl_b.clone())],
        );
        let (complete_tx, mut complete_rx) = mpsc::channel::<CompletedDelivery>(8);
        let mut running_a = BTreeMap::new();
        let mut running_b = BTreeMap::new();

        dispatch_due(
            "dept_a",
            &decl_a,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_secs(30),
            1,
            16,
            1,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running_a,
        );
        wait_for_started_count(&started, 1).await;
        assert_eq!(running_a.len(), 1);

        dispatch_due(
            "dept_b",
            &decl_b,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_secs(30),
            1,
            16,
            1,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running_b,
        );
        wait_for_started_count(&started, 2).await;
        assert_eq!(running_b.len(), 1);

        fs::write(&release, "").unwrap();
        for _ in 0..2 {
            let done = timeout(Duration::from_secs(2), complete_rx.recv())
                .await
                .expect("durable child should complete")
                .expect("completion channel should remain open");
            let dept = done.record.dept.clone();
            if dept == "dept_a" {
                running_a.remove(&done.record.delivery_id);
            } else {
                running_b.remove(&done.record.delivery_id);
            }
            finish_durable_record(
                &dept,
                Some(store.as_ref()),
                &router,
                None,
                &policy(3),
                done,
                &SupervisorJournal::disabled(),
            );
        }

        assert!(store.get("dept-a-one").unwrap().is_none());
        assert!(store.get("dept-b-one").unwrap().is_none());
    }

    #[tokio::test]
    async fn durable_dispatch_does_not_duplicate_same_dept_while_running() {
        let _resource_guard = resource_heavy_test_lock();
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("one")).unwrap();
        store.enqueue(&record("two")).unwrap();
        let started = temp.path().join("started");
        let release = temp.path().join("release");
        let binary = temp.path().join("fkst-framework");
        let lua = temp.path().join("departments/worker/main.lua");
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(lua.parent().unwrap()).unwrap();
        fs::write(&lua, "return {}\n").unwrap();
        write_executable(
            &binary,
            &format!(
                "printf 'started\\n' >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexit 0",
                started.display(),
                release.display()
            ),
        );
        write_single_package_workspace(temp.path());
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let decl = durable_decl(temp.path(), "worker", "1ms");
        let router = durable_router(store.clone(), &[("worker", decl.clone())]);
        let (complete_tx, mut complete_rx) = mpsc::channel::<CompletedDelivery>(8);
        let mut running = BTreeMap::new();

        dispatch_due(
            "worker",
            &decl,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_millis(1),
            1,
            16,
            1,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running,
        );
        wait_for_started_count(&started, 1).await;
        assert_eq!(running.len(), 1);
        assert_eq!(durable_dispatch_capacity(running.len(), 1, 1), 0);

        // A saturated max-in-flight gate keeps due records unleased while the running child
        // renews its lease.
        renew_running(
            "worker",
            Some(store.as_ref()),
            Duration::from_millis(1),
            &mut running,
        );
        dispatch_due(
            "worker",
            &decl,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_millis(1),
            1,
            1,
            1,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running,
        );
        assert_eq!(
            fs::read_to_string(&started).unwrap().lines().count(),
            1,
            "second due record must not dispatch while max-in-flight is saturated"
        );

        fs::write(&release, "").unwrap();
        let done = timeout(Duration::from_secs(2), complete_rx.recv())
            .await
            .expect("first durable child should complete")
            .expect("completion channel should remain open");
        running.remove(&done.record.delivery_id);
        finish_durable_record(
            "worker",
            Some(store.as_ref()),
            &router,
            None,
            &policy(3),
            done,
            &SupervisorJournal::disabled(),
        );

        dispatch_due(
            "worker",
            &decl,
            temp.path(),
            &roots,
            &binary,
            &router,
            Some(store.clone()),
            None,
            &log_dir,
            Duration::from_millis(1),
            1,
            1,
            1,
            ProcessGroupRegistry::default(),
            SupervisorJournal::disabled(),
            &complete_tx,
            &mut running,
        );
        wait_for_started_count(&started, 2).await;

        fs::write(&release, "").unwrap();
        let done = timeout(Duration::from_secs(2), complete_rx.recv())
            .await
            .expect("second durable child should complete")
            .expect("completion channel should remain open");
        running.remove(&done.record.delivery_id);
        finish_durable_record(
            "worker",
            Some(store.as_ref()),
            &router,
            None,
            &policy(3),
            done,
            &SupervisorJournal::disabled(),
        );

        assert!(store.get("one").unwrap().is_none());
        assert!(store.get("two").unwrap().is_none());
    }

    #[tokio::test]
    async fn renew_running_skips_ticks_until_deadline_then_batches() {
        let temp = TempDir::new().unwrap();
        let store = DeliveryStore::open(temp.path().join("delivery.redb")).unwrap();
        store.enqueue(&record("one")).unwrap();
        store.enqueue(&record("two")).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 2, Duration::from_secs(30))
            .unwrap();
        let first_until = leased[0].lease_until_ms.unwrap();
        let mut running = BTreeMap::new();
        for mut record in leased {
            record.dept = "worker".to_string();
            let handle = tokio::spawn(async {
                std::future::pending::<()>().await;
            });
            running.insert(
                record.delivery_id.clone(),
                RunningDelivery {
                    next_renew_at_ms: u64::MAX,
                    record,
                    handle,
                },
            );
        }

        renew_running(
            "worker",
            Some(&store),
            Duration::from_secs(30),
            &mut running,
        );
        assert_eq!(
            store.get("one").unwrap().unwrap().lease_until_ms,
            Some(first_until)
        );

        for running_delivery in running.values_mut() {
            running_delivery.next_renew_at_ms = 0;
        }
        renew_running(
            "worker",
            Some(&store),
            Duration::from_secs(30),
            &mut running,
        );

        let one = store.get("one").unwrap().unwrap();
        let two = store.get("two").unwrap().unwrap();
        assert!(one.lease_until_ms.unwrap() > first_until);
        assert_eq!(one.lease_until_ms, two.lease_until_ms);
        assert_eq!(one.lease_generation, 1);
        assert_eq!(two.lease_generation, 1);
        assert!(running
            .values()
            .all(|delivery| delivery.next_renew_at_ms > 0));
        for delivery in running.into_values() {
            delivery.handle.abort();
        }
    }

    #[test]
    fn durable_success_ack_removes_delivery() {
        let temp = TempDir::new().unwrap();
        let store = DeliveryStore::open(temp.path().join("delivery.redb")).unwrap();
        store.enqueue(&record("one")).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(50))
            .unwrap();

        assert!(store
            .ack(&leased[0].delivery_id, leased[0].lease_generation)
            .unwrap());

        assert!(store.get("one").unwrap().is_none());
    }

    #[tokio::test]
    async fn reserved_lock_busy_exit_without_stderr_defers_without_attempt_or_dead_letter() {
        let _resource_guard = resource_heavy_test_lock();
        let temp = TempDir::new().unwrap();
        let binary = temp.path().join("fkst-framework");
        let lua = temp.path().join("departments/worker/main.lua");
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(lua.parent().unwrap()).unwrap();
        fs::write(&lua, "return {}\n").unwrap();
        write_executable(&binary, "exit 75");

        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("one")).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 1, Duration::from_secs(30))
            .unwrap()
            .remove(0);
        let fanout = Fanout::new();
        let mut failure_rx = fanout.subscribe("fkst.failure_fact", 8).await;
        let router = router_with_reliable_jobs_and_failure_fact(store.clone(), fanout);
        let journal_path = temp.path().join("supervisor.log");
        let journal = SupervisorJournal::open_at(&journal_path);
        let before_defer = now_unix_millis();
        let args = SpawnArgs {
            framework_bin: binary,
            lua_full: lua,
            project_root: temp.path().into(),
            graph_package_roots: vec![temp.path().into()],
            event_json: "{}".to_string(),
            stall_window: Duration::from_secs(30),
            codex_permit_slots: 1,
            raised_auth_token: Some("trusted-token".to_string()),
            replay_scratch_bypass: false,
            log_dir,
            owner_namespace: "pkg".to_string(),
            process_groups: ProcessGroupRegistry::default(),
        };

        let done = timeout(
            Duration::from_secs(5),
            run_durable_record("worker", &router, leased, args, &journal),
        )
        .await
        .expect("reserved lock busy child must return promptly");
        let lock_busy = done
            .lock_busy
            .as_ref()
            .expect("exit 75 must classify as lock busy");
        assert!(lock_busy.lock.is_none());
        assert!(done.failure.is_none());

        let retry_policy = policy(1);
        finish_durable_record(
            "worker",
            Some(store.as_ref()),
            &router,
            Some(&retry_policy),
            &retry_policy,
            done,
            &journal,
        );

        let deferred = store
            .get("one")
            .unwrap()
            .expect("delivery must remain live");
        assert_eq!(deferred.attempt, 0);
        assert!(deferred.lease_until_ms.is_none());
        assert!(deferred.not_before_ms > before_defer);
        assert!(store.get_dead("one").unwrap().is_none());

        let fact = timeout(Duration::from_secs(2), failure_rx.recv())
            .await
            .expect("lock busy failure fact should publish promptly")
            .expect("failure fact subscription should remain open");
        assert_eq!(fact.payload["error_class"], "lock_busy");
        assert!(fact.payload["lock"].is_null());
        assert_eq!(fact.payload["delivery_id"], "one");
        assert_eq!(fact.payload["attempt"], 0);

        let journal_body = fs::read_to_string(journal_path).unwrap();
        assert!(
            journal_body.contains("event=lock_busy"),
            "journal: {journal_body}"
        );
        assert!(
            journal_body.contains("lock=unknown"),
            "journal: {journal_body}"
        );
        assert!(
            journal_body.contains("outcome=deferred"),
            "journal: {journal_body}"
        );
        assert!(
            !journal_body.contains("event=acked"),
            "journal: {journal_body}"
        );
        assert!(
            !journal_body.contains("event=dead_lettered"),
            "journal: {journal_body}"
        );
    }

    #[tokio::test]
    async fn real_lock_contention_defers_without_attempt_or_dead_letter_then_reacquires() {
        let _resource_guard = resource_heavy_test_lock();
        let temp = TempDir::new().unwrap();
        let lock = format!("test/lock-busy-{}", ulid::Ulid::new());
        let callback_entered = temp.path().join("callback-entered");
        let lua = temp.path().join("departments/worker/main.lua");
        let log_dir = temp.path().join("logs");
        let runtime_root = temp.path().join("runtime");
        let framework = temp.path().join("fkst-framework-with-runtime");
        write_executable(
            &framework,
            &format!(
                "export FKST_RUNTIME_ROOT={}\nexec {} \"$@\"",
                shell_quote_path(&runtime_root),
                shell_quote_path(&built_framework_binary()),
            ),
        );
        fs::create_dir_all(lua.parent().unwrap()).unwrap();
        fs::write(
            &lua,
            format!(
                r#"
local M = {{}}
M.spec = {{ consumes = {{ "jobs" }}, stall_window = "30s" }}
function M.pipeline(event)
  with_lock("{lock}", function()
    file.write(event.payload.entered, "entered\n")
  end)
end
return M
"#,
            ),
        )
        .unwrap();
        write_single_package_workspace(temp.path());
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let decl = DepartmentDecl {
            lua: "departments/worker/main.lua".into(),
            owner_root: temp.path().canonicalize().unwrap(),
            owner_namespace: package_namespace(temp.path()),
            consumes: vec!["jobs".to_string()],
            produces: Vec::new(),
            published_seam: Vec::new(),
            ephemeral: Vec::new(),
            stall_window: "30s".to_string(),
            graph_json: false,
            retry: None,
        };

        let layout = fkst_common::RuntimeLayout::new(&runtime_root).unwrap();
        let lock_path = fkst_common::runtime_key_file(
            &layout.runtime_dir(fkst_common::RuntimeKind::Locks),
            &lock,
            fkst_common::RUNTIME_LOCK_LEAF,
        );
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let holder = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        nix::fcntl::flock(
            std::os::fd::AsRawFd::as_raw_fd(&holder),
            nix::fcntl::FlockArg::LockExclusive,
        )
        .unwrap();

        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let mut pending = record("one");
        pending.payload = serde_json::json!({"entered": callback_entered});
        store.enqueue(&pending).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 1, Duration::from_secs(30))
            .unwrap()
            .remove(0);
        let fanout = Fanout::new();
        let mut failure_rx = fanout.subscribe("fkst.failure_fact", 8).await;
        let router = router_with_reliable_jobs_and_failure_fact(store.clone(), fanout);
        let journal_path = temp.path().join("supervisor.log");
        let journal = SupervisorJournal::open_at(&journal_path);
        let before_defer = now_unix_millis();
        let args = spawn_args(
            &decl,
            temp.path(),
            &roots,
            &framework,
            &log_dir,
            event_from_record(&leased),
            Duration::from_secs(30),
            1,
            ProcessGroupRegistry::default(),
        )
        .unwrap();

        let done = timeout(
            Duration::from_secs(30),
            run_durable_record("worker", &router, leased, args, &journal),
        )
        .await
        .expect("contended framework child must return promptly");
        assert_eq!(
            done.lock_busy
                .as_ref()
                .and_then(|lock_busy| lock_busy.lock.as_deref()),
            Some(lock.as_str())
        );
        assert!(done.failure.is_none());
        assert!(!callback_entered.exists(), "busy callback must not execute");

        let retry_policy = policy(1);
        finish_durable_record(
            "worker",
            Some(store.as_ref()),
            &router,
            Some(&retry_policy),
            &retry_policy,
            done,
            &journal,
        );

        let deferred = store
            .get("one")
            .unwrap()
            .expect("delivery must remain live");
        assert_eq!(deferred.attempt, 0);
        assert!(deferred.lease_until_ms.is_none());
        assert!(deferred.not_before_ms > before_defer);
        assert!(store.get_dead("one").unwrap().is_none());
        assert!(store
            .lease_for_dept("worker", before_defer, 1, Duration::from_secs(30))
            .unwrap()
            .is_empty());

        let fact = timeout(Duration::from_secs(1), failure_rx.recv())
            .await
            .expect("lock busy failure fact should publish promptly")
            .expect("failure fact subscription should remain open");
        assert_eq!(fact.payload["error_class"], "lock_busy");
        assert_eq!(fact.payload["lock"], lock);
        assert_eq!(fact.payload["attempt"], 0);
        assert!(fact.payload["fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("lock_busy:"));

        let journal_body = fs::read_to_string(journal_path).unwrap();
        assert!(
            journal_body.contains("event=lock_busy"),
            "journal: {journal_body}"
        );
        assert!(
            journal_body.contains("error_class=lock_busy"),
            "journal: {journal_body}"
        );
        assert!(
            journal_body.contains(&format!("lock={lock}")),
            "journal: {journal_body}"
        );
        assert!(
            journal_body.contains("outcome=deferred"),
            "journal: {journal_body}"
        );

        let child_logs = fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect::<String>();
        assert!(
            child_logs.contains(&format!(
                "event=with_lock_busy error_class=lock_busy lock={lock} outcome=deferred"
            )),
            "child logs: {child_logs}"
        );

        let due_later = store
            .lease_for_dept("worker", u64::MAX, 1, Duration::from_secs(30))
            .unwrap()
            .remove(0);
        assert_eq!(due_later.attempt, 0);

        drop(holder);
        let mut args = spawn_args(
            &decl,
            temp.path(),
            &roots,
            &framework,
            &log_dir,
            event_from_record(&due_later),
            Duration::from_secs(30),
            1,
            ProcessGroupRegistry::default(),
        )
        .unwrap();
        args.replay_scratch_bypass = requires_replay_scratch_bypass(&due_later);
        let done = timeout(
            Duration::from_secs(30),
            run_durable_record("worker", &router, due_later, args, &journal),
        )
        .await
        .expect("framework child must acquire after holder releases");
        assert!(done.lock_busy.is_none());
        assert!(done.failure.is_none());
        finish_durable_record(
            "worker",
            Some(store.as_ref()),
            &router,
            Some(&retry_policy),
            &retry_policy,
            done,
            &journal,
        );
        assert!(callback_entered.exists());
        assert!(store.get("one").unwrap().is_none());
    }

    #[test]
    fn durable_failure_retries_then_success_acks() {
        let temp = TempDir::new().unwrap();
        let store = DeliveryStore::open(temp.path().join("delivery.redb")).unwrap();
        store.enqueue(&record("one")).unwrap();
        let first = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(1))
            .unwrap()
            .remove(0);
        assert_eq!(
            store
                .retry(
                    &first.delivery_id,
                    first.lease_generation,
                    &RetryFailure {
                        message: "failure".to_string(),
                        replayable: false,
                    },
                    &policy(3),
                    now_unix_millis()
                )
                .unwrap(),
            RetryOutcome::Scheduled
        );
        std::thread::sleep(Duration::from_millis(3));
        let second = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(50))
            .unwrap()
            .remove(0);

        assert_eq!(second.attempt, 1);
        assert!(store
            .ack(&second.delivery_id, second.lease_generation)
            .unwrap());
        assert!(store.get("one").unwrap().is_none());
    }

    #[test]
    fn expired_lease_is_released_after_crash_like_gap() {
        let temp = TempDir::new().unwrap();
        let store = DeliveryStore::open(temp.path().join("delivery.redb")).unwrap();
        store.enqueue(&record("one")).unwrap();
        let first = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(1))
            .unwrap()
            .remove(0);
        std::thread::sleep(Duration::from_millis(3));

        let second = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(50))
            .unwrap()
            .remove(0);

        assert_eq!(first.delivery_id, second.delivery_id);
        assert!(second.lease_generation > first.lease_generation);
    }

    #[test]
    fn retry_at_max_writes_dead_and_publishes_dead_letter_without_looping() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("one")).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(50))
            .unwrap()
            .remove(0);
        let router = router_with_dead_letter(store.clone());

        retry_record(
            &store,
            &router,
            Some(&policy(1)),
            &leased,
            DeliveryFailure::permanent("failure"),
            &SupervisorJournal::disabled(),
        );

        assert!(store.get("one").unwrap().is_none());
        assert!(store.get_dead("one").unwrap().is_some());
    }

    #[tokio::test]
    async fn dead_letter_payload_exports_error_fact_fields() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("one")).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(50))
            .unwrap()
            .remove(0);
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("dead_letter", 8).await;
        let router = router_with_dead_letter_and_fanout(store.clone(), fanout);

        retry_record(
            &store,
            &router,
            Some(&policy(1)),
            &leased,
            DeliveryFailure::permanent("exit=7 stderr=bad"),
            &SupervisorJournal::disabled(),
        );

        let event = rx.recv().await.unwrap();
        assert_eq!(event.queue, "dead_letter");
        assert_eq!(event.payload["delivery_id"], "one");
        assert_eq!(event.payload["dedup_id"], "one");
        assert_eq!(event.payload["origin_queue"], "jobs");
        assert_eq!(event.payload["origin_dept"], "worker");
        assert_eq!(event.payload["attempt"], 1);
        assert_eq!(event.payload["error_class"], "framework_child_nonzero");
        assert_eq!(event.payload["source_ref"]["kind"], "cron");
        assert!(event.payload["fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("framework_child_nonzero:"));
    }

    #[tokio::test]
    async fn failure_fact_publish_is_optional_and_subscribable() {
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("fkst.failure_fact", 8).await;
        let router = router_with_failure_fact_and_fanout(fanout);
        let fact = delivery_failure_fact(&record("one"), "spawn error: missing binary");

        router.publish_failure_fact(fact).unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.queue, "fkst.failure_fact");
        assert_eq!(event.payload["schema"], "fkst.failure_fact.v1");
        assert_eq!(event.payload["error_class"], "framework_child_spawn");
        assert_eq!(event.payload["origin_queue"], "jobs");
        assert_eq!(event.payload["origin_dept"], "worker");
    }

    #[test]
    fn raised_output_cannot_publish_engine_failure_fact_queue() {
        let router = router_with_failure_fact_and_fanout(Fanout::new());
        let parent = record("parent");
        let raised = authenticated_raised_stdout(
            "trusted-token",
            serde_json::json!([
                {"queue": "fkst.failure_fact", "payload": {"n": 1}}
            ]),
        );

        let err = publish_raised(&router, &raised, Some("trusted-token"), &parent).unwrap_err();

        assert!(
            err.to_string()
                .contains("reserved engine queue `fkst.failure_fact`"),
            "{err}"
        );
    }

    #[test]
    fn durable_timeout_failure_is_stored_as_replayable_dead_record() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("one")).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(50))
            .unwrap()
            .remove(0);
        let router = router_with_dead_letter(store.clone());
        let failure = DeliveryFailure::from_spawn_result(&SpawnResult {
            pid: 123,
            exit_code: 124,
            stdout: String::new(),
            stderr: "codex timed out".to_string(),
            elapsed_ms: 1_000,
            log_path: None,
        });

        retry_record(
            &store,
            &router,
            Some(&policy(1)),
            &leased,
            failure,
            &SupervisorJournal::disabled(),
        );

        assert!(store.get("one").unwrap().is_none());
        let dead = store.get_dead("one").unwrap().unwrap();
        assert!(dead.replayable);
        assert!(!dead.permanent);
        assert!(dead.record.is_some());
    }

    #[test]
    fn framework_exit_classifier_keeps_timeout_as_the_only_replayable_exit() {
        assert!(is_replayable_framework_exit_code(124));
        for exit_code in [1, 123, 125] {
            assert!(!is_replayable_framework_exit_code(exit_code));
        }
    }

    #[test]
    fn spawn_error_classifier_separates_transient_resource_failures() {
        for errno in [
            Errno::EMFILE,
            Errno::ENFILE,
            Errno::EAGAIN,
            Errno::EWOULDBLOCK,
            Errno::ENOMEM,
        ] {
            let error = anyhow::Error::new(std::io::Error::from_raw_os_error(errno as i32))
                .context("spawn fkst-framework");

            assert!(
                DeliveryFailure::from_spawn_error(&error).replayable,
                "{errno} must be replayable"
            );
        }

        for errno in [Errno::ENOENT, Errno::EACCES, Errno::ENOEXEC] {
            let error = anyhow::Error::new(std::io::Error::from_raw_os_error(errno as i32))
                .context("spawn fkst-framework");

            assert!(
                !DeliveryFailure::from_spawn_error(&error).replayable,
                "{errno} must be permanent"
            );
        }
    }

    #[test]
    fn exhausted_emfile_spawn_failure_is_retained_for_redrive() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("one")).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(50))
            .unwrap()
            .remove(0);
        let router = router_with_dead_letter(store.clone());
        let error = anyhow::Error::new(std::io::Error::from_raw_os_error(Errno::EMFILE as i32))
            .context("spawn fkst-framework");

        retry_record(
            &store,
            &router,
            Some(&policy(1)),
            &leased,
            DeliveryFailure::from_spawn_error(&error),
            &SupervisorJournal::disabled(),
        );

        let dead = store.get_dead("one").unwrap().unwrap();
        assert!(dead.replayable);
        assert!(!dead.permanent);
        assert!(dead.record.is_some());

        let result = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::ZERO,
                },
                u64::MAX,
                1,
            )
            .unwrap();
        assert_eq!(result.redriven.len(), 1);
        assert_eq!(result.redriven[0].delivery_id, "one");
    }

    #[test]
    fn exhausted_enoent_spawn_failure_is_permanent() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        store.enqueue(&record("one")).unwrap();
        let leased = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(50))
            .unwrap()
            .remove(0);
        let router = router_with_dead_letter(store.clone());
        let error = anyhow::Error::new(std::io::Error::from_raw_os_error(Errno::ENOENT as i32))
            .context("spawn fkst-framework");

        retry_record(
            &store,
            &router,
            Some(&policy(1)),
            &leased,
            DeliveryFailure::from_spawn_error(&error),
            &SupervisorJournal::disabled(),
        );

        let dead = store.get_dead("one").unwrap().unwrap();
        assert!(!dead.replayable);
        assert!(dead.permanent);
        assert!(dead.record.is_none());

        let result = store
            .redrive_due(
                &RedrivePolicy {
                    max_redrives: 3,
                    cooldown: Duration::ZERO,
                },
                u64::MAX,
                1,
            )
            .unwrap();
        assert!(result.redriven.is_empty());
    }

    #[test]
    fn raised_to_reliable_queue_without_source_ref_returns_error() {
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
                published_seam: Vec::new(),
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
            },
        };
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store), None);
        let raised = authenticated_raised_stdout(
            "trusted-token",
            serde_json::json!([
                {"queue": "next", "payload": {"n": 2}}
            ]),
        );

        let mut parent = record("parent");
        parent.source = None;
        let err = publish_raised(&router, &raised, Some("trusted-token"), &parent).unwrap_err();

        assert!(err.to_string().contains("requires source_ref"), "{err}");
    }

    #[test]
    fn raised_replay_to_reliable_queue_is_idempotent() {
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
                published_seam: Vec::new(),
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
            },
        };
        let temp = TempDir::new().unwrap();
        let store = Arc::new(DeliveryStore::open(temp.path().join("delivery.redb")).unwrap());
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()), None);
        let parent = record("parent");
        let raised = authenticated_raised_stdout(
            "trusted-token",
            serde_json::json!([
                {"queue": "next", "payload": {"n": 2}}
            ]),
        );

        publish_raised(&router, &raised, Some("trusted-token"), &parent).unwrap();
        publish_raised(&router, &raised, Some("trusted-token"), &parent).unwrap();

        let leased = store
            .lease_for_dept(
                "next_worker",
                now_unix_millis(),
                8,
                Duration::from_millis(50),
            )
            .unwrap();
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].payload, serde_json::json!({"n": 2}));
    }

    #[test]
    fn lease_excluding_running_delivery_prevents_same_process_duplicate_dispatch() {
        let temp = TempDir::new().unwrap();
        let store = DeliveryStore::open(temp.path().join("delivery.redb")).unwrap();
        store.enqueue(&record("one")).unwrap();
        let first = store
            .lease_for_dept("worker", now_unix_millis(), 8, Duration::from_millis(1))
            .unwrap()
            .remove(0);
        std::thread::sleep(Duration::from_millis(3));
        let excluded = [first.delivery_id.clone()].into_iter().collect();

        let leased = store
            .lease_for_dept_excluding(
                "worker",
                now_unix_millis(),
                8,
                Duration::from_millis(50),
                &excluded,
            )
            .unwrap();

        assert!(leased.is_empty());
    }
}

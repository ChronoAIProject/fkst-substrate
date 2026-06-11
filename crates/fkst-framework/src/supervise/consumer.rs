//! Per-department consumer task.

use super::delivery_router::{now_unix_millis, DeliveryRouter, DerivedDelivery, PublishEnvelope};
use super::delivery_store::{DeliveryStore, RetryFailure, RetryOutcome};
use super::delivery_types::{
    DeadRecord, DeliveryRecord, RedrivePolicy, RetryPolicy, SourceKind, SourceRef,
};
use super::event_fanout::Fanout;
use super::raised::parse_raised;
use super::source_runner::parse_duration;
use super::spawner::{spawn_framework, SpawnResult};
use crate::path_resolver::PackageRoots;
use fkst_common::config::{DepartmentDecl, RetryDecl};
use fkst_common::{Event, RuntimeKind};
use std::collections::BTreeMap;
use std::path::PathBuf;
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
                        &complete_tx,
                        &mut running,
                    );
                }
                _ = tick.tick(), if !reliable_queues.is_empty() => {
                    renew_running(&name, store.as_deref(), stall_window, &running);
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
                        &complete_tx,
                        &mut running,
                    );
                }
                maybe_done = complete_rx.recv(), if !running.is_empty() => {
                    if let Some(done) = maybe_done {
                        running.remove(&done.record.delivery_id);
                        finish_durable_record(&name, store.as_deref(), &router, retry_policy.as_ref(), done);
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
) {
    let args = match spawn_args(
        decl,
        project_root,
        roots,
        framework_binary,
        log_dir,
        event,
        stall_window,
        codex_permit_slots,
    ) {
        Ok(args) => args,
        Err(err) => {
            error!(dept = %name, error = %err, "build spawn args failed");
            return;
        }
    };
    let dept_name = name.to_string();
    let router = router.clone();
    tokio::spawn(async move {
        match spawn_and_report(&dept_name, &args).await {
            Ok(result) => {
                if let Err(err) = publish_ephemeral_raised(&router, &result.stdout) {
                    error!(dept = %dept_name, error = %err, "publish raised failed");
                }
            }
            Err(err) => {
                error!(
                    dept = %dept_name,
                    log_dir = %args.log_dir.display(),
                    error = %err,
                    "framework spawn error"
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
    complete_tx: &mpsc::Sender<CompletedDelivery>,
    running: &mut BTreeMap<String, RunningDelivery>,
) {
    let Some(store) = store else {
        error!(dept = %name, "reliable consumer missing delivery store");
        return;
    };
    let lease = retry_lease(stall_window);
    let excluded = running.keys().cloned().collect();
    let leased = match store.lease_for_dept_excluding(
        name,
        now_unix_millis(),
        DISPATCH_BATCH,
        lease,
        &excluded,
    ) {
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
        let args = match spawn_args(
            decl,
            project_root,
            roots,
            framework_binary,
            log_dir,
            event_from_record(&record),
            stall_window,
            codex_permit_slots,
        ) {
            Ok(args) => args,
            Err(err) => {
                retry_record(
                    &store,
                    router,
                    retry_policy,
                    &record,
                    format!("build spawn args: {err}"),
                );
                continue;
            }
        };
        let dept_name = name.to_string();
        let router = router.clone();
        let complete_tx = complete_tx.clone();
        let running_record = record.clone();
        let delivery_id = record.delivery_id.clone();
        let handle = tokio::spawn(async move {
            let result = run_durable_record(&dept_name, &router, record, args).await;
            if complete_tx.send(result).await.is_err() {
                warn!(dept = %dept_name, delivery_id = %delivery_id, "delivery completion receiver closed");
            }
        });
        running.insert(
            running_record.delivery_id.clone(),
            RunningDelivery {
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
) -> CompletedDelivery {
    let result = spawn_and_report(dept_name, &args).await;
    let failure = match result {
        Ok(result) if result.exit_code == 0 => {
            match publish_raised(router, &result.stdout, &record) {
                Ok(()) => None,
                Err(err) => Some(format!("raised publish error: {err}")),
            }
        }
        Ok(result) => Some(format!(
            "exit={} stderr={}",
            result.exit_code, result.stderr
        )),
        Err(err) => {
            error!(
                dept = %dept_name,
                log_dir = %args.log_dir.display(),
                error = %err,
                "framework spawn error"
            );
            Some(format!("spawn error: {err}"))
        }
    };

    CompletedDelivery { record, failure }
}

fn finish_durable_record(
    dept_name: &str,
    store: Option<&DeliveryStore>,
    router: &DeliveryRouter,
    retry_policy: Option<&RetryPolicy>,
    done: CompletedDelivery,
) {
    let Some(store) = store else {
        error!(dept = %dept_name, "reliable consumer missing delivery store");
        return;
    };
    if let Some(error) = done.failure {
        retry_record(store, router, retry_policy, &done.record, error);
        return;
    }

    match store.ack(&done.record.delivery_id, done.record.lease_generation) {
        Ok(true) => {
            info!(
                dept = %dept_name,
                delivery_id = %done.record.delivery_id,
                generation = done.record.lease_generation,
                "delivery acked"
            );
        }
        Ok(false) => {
            warn!(
                dept = %dept_name,
                delivery_id = %done.record.delivery_id,
                generation = done.record.lease_generation,
                "delivery ack stale or missing"
            );
        }
        Err(err) => {
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

fn renew_running(
    dept_name: &str,
    store: Option<&DeliveryStore>,
    stall_window: Duration,
    running: &BTreeMap<String, RunningDelivery>,
) {
    let Some(store) = store else {
        error!(dept = %dept_name, "reliable consumer missing delivery store");
        return;
    };
    let lease_until = now_unix_millis().saturating_add(duration_millis(retry_lease(stall_window)));
    for running_delivery in running.values() {
        if running_delivery.handle.is_finished() {
            continue;
        }
        match store.renew_lease(
            &running_delivery.record.delivery_id,
            running_delivery.record.lease_generation,
            lease_until,
        ) {
            Ok(true) => {}
            Ok(false) => {
                warn!(
                    dept = %dept_name,
                    delivery_id = %running_delivery.record.delivery_id,
                    generation = running_delivery.record.lease_generation,
                    "delivery lease renewal stale or missing"
                );
            }
            Err(err) => {
                error!(
                    dept = %dept_name,
                    delivery_id = %running_delivery.record.delivery_id,
                    generation = running_delivery.record.lease_generation,
                    error = %err,
                    "delivery lease renewal failed"
                );
            }
        }
    }
}

fn retry_record(
    store: &DeliveryStore,
    router: &DeliveryRouter,
    retry_policy: Option<&RetryPolicy>,
    record: &DeliveryRecord,
    error: String,
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
        return;
    };
    match store.retry(
        &record.delivery_id,
        record.lease_generation,
        &RetryFailure {
            message: error,
            replayable: false,
        },
        policy,
        now_unix_millis(),
    ) {
        Ok(RetryOutcome::DeadPendingRedrive) => {}
        Ok(RetryOutcome::PermanentDead) => match store.get_dead(&record.delivery_id) {
            Ok(Some(dead)) => publish_permanent_dead_letter(router, &dead),
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
        Ok(RetryOutcome::Scheduled | RetryOutcome::Stale | RetryOutcome::Missing) => {}
        Err(err) => {
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
    match store.redrive_due(&policy, now_unix_millis(), DISPATCH_BATCH) {
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
    let event = Event::new(
        queue,
        serde_json::json!({
            "delivery_id": dead.delivery_id,
            "queue": dead.queue,
            "dept": dead.dept,
            "attempt": dead.attempts,
            "redrive_count": dead.redrive_count,
            "error": dead.error_excerpt,
        }),
    );
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

fn publish_raised(
    router: &DeliveryRouter,
    stdout: &str,
    parent: &DeliveryRecord,
) -> anyhow::Result<()> {
    for (ordinal, mut raised_ev) in parse_raised(stdout).into_iter().enumerate() {
        raised_ev.ts = parent.observed_at_ms;
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

fn publish_ephemeral_raised(router: &DeliveryRouter, stdout: &str) -> anyhow::Result<()> {
    for raised_ev in parse_raised(stdout) {
        router.publish(PublishEnvelope {
            event: raised_ev,
            source: None,
            cron_payload: None,
            derived: None,
        })?;
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
        owner_namespace: decl.owner_namespace.clone(),
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
    owner_namespace: String,
}

async fn spawn_and_report(dept_name: &str, args: &SpawnArgs) -> anyhow::Result<SpawnResult> {
    let result = spawn_framework(
        &args.framework_bin,
        &args.lua_full,
        &args.project_root,
        &args.graph_package_roots,
        &args.owner_namespace,
        &args.event_json,
        args.codex_permit_slots,
        dept_name,
        &args.log_dir,
    )
    .await?;

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
    record: DeliveryRecord,
    handle: JoinHandle<()>,
}

struct CompletedDelivery {
    record: DeliveryRecord,
    failure: Option<String>,
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
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

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

    fn router_with_dead_letter(store: Arc<DeliveryStore>) -> DeliveryRouter {
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
                ephemeral: vec!["dead_letter".to_string()],
                stall_window: "30s".to_string(),
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
        DeliveryRouter::new(&cfg, Fanout::new(), Some(store))
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
        let roots =
            PackageRoots::resolve(&host, vec![github_devloop.clone(), consensus.clone()]).unwrap();
        let decl = DepartmentDecl {
            lua: "departments/producer/main.lua".into(),
            owner_root: github_devloop.canonicalize().unwrap(),
            owner_namespace: "github-devloop".to_string(),
            consumes: vec!["github-devloop.tick".to_string()],
            produces: vec!["consensus.proposal".to_string()],
            ephemeral: vec!["github-devloop.tick".to_string()],
            stall_window: "30s".to_string(),
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
        )
        .unwrap();

        assert_eq!(args.graph_package_roots, roots.package_roots());
        assert_eq!(args.owner_namespace, "github-devloop");
    }

    #[test]
    fn spawn_args_keeps_folded_single_package_root_form() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path()).unwrap();
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let owner_namespace = package_namespace(temp.path());
        let decl = DepartmentDecl {
            lua: "departments/producer/main.lua".into(),
            owner_root: temp.path().canonicalize().unwrap(),
            owner_namespace: owner_namespace.clone(),
            consumes: vec!["tick".to_string()],
            produces: vec!["done".to_string()],
            ephemeral: vec!["tick".to_string()],
            stall_window: "30s".to_string(),
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
        )
        .unwrap();

        assert_eq!(
            args.graph_package_roots,
            vec![temp.path().canonicalize().unwrap()]
        );
        assert_eq!(args.owner_namespace, owner_namespace);
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
            "failure".to_string(),
        );

        assert!(store.get("one").unwrap().is_none());
        assert!(store.get_dead("one").unwrap().is_some());
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
                ephemeral: Vec::new(),
                stall_window: "30s".to_string(),
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
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store));
        let stdout = format!(
            "RAISED: {}\n",
            base64::engine::general_purpose::URL_SAFE.encode(
                serde_json::to_vec(&serde_json::json!([
                    {"queue": "next", "payload": {"n": 2}}
                ]))
                .unwrap()
            )
        );

        let mut parent = record("parent");
        parent.source = None;
        let err = publish_raised(&router, &stdout, &parent).unwrap_err();

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
                ephemeral: Vec::new(),
                stall_window: "30s".to_string(),
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
        let router = DeliveryRouter::new(&cfg, Fanout::new(), Some(store.clone()));
        let parent = record("parent");
        let stdout = format!(
            "RAISED: {}\n",
            base64::engine::general_purpose::URL_SAFE.encode(
                serde_json::to_vec(&serde_json::json!([
                    {"queue": "next", "payload": {"n": 2}}
                ]))
                .unwrap()
            )
        );

        publish_raised(&router, &stdout, &parent).unwrap();
        publish_raised(&router, &stdout, &parent).unwrap();

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

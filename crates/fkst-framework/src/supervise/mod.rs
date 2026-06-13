use anyhow::Result;
use fkst_common::config::{Config, RaiserDecl};
use fkst_common::validation::validate;
use fkst_common::DurableLayout;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::path_resolver::PackageRoots;

mod consumer;
mod delivery_index;
mod delivery_retry;
mod delivery_router;
pub(crate) mod delivery_store;
mod delivery_transition;
pub(crate) mod delivery_types;
mod delivery_watch;
pub mod event_fanout;
pub(crate) mod failure_fact;
pub(crate) mod graph_scan;
mod journal;
mod raised;
pub(crate) mod source_runner;
mod spawner;

use crate::config_registry::{ConfigContext, ConfigKey};
use crate::process_tree::ProcessGroupRegistry;
use consumer::spawn_consumer;
use delivery_router::DeliveryRouter;
use delivery_store::DeliveryStore;
use event_fanout::Fanout;
use failure_fact::schema_validation_failure_fact;
use journal::SupervisorJournal;
use source_runner::{spawn_cron, spawn_file_watch};

pub(crate) fn load_host_graph_for_conformance(roots: &PackageRoots) -> Result<Config> {
    graph_scan::load_roots(roots)
}

pub async fn supervise(roots: PackageRoots, framework_bin: PathBuf) -> Result<()> {
    let project_root = roots.host_root().to_path_buf();
    let journal = SupervisorJournal::open();
    info!(
        package_roots = ?roots.package_roots(),
        host_root = %project_root.display(),
        "scanning graph from package roots and host root"
    );
    journal.event(
        "startup",
        [
            ("host_root", project_root.display().to_string()),
            ("package_roots", path_list(roots.package_roots())),
            ("framework_bin", framework_bin.display().to_string()),
            (
                "runtime_root",
                std::env::var(fkst_common::runtime_layout::RUNTIME_ROOT_ENV)
                    .unwrap_or_else(|_| "unset".into()),
            ),
            (
                "durable_root_set",
                std::env::var_os(fkst_common::DURABLE_ROOT_ENV)
                    .is_some()
                    .to_string(),
            ),
            (
                "package_roots_env_set",
                std::env::var_os("FKST_PACKAGE_ROOTS").is_some().to_string(),
            ),
            (
                "package_root_env_set",
                std::env::var_os("FKST_PACKAGE_ROOT").is_some().to_string(),
            ),
        ],
    );

    let cfg = graph_scan::load_roots(&roots).map_err(|e| {
        error!(error = %e, "graph scan failed");
        journal.event("startup_failed", [("reason", format!("graph_scan:{e}"))]);
        e
    })?;

    let schema_warnings = validate(&cfg, &project_root).map_err(|e| {
        let message = e.to_string();
        error!(error = %message, "schema validation failed, refusing to start");
        journal.event(
            "startup_failed",
            [("reason", format!("schema_validation:{message}"))],
        );
        publish_startup_validation_failure(&cfg, &message);
        anyhow::Error::msg(message)
    })?;
    for warning in schema_warnings {
        warn!(warning = %warning, "schema validation warning");
    }
    info!("schema validation passed");
    let config_context = ConfigContext::from_host_root(&project_root)?;
    let rate_pools = crate::rate_pool::RatePoolRegistry::from_config(&config_context)?;
    if !rate_pools.is_empty() {
        let framework_for_shim = std::env::current_exe().unwrap_or_else(|_| framework_bin.clone());
        let shim_dir = crate::rate_shim::ensure_rate_shims(&rate_pools, &framework_for_shim)?;
        info!(shim_dir = %shim_dir.display(), "rate pool shims ensured");
    }

    let fanout = Fanout::new();
    let delivery_store = delivery_store_for_config(&cfg)?;
    let router = DeliveryRouter::new(
        &cfg,
        fanout.clone(),
        delivery_store.clone(),
        Some(journal.clone()),
    );
    delivery_watch::set_failure_fact_publisher(router.failure_fact_publisher());
    let codex_permit_slots = cfg.limits.global_codex_processes;
    let max_in_flight_per_dept =
        config_context.resolved_positive_usize(ConfigKey::MaxInFlightPerDept)?;
    let durable_admission_burst_per_dept =
        config_context.resolved_positive_usize(ConfigKey::DurableAdmissionBurstPerDept)?;
    let process_groups = ProcessGroupRegistry::default();
    let mut handles = vec![];

    let mut departments = cfg.department.iter().collect::<Vec<_>>();
    departments.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, decl) in departments {
        let q_cap = decl
            .consumes
            .iter()
            .map(|q| {
                cfg.queue.get(q).map(|qd| qd.capacity).ok_or_else(|| {
                    anyhow::anyhow!("department `{name}` consumes undeclared queue `{q}`")
                })
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .ok_or_else(|| anyhow::anyhow!("department `{name}` has no consumed queue"))?;
        handles.push(
            spawn_consumer(
                name.clone(),
                decl.clone(),
                project_root.clone(),
                roots.clone(),
                framework_bin.clone(),
                fanout.clone(),
                router.clone(),
                delivery_store.clone(),
                q_cap,
                codex_permit_slots,
                max_in_flight_per_dept,
                durable_admission_burst_per_dept,
                process_groups.clone(),
                journal.clone(),
            )
            .await,
        );
    }

    // dispatch stays tied to
    // `RaiserDecl`'s implemented source kinds; unsupported types fail at parse time.
    let mut raisers = cfg.raiser.iter().collect::<Vec<_>>();
    raisers.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, raiser) in raisers {
        match raiser {
            RaiserDecl::Cron { interval, produces } => {
                handles.push(spawn_cron(
                    name.clone(),
                    interval,
                    produces.clone(),
                    router.clone(),
                    journal.clone(),
                )?);
            }
            RaiserDecl::FileWatch { glob, produces } => {
                handles.push(spawn_file_watch(
                    name.clone(),
                    glob,
                    &project_root,
                    produces.clone(),
                    router.clone(),
                    journal.clone(),
                )?);
            }
        }
    }

    info!(handles = handles.len(), "event runtime running");
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = sigint.recv() => {
            warn!("event runtime received SIGINT");
            journal.event("signal_received", [("signal", "SIGINT".to_string())]);
            journal.event("shutdown_initiated", [("reason", "signal:SIGINT".to_string())]);
        }
        _ = sigterm.recv() => {
            warn!("event runtime received SIGTERM");
            journal.event("signal_received", [("signal", "SIGTERM".to_string())]);
            journal.event("shutdown_initiated", [("reason", "signal:SIGTERM".to_string())]);
        }
    }
    shutdown_runtime(handles, &process_groups, &journal).await;
    Ok(())
}

async fn shutdown_runtime(
    handles: Vec<tokio::task::JoinHandle<()>>,
    process_groups: &ProcessGroupRegistry,
    journal: &SupervisorJournal,
) {
    journal.event(
        "teardown_step",
        [("step", "terminate_process_groups".to_string())],
    );
    process_groups.terminate_all("department").await;
    journal.event(
        "teardown_step",
        [("step", "abort_runtime_tasks".to_string())],
    );
    for handle in handles {
        handle.abort();
    }
    journal.event("teardown_step", [("step", "complete".to_string())]);
}

fn delivery_store_for_config(cfg: &Config) -> Result<Option<Arc<DeliveryStore>>> {
    if !DeliveryRouter::has_reliable_subscriptions(cfg) {
        return Ok(None);
    }
    let durable = DurableLayout::from_env().map_err(|e| {
        error!(error = %e, "durable layout required for reliable subscriptions");
        e
    })?;
    Ok(Some(Arc::new(DeliveryStore::open(
        durable.delivery_db_path(),
    )?)))
}

fn publish_startup_validation_failure(cfg: &Config, error: &str) {
    let fact = schema_validation_failure_fact(error);
    let fanout = Fanout::new();
    let store = match delivery_store_for_config(cfg) {
        Ok(store) => store,
        Err(err) => {
            warn!(error = %err, "startup failure fact store unavailable");
            None
        }
    };
    let router = DeliveryRouter::new(cfg, fanout, store, None);
    if let Err(err) = router.publish_failure_fact(fact.clone()) {
        warn!(error = %err, "startup failure fact publish failed");
    }
    error!(queue = %fact.queue, payload = %fact.payload, "engine failure fact");
}

fn path_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fkst_common::config::{DepartmentDecl, LimitsDecl, QueueDecl};
    use fkst_common::DURABLE_ROOT_ENV;
    use std::collections::BTreeMap;
    use std::process::Command;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use tokio::sync::oneshot;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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
            },
        }
    }

    struct EnvGuard {
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn unset() -> Self {
            let old = std::env::var_os(DURABLE_ROOT_ENV);
            std::env::remove_var(DURABLE_ROOT_ENV);
            Self { old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(old) = self.old.take() {
                std::env::set_var(DURABLE_ROOT_ENV, old);
            } else {
                std::env::remove_var(DURABLE_ROOT_ENV);
            }
        }
    }

    #[test]
    fn reliable_subscription_without_durable_root_fails_closed() {
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _guard = EnvGuard::unset();

        let err = match delivery_store_for_config(&config(false)) {
            Ok(_) => panic!("reliable config should require durable root"),
            Err(err) => err,
        };

        assert!(format!("{err:#}").contains("FKST_DURABLE_ROOT must be set"));
    }

    #[test]
    fn pure_ephemeral_config_does_not_require_durable_root() {
        let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let _guard = EnvGuard::unset();

        assert!(delivery_store_for_config(&config(true)).unwrap().is_none());
    }

    #[tokio::test]
    async fn shutdown_terminates_process_groups_before_aborting_tasks() {
        use std::os::unix::process::CommandExt;

        let process_groups = ProcessGroupRegistry::default();
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 60")
            .process_group(0)
            .spawn()
            .unwrap();
        let child_pid = child.id();
        let (registered_tx, registered_rx) = oneshot::channel();
        let process_groups_for_task = process_groups.clone();
        let handle = tokio::spawn(async move {
            let _registration = process_groups_for_task.register(child_pid);
            let _ = registered_tx.send(());
            std::future::pending::<()>().await;
        });
        registered_rx.await.unwrap();

        shutdown_runtime(
            vec![handle],
            &process_groups,
            &SupervisorJournal::disabled(),
        )
        .await;

        assert!(
            wait_for_child_exit(&mut child, Duration::from_secs(5)),
            "process group registration was dropped before termination"
        );
    }

    fn wait_for_child_exit(child: &mut std::process::Child, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

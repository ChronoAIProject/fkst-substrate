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
mod raised;
pub(crate) mod source_runner;
mod spawner;

use consumer::spawn_consumer;
use delivery_router::DeliveryRouter;
use delivery_store::DeliveryStore;
use event_fanout::Fanout;
use failure_fact::schema_validation_failure_fact;
use source_runner::{spawn_cron, spawn_file_watch};

pub(crate) fn load_host_graph_for_conformance(roots: &PackageRoots) -> Result<Config> {
    graph_scan::load_roots(roots)
}

pub async fn supervise(roots: PackageRoots, framework_bin: PathBuf) -> Result<()> {
    let project_root = roots.host_root().to_path_buf();
    info!(
        package_roots = ?roots.package_roots(),
        host_root = %project_root.display(),
        "scanning graph from package roots and host root"
    );

    let cfg = graph_scan::load_roots(&roots).map_err(|e| {
        error!(error = %e, "graph scan failed");
        e
    })?;

    let schema_warnings = validate(&cfg, &project_root).map_err(|e| {
        let message = e.to_string();
        error!(error = %message, "schema validation failed, refusing to start");
        publish_startup_validation_failure(&cfg, &message);
        anyhow::Error::msg(message)
    })?;
    for warning in schema_warnings {
        warn!(warning = %warning, "schema validation warning");
    }
    info!("schema validation passed");
    let config_context = crate::config_registry::ConfigContext::from_host_root(&project_root)?;
    let rate_pools = crate::rate_pool::RatePoolRegistry::from_config(&config_context)?;
    if !rate_pools.is_empty() {
        let framework_for_shim = std::env::current_exe().unwrap_or_else(|_| framework_bin.clone());
        let shim_dir = crate::rate_shim::ensure_rate_shims(&rate_pools, &framework_for_shim)?;
        info!(shim_dir = %shim_dir.display(), "rate pool shims ensured");
    }

    let fanout = Fanout::new();
    let delivery_store = delivery_store_for_config(&cfg)?;
    let router = DeliveryRouter::new(&cfg, fanout.clone(), delivery_store.clone());
    delivery_watch::set_failure_fact_publisher(router.failure_fact_publisher());
    let codex_permit_slots = cfg.limits.global_codex_processes;
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
                )?);
            }
            RaiserDecl::FileWatch { glob, produces } => {
                handles.push(spawn_file_watch(
                    name.clone(),
                    glob,
                    &project_root,
                    produces.clone(),
                    router.clone(),
                )?);
            }
        }
    }

    info!(handles = handles.len(), "event runtime running");
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
    for handle in handles {
        handle.abort();
    }
    Ok(())
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
    let router = DeliveryRouter::new(cfg, fanout, store);
    if let Err(err) = router.publish_failure_fact(fact.clone()) {
        warn!(error = %err, "startup failure fact publish failed");
    }
    error!(queue = %fact.queue, payload = %fact.payload, "engine failure fact");
}

#[cfg(test)]
mod tests {
    use super::*;
    use fkst_common::config::{DepartmentDecl, LimitsDecl, QueueDecl};
    use fkst_common::DURABLE_ROOT_ENV;
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};

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
}

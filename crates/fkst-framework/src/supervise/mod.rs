use anyhow::Result;
use fkst_common::config::{Config, RaiserDecl};
use fkst_common::validation::validate;
use std::path::PathBuf;
use tracing::{error, info, warn};

use crate::path_resolver::PackageRoots;

mod consumer;
pub mod event_fanout;
mod graph_scan;
mod raised;
mod source_runner;
mod spawner;

use consumer::spawn_consumer;
use event_fanout::Fanout;
use source_runner::{spawn_cron, spawn_file_watch};

pub(crate) fn load_host_graph_for_conformance(roots: &PackageRoots) -> Result<Config> {
    graph_scan::load_roots(roots)
}

pub async fn supervise(roots: PackageRoots, framework_bin: PathBuf) -> Result<()> {
    let project_root = roots.host_root().to_path_buf();
    let package_root = roots.package_root().to_path_buf();
    info!(
        package_root = %package_root.display(),
        host_root = %project_root.display(),
        "scanning graph from package root and host root"
    );

    let cfg = graph_scan::load_roots(&roots).map_err(|e| {
        error!(error = %e, "graph scan failed");
        e
    })?;

    let schema_warnings = validate(&cfg, &project_root).map_err(|e| {
        error!(error = %e, "schema validation failed, refusing to start");
        anyhow::Error::msg(e.to_string())
    })?;
    for warning in schema_warnings {
        warn!(warning = %warning, "schema validation warning");
    }
    info!("schema validation passed");

    let fanout = Fanout::new();
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
                package_root.clone(),
                framework_bin.clone(),
                fanout.clone(),
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
                    fanout.clone(),
                )?);
            }
            RaiserDecl::FileWatch { glob, produces } => {
                handles.push(spawn_file_watch(
                    name.clone(),
                    glob,
                    &project_root,
                    produces.clone(),
                    fanout.clone(),
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

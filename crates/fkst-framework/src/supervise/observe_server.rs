use super::delivery_store::{DeliveryObserveOptions, DeliveryStore};
use anyhow::{Context, Result};
use fkst_common::DurableLayout;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::warn;

use crate::observe::{ObserveSocketRequest, ObserveSocketResponse};

const MAX_OBSERVE_LIMIT: usize = 10_000;

#[derive(Clone, Default)]
pub(crate) struct ObserveEndpoint {
    durable_root: PathBuf,
    database: PathBuf,
    socket: PathBuf,
}

pub(crate) fn endpoint_for_layout(layout: &DurableLayout) -> ObserveEndpoint {
    ObserveEndpoint {
        durable_root: layout.durable_root().to_path_buf(),
        database: layout.delivery_db_path(),
        socket: crate::observe::socket_path(layout),
    }
}

pub(crate) fn spawn_observe_server(
    endpoint: ObserveEndpoint,
    store: Arc<DeliveryStore>,
) -> Result<tokio::task::JoinHandle<()>> {
    if let Some(parent) = endpoint.socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create observe socket directory `{}`", parent.display()))?;
    }
    remove_stale_socket(&endpoint.socket)?;
    let listener = UnixListener::bind(&endpoint.socket)
        .with_context(|| format!("bind observe socket `{}`", endpoint.socket.display()))?;
    let socket = endpoint.socket.clone();
    Ok(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let store = store.clone();
                    let endpoint = endpoint.clone();
                    tokio::spawn(async move {
                        if let Err(err) = serve_connection(stream, store, endpoint).await {
                            warn!(error = %err, "observe request failed");
                        }
                    });
                }
                Err(err) => {
                    warn!(error = %err, "observe socket accept failed");
                    break;
                }
            }
        }
        let _ = std::fs::remove_file(socket);
    }))
}

async fn serve_connection(
    mut stream: UnixStream,
    store: Arc<DeliveryStore>,
    endpoint: ObserveEndpoint,
) -> Result<()> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut line)
            .await
            .context("read observe request")?;
    }
    let response = match serde_json::from_str::<ObserveSocketRequest>(&line) {
        Ok(request) => match store.observe_snapshot(
            &endpoint.durable_root,
            &endpoint.database,
            &DeliveryObserveOptions {
                now_ms: request.now_ms,
                limit: request.limit.clamp(1, MAX_OBSERVE_LIMIT),
            },
        ) {
            Ok(snapshot) => ObserveSocketResponse::Ok { snapshot },
            Err(err) => ObserveSocketResponse::Err {
                error: err.to_string(),
            },
        },
        Err(err) => ObserveSocketResponse::Err {
            error: format!("decode observe request: {err}"),
        },
    };
    let mut body = serde_json::to_vec(&response)?;
    body.push(b'\n');
    stream
        .write_all(&body)
        .await
        .context("write observe response")?;
    stream
        .shutdown()
        .await
        .context("shutdown observe response")?;
    Ok(())
}

fn remove_stale_socket(socket: &Path) -> Result<()> {
    match std::fs::remove_file(socket) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("remove stale observe socket `{}`", socket.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe;
    use crate::supervise::delivery_types::DeliveryRecord;
    use tempfile::TempDir;

    fn record(id: &str) -> DeliveryRecord {
        DeliveryRecord {
            delivery_id: id.to_string(),
            queue: "jobs".to_string(),
            dept: "worker".to_string(),
            payload: serde_json::json!({"schema": "test.job", "dedup_key": id}),
            source: None,
            cron_payload: None,
            observed_at_ms: 10,
            attempt: 0,
            redrive_count: 0,
            collapse_by_dedup_id: false,
            lease_generation: 0,
            lease_until_ms: None,
            not_before_ms: 10,
            last_error_excerpt: None,
        }
    }

    #[tokio::test]
    async fn observe_socket_serves_snapshot_while_store_handle_is_open() {
        let temp = TempDir::new().unwrap();
        let layout = DurableLayout::new(temp.path()).unwrap();
        let database = layout.delivery_db_path();
        let store = Arc::new(DeliveryStore::open(&database).unwrap());
        store.enqueue(&record("one")).unwrap();
        let handle = spawn_observe_server(endpoint_for_layout(&layout), store.clone()).unwrap();

        match DeliveryStore::open_existing(&database) {
            Ok(_) => panic!("second redb open should fail while owner handle is open"),
            Err(_) => {}
        };

        let snapshot =
            tokio::task::spawn_blocking(move || observe::request_live_snapshot(&layout, 10))
                .await
                .unwrap()
                .unwrap()
                .expect("live owner process should answer observe request");

        assert_eq!(snapshot.deliveries.len(), 1);
        assert_eq!(snapshot.deliveries[0].delivery_id, "one");
        assert_eq!(snapshot.queues[0].queue, "jobs");

        handle.abort();
    }
}

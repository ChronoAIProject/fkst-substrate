use super::delivery_observe::{observe_lineage, observe_snapshot, DeliveryObserveOptions};
use super::delivery_store::DeliveryStore;
use anyhow::{Context, Result};
use fkst_common::DurableLayout;
use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::warn;

use crate::observe::{ObserveSocketRequest, ObserveSocketResponse, MAX_LIMIT};

const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

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
    current_subscriber_queues: BTreeSet<String>,
) -> Result<tokio::task::JoinHandle<()>> {
    if let Some(parent) = endpoint.socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create observe socket directory `{}`", parent.display()))?;
    }
    remove_stale_socket(&endpoint.socket)?;
    let listener = UnixListener::bind(&endpoint.socket)
        .with_context(|| format!("bind observe socket `{}`", endpoint.socket.display()))?;
    let listener = Arc::new(listener);
    Ok(tokio::spawn(serve_observe_requests(
        move || {
            let listener = listener.clone();
            async move { listener.accept().await.map(|(stream, _)| stream) }
        },
        store,
        endpoint,
        current_subscriber_queues,
        ACCEPT_RETRY_DELAY,
    )))
}

async fn serve_observe_requests<F, Fut>(
    mut accept: F,
    store: Arc<DeliveryStore>,
    endpoint: ObserveEndpoint,
    current_subscriber_queues: BTreeSet<String>,
    retry_delay: Duration,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<UnixStream>>,
{
    loop {
        let stream = accept_with_retry(&mut accept, retry_delay).await;
        let store = store.clone();
        let endpoint = endpoint.clone();
        let current_subscriber_queues = current_subscriber_queues.clone();
        tokio::spawn(async move {
            if let Err(err) =
                serve_connection(stream, store, endpoint, current_subscriber_queues).await
            {
                warn!(error = %err, "observe request failed");
            }
        });
    }
}

async fn accept_with_retry<T, F, Fut>(accept: &mut F, retry_delay: Duration) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    loop {
        match accept().await {
            Ok(value) => return value,
            Err(err) => {
                warn!(
                    error = %err,
                    retry_delay_ms = retry_delay.as_millis() as u64,
                    "observe socket accept failed; retrying"
                );
                tokio::time::sleep(retry_delay).await;
            }
        }
    }
}

async fn serve_connection(
    mut stream: UnixStream,
    store: Arc<DeliveryStore>,
    endpoint: ObserveEndpoint,
    current_subscriber_queues: BTreeSet<String>,
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
        Ok(request) => {
            if let Some(lineage) = request.lineage {
                match observe_lineage(
                    &store,
                    &lineage.queue,
                    &lineage.dept,
                    &lineage.source_ref,
                    request.now_ms,
                ) {
                    Ok(lineage) => ObserveSocketResponse::LineageOk { lineage },
                    Err(err) => ObserveSocketResponse::Err {
                        error: err.to_string(),
                    },
                }
            } else {
                match crate::observe::validate_dead_letter_page(
                    request.page.as_ref(),
                    request.since.as_deref(),
                ) {
                    Ok(dead_letter_page) => match observe_snapshot(
                        &store,
                        &endpoint.durable_root,
                        &endpoint.database,
                        &DeliveryObserveOptions {
                            now_ms: request.now_ms,
                            limit: request.limit.clamp(1, MAX_LIMIT),
                            since: request.since,
                            dead_letter_page,
                            current_subscriber_queues: Some(current_subscriber_queues),
                            projection: request.projection,
                        },
                    ) {
                        Ok(snapshot) => ObserveSocketResponse::Ok { snapshot },
                        Err(err) => ObserveSocketResponse::Err {
                            error: err.to_string(),
                        },
                    },
                    Err(err) => ObserveSocketResponse::Err {
                        error: err.to_string(),
                    },
                }
            }
        }
        Err(err) => ObserveSocketResponse::Err {
            error: format!("decode observe request: {err}"),
        },
    };
    write_response(&mut stream, &response).await?;
    Ok(())
}

async fn write_response(stream: &mut UnixStream, response: &ObserveSocketResponse) -> Result<()> {
    let mut body = serde_json::to_vec(response)?;
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
    use crate::supervise::delivery_observe::QueueSubscriberStatus;
    use crate::supervise::delivery_types::{DeliveryRecord, SourceKind, SourceRef};
    use std::time::Duration;
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
            pending_dirty: false,
            subscriber_absent_since_ms: None,
            lease_generation: 0,
            lease_until_ms: None,
            not_before_ms: 10,
            last_error_excerpt: None,
        }
    }

    #[tokio::test]
    async fn connection_serves_bounded_lineage_result() {
        let temp = TempDir::new().unwrap();
        let layout = DurableLayout::new(temp.path()).unwrap();
        let database = layout.delivery_db_path();
        let store = Arc::new(DeliveryStore::open(&database).unwrap());
        let source_ref = SourceRef {
            kind: SourceKind::External,
            reference: "owner/repo#issue/42".to_string(),
        };
        let mut delivery = record("one");
        delivery.source = Some(source_ref.clone());
        store.enqueue(&delivery).unwrap();
        let endpoint = endpoint_for_layout(&layout);
        let (server, mut client) = UnixStream::pair().unwrap();
        let server_task = tokio::spawn(serve_connection(
            server,
            store,
            endpoint,
            BTreeSet::from(["jobs".to_string()]),
        ));
        let request = crate::observe::ObserveSocketRequest {
            limit: crate::observe::DEFAULT_LIMIT,
            since: None,
            page: None,
            lineage: Some(crate::observe::LineageObserveRequest {
                queue: "jobs".to_string(),
                dept: "worker".to_string(),
                source_ref,
            }),
            projection: Default::default(),
            now_ms: 100,
        };
        let mut body = serde_json::to_vec(&request).unwrap();
        body.push(b'\n');
        client.write_all(&body).await.unwrap();

        let mut response_line = String::new();
        BufReader::new(&mut client)
            .read_line(&mut response_line)
            .await
            .unwrap();
        server_task.await.unwrap().unwrap();
        let response: crate::observe::ObserveSocketResponse =
            serde_json::from_str(&response_line).unwrap();

        let crate::observe::ObserveSocketResponse::LineageOk { lineage } = response else {
            panic!("lineage request must return lineage_ok");
        };
        assert_eq!(lineage.live_delivery.unwrap().delivery_id, "one");
        assert!(lineage.terminal_dead_letter.is_none());
    }

    #[tokio::test]
    async fn connection_pushes_event_projection_into_bounded_store_read() {
        let temp = TempDir::new().unwrap();
        let layout = DurableLayout::new(temp.path()).unwrap();
        let database = layout.delivery_db_path();
        let store = Arc::new(DeliveryStore::open(&database).unwrap());
        for index in 0..32 {
            store
                .enqueue(&record(&format!("delivery-{index:02}")))
                .unwrap();
        }
        let endpoint = endpoint_for_layout(&layout);
        let (server, mut client) = UnixStream::pair().unwrap();
        DeliveryStore::reset_observation_record_read_counts();
        let server_task = tokio::spawn(serve_connection(
            server,
            store,
            endpoint,
            BTreeSet::from(["jobs".to_string()]),
        ));
        let request = crate::observe::ObserveSocketRequest {
            limit: 1,
            since: None,
            page: None,
            lineage: None,
            projection: crate::supervise::delivery_store::DeliveryObservationProjection {
                queue_aggregates: false,
                deliveries: true,
                dead_letters: false,
            },
            now_ms: 100,
        };
        let mut body = serde_json::to_vec(&request).unwrap();
        body.push(b'\n');
        client.write_all(&body).await.unwrap();

        let mut response_line = String::new();
        BufReader::new(&mut client)
            .read_line(&mut response_line)
            .await
            .unwrap();
        server_task.await.unwrap().unwrap();
        let response: crate::observe::ObserveSocketResponse =
            serde_json::from_str(&response_line).unwrap();
        let crate::observe::ObserveSocketResponse::Ok { snapshot } = response else {
            panic!("snapshot request must return ok");
        };

        assert!(snapshot.queues.is_empty());
        assert_eq!(snapshot.deliveries.len(), 1);
        assert!(snapshot.dead_letters.is_empty());
        assert_eq!(DeliveryStore::observation_record_read_counts(), (0, 2, 0));
    }

    #[tokio::test]
    async fn accept_failure_does_not_break_live_observation() {
        let temp = TempDir::new().unwrap();
        let layout = DurableLayout::new(temp.path()).unwrap();
        let database = layout.delivery_db_path();
        let store = Arc::new(DeliveryStore::open(&database).unwrap());
        store.enqueue(&record("one")).unwrap();
        let endpoint = endpoint_for_layout(&layout);
        remove_stale_socket(&endpoint.socket).unwrap();
        let listener = match UnixListener::bind(&endpoint.socket) {
            Ok(listener) => Arc::new(listener),
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    && endpoint.socket.starts_with("/tmp") =>
            {
                return;
            }
            Err(err) => panic!(
                "bind observe socket `{}` failed: {err}",
                endpoint.socket.display()
            ),
        };
        let socket = endpoint.socket.clone();
        let mut inject_failure = true;
        let handle = tokio::spawn(serve_observe_requests(
            move || {
                let listener = listener.clone();
                let fail = std::mem::take(&mut inject_failure);
                async move {
                    if fail {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionAborted,
                            "injected accept failure",
                        ))
                    } else {
                        listener.accept().await.map(|(stream, _)| stream)
                    }
                }
            },
            store.clone(),
            endpoint,
            BTreeSet::from(["jobs".to_string()]),
            Duration::ZERO,
        ));

        match DeliveryStore::open_existing(&database) {
            Ok(_) => panic!("second redb open should fail while owner handle is open"),
            Err(_) => {}
        };

        let first_root = temp.path().to_path_buf();
        let first = tokio::task::spawn_blocking(move || {
            observe::snapshot_for_durable_root(
                first_root,
                &observe::ObserveSnapshotOptions {
                    limit: 10,
                    since: None,
                    page: None,
                    projection: Default::default(),
                },
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.deliveries.len(), 1);
        assert_eq!(first.deliveries[0].delivery_id, "one");

        store.enqueue(&record("two")).unwrap();
        let second_root = temp.path().to_path_buf();
        let second = tokio::task::spawn_blocking(move || {
            observe::snapshot_for_durable_root(
                second_root,
                &observe::ObserveSnapshotOptions {
                    limit: 10,
                    since: None,
                    page: None,
                    projection: Default::default(),
                },
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(second.deliveries.len(), 2);
        assert_eq!(second.deliveries[0].delivery_id, "one");
        assert_eq!(second.deliveries[1].delivery_id, "two");

        handle.abort();
        let _ = handle.await;
        let _ = std::fs::remove_file(socket);
    }

    #[tokio::test]
    async fn observe_socket_serves_snapshot_while_store_handle_is_open() {
        let temp = TempDir::new().unwrap();
        let layout = DurableLayout::new(temp.path()).unwrap();
        let database = layout.delivery_db_path();
        let store = Arc::new(DeliveryStore::open(&database).unwrap());
        store.enqueue(&record("one")).unwrap();
        let handle = match spawn_observe_server(
            endpoint_for_layout(&layout),
            store.clone(),
            BTreeSet::from(["jobs".to_string()]),
        ) {
            Ok(handle) => handle,
            Err(err)
                if format!("{err:#}").contains("Operation not permitted")
                    && crate::observe::socket_path(&layout).starts_with("/tmp") =>
            {
                return;
            }
            Err(err) => panic!("spawn observe server failed: {err:#}"),
        };

        match DeliveryStore::open_existing(&database) {
            Ok(_) => panic!("second redb open should fail while owner handle is open"),
            Err(_) => {}
        };

        let snapshot = tokio::task::spawn_blocking(move || {
            observe::request_live_snapshot(
                &layout,
                &observe::ObserveSnapshotOptions {
                    limit: 10,
                    since: None,
                    page: None,
                    projection: Default::default(),
                },
            )
        })
        .await
        .unwrap()
        .unwrap()
        .expect("live owner process should answer observe request");

        assert_eq!(snapshot.deliveries.len(), 1);
        assert_eq!(snapshot.deliveries[0].delivery_id, "one");
        assert_eq!(snapshot.queues[0].queue, "jobs");
        assert_eq!(
            snapshot.queues[0].subscriber_status,
            QueueSubscriberStatus::Current
        );
        assert_eq!(snapshot.queues[0].has_current_subscriber, Some(true));

        let page_layout = DurableLayout::new(temp.path()).unwrap();
        let page_snapshot = tokio::task::spawn_blocking(move || {
            observe::request_live_snapshot(
                &page_layout,
                &observe::ObserveSnapshotOptions {
                    limit: 10,
                    since: None,
                    page: Some(observe::DeadLetterPageRequest {
                        section: "dead_letters".to_string(),
                        after: None,
                    }),
                    projection: Default::default(),
                },
            )
        })
        .await
        .unwrap()
        .unwrap()
        .expect("live owner process should answer paged observe request");

        let page = page_snapshot.page.expect("page metadata must be present");
        assert_eq!(page.section, "dead_letters");
        assert_eq!(page.next, None);

        handle.abort();
    }
}

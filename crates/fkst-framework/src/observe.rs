use crate::supervise::delivery_observe::{
    observe_lineage, observe_snapshot, DeliveryObserveOptions, DeliveryObserveSnapshot,
    LineageObserveResult,
};
use crate::supervise::delivery_store::DeliveryStore;
use crate::supervise::delivery_types::SourceRef;
use anyhow::{Context, Result};
use base64::Engine;
use fkst_common::{validate_runtime_key, DurableLayout};
use serde::{Deserialize, Serialize};
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const DEFAULT_LIMIT: usize = 500;
pub(crate) const MAX_LIMIT: usize = 10_000;

#[derive(Clone, Debug)]
pub(crate) struct ObserveOptions {
    pub(crate) durable_root: PathBuf,
    pub(crate) json: bool,
    pub(crate) limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObserveSnapshotOptions {
    pub(crate) limit: usize,
    pub(crate) since: Option<String>,
    pub(crate) page: Option<DeadLetterPageRequest>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LineageObserveRequest {
    pub(crate) queue: String,
    pub(crate) dept: String,
    pub(crate) source_ref: SourceRef,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeadLetterPageRequest {
    pub(crate) section: String,
    #[serde(default)]
    pub(crate) after: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeadLetterPageOptions {
    pub(crate) after: Option<DeadLetterPageCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeadLetterPageCursor {
    pub(crate) dead_at_ms: u64,
    pub(crate) delivery_id: String,
}

pub(crate) fn parse_args(args: &[String]) -> Result<ObserveOptions> {
    let mut durable_root: Option<PathBuf> = None;
    let mut json = false;
    let mut limit = DEFAULT_LIMIT;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--durable-root" => {
                if durable_root.is_some() {
                    anyhow::bail!("duplicate --durable-root");
                }
                i += 1;
                durable_root = Some(next_value(args, i, "--durable-root")?.into());
            }
            "--json" => json = true,
            "--limit" => {
                i += 1;
                let raw = next_value(args, i, "--limit")?;
                limit = raw
                    .parse::<usize>()
                    .with_context(|| format!("invalid --limit value `{raw}`"))?;
                if limit == 0 || limit > MAX_LIMIT {
                    anyhow::bail!("--limit must be between 1 and {MAX_LIMIT}");
                }
            }
            other => anyhow::bail!("unknown observe argument: {}", other),
        }
        i += 1;
    }
    Ok(ObserveOptions {
        durable_root: durable_root.ok_or_else(|| anyhow::anyhow!("missing --durable-root"))?,
        json,
        limit,
    })
}

pub(crate) fn run(options: ObserveOptions) -> Result<i32> {
    let snapshot = snapshot_for_durable_root(
        &options.durable_root,
        &ObserveSnapshotOptions {
            limit: options.limit,
            since: None,
            page: None,
        },
    )?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        print_human(&snapshot);
    }
    Ok(0)
}

pub(crate) fn snapshot_for_durable_root(
    durable_root: impl Into<PathBuf>,
    options: &ObserveSnapshotOptions,
) -> Result<DeliveryObserveSnapshot> {
    let limit = validate_limit(options.limit)?;
    validate_since(options.since.as_deref())?;
    let dead_letter_page =
        validate_dead_letter_page(options.page.as_ref(), options.since.as_deref())?;
    let durable_root = durable_root.into();
    let layout = DurableLayout::new(&durable_root)?;
    let database = layout.delivery_db_path();
    let snapshot = match request_live_snapshot(
        &layout,
        &ObserveSnapshotOptions {
            limit,
            since: options.since.clone(),
            page: options.page.clone(),
        },
    )? {
        Some(snapshot) => Ok(snapshot),
        None => {
            let store = open_offline_store(&layout, &database)?;
            observe_snapshot(
                &store,
                layout.durable_root(),
                &database,
                &DeliveryObserveOptions {
                    now_ms: now_ms(),
                    limit,
                    since: options.since.clone(),
                    dead_letter_page,
                    current_subscriber_queues: None,
                },
            )
        }
    }?;
    Ok(snapshot)
}

pub(crate) fn lineage_for_durable_root(
    durable_root: impl Into<PathBuf>,
    lineage: &LineageObserveRequest,
) -> Result<LineageObserveResult> {
    let durable_root = durable_root.into();
    let layout = DurableLayout::new(&durable_root)?;
    let database = layout.delivery_db_path();
    match request_live_lineage(&layout, lineage)? {
        Some(result) => Ok(result),
        None => {
            let store = open_offline_store(&layout, &database)?;
            observe_lineage(
                &store,
                &lineage.queue,
                &lineage.dept,
                &lineage.source_ref,
                now_ms(),
            )
        }
    }
}

fn open_offline_store(layout: &DurableLayout, database: &Path) -> Result<DeliveryStore> {
    match DeliveryStore::open_existing(database) {
        Ok(store) => Ok(store),
        Err(err) if is_database_already_open(&err) => Err(err).with_context(|| {
            format!(
                "observe-live-owner-unavailable: live observe socket `{}` is unavailable while durable delivery database `{}` remains exclusively owned; restart the database-owning `supervise` process to restore the live endpoint, or stop that process before offline inspection",
                socket_path(layout).display(),
                database.display()
            )
        }),
        Err(err) => Err(err),
    }
}

fn is_database_already_open(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<redb::DatabaseError>()
            .is_some_and(|err| matches!(err, redb::DatabaseError::DatabaseAlreadyOpen))
    })
}

pub(crate) fn socket_path(layout: &DurableLayout) -> PathBuf {
    layout.observe_socket_path()
}

pub(crate) fn request_live_snapshot(
    layout: &DurableLayout,
    options: &ObserveSnapshotOptions,
) -> Result<Option<DeliveryObserveSnapshot>> {
    let request = ObserveSocketRequest {
        limit: options.limit,
        since: options.since.clone(),
        page: options.page.clone(),
        lineage: None,
        now_ms: now_ms(),
    };
    let Some(response) = request_live_observe(layout, &request)? else {
        return Ok(None);
    };
    match response {
        ObserveSocketResponse::Ok { snapshot } => Ok(Some(snapshot)),
        ObserveSocketResponse::LineageOk { .. } => {
            anyhow::bail!("live observe socket returned lineage result for snapshot request")
        }
        ObserveSocketResponse::Err { error } => {
            anyhow::bail!("live observe socket failed: {error}")
        }
    }
}

pub(crate) fn request_live_lineage(
    layout: &DurableLayout,
    lineage: &LineageObserveRequest,
) -> Result<Option<LineageObserveResult>> {
    let request = ObserveSocketRequest {
        limit: DEFAULT_LIMIT,
        since: None,
        page: None,
        lineage: Some(lineage.clone()),
        now_ms: now_ms(),
    };
    let Some(response) = request_live_observe(layout, &request)? else {
        return Ok(None);
    };
    match response {
        ObserveSocketResponse::LineageOk { lineage } => Ok(Some(lineage)),
        ObserveSocketResponse::Ok { .. } => {
            anyhow::bail!("live observe socket returned snapshot for lineage request")
        }
        ObserveSocketResponse::Err { error } => {
            anyhow::bail!("live observe socket failed: {error}")
        }
    }
}

fn request_live_observe(
    layout: &DurableLayout,
    request: &ObserveSocketRequest,
) -> Result<Option<ObserveSocketResponse>> {
    let path = socket_path(layout);
    if !path.exists() {
        return Ok(None);
    }
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(err) if is_absent_socket_error(&err) => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("connect live observe socket `{}`", path.display()))
        }
    };
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut response = String::new();
    BufReader::new(stream).read_to_string(&mut response)?;
    if response.trim().is_empty() {
        anyhow::bail!("live observe socket returned an empty response");
    }
    let response: ObserveSocketResponse = serde_json::from_str(&response).with_context(|| {
        format!(
            "decode live observe socket response from `{}`",
            path.display()
        )
    })?;
    Ok(Some(response))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ObserveSocketRequest {
    pub(crate) limit: usize,
    #[serde(default)]
    pub(crate) since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) page: Option<DeadLetterPageRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lineage: Option<LineageObserveRequest>,
    pub(crate) now_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ObserveSocketResponse {
    Ok { snapshot: DeliveryObserveSnapshot },
    LineageOk { lineage: LineageObserveResult },
    Err { error: String },
}

fn is_absent_socket_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

fn print_human(snapshot: &DeliveryObserveSnapshot) {
    println!(
        "durable_root={} database={} generated_at_ms={}",
        snapshot.source.durable_root, snapshot.source.database, snapshot.generated_at_ms
    );
    println!("{}", snapshot.source.history_semantics);
    println!("queues");
    if snapshot.queues.is_empty() {
        println!("  none");
    } else {
        for queue in &snapshot.queues {
            let oldest = queue
                .oldest_pending_age_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "  queue={} depth={} pending={} in_flight={} retrying={} oldest_pending_age_ms={} subscriber_status={}",
                queue.queue,
                queue.depth,
                queue.pending,
                queue.in_flight,
                queue.retrying,
                oldest,
                queue.subscriber_status
            );
        }
    }

    println!("deliveries");
    if snapshot.deliveries.is_empty() {
        println!("  none");
    } else {
        for delivery in &snapshot.deliveries {
            println!(
                "  id={} queue={} dept={} status={:?} attempt={} lease_generation={} not_before_ms={} digest={}",
                delivery.delivery_id,
                delivery.queue,
                delivery.dept,
                delivery.status,
                delivery.attempt,
                delivery.lease_generation,
                delivery.not_before_ms,
                delivery.payload.digest
            );
        }
    }

    println!("dead_letters");
    if snapshot.dead_letters.is_empty() {
        println!("  none");
    } else {
        for dead in &snapshot.dead_letters {
            println!(
                "  id={} queue={} dept={} attempts={} permanent={} replayable={} dead_at_ms={} digest={}",
                dead.delivery_id,
                dead.queue,
                dead.dept,
                dead.attempts,
                dead.permanent,
                dead.replayable,
                dead.dead_at_ms,
                dead.payload.digest
            );
        }
    }

    if snapshot.truncated.deliveries || snapshot.truncated.dead_letters {
        println!(
            "truncated deliveries={} dead_letters={}",
            snapshot.truncated.deliveries, snapshot.truncated.dead_letters
        );
    }
}

fn next_value(args: &[String], index: usize, flag: &str) -> Result<String> {
    args.get(index)
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing {} value", flag))
}

pub(crate) fn validate_limit(limit: usize) -> Result<usize> {
    if limit == 0 || limit > MAX_LIMIT {
        anyhow::bail!("observe limit must be between 1 and {MAX_LIMIT}");
    }
    Ok(limit)
}

pub(crate) fn validate_since(since: Option<&str>) -> Result<()> {
    if since.is_some_and(str::is_empty) {
        anyhow::bail!("observe since cursor must not be empty");
    }
    Ok(())
}

pub(crate) fn validate_dead_letter_page(
    page: Option<&DeadLetterPageRequest>,
    since: Option<&str>,
) -> Result<Option<DeadLetterPageOptions>> {
    let Some(page) = page else {
        return Ok(None);
    };
    if page.section != "dead_letters" {
        anyhow::bail!("fkst.observe page section must be `dead_letters`");
    }
    if since.is_some() {
        anyhow::bail!("fkst.observe page cannot be combined with since");
    }
    let after = page
        .after
        .as_deref()
        .map(decode_dead_letter_cursor)
        .transpose()?;
    Ok(Some(DeadLetterPageOptions { after }))
}

pub(crate) fn encode_dead_letter_cursor(cursor: &DeadLetterPageCursor) -> Result<String> {
    let envelope = DeadLetterCursorEnvelope {
        version: 1,
        section: "dead_letters".to_string(),
        dead_at_ms: cursor.dead_at_ms,
        delivery_id: cursor.delivery_id.clone(),
    };
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope)?))
}

fn decode_dead_letter_cursor(encoded: &str) -> Result<DeadLetterPageCursor> {
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|err| anyhow::anyhow!("observe dead-letter cursor invalid: base64: {err}"))?;
    let envelope: DeadLetterCursorEnvelope = serde_json::from_slice(&decoded)
        .map_err(|err| anyhow::anyhow!("observe dead-letter cursor invalid: json: {err}"))?;
    if envelope.version != 1 {
        anyhow::bail!(
            "observe dead-letter cursor invalid: unsupported version {}",
            envelope.version
        );
    }
    if envelope.section != "dead_letters" {
        anyhow::bail!("observe dead-letter cursor invalid: section mismatch");
    }
    validate_runtime_key(&envelope.delivery_id)
        .map_err(|err| anyhow::anyhow!("observe dead-letter cursor invalid: delivery_id: {err}"))?;
    Ok(DeadLetterPageCursor {
        dead_at_ms: envelope.dead_at_ms,
        delivery_id: envelope.delivery_id,
    })
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeadLetterCursorEnvelope {
    version: u32,
    section: String,
    dead_at_ms: u64,
    delivery_id: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::delivery_store::{RetryFailure, RetryOutcome};
    use crate::supervise::delivery_types::{DeliveryRecord, RetryPolicy, SourceKind, SourceRef};
    use std::time::Duration;
    use tempfile::TempDir;

    fn cursor_fixture(value: serde_json::Value) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap())
    }

    #[test]
    fn parse_requires_durable_root() {
        let err = parse_args(&[]).unwrap_err();
        assert!(format!("{err:#}").contains("missing --durable-root"));
    }

    #[test]
    fn parse_rejects_unbounded_limit() {
        let err = parse_args(&[
            "--durable-root".to_string(),
            "/tmp/fkst-durable".to_string(),
            "--limit".to_string(),
            "10001".to_string(),
        ])
        .unwrap_err();
        assert!(format!("{err:#}").contains("--limit must be between"));
    }

    #[test]
    fn snapshot_options_reject_empty_since_cursor() {
        let err = snapshot_for_durable_root(
            "/tmp/fkst-durable",
            &ObserveSnapshotOptions {
                limit: DEFAULT_LIMIT,
                since: Some(String::new()),
                page: None,
            },
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("observe since cursor must not be empty"));
    }

    #[test]
    fn dead_letter_cursor_rejects_invalid_envelope() {
        for cursor in [
            cursor_fixture(serde_json::json!({
                "version": 2,
                "section": "dead_letters",
                "dead_at_ms": 10,
                "delivery_id": "dead-one"
            })),
            cursor_fixture(serde_json::json!({
                "version": 1,
                "section": "deliveries",
                "dead_at_ms": 10,
                "delivery_id": "dead-one"
            })),
            cursor_fixture(serde_json::json!({
                "version": 1,
                "section": "dead_letters",
                "dead_at_ms": 10,
                "delivery_id": "bad key"
            })),
        ] {
            let err = decode_dead_letter_cursor(&cursor).unwrap_err();
            assert!(format!("{err:#}").contains("observe dead-letter cursor invalid"));
        }
    }

    #[test]
    fn non_page_socket_request_serialization_is_unchanged() {
        let request = ObserveSocketRequest {
            limit: DEFAULT_LIMIT,
            since: None,
            page: None,
            lineage: None,
            now_ms: 1,
        };

        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"limit":500,"since":null,"now_ms":1}"#
        );
    }

    #[test]
    fn offline_lineage_observe_reads_bounded_store_result() {
        let temp = TempDir::new().unwrap();
        let layout = DurableLayout::new(temp.path()).unwrap();
        let store = DeliveryStore::open(layout.delivery_db_path()).unwrap();
        let source_ref = SourceRef {
            kind: SourceKind::External,
            reference: "owner/repo#issue/42".to_string(),
        };
        let record = |delivery_id: &str| DeliveryRecord {
            delivery_id: delivery_id.to_string(),
            queue: "target.queue".to_string(),
            dept: "target-dept".to_string(),
            payload: serde_json::json!({"issue": 42}),
            source: Some(source_ref.clone()),
            cron_payload: None,
            observed_at_ms: 10,
            attempt: 0,
            redrive_count: 0,
            collapse_by_dedup_id: false,
            pending_dirty: false,
            subscriber_absent_since_ms: None,
            lease_generation: 0,
            lease_until_ms: None,
            not_before_ms: 100,
            last_error_excerpt: None,
        };

        store.enqueue(&record("terminal-one")).unwrap();
        let leased = store
            .lease(100, 1, Duration::from_secs(10))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            store
                .retry(
                    &leased.delivery_id,
                    leased.lease_generation,
                    &RetryFailure {
                        message: "terminal failure".to_string(),
                        replayable: false,
                    },
                    &RetryPolicy {
                        max_attempts: 1,
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(1),
                    },
                    200,
                )
                .unwrap(),
            RetryOutcome::PermanentDead
        );
        store.enqueue(&record("live-one")).unwrap();
        drop(store);

        let result = lineage_for_durable_root(
            temp.path(),
            &LineageObserveRequest {
                queue: "target.queue".to_string(),
                dept: "target-dept".to_string(),
                source_ref,
            },
        )
        .unwrap();

        assert_eq!(result.live_delivery.unwrap().delivery_id, "live-one");
        assert_eq!(
            result.terminal_dead_letter.unwrap().delivery_id,
            "terminal-one"
        );
    }
}

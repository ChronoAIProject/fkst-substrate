use crate::supervise::delivery_observe::{
    observe_snapshot, DeliveryObserveOptions, DeliveryObserveSnapshot,
};
use crate::supervise::delivery_store::DeliveryStore;
use anyhow::{Context, Result};
use fkst_common::DurableLayout;
use serde::{Deserialize, Serialize};
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 10_000;

#[derive(Clone, Debug)]
pub(crate) struct ObserveOptions {
    pub(crate) durable_root: PathBuf,
    pub(crate) json: bool,
    pub(crate) limit: usize,
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
    let layout = DurableLayout::new(&options.durable_root)?;
    let database = layout.delivery_db_path();
    let snapshot = match request_live_snapshot(&layout, options.limit)? {
        Some(snapshot) => Ok(snapshot),
        None => {
            let store = DeliveryStore::open_existing(&database)?;
            observe_snapshot(
                &store,
                layout.durable_root(),
                &database,
                &DeliveryObserveOptions {
                    now_ms: now_ms(),
                    limit: options.limit,
                },
            )
        }
    }?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        print_human(&snapshot);
    }
    Ok(0)
}

pub(crate) fn socket_path(layout: &DurableLayout) -> PathBuf {
    layout.observe_socket_path()
}

pub(crate) fn request_live_snapshot(
    layout: &DurableLayout,
    limit: usize,
) -> Result<Option<DeliveryObserveSnapshot>> {
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
    let request = ObserveSocketRequest {
        limit,
        now_ms: now_ms(),
    };
    serde_json::to_writer(&mut stream, &request)?;
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
    match response {
        ObserveSocketResponse::Ok { snapshot } => Ok(Some(snapshot)),
        ObserveSocketResponse::Err { error } => {
            anyhow::bail!("live observe socket failed: {error}")
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ObserveSocketRequest {
    pub(crate) limit: usize,
    pub(crate) now_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ObserveSocketResponse {
    Ok { snapshot: DeliveryObserveSnapshot },
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
                "  queue={} depth={} pending={} in_flight={} retrying={} oldest_pending_age_ms={}",
                queue.queue, queue.depth, queue.pending, queue.in_flight, queue.retrying, oldest
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
}

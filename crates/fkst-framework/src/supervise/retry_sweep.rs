//! Retry scratch sweeper. It re-injects due retry records without owning facts.

use super::event_fanout::Fanout;
use super::retry_state::{self, RetryRecord};
use fkst_common::{validate_runtime_key, Event, RuntimeKind, RuntimeLayout};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

pub fn spawn_retry_sweeper(layout: RuntimeLayout, fanout: Fanout) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            retry_dir = %layout.runtime_dir(RuntimeKind::Retry).display(),
            "retry sweeper starting"
        );
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(err) = sweep_once(&layout, &fanout) {
                warn!(error = %err, "retry sweep failed");
            }
        }
    })
}

pub(crate) fn sweep_once(layout: &RuntimeLayout, fanout: &Fanout) -> anyhow::Result<usize> {
    let retry_dir = layout.runtime_dir(RuntimeKind::Retry);
    let mut sent = 0;
    for path in retry_state::retry_files(&retry_dir) {
        let Some(key) = retry_state::key_for_path(&retry_dir, &path) else {
            continue;
        };
        if validate_runtime_key(&key).is_err() {
            warn!(key = %key, path = %path.display(), "retry record key is invalid");
            continue;
        }
        let record = match retry_state::read_retry_record(&path) {
            Ok(record) => record,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "retry record parse failed");
                continue;
            }
        };
        if now_unix_millis() < record.due_at {
            continue;
        }

        let lock_file =
            retry_state::lock_reliable_key(&layout.runtime_dir(RuntimeKind::Locks), &key)?;
        if retry_state::marker_exists(layout, &key) {
            retry_state::remove_file_best_effort(&path);
            drop(lock_file);
            continue;
        }
        let record = match read_retry_record_after_lock(&path) {
            RetryRead::Record(record) => record,
            RetryRead::Missing => {
                drop(lock_file);
                continue;
            }
            RetryRead::Invalid => {
                drop(lock_file);
                continue;
            }
        };
        if now_unix_millis() < record.due_at {
            drop(lock_file);
            continue;
        }
        let queue = record.queue.clone();
        fanout.send(&queue, Event::new(queue.clone(), record.payload))?;
        sent += 1;
        drop(lock_file);
    }
    Ok(sent)
}

enum RetryRead {
    Record(RetryRecord),
    Missing,
    Invalid,
}

fn read_retry_record_after_lock(path: &Path) -> RetryRead {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return RetryRead::Missing,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "retry record read failed");
            return RetryRead::Invalid;
        }
    };
    match serde_json::from_str(&body) {
        Ok(record) => RetryRead::Record(record),
        Err(err) => {
            warn!(path = %path.display(), error = %err, "retry record parse failed");
            RetryRead::Invalid
        }
    }
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::retry_state::write_json_atomic;
    use std::sync::mpsc;
    use std::thread;
    use tempfile::TempDir;
    use tokio::time::{timeout, Duration};

    fn record(queue: &str, due_at: u64) -> RetryRecord {
        RetryRecord {
            queue: queue.to_string(),
            payload: serde_json::json!({"id": 7}),
            dept: "worker".to_string(),
            dedup_key: "jobs/one".to_string(),
            attempt: 1,
            generation: 1,
            due_at,
            last_error_excerpt: "temporary".to_string(),
        }
    }

    #[tokio::test]
    async fn retry_sweeper_reinjects_due_record_to_original_queue() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let target = layout.runtime_dir(RuntimeKind::Retry).join("jobs/one");
        write_json_atomic(&target, &record("input", 0)).unwrap();
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("input", 8).await;

        let sent = sweep_once(&layout, &fanout).unwrap();

        assert_eq!(sent, 1);
        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.queue, "input");
        assert_eq!(event.payload, serde_json::json!({"id": 7}));
    }

    #[tokio::test]
    async fn retry_sweeper_deletes_record_when_marker_exists() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let target = layout.runtime_dir(RuntimeKind::Retry).join("jobs/one");
        write_json_atomic(&target, &record("input", 0)).unwrap();
        std::fs::create_dir_all(layout.runtime_dir(RuntimeKind::Marks).join("jobs")).unwrap();
        std::fs::write(
            layout.runtime_dir(RuntimeKind::Marks).join("jobs/one"),
            "marked",
        )
        .unwrap();
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("input", 8).await;

        let sent = sweep_once(&layout, &fanout).unwrap();

        assert_eq!(sent, 0);
        assert!(!target.exists());
        assert!(timeout(Duration::from_millis(50), rx.recv()).await.is_err());
    }

    #[tokio::test]
    async fn retry_sweeper_skips_record_before_due_at() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let target = layout.runtime_dir(RuntimeKind::Retry).join("jobs/one");
        write_json_atomic(&target, &record("input", now_unix_millis() + 60_000)).unwrap();
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("input", 8).await;

        let sent = sweep_once(&layout, &fanout).unwrap();

        assert_eq!(sent, 0);
        assert!(target.exists());
        assert!(timeout(Duration::from_millis(50), rx.recv()).await.is_err());
    }

    #[tokio::test]
    async fn retry_sweeper_rechecks_due_at_after_lock() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let target = layout.runtime_dir(RuntimeKind::Retry).join("jobs/one");
        write_json_atomic(&target, &record("input", 0)).unwrap();
        let lock_file =
            retry_state::lock_reliable_key(&layout.runtime_dir(RuntimeKind::Locks), "jobs/one")
                .unwrap();
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("input", 8).await;
        let (ready_tx, ready_rx) = mpsc::channel();
        let layout_for_thread = layout.clone();
        let fanout_for_thread = fanout.clone();
        let worker = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            sweep_once(&layout_for_thread, &fanout_for_thread).unwrap()
        });
        ready_rx.recv().unwrap();
        write_json_atomic(&target, &record("input", now_unix_millis() + 60_000)).unwrap();

        drop(lock_file);
        let sent = worker.join().unwrap();

        assert_eq!(sent, 0);
        assert!(target.exists());
        assert!(timeout(Duration::from_millis(50), rx.recv()).await.is_err());
    }

    #[tokio::test]
    async fn retry_sweeper_keeps_attempt_unchanged_after_reinject() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let target = layout.runtime_dir(RuntimeKind::Retry).join("jobs/one");
        let mut retry = record("input", 0);
        retry.attempt = 2;
        write_json_atomic(&target, &retry).unwrap();
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("input", 8).await;

        let sent = sweep_once(&layout, &fanout).unwrap();

        assert_eq!(sent, 1);
        let _ = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let persisted: RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(persisted.attempt, 2);
    }

    #[tokio::test]
    async fn retry_sweeper_reinjects_expired_write_ahead_record_without_incrementing_attempt() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let target = layout.runtime_dir(RuntimeKind::Retry).join("jobs/one");
        let mut retry = record("input", 0);
        retry.attempt = 0;
        retry.last_error_excerpt = String::new();
        write_json_atomic(&target, &retry).unwrap();
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("input", 8).await;

        let sent = sweep_once(&layout, &fanout).unwrap();

        assert_eq!(sent, 1);
        let _ = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let persisted: RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(persisted.attempt, 0);
        assert!(!layout.runtime_dir(RuntimeKind::Dead).exists());
    }

    #[test]
    fn start_decision_writes_lease_record_with_attempt_zero() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let event = Event::new("input", serde_json::json!({"dedup_key": "jobs/one"}));
        let before = retry_state::now_unix_millis();

        let decision =
            retry_state::start_decision(&layout, "worker", &event, Duration::from_secs(35))
                .unwrap();

        assert!(matches!(
            decision,
            retry_state::StartDecision::Run { generation: 1, .. }
        ));
        let target = layout
            .runtime_dir(RuntimeKind::Retry)
            .join("worker/jobs/one");
        let record: RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(target).unwrap()).unwrap();
        assert_eq!(record.attempt, 0);
        assert_eq!(record.generation, 1);
        assert_eq!(record.last_error_excerpt, "");
        assert!(record.due_at >= before + 35_000);
        assert!(record.due_at <= before + 36_000);
    }

    #[tokio::test]
    async fn retry_sweeper_skips_corrupt_record_and_continues() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let bad = layout.runtime_dir(RuntimeKind::Retry).join("jobs/bad");
        let good = layout.runtime_dir(RuntimeKind::Retry).join("jobs/one");
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(&bad, "{not json").unwrap();
        write_json_atomic(&good, &record("input", 0)).unwrap();
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("input", 8).await;

        let sent = sweep_once(&layout, &fanout).unwrap();

        assert_eq!(sent, 1);
        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.queue, "input");
        assert!(bad.exists());
    }
}

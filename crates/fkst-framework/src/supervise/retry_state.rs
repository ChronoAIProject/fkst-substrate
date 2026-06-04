//! Reliable retry scratch state for supervised department execution.

use super::event_fanout::Fanout;
use super::source_runner::parse_duration;
use fkst_common::config::RetryDecl;
use fkst_common::{validate_runtime_key, Event, RuntimeKind, RuntimeLayout};
use nix::fcntl::{flock, FlockArg};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

const ERROR_EXCERPT_LIMIT: usize = 512;
const DEAD_LETTER_QUEUE: &str = "dead_letter";

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RetryRecord {
    pub queue: String,
    pub payload: JsonValue,
    pub dept: String,
    pub dedup_key: String,
    pub attempt: u64,
    pub generation: u64,
    pub due_at: u64,
    pub last_error_excerpt: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DeadRecord {
    pub original_queue: String,
    pub payload: JsonValue,
    pub dept: String,
    pub dedup_key: String,
    pub attempts: u64,
    pub last_error_excerpt: String,
    pub failed_at: String,
}

#[derive(Clone, Debug)]
pub(crate) struct RetryPolicy {
    pub max_attempts: u64,
    pub base: Duration,
    pub cap: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct ReliableKey {
    key: String,
}

impl ReliableKey {
    pub(crate) fn as_str(&self) -> &str {
        &self.key
    }
}

pub(crate) enum StartDecision {
    Run { key: ReliableKey, generation: u64 },
    SkipMarked(ReliableKey),
    SkipPending(ReliableKey),
    RunUntracked,
}

#[derive(Debug)]
pub(crate) enum StartDecisionError {
    InvalidDedupKey(anyhow::Error),
    State(anyhow::Error),
}

impl std::fmt::Display for StartDecisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDedupKey(err) | Self::State(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for StartDecisionError {}

pub(crate) enum CompletionStatus {
    Success,
    Failure { error: String },
}

pub(crate) fn policy_from_decl(decl: &RetryDecl) -> anyhow::Result<RetryPolicy> {
    let base = parse_duration(&decl.base)?;
    let cap = parse_duration(&decl.cap)?;
    Ok(RetryPolicy {
        max_attempts: decl.max_attempts,
        base,
        cap,
    })
}

pub(crate) fn reliable_key(dept: &str, dedup_key: &str) -> anyhow::Result<ReliableKey> {
    let dept = sanitize_segment(dept);
    let sanitized_dedup_key = sanitize_runtime_key(dedup_key);
    if sanitized_dedup_key != dedup_key {
        warn!(
            dedup_key = %dedup_key,
            sanitized = %sanitized_dedup_key,
            "reliable_retry dedup_key sanitized lossily"
        );
    }
    let key = format!("{dept}/{sanitized_dedup_key}");
    validate_runtime_key(&key).map_err(|err| anyhow::anyhow!(err.to_string()))?;
    Ok(ReliableKey { key })
}

pub(crate) fn start_decision(
    layout: &RuntimeLayout,
    dept: &str,
    event: &Event,
    lease: Duration,
) -> Result<StartDecision, StartDecisionError> {
    let Some(dedup_key) = event.payload.get("dedup_key").and_then(JsonValue::as_str) else {
        return Ok(StartDecision::RunUntracked);
    };
    let key = reliable_key(dept, dedup_key).map_err(StartDecisionError::InvalidDedupKey)?;
    let lock_file = lock_reliable_key(&layout.runtime_dir(RuntimeKind::Locks), key.as_str())
        .map_err(StartDecisionError::State)?;
    let retry_path = retry_path(layout, &key);
    if marker_path(layout, &key).exists() {
        remove_file_best_effort(&retry_path);
        drop(lock_file);
        return Ok(StartDecision::SkipMarked(key));
    }
    let now = now_unix_millis();
    let existing = read_retry_record_optional(&retry_path).map_err(StartDecisionError::State)?;
    if existing.as_ref().is_some_and(|record| record.due_at > now) {
        drop(lock_file);
        return Ok(StartDecision::SkipPending(key));
    }
    let attempt = existing.as_ref().map(|record| record.attempt).unwrap_or(0);
    let generation = existing
        .as_ref()
        .map(|record| record.generation)
        .unwrap_or(0)
        .saturating_add(1);
    let last_error_excerpt = existing
        .map(|record| record.last_error_excerpt)
        .unwrap_or_default();
    let record = RetryRecord {
        queue: event.queue.clone(),
        payload: event.payload.clone(),
        dept: dept.to_string(),
        dedup_key: key.as_str().to_string(),
        attempt,
        generation,
        due_at: now.saturating_add(lease.as_millis() as u64),
        last_error_excerpt,
    };
    write_json_atomic(&retry_path, &record).map_err(|err| StartDecisionError::State(err.into()))?;
    drop(lock_file);
    Ok(StartDecision::Run { key, generation })
}

pub(crate) fn complete(
    layout: &RuntimeLayout,
    fanout: &Fanout,
    policy: &RetryPolicy,
    dept: &str,
    event: &Event,
    key: &ReliableKey,
    generation: u64,
    status: CompletionStatus,
) -> anyhow::Result<()> {
    let lock_file = lock_reliable_key(&layout.runtime_dir(RuntimeKind::Locks), key.as_str())?;
    let retry = retry_path(layout, key);
    let Some(current) = read_retry_record_optional(&retry)? else {
        drop(lock_file);
        return Ok(());
    };
    if current.generation != generation {
        drop(lock_file);
        return Ok(());
    }

    match status {
        CompletionStatus::Success => {
            let marker = marker_path(layout, key);
            write_marker(&marker, key.as_str())?;
            remove_file_best_effort(&retry);
        }
        CompletionStatus::Failure { error } => {
            let attempt = current.attempt + 1;
            let excerpt = error_excerpt(&error);
            if attempt >= policy.max_attempts {
                let record = DeadRecord {
                    original_queue: event.queue.clone(),
                    payload: event.payload.clone(),
                    dept: dept.to_string(),
                    dedup_key: key.as_str().to_string(),
                    attempts: attempt,
                    last_error_excerpt: excerpt.clone(),
                    failed_at: rfc3339_utc_now(),
                };
                write_json_atomic(&dead_path(layout, key), &record)?;
                remove_file_best_effort(&retry);
                if event.queue != DEAD_LETTER_QUEUE {
                    fanout.send(
                        DEAD_LETTER_QUEUE,
                        Event::new(
                            DEAD_LETTER_QUEUE,
                            serde_json::json!({
                                "dedup_key": record.dedup_key,
                                "dept": record.dept,
                                "original_queue": record.original_queue,
                                "attempts": record.attempts,
                                "last_error_excerpt": record.last_error_excerpt,
                                "failed_at": record.failed_at,
                            }),
                        ),
                    )?;
                }
            } else {
                let delay = backoff_delay(policy.base, policy.cap, attempt);
                let record = RetryRecord {
                    queue: event.queue.clone(),
                    payload: event.payload.clone(),
                    dept: dept.to_string(),
                    dedup_key: key.as_str().to_string(),
                    attempt,
                    generation,
                    due_at: now_unix_millis().saturating_add(delay.as_millis() as u64),
                    last_error_excerpt: excerpt,
                };
                write_json_atomic(&retry, &record)?;
            }
        }
    }
    drop(lock_file);
    Ok(())
}

pub(crate) fn renew_lease(
    layout: &RuntimeLayout,
    key: &ReliableKey,
    generation: u64,
    lease: Duration,
) -> anyhow::Result<bool> {
    let lock_file = lock_reliable_key(&layout.runtime_dir(RuntimeKind::Locks), key.as_str())?;
    let retry = retry_path(layout, key);
    if marker_path(layout, key).exists() {
        remove_file_best_effort(&retry);
        drop(lock_file);
        return Ok(false);
    }
    let Some(mut current) = read_retry_record_optional(&retry)? else {
        drop(lock_file);
        return Ok(false);
    };
    if current.generation != generation {
        drop(lock_file);
        return Ok(false);
    }
    current.due_at = now_unix_millis().saturating_add(lease.as_millis() as u64);
    write_json_atomic(&retry, &current)?;
    drop(lock_file);
    Ok(true)
}

pub(crate) fn backoff_delay(base: Duration, cap: Duration, attempt: u64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(63);
    let multiplier = 1_u128 << exponent;
    let millis = base.as_millis().saturating_mul(multiplier);
    let capped = millis.min(cap.as_millis());
    Duration::from_millis(capped.min(u64::MAX as u128) as u64)
}

pub(crate) fn read_retry_record(path: &Path) -> anyhow::Result<RetryRecord> {
    let body = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&body)?)
}

pub(crate) fn read_retry_record_optional(path: &Path) -> anyhow::Result<Option<RetryRecord>> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(Some(serde_json::from_str(&body)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn retry_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    visit_retry_files(root, &mut files);
    files
}

pub(crate) fn key_for_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(
        rel.components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

pub(crate) fn marker_exists(layout: &RuntimeLayout, key: &str) -> bool {
    layout.runtime_dir(RuntimeKind::Marks).join(key).exists()
}

pub(crate) fn lock_reliable_key(locks: &Path, key: &str) -> anyhow::Result<std::fs::File> {
    let lock_path = locks.join("reliable").join(key);
    let parent = lock_path.parent().ok_or_else(|| {
        anyhow::anyhow!("retry lock target '{}' has no parent", lock_path.display())
    })?;
    std::fs::create_dir_all(parent)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    flock(lock_file.as_raw_fd(), FlockArg::LockExclusive)?;
    Ok(lock_file)
}

pub(crate) fn write_json_atomic<T: Serialize>(target: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    write_atomic(target, &bytes)
}

pub(crate) fn remove_file_best_effort(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(path = %path.display(), error = %err, "retry cleanup remove failed"),
    }
}

pub(crate) fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

pub(crate) fn error_excerpt(error: &str) -> String {
    let mut excerpt = error.replace('\r', "\\r").replace('\n', "\\n");
    if excerpt.len() > ERROR_EXCERPT_LIMIT {
        let boundary = excerpt
            .char_indices()
            .map(|(idx, _)| idx)
            .take_while(|idx| *idx <= ERROR_EXCERPT_LIMIT)
            .last()
            .unwrap_or(0);
        excerpt.truncate(boundary);
    }
    excerpt
}

fn sanitize_runtime_key(raw: &str) -> String {
    raw.split('/')
        .filter(|segment| !segment.is_empty())
        .map(sanitize_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn sanitize_segment(raw: &str) -> String {
    let mut segment = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if segment.is_empty() || segment.bytes().all(|byte| byte == b'.') {
        segment = "-".to_string();
    }
    if segment.len() > 255 {
        let boundary = segment
            .char_indices()
            .map(|(idx, _)| idx)
            .take_while(|idx| *idx <= 255)
            .last()
            .unwrap_or(255);
        segment.truncate(boundary);
    }
    segment
}

fn marker_path(layout: &RuntimeLayout, key: &ReliableKey) -> PathBuf {
    layout.runtime_dir(RuntimeKind::Marks).join(key.as_str())
}

fn retry_path(layout: &RuntimeLayout, key: &ReliableKey) -> PathBuf {
    layout.runtime_dir(RuntimeKind::Retry).join(key.as_str())
}

fn dead_path(layout: &RuntimeLayout, key: &ReliableKey) -> PathBuf {
    layout.runtime_dir(RuntimeKind::Dead).join(key.as_str())
}

fn write_marker(marker: &Path, key: &str) -> std::io::Result<()> {
    let marked_at = rfc3339_utc_now();
    write_atomic(
        marker,
        format!("key={key}\nmarked_at={marked_at}\n").as_bytes(),
    )
}

fn write_atomic(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("target '{}' has no parent", target.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("target '{}' has no file name", target.display()),
            )
        })?;
    let temp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(temp, target)?;
    Ok(())
}

fn rfc3339_utc_now() -> String {
    crate::sdk_log::rfc3339_utc_now()
}

fn visit_retry_files(path: &Path, files: &mut Vec<PathBuf>) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.is_file() {
        files.push(path.to_path_buf());
        return;
    }
    if !metadata.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        visit_retry_files(&entry.path(), files);
    }
}

#[cfg(test)]
mod tests {
    use super::super::event_fanout::Fanout;
    use super::*;
    use fkst_common::RuntimeLayout;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWriter(self.0.clone())
        }
    }

    fn capture_warns(f: impl FnOnce()) -> String {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .finish();

        tracing::subscriber::with_default(subscriber, f);

        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn reliable_key_sanitizes_dept_and_dedup_key() {
        let key = reliable_key("review/dept", "owner/repo#pr#4@2026-06-04").unwrap();

        assert_eq!(key.as_str(), "review-dept/owner/repo-pr-4-2026-06-04");
        validate_runtime_key(key.as_str()).unwrap();
    }

    #[test]
    fn lossy_dedup_key_sanitization_warns() {
        let logs = capture_warns(|| {
            let key = reliable_key("worker", "owner/repo#pr#4@2026").unwrap();
            assert_eq!(key.as_str(), "worker/owner/repo-pr-4-2026");
        });

        assert!(
            logs.contains("reliable_retry dedup_key sanitized lossily"),
            "{logs}"
        );
        assert!(logs.contains("dedup_key=owner/repo#pr#4@2026"), "{logs}");
        assert!(logs.contains("sanitized=owner/repo-pr-4-2026"), "{logs}");
    }

    #[test]
    fn error_excerpt_truncates_on_utf8_boundary() {
        let excerpt = error_excerpt(&"é".repeat(300));

        assert!(excerpt.len() <= ERROR_EXCERPT_LIMIT);
        assert!(excerpt.is_char_boundary(excerpt.len()));
    }

    fn retry_policy(max_attempts: u64) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base: Duration::from_secs(60),
            cap: Duration::from_secs(60),
        }
    }

    #[test]
    fn start_decision_skips_pending_without_refreshing_record() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let event = Event::new("input", serde_json::json!({"dedup_key": "jobs/one"}));
        let first = start_decision(&layout, "worker", &event, Duration::from_secs(60)).unwrap();
        let StartDecision::Run { generation, .. } = first else {
            panic!("expected run");
        };
        let target = layout
            .runtime_dir(RuntimeKind::Retry)
            .join("worker/jobs/one");
        let before: RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();

        let second = start_decision(&layout, "worker", &event, Duration::from_secs(120)).unwrap();

        assert!(matches!(second, StartDecision::SkipPending(_)));
        let after: RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(after.attempt, before.attempt);
        assert_eq!(after.generation, generation);
        assert_eq!(after.due_at, before.due_at);
    }

    #[test]
    fn renew_lease_extends_due_at_without_changing_attempt_or_generation() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let event = Event::new("input", serde_json::json!({"dedup_key": "jobs/one"}));
        let StartDecision::Run { key, generation } =
            start_decision(&layout, "worker", &event, Duration::from_millis(1)).unwrap()
        else {
            panic!("expected run");
        };
        let target = layout
            .runtime_dir(RuntimeKind::Retry)
            .join("worker/jobs/one");
        let before: RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();

        assert!(renew_lease(&layout, &key, generation, Duration::from_secs(60)).unwrap());

        let after: RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(target).unwrap()).unwrap();
        assert_eq!(after.attempt, before.attempt);
        assert_eq!(after.generation, before.generation);
        assert!(after.due_at > before.due_at);
    }

    #[test]
    fn stale_failure_completion_does_not_increment_or_dead_letter() {
        let runtime = TempDir::new().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let fanout = Fanout::new();
        let event = Event::new("input", serde_json::json!({"dedup_key": "jobs/one"}));
        let StartDecision::Run {
            key,
            generation: first_generation,
        } = start_decision(&layout, "worker", &event, Duration::ZERO).unwrap()
        else {
            panic!("expected first run");
        };
        let StartDecision::Run {
            generation: second_generation,
            ..
        } = start_decision(&layout, "worker", &event, Duration::from_secs(60)).unwrap()
        else {
            panic!("expected second run");
        };

        complete(
            &layout,
            &fanout,
            &retry_policy(2),
            "worker",
            &event,
            &key,
            first_generation,
            CompletionStatus::Failure {
                error: "stale failure".to_string(),
            },
        )
        .unwrap();
        complete(
            &layout,
            &fanout,
            &retry_policy(2),
            "worker",
            &event,
            &key,
            second_generation,
            CompletionStatus::Failure {
                error: "current failure".to_string(),
            },
        )
        .unwrap();

        let target = layout
            .runtime_dir(RuntimeKind::Retry)
            .join("worker/jobs/one");
        let record: RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(target).unwrap()).unwrap();
        assert_eq!(record.attempt, 1);
        assert_eq!(record.generation, second_generation);
        assert!(!layout
            .runtime_dir(RuntimeKind::Dead)
            .join("worker/jobs/one")
            .exists());
    }
}

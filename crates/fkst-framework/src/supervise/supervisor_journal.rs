//! Engine-owned supervise event journal.

use anyhow::Result;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

const CODEX_LOG_MAX_AGE_ENV: &str = "FKST_CODEX_LOG_MAX_AGE";
const CODEX_LOG_MAX_BYTES_ENV: &str = "FKST_CODEX_LOG_MAX_BYTES";
const DEFAULT_LOG_MAX_AGE: Duration = Duration::from_secs(48 * 60 * 60);

#[derive(Clone)]
pub(crate) struct SupervisorJournal {
    inner: Option<Arc<Mutex<JournalFile>>>,
}

impl SupervisorJournal {
    pub(crate) fn open(logs_root: &Path) -> Self {
        match JournalFile::open(logs_root) {
            Ok(file) => Self {
                inner: Some(Arc::new(Mutex::new(file))),
            },
            Err(err) => {
                warn!(
                    dir = %logs_root.display(),
                    error = %err,
                    "supervisor journal unavailable"
                );
                Self { inner: None }
            }
        }
    }

    pub(crate) fn disabled() -> Self {
        Self { inner: None }
    }

    pub(crate) fn event(&self, event: &str, fields: &[(&str, String)]) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut line = format!(
            "ts={} event={}",
            unix_millis(SystemTime::now()),
            escape_value(event)
        );
        for (key, value) in fields {
            line.push(' ');
            line.push_str(key);
            line.push('=');
            line.push_str(&escape_value(value));
        }
        line.push('\n');

        let mut file = inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.write_line(&line);
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> Option<PathBuf> {
        let inner = self.inner.as_ref()?;
        let file = inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Some(file.path.clone())
    }
}

struct JournalFile {
    #[cfg(test)]
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl JournalFile {
    fn open(logs_root: &Path) -> Result<Self> {
        let journal_dir = logs_root.join("supervisor");
        std::fs::create_dir_all(&journal_dir)?;
        let path = journal_dir.join(format!("supervisor-{}.log", filename_timestamp()));
        prune_supervisor_logs(
            &journal_dir,
            &path,
            RetentionPolicy::from_env(),
            SystemTime::now(),
        );
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            #[cfg(test)]
            path,
            file: Some(file),
        })
    }

    fn write_line(&mut self, line: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file
            .write_all(line.as_bytes())
            .and_then(|_| file.flush())
            .is_err()
        {
            self.file = None;
        }
    }
}

#[derive(Clone, Copy)]
struct RetentionPolicy {
    max_age: Option<Duration>,
    max_bytes: Option<u64>,
}

impl RetentionPolicy {
    fn from_env() -> Self {
        Self {
            max_age: retention_max_age_from_env(),
            max_bytes: retention_max_bytes_from_env(),
        }
    }
}

struct LogEntry {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

fn prune_supervisor_logs(
    log_dir: &Path,
    current_log_path: &Path,
    policy: RetentionPolicy,
    now: SystemTime,
) {
    if let Err(err) = prune_supervisor_logs_result(log_dir, current_log_path, policy, now) {
        warn!(
            dir = %log_dir.display(),
            current_log = %current_log_path.display(),
            error = %err,
            "supervisor journal prune failed"
        );
    }
}

fn prune_supervisor_logs_result(
    log_dir: &Path,
    current_log_path: &Path,
    policy: RetentionPolicy,
    now: SystemTime,
) -> Result<()> {
    if policy.max_age.is_none() && policy.max_bytes.is_none() {
        return Ok(());
    }
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let current_log_path = current_log_path.to_path_buf();
    let mut retained = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                warn!(dir = %log_dir.display(), error = %err, "supervisor journal prune entry skipped");
                continue;
            }
        };
        let path = entry.path();
        if path == current_log_path || path.extension().and_then(OsStr::to_str) != Some("log") {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "supervisor journal prune metadata skipped");
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        if let Some(max_age) = policy.max_age {
            if now.duration_since(modified).unwrap_or(Duration::ZERO) > max_age {
                remove_supervisor_log(&path);
                continue;
            }
        }
        retained.push(LogEntry {
            path,
            len: metadata.len(),
            modified,
        });
    }
    if let Some(max_bytes) = policy.max_bytes {
        prune_supervisor_logs_by_size(retained, max_bytes);
    }
    Ok(())
}

fn prune_supervisor_logs_by_size(mut entries: Vec<LogEntry>, max_bytes: u64) {
    let mut total = entries
        .iter()
        .fold(0_u64, |sum, entry| sum.saturating_add(entry.len));
    if total <= max_bytes {
        return;
    }
    entries.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    for entry in entries {
        if total <= max_bytes {
            break;
        }
        remove_supervisor_log(&entry.path);
        total = total.saturating_sub(entry.len);
    }
}

fn remove_supervisor_log(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        warn!(path = %path.display(), error = %err, "supervisor journal prune remove failed");
    }
}

fn retention_max_age_from_env() -> Option<Duration> {
    match std::env::var(CODEX_LOG_MAX_AGE_ENV) {
        Ok(raw) => parse_retention_duration(&raw).unwrap_or_else(|err| {
            warn!(env = CODEX_LOG_MAX_AGE_ENV, value = ?raw, error = %err, "supervisor journal retention ignored");
            Some(DEFAULT_LOG_MAX_AGE)
        }),
        Err(std::env::VarError::NotPresent) => Some(DEFAULT_LOG_MAX_AGE),
        Err(std::env::VarError::NotUnicode(_)) => {
            warn!(env = CODEX_LOG_MAX_AGE_ENV, "supervisor journal retention ignored non-unicode value");
            Some(DEFAULT_LOG_MAX_AGE)
        }
    }
}

fn retention_max_bytes_from_env() -> Option<u64> {
    match std::env::var(CODEX_LOG_MAX_BYTES_ENV) {
        Ok(raw) => parse_optional_u64(&raw).unwrap_or_else(|err| {
            warn!(env = CODEX_LOG_MAX_BYTES_ENV, value = ?raw, error = %err, "supervisor journal retention ignored");
            None
        }),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            warn!(env = CODEX_LOG_MAX_BYTES_ENV, "supervisor journal retention ignored non-unicode value");
            None
        }
    }
}

fn parse_retention_duration(raw: &str) -> std::result::Result<Option<Duration>, String> {
    let value = raw.trim();
    if value.is_empty() || value == "0" {
        return Ok(None);
    }
    let (number, multiplier) = match value.as_bytes().last().copied() {
        Some(b's') => (&value[..value.len() - 1], 1_u64),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 60 * 60),
        Some(b'd') => (&value[..value.len() - 1], 24 * 60 * 60),
        Some(byte) if byte.is_ascii_digit() => (value, 1),
        _ => return Err("expected integer seconds or s/m/h/d suffix".to_string()),
    };
    let count = number
        .parse::<u64>()
        .map_err(|_| "expected positive integer duration".to_string())?;
    if count == 0 {
        return Ok(None);
    }
    count
        .checked_mul(multiplier)
        .map(Duration::from_secs)
        .map(Some)
        .ok_or_else(|| "duration overflow".to_string())
}

fn parse_optional_u64(raw: &str) -> std::result::Result<Option<u64>, String> {
    let value = raw.trim();
    if value.is_empty() || value == "0" {
        return Ok(None);
    }
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| "expected non-negative integer bytes".to_string())
}

fn escape_value(value: &str) -> String {
    let mut escaped = String::new();
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

fn filename_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    format!("{}-{:09}", duration.as_secs(), duration.subsec_nanos())
}

fn unix_millis(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    #[test]
    fn writes_key_value_lines_with_escaped_values() {
        let temp = TempDir::new().unwrap();
        let journal = SupervisorJournal::open(temp.path());

        journal.event(
            "shutdown",
            &[
                ("reason", "SIGTERM received".to_string()),
                ("detail", "a\nb".to_string()),
            ],
        );

        let path = journal.path().unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("event=\"shutdown\""), "{content}");
        assert!(content.contains("reason=\"SIGTERM received\""), "{content}");
        assert!(content.contains("detail=\"a\\nb\""), "{content}");
    }

    #[test]
    fn prunes_old_supervisor_logs_by_age() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("supervisor");
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.log");
        let keep = dir.join("keep.log");
        let current = dir.join("current.log");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&keep, "keep").unwrap();
        std::fs::write(&current, "current").unwrap();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        set_file_mtime(
            &old,
            FileTime::from_system_time(now - Duration::from_secs(200)),
        )
        .unwrap();
        set_file_mtime(
            &keep,
            FileTime::from_system_time(now - Duration::from_secs(20)),
        )
        .unwrap();

        prune_supervisor_logs_result(
            &dir,
            &current,
            RetentionPolicy {
                max_age: Some(Duration::from_secs(100)),
                max_bytes: None,
            },
            now,
        )
        .unwrap();

        assert!(!old.exists());
        assert!(keep.exists());
        assert!(current.exists());
    }
}

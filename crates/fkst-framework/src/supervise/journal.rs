//! Engine-owned supervisor journal.

use fkst_common::{RuntimeKind, RuntimeLayout};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

#[derive(Clone)]
pub(crate) struct SupervisorJournal {
    inner: Arc<Mutex<JournalWriter>>,
}

struct JournalWriter {
    file: Option<File>,
    path: Option<PathBuf>,
}

impl SupervisorJournal {
    pub(crate) fn open() -> Self {
        match Self::try_open() {
            Ok(journal) => journal,
            Err(err) => {
                warn!(error = %err, "supervisor journal disabled");
                Self::disabled()
            }
        }
    }

    fn try_open() -> std::io::Result<Self> {
        let layout = RuntimeLayout::from_env().map_err(std::io::Error::other)?;
        let log_dir = layout.runtime_dir(RuntimeKind::Logs);
        std::fs::create_dir_all(&log_dir)?;
        let path = log_dir.join(format!(
            "supervisor-{}-{}.log",
            unix_seconds(),
            std::process::id()
        ));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(JournalWriter {
                file: Some(file),
                path: Some(path),
            })),
        })
    }

    pub(crate) fn disabled() -> Self {
        Self {
            inner: Arc::new(Mutex::new(JournalWriter {
                file: None,
                path: None,
            })),
        }
    }

    pub(crate) fn path(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .path
            .clone()
    }

    pub(crate) fn event<'a>(
        &self,
        event: &str,
        fields: impl IntoIterator<Item = (&'a str, String)>,
    ) {
        let line = format_line(event, fields);
        let mut writer = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(file) = writer.file.as_mut() else {
            return;
        };
        if let Err(err) = file.write_all(line.as_bytes()).and_then(|_| file.flush()) {
            warn!(error = %err, "supervisor journal write failed");
            writer.file = None;
            writer.path = None;
        }
    }
}

pub(crate) fn optional_path(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn format_line<'a>(event: &str, fields: impl IntoIterator<Item = (&'a str, String)>) -> String {
    let mut line = format!("event={}", escape_value(event));
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&escape_value(&value));
    }
    line.push('\n');
    line
}

fn escape_value(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ' ' => escaped.push_str("\\s"),
            '=' => escaped.push_str("\\="),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatter_escapes_key_value_delimiters() {
        let line = format_line(
            "dept child exit",
            [
                ("dept", "worker one".to_string()),
                ("reason", "exit=0\nok\tpath\\tail".to_string()),
            ],
        );

        assert_eq!(
            line,
            "event=dept\\schild\\sexit dept=worker\\sone reason=exit\\=0\\nok\\tpath\\\\tail\n"
        );
    }
}

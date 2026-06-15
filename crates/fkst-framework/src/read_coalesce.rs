//! Opt-in cross-process read coalescing for external command results.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use fkst_common::{
    runtime_key_file, validate_runtime_key, RuntimeKind, RUNTIME_LOCK_LEAF, RUNTIME_VALUE_LEAF,
};
use nix::fcntl::{flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::runtime_context;
use crate::sdk_basic::ExecResult;

const MAX_TTL: Duration = Duration::from_secs(300);
const CACHE_PREFIX: &str = "exec-read-coalesce";

#[derive(Clone, Debug)]
pub(crate) struct ReadCoalesceOptions {
    pub(crate) key: String,
    pub(crate) ttl: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct ReadCoalescePlan {
    cache_key: String,
    fingerprint: Fingerprint,
    ttl: Duration,
}

#[derive(Clone)]
pub(crate) struct CoalesceClock(Arc<dyn Fn() -> u128 + Send + Sync>);

#[derive(Debug)]
pub(crate) struct CoalesceLock {
    _file: File,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
struct Fingerprint {
    caller_key: String,
    program: String,
    args: Vec<String>,
    cwd: String,
    env: BTreeMap<String, String>,
    timeout_nanos: Option<u128>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    fingerprint: Fingerprint,
    expires_at_nanos: u128,
    stdout_b64: String,
    stderr_b64: String,
    exit_code: i32,
    timed_out: Option<bool>,
    error_class: Option<String>,
}

impl ReadCoalesceOptions {
    pub(crate) fn new(key: String, ttl_seconds: f64) -> Result<Self> {
        validate_runtime_key(&key).context("read_coalesce key is invalid")?;
        if !ttl_seconds.is_finite() || ttl_seconds <= 0.0 {
            bail!("read_coalesce ttl_seconds must be a positive finite number");
        }
        let ttl = if ttl_seconds > MAX_TTL.as_secs_f64() {
            MAX_TTL
        } else {
            Duration::from_secs_f64(ttl_seconds)
        };
        Ok(Self { key, ttl })
    }
}

impl CoalesceClock {
    pub(crate) fn system() -> Self {
        Self(Arc::new(|| unix_nanos_for_system_time(SystemTime::now())))
    }

    pub(crate) fn now(&self) -> u128 {
        (self.0)()
    }

    #[cfg(test)]
    pub(crate) fn for_test(clock: Arc<dyn Fn() -> u128 + Send + Sync>) -> Self {
        Self(clock)
    }
}

impl ReadCoalescePlan {
    pub(crate) fn new(
        opts: ReadCoalesceOptions,
        program: &Path,
        args: &[String],
        cwd: &Path,
        env: BTreeMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<Self> {
        let fingerprint = Fingerprint {
            caller_key: opts.key,
            program: program
                .canonicalize()
                .unwrap_or_else(|_| program.to_path_buf())
                .to_string_lossy()
                .to_string(),
            args: args.to_vec(),
            cwd: cwd.to_string_lossy().to_string(),
            env,
            timeout_nanos: timeout.map(|duration| duration.as_nanos()),
        };
        let digest = digest_json(&fingerprint)?;
        Ok(Self {
            cache_key: format!("{CACHE_PREFIX}/{digest}"),
            fingerprint,
            ttl: opts.ttl,
        })
    }

    pub(crate) fn get(
        &self,
        host_root: &Path,
        clock: &CoalesceClock,
    ) -> Result<Option<ExecResult>> {
        let layout = runtime_context::layout_from_host_root(host_root)?;
        let target = runtime_key_file(
            &layout.runtime_dir(RuntimeKind::Cache),
            &self.cache_key,
            RUNTIME_VALUE_LEAF,
        );
        let raw = match std::fs::read(&target) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("read {}", target.display())),
        };
        let entry: CacheEntry = match serde_json::from_slice(&raw) {
            Ok(entry) => entry,
            Err(_) => return Ok(None),
        };
        if entry.fingerprint != self.fingerprint {
            return Ok(None);
        }
        if clock.now() >= entry.expires_at_nanos {
            return Ok(None);
        }
        if entry.exit_code != 0 {
            return Ok(None);
        }
        Ok(Some(ExecResult {
            stdout: decode_text(&entry.stdout_b64)?,
            stderr: decode_text(&entry.stderr_b64)?,
            exit_code: entry.exit_code,
            timed_out: entry.timed_out,
            error_class: entry.error_class,
        }))
    }

    pub(crate) fn acquire_lock(&self, host_root: &Path) -> Result<CoalesceLock> {
        let layout = runtime_context::layout_from_host_root(host_root)?;
        let path = runtime_key_file(
            &layout.runtime_dir(RuntimeKind::Locks),
            &self.cache_key,
            RUNTIME_LOCK_LEAF,
        );
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("read coalesce lock '{}' has no parent", path.display()))?;
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        flock(file.as_raw_fd(), FlockArg::LockExclusive)
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(CoalesceLock { _file: file })
    }

    pub(crate) fn put_success(
        &self,
        host_root: &Path,
        output: &ExecResult,
        clock: &CoalesceClock,
    ) -> Result<()> {
        if output.exit_code != 0 {
            return Ok(());
        }
        let layout = runtime_context::layout_from_host_root(host_root)?;
        let target = runtime_key_file(
            &layout.runtime_dir(RuntimeKind::Cache),
            &self.cache_key,
            RUNTIME_VALUE_LEAF,
        );
        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("read coalesce cache '{}' has no parent", target.display()))?;
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let entry = CacheEntry {
            fingerprint: self.fingerprint.clone(),
            expires_at_nanos: clock.now().saturating_add(self.ttl.as_nanos()),
            stdout_b64: encode_text(&output.stdout),
            stderr_b64: encode_text(&output.stderr),
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            error_class: output.error_class.clone(),
        };
        let raw = serde_json::to_vec(&entry)?;
        let temp = parent.join(format!(
            ".=value.{}.{}.tmp",
            std::process::id(),
            ulid::Ulid::new()
        ));
        std::fs::write(&temp, raw).with_context(|| format!("write {}", temp.display()))?;
        std::fs::rename(&temp, &target)
            .with_context(|| format!("rename {} to {}", temp.display(), target.display()))?;
        Ok(())
    }
}

pub(crate) fn command_environment(overrides: &[(String, String)]) -> BTreeMap<String, String> {
    let mut env = std::env::vars().collect::<BTreeMap<_, _>>();
    for (key, value) in overrides {
        env.insert(key.clone(), value.clone());
    }
    env
}

pub(crate) fn execution_cwd(cwd: Option<&Path>) -> Result<PathBuf> {
    match cwd {
        Some(path) => Ok(path.to_path_buf()),
        None => std::env::current_dir().context("read current cwd"),
    }
}

fn digest_json(value: &Fingerprint) -> Result<String> {
    let raw = serde_json::to_vec(value)?;
    Ok(fnv1a64_hex(&raw))
}

fn fnv1a64_hex(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn encode_text(value: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(value.as_bytes())
}

fn decode_text(value: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .context("decode read coalesce cache value")?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn unix_nanos_for_system_time(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

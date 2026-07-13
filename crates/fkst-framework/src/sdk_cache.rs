//! SDK: best-effort host-local scratch key-value cache.

use fkst_common::{
    runtime_key_file, validate_runtime_key, RuntimeKind, RUNTIME_LOCK_LEAF, RUNTIME_VALUE_LEAF,
};
use mlua::{Lua, Result};
use nix::fcntl::{flock, FlockArg};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::runtime_context;

const HEADER_PREFIX: &str = "fkst-cache-v1 expires_at=";
const HEADER_NEVER: &str = "never";
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
pub(crate) const CACHE_REPLAY_BYPASS_ENV: &str = "FKST_INTERNAL_CACHE_REPLAY_BYPASS";

pub(crate) type Clock = Arc<dyn Fn() -> u128 + Send + Sync>;

pub fn register(lua: &Lua, host_root: &Path) -> Result<()> {
    register_with_clock(lua, host_root, system_clock())
}

pub(crate) fn register_with_clock(lua: &Lua, host_root: &Path, clock: Clock) -> Result<()> {
    let replay_bypass = take_replay_bypass_env();
    let invocation_writes = Arc::new(Mutex::new(BTreeSet::<String>::new()));
    let set_host_root = host_root.to_path_buf();
    let set_clock = clock.clone();
    let set_invocation_writes = invocation_writes.clone();
    lua.globals().set(
        "cache_set",
        lua.create_function(
            move |_, (key, value, ttl_seconds): (String, mlua::String, Option<f64>)| {
                let invocation_key = key.clone();
                cache_set(
                    &set_host_root,
                    key,
                    value.as_bytes().as_ref(),
                    ttl_seconds,
                    &set_clock,
                )?;
                if replay_bypass {
                    set_invocation_writes
                        .lock()
                        .map_err(|_| mlua::Error::external("cache replay write set lock poisoned"))?
                        .insert(invocation_key);
                }
                Ok(())
            },
        )?,
    )?;

    let expire_host_root = host_root.to_path_buf();
    let expire_invocation_writes = invocation_writes.clone();
    lua.globals().set(
        "cache_expire",
        lua.create_function(move |_, key: String| {
            cache_expire(&expire_host_root, key.clone())?;
            if replay_bypass {
                expire_invocation_writes
                    .lock()
                    .map_err(|_| mlua::Error::external("cache replay write set lock poisoned"))?
                    .remove(&key);
            }
            Ok(())
        })?,
    )?;

    let get_host_root = host_root.to_path_buf();
    let get_clock = clock;
    let get_invocation_writes = invocation_writes;
    lua.globals().set(
        "cache_get",
        lua.create_function(move |lua, key: String| {
            if replay_bypass {
                validate_runtime_key(&key).map_err(mlua::Error::external)?;
                if !get_invocation_writes
                    .lock()
                    .map_err(|_| mlua::Error::external("cache replay write set lock poisoned"))?
                    .contains(&key)
                {
                    return Ok(None);
                }
            }
            cache_get(&get_host_root, key, &get_clock)?
                .map(|value| lua.create_string(&value))
                .transpose()
        })?,
    )?;
    Ok(())
}

fn take_replay_bypass_env() -> bool {
    let enabled = std::env::var(CACHE_REPLAY_BYPASS_ENV).as_deref() == Ok("1");
    std::env::remove_var(CACHE_REPLAY_BYPASS_ENV);
    enabled
}

fn cache_set(
    host_root: &Path,
    key: String,
    value: &[u8],
    ttl_seconds: Option<f64>,
    clock: &Clock,
) -> Result<()> {
    let key = validate_runtime_key(&key).map_err(mlua::Error::external)?;
    let ttl = parse_ttl(ttl_seconds)?;
    let _lock = acquire_cache_lock(host_root, key)?;
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let target = runtime_key_file(
        &layout.runtime_dir(RuntimeKind::Cache),
        key,
        RUNTIME_VALUE_LEAF,
    );
    let parent = target.parent().ok_or_else(|| {
        mlua::Error::external(anyhow::anyhow!(
            "cache target '{}' has no parent",
            target.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(mlua::Error::external)?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            mlua::Error::external(anyhow::anyhow!(
                "cache target '{}' has no file name",
                target.display()
            ))
        })?;
    let temp = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temp, encode_entry(value, expires_at(ttl, clock)))
        .map_err(mlua::Error::external)?;
    std::fs::rename(temp, target).map_err(mlua::Error::external)?;
    Ok(())
}

fn cache_get(host_root: &Path, key: String, clock: &Clock) -> Result<Option<Vec<u8>>> {
    let key = validate_runtime_key(&key).map_err(mlua::Error::external)?;
    let _lock = acquire_cache_lock(host_root, key)?;
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let target = runtime_key_file(
        &layout.runtime_dir(RuntimeKind::Cache),
        key,
        RUNTIME_VALUE_LEAF,
    );
    match std::fs::read(&target) {
        Ok(raw) => match decode_entry(&raw) {
            Some((Some(expires_at), _)) if clock() >= expires_at => {
                remove_cache_file(&target)?;
                Ok(None)
            }
            Some((_, value)) => Ok(Some(value.to_vec())),
            None => Ok(None),
        },
        Err(_err) => Ok(None),
    }
}

fn cache_expire(host_root: &Path, key: String) -> Result<()> {
    let key = validate_runtime_key(&key).map_err(mlua::Error::external)?;
    let _lock = acquire_cache_lock(host_root, key)?;
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let target = runtime_key_file(
        &layout.runtime_dir(RuntimeKind::Cache),
        key,
        RUNTIME_VALUE_LEAF,
    );
    remove_cache_file(&target)
}

fn acquire_cache_lock(host_root: &Path, key: &str) -> Result<File> {
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let cache_locks = layout.runtime_dir(RuntimeKind::Locks).join("cache");
    let path = runtime_key_file(&cache_locks, key, RUNTIME_LOCK_LEAF);
    let parent = path.parent().ok_or_else(|| {
        mlua::Error::external(anyhow::anyhow!(
            "cache lock target '{}' has no parent",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(mlua::Error::external)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(mlua::Error::external)?;
    flock(file.as_raw_fd(), FlockArg::LockExclusive).map_err(mlua::Error::external)?;
    Ok(file)
}

fn parse_ttl(ttl_seconds: Option<f64>) -> Result<Option<u64>> {
    match ttl_seconds {
        Some(ttl) if ttl.is_finite() && ttl > 0.0 => Ok(Some(ttl_seconds_to_nanos(ttl)?)),
        Some(_) => Err(mlua::Error::external(anyhow::anyhow!(
            "cache ttl_seconds must be a positive finite number"
        ))),
        None => Ok(None),
    }
}

fn ttl_seconds_to_nanos(ttl_seconds: f64) -> Result<u64> {
    let nanos = ttl_seconds * 1_000_000_000.0;
    if nanos > u64::MAX as f64 {
        return Err(mlua::Error::external(anyhow::anyhow!(
            "cache ttl_seconds is too large"
        )));
    }
    Ok(nanos.ceil() as u64)
}

fn expires_at(ttl_nanos: Option<u64>, clock: &Clock) -> Option<u128> {
    ttl_nanos.map(|ttl| clock().saturating_add(u128::from(ttl)))
}

fn encode_entry(value: &[u8], expires_at: Option<u128>) -> Vec<u8> {
    let expires = expires_at
        .map(|nanos| nanos.to_string())
        .unwrap_or_else(|| HEADER_NEVER.to_string());
    let mut entry = format!("{HEADER_PREFIX}{expires}\n").into_bytes();
    entry.extend_from_slice(value);
    entry
}

fn decode_entry(raw: &[u8]) -> Option<(Option<u128>, &[u8])> {
    let newline = raw.iter().position(|byte| *byte == b'\n')?;
    let header = std::str::from_utf8(&raw[..newline]).ok()?;
    let value = &raw[newline + 1..];
    let expires = header.strip_prefix(HEADER_PREFIX)?;
    if expires == HEADER_NEVER {
        return Some((None, value));
    }
    expires
        .parse::<u128>()
        .ok()
        .map(|nanos| (Some(nanos), value))
}

fn remove_cache_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(mlua::Error::external(err)),
    }
}

fn system_clock() -> Clock {
    Arc::new(|| unix_nanos_for_system_time(SystemTime::now()))
}

pub(crate) fn unix_nanos_for_system_time(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

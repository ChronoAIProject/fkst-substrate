//! SDK: `once(key, fn) -> boolean` best-effort per-key debounce marker.

use fkst_common::{
    runtime_key_file, validate_runtime_key, RuntimeKind, RUNTIME_LOCK_LEAF, RUNTIME_MARK_LEAF,
};
use mlua::{Function, Lua, Result};
use nix::fcntl::{flock, FlockArg};
use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::{runtime_context, sdk_log};

pub(crate) const ONCE_REPLAY_BYPASS_ENV: &str = "FKST_INTERNAL_ONCE_REPLAY_BYPASS";

pub fn register(lua: &Lua, host_root: &Path, saga_recovery: bool) -> Result<()> {
    let replay_bypass = take_replay_bypass_env();
    let invocation_marks = Arc::new(Mutex::new(BTreeSet::<String>::new()));
    let host_root = host_root.to_path_buf();
    lua.globals().set(
        "once",
        lua.create_function(move |_, (key, f): (String, Function)| {
            let bypass_marker = replay_bypass
                && !invocation_marks
                    .lock()
                    .map_err(|_| mlua::Error::external("once replay set lock poisoned"))?
                    .contains(&key);
            let invocation_key = key.clone();
            let ran = once(&host_root, saga_recovery, bypass_marker, key, f)?;
            if replay_bypass && ran {
                invocation_marks
                    .lock()
                    .map_err(|_| mlua::Error::external("once replay set lock poisoned"))?
                    .insert(invocation_key);
            }
            Ok(ran)
        })?,
    )?;
    Ok(())
}

fn once(
    host_root: &Path,
    saga_recovery: bool,
    replay_bypass: bool,
    key: String,
    f: Function,
) -> Result<bool> {
    if !saga_recovery {
        return Err(mlua::Error::external(anyhow::anyhow!(
            "once requires saga_recovery capability"
        )));
    }
    let key = validate_runtime_key(&key).map_err(mlua::Error::external)?;
    let layout =
        runtime_context::layout_from_host_root(host_root).map_err(mlua::Error::external)?;
    let once_locks = layout.runtime_dir(RuntimeKind::Locks).join("once");
    let lock_path = runtime_key_file(&once_locks, key, RUNTIME_LOCK_LEAF);
    let lock_parent = lock_path.parent().ok_or_else(|| {
        mlua::Error::external(anyhow::anyhow!(
            "once lock target '{}' has no parent",
            lock_path.display()
        ))
    })?;
    std::fs::create_dir_all(lock_parent).map_err(mlua::Error::external)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(mlua::Error::external)?;

    flock(lock_file.as_raw_fd(), FlockArg::LockExclusive).map_err(mlua::Error::external)?;

    let marker = runtime_key_file(
        &layout.runtime_dir(RuntimeKind::Marks),
        key,
        RUNTIME_MARK_LEAF,
    );
    let marker_preexisting = marker.exists();
    if marker_preexisting && !replay_bypass {
        sdk_log::info(&format!("once decision=skip-marked key={key}"));
        drop(lock_file);
        return Ok(false);
    }

    let result = f.call::<()>(());
    match result {
        Ok(()) => {
            if marker_preexisting {
                sdk_log::info(&format!("once decision=ran-replay-marked key={key}"));
                drop(lock_file);
                return Ok(true);
            }
            let marker_parent = marker.parent().ok_or_else(|| {
                mlua::Error::external(anyhow::anyhow!(
                    "once marker target '{}' has no parent",
                    marker.display()
                ))
            })?;
            std::fs::create_dir_all(marker_parent).map_err(mlua::Error::external)?;
            let marker_name = marker
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    mlua::Error::external(anyhow::anyhow!(
                        "once marker target '{}' has no file name",
                        marker.display()
                    ))
                })?;
            let temp_marker = marker_parent.join(format!(".{marker_name}.tmp"));
            let marked_at = sdk_log::rfc3339_utc_now();
            std::fs::write(&temp_marker, format!("key={key}\nmarked_at={marked_at}\n"))
                .map_err(mlua::Error::external)?;
            std::fs::rename(temp_marker, marker).map_err(mlua::Error::external)?;
            sdk_log::info(&format!("once decision=ran-marked key={key}"));
            drop(lock_file);
            Ok(true)
        }
        Err(err) => {
            drop(lock_file);
            Err(err)
        }
    }
}

fn take_replay_bypass_env() -> bool {
    let enabled = std::env::var(ONCE_REPLAY_BYPASS_ENV).as_deref() == Ok("1");
    std::env::remove_var(ONCE_REPLAY_BYPASS_ENV);
    enabled
}

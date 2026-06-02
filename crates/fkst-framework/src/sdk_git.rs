//! SDK: git + filesystem-lock helpers.
//! - with_lock(path, fn)        -- exclusive flock for cross-process mutual exclusion
//! - setup_worktree(prefix)     -- git worktree add -b <branch> <runtime>/worktrees/<prefix>-<ULID>
//! - git_log_count(grep, since) -- count `git log --grep=<g> --since=<s>` entries
//! - git_log_grep(grep, since)  -- list commit hashes matching `git log --grep=<g> --since=<s>`
//! - count_worktrees()          -- count linked worktrees excluding main checkout
//! - list_orphan_worktrees(pfx) -- list <runtime>/worktrees/<pfx>* linked worktree paths

use fkst_common::{RuntimeKind, RuntimeLayout};
use mlua::{Function, Lua, Result};
use nix::fcntl::{flock, FlockArg};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const PACKAGE_ROOT_ENV: &str = "FKST_PACKAGE_ROOT";

pub fn register(lua: &Lua) -> Result<()> {
    register_with_lock(lua)?;
    register_setup_worktree(lua)?;
    register_git_log_count(lua)?;
    register_git_log_grep(lua)?;
    register_count_worktrees(lua)?;
    register_list_orphan_worktrees(lua)?;
    Ok(())
}

fn read_failure(label: &str, stderr: &[u8]) -> mlua::Error {
    mlua::Error::external(anyhow::anyhow!(
        "{}: {}",
        label,
        String::from_utf8_lossy(stderr).trim()
    ))
}

fn register_with_lock(lua: &Lua) -> Result<()> {
    lua.globals().set(
        "with_lock",
        lua.create_function(|_, (path, f): (String, Function)| {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .map_err(mlua::Error::external)?;

            flock(file.as_raw_fd(), FlockArg::LockExclusive).map_err(mlua::Error::external)?;

            let result = f.call::<mlua::Value>(());
            drop(file);
            result
        })?,
    )?;
    Ok(())
}

fn register_setup_worktree(lua: &Lua) -> Result<()> {
    lua.globals().set(
        "setup_worktree",
        lua.create_function(|_, prefix: String| {
            let ulid = ulid::Ulid::new().to_string();
            let (candidate_prefix, candidate_from_sep) = candidate_branch_config()?;
            let layout = RuntimeLayout::from_env().map_err(mlua::Error::external)?;
            let worktrees = layout.runtime_dir(RuntimeKind::Worktrees);
            let path = worktrees.join(format!("{}-{}", prefix, ulid));
            let path = path.to_string_lossy().into_owned();
            let parent_ref = current_parent_ref()?;
            let parent_name = branch_slug(&parent_ref);
            let branch = format!(
                "{}-{}-{}-{}{}{}",
                candidate_prefix,
                today_utc(),
                branch_slug(&prefix),
                ulid,
                candidate_from_sep,
                parent_name
            );

            // worktrees live below RuntimeLayout worktrees dir.
            std::fs::create_dir_all(worktrees).map_err(mlua::Error::external)?;

            let out = Command::new("git")
                .args(["worktree", "add", "-b", &branch, &path, &parent_ref])
                .output()
                .map_err(mlua::Error::external)?;

            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                return Err(mlua::Error::external(anyhow::anyhow!(
                    "git worktree add failed: {}",
                    err
                )));
            }

            Ok(path)
        })?,
    )?;
    Ok(())
}

fn candidate_branch_config() -> Result<(String, String)> {
    let prefix =
        candidate_branch_config_value("FKST_CANDIDATE_PREFIX", "tunables/candidate_prefix.txt")?;
    let from_sep = candidate_branch_config_value(
        "FKST_CANDIDATE_FROM_SEP",
        "tunables/candidate_from_sep.txt",
    )?;
    validate_branch_fragment("FKST_CANDIDATE_PREFIX", &prefix)?;
    validate_branch_fragment("FKST_CANDIDATE_FROM_SEP", &from_sep)?;
    Ok((prefix, from_sep))
}

fn candidate_branch_config_value(key: &str, tunable_rel: &str) -> Result<String> {
    match std::env::var(key) {
        Ok(value) => return Ok(value.trim().to_string()),
        Err(std::env::VarError::NotPresent) => {}
        Err(std::env::VarError::NotUnicode(_)) => Err(mlua::Error::external(anyhow::anyhow!(
            "{key} must be valid UTF-8"
        )))?,
    }
    if let Some(value) = read_package_tunable(tunable_rel)? {
        return Ok(value);
    }
    Err(mlua::Error::external(anyhow::anyhow!(
        "{}",
        missing_candidate_config_message(key, tunable_rel)
    )))
}

fn missing_candidate_config_message(key: &str, tunable_rel: &str) -> String {
    match key {
        "FKST_CANDIDATE_PREFIX" => {
            "FKST candidate branch prefix tunable or FKST_CANDIDATE_PREFIX required".to_string()
        }
        "FKST_CANDIDATE_FROM_SEP" => {
            "FKST candidate branch from separator tunable or FKST_CANDIDATE_FROM_SEP required"
                .to_string()
        }
        _ => format!("{key} tunable {tunable_rel} required"),
    }
}

fn read_package_tunable(tunable_rel: &str) -> Result<Option<String>> {
    let Some(root) = std::env::var_os(PACKAGE_ROOT_ENV) else {
        return Ok(None);
    };
    let path = PathBuf::from(root).join(tunable_rel);
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content.trim().to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(mlua::Error::external(anyhow::anyhow!(
            "read {} failed: {err}",
            path.display()
        ))),
    }
}

fn validate_branch_fragment(name: &str, value: &str) -> Result<()> {
    let invalid = value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.ends_with('.')
        || value.ends_with(".lock")
        || value.contains("..")
        || value.contains("//")
        || value.contains("@{")
        || value
            .chars()
            .any(|ch| ch.is_ascii_control() || ch.is_ascii_whitespace() || "~^:?*[\\".contains(ch));
    if invalid {
        return Err(mlua::Error::external(anyhow::anyhow!(
            "{name} is not a valid branch fragment"
        )));
    }
    Ok(())
}

fn current_parent_ref() -> Result<String> {
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map_err(mlua::Error::external)?;
    if !branch.status.success() {
        return Err(read_failure("git-current-branch-failed", &branch.stderr));
    }
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
    if !branch.is_empty() && branch != "HEAD" {
        return Ok(branch);
    }

    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .map_err(mlua::Error::external)?;
    if !sha.status.success() {
        return Err(read_failure("git-current-sha-failed", &sha.stderr));
    }
    Ok(String::from_utf8_lossy(&sha.stdout).trim().to_string())
}

fn today_utc() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}{month:02}{day:02}")
}

fn branch_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if !last_dash {
                slug.push(mapped);
            }
            last_dash = true;
        } else {
            slug.push(mapped);
            last_dash = false;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "worktree".to_string()
    } else {
        slug
    }
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

fn register_git_log_count(lua: &Lua) -> Result<()> {
    lua.globals().set(
        "git_log_count",
        lua.create_function(|_, (grep, since): (String, String)| {
            let out = std::process::Command::new("git")
                .args([
                    "log",
                    &format!("--grep={}", grep),
                    &format!("--since={}", since),
                    "--oneline",
                ])
                .output()
                .map_err(mlua::Error::external)?;

            if !out.status.success() {
                return Err(read_failure("git-log-count-failed", &out.stderr));
            }

            let count = String::from_utf8_lossy(&out.stdout).lines().count();
            Ok(count as i64)
        })?,
    )?;
    Ok(())
}

fn register_git_log_grep(lua: &Lua) -> Result<()> {
    lua.globals().set(
        "git_log_grep",
        lua.create_function(|_, (grep, since): (String, String)| {
            let out = std::process::Command::new("git")
                .args([
                    "log",
                    &format!("--grep={}", grep),
                    &format!("--since={}", since),
                    "--format=%H",
                ])
                .output()
                .map_err(mlua::Error::external)?;

            if !out.status.success() {
                return Err(read_failure("git-log-grep-failed", &out.stderr));
            }

            let shas = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();

            Ok(shas)
        })?,
    )?;
    Ok(())
}

pub(crate) fn parse_worktree_paths(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(ToOwned::to_owned)
        .collect()
}

fn register_count_worktrees(lua: &Lua) -> Result<()> {
    lua.globals().set(
        "count_worktrees",
        lua.create_function(|_, ()| {
            let out = std::process::Command::new("git")
                .args(["worktree", "list", "--porcelain"])
                .output()
                .map_err(mlua::Error::external)?;

            if !out.status.success() {
                return Err(read_failure("git-worktree-list-failed", &out.stderr));
            }

            let count = parse_worktree_paths(&out.stdout).len().saturating_sub(1);
            Ok(count as i64)
        })?,
    )?;
    Ok(())
}

fn register_list_orphan_worktrees(lua: &Lua) -> Result<()> {
    lua.globals().set(
        "list_orphan_worktrees",
        lua.create_function(|_, prefix: String| {
            let out = std::process::Command::new("git")
                .args(["worktree", "list", "--porcelain"])
                .output()
                .map_err(mlua::Error::external)?;

            if !out.status.success() {
                return Err(read_failure("git-worktree-list-failed", &out.stderr));
            }

            let paths = parse_worktree_paths(&out.stdout);
            let Some(repo) = paths.first() else {
                return Ok(Vec::<String>::new());
            };

            let layout = RuntimeLayout::from_env().map_err(mlua::Error::external)?;
            let target_prefix = runtime_dir_abs(repo, &layout, RuntimeKind::Worktrees)
                .join(prefix)
                .to_string_lossy()
                .to_string();
            let orphans = paths
                .into_iter()
                .skip(1)
                .filter(|path| path.starts_with(&target_prefix))
                .collect::<Vec<_>>();

            Ok(orphans)
        })?,
    )?;
    Ok(())
}

fn runtime_dir_abs(repo: &str, layout: &RuntimeLayout, kind: RuntimeKind) -> std::path::PathBuf {
    let path = layout.runtime_dir(kind);
    let absolute = if path.is_absolute() {
        path.clone()
    } else {
        Path::new(repo).join(path)
    };
    match std::fs::canonicalize(&absolute) {
        Ok(canonical) => canonical,
        Err(_) => absolute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_worktree_paths_reads_porcelain_worktree_lines() {
        let paths = parse_worktree_paths(
            b"worktree /repo\nHEAD abc\n\nworktree /repo/.worktrees/a\nHEAD def\n",
        );

        assert_eq!(paths, vec!["/repo", "/repo/.worktrees/a"]);
    }
}

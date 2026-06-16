//! SDK: git + filesystem-lock helpers.
//! - with_lock(name, fn)        -- exclusive flock under <runtime>/locks
//! - setup_worktree(prefix)     -- git worktree add -b <branch> <runtime>/worktrees/<prefix>-<ULID>
//! - git_log_count(grep, since) -- count `git log --grep=<g> --since=<s>` entries
//! - git_log_grep(grep, since)  -- list commit hashes matching `git log --grep=<g> --since=<s>`
//! - count_worktrees()          -- count linked worktrees excluding main checkout
//! - list_orphan_worktrees(pfx) -- list <runtime>/worktrees/<pfx>* linked worktree paths

use fkst_common::{
    runtime_key_file, validate_runtime_key, RuntimeKind, RuntimeLayout, RUNTIME_LOCK_LEAF,
};
use mlua::{Function, Lua, Result};
use nix::fcntl::{flock, FlockArg};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config_registry::{ConfigContext, ConfigKey};
use crate::external_command::{
    format_command, CommandSpec, MockCommandInvocation, MockCommandPlan, MockCommandResult,
    MockCommandState,
};
use crate::rate_pool::RatePoolRegistry;
use crate::runtime_context;

pub fn register(lua: &Lua, host_root: &Path, config: ConfigContext) -> Result<()> {
    register_with_runner(lua, host_root, config, None)
}

pub(crate) fn register_with_runner(
    lua: &Lua,
    host_root: &Path,
    config: ConfigContext,
    runner: Option<MockCommandState>,
) -> Result<()> {
    let host_root = host_root.to_path_buf();
    let rate_pools = RatePoolRegistry::from_config(&config).map_err(mlua::Error::external)?;
    register_with_lock(lua, host_root.clone())?;
    register_setup_worktree(
        lua,
        host_root.clone(),
        config,
        rate_pools.clone(),
        runner.clone(),
    )?;
    register_git_log_count(lua, host_root.clone(), rate_pools.clone(), runner.clone())?;
    register_git_log_grep(lua, host_root.clone(), rate_pools.clone(), runner.clone())?;
    register_count_worktrees(lua, host_root.clone(), rate_pools.clone(), runner.clone())?;
    register_list_orphan_worktrees(lua, host_root, rate_pools, runner)?;
    Ok(())
}

fn read_failure(label: &str, stderr: &[u8]) -> mlua::Error {
    mlua::Error::external(anyhow::anyhow!(
        "{}: {}",
        label,
        String::from_utf8_lossy(stderr).trim()
    ))
}

fn register_with_lock(lua: &Lua, host_root: PathBuf) -> Result<()> {
    lua.globals().set(
        "with_lock",
        lua.create_function(move |_, (name, f): (String, Function)| {
            let name = validate_runtime_key(&name).map_err(mlua::Error::external)?;
            if name == "once" || name.starts_with("once/") {
                return Err(mlua::Error::external(anyhow::anyhow!(
                    "with_lock name '{name}' uses reserved lock namespace 'once'"
                )));
            }
            if name == "cache" || name.starts_with("cache/") {
                return Err(mlua::Error::external(anyhow::anyhow!(
                    "with_lock name '{name}' uses reserved lock namespace 'cache'"
                )));
            }
            let layout = runtime_context::layout_from_host_root(&host_root)
                .map_err(mlua::Error::external)?;
            let locks = layout.runtime_dir(RuntimeKind::Locks);
            let path = runtime_key_file(&locks, name, RUNTIME_LOCK_LEAF);
            let parent = path.parent().ok_or_else(|| {
                mlua::Error::external(anyhow::anyhow!(
                    "with_lock target '{}' has no parent",
                    path.display()
                ))
            })?;
            std::fs::create_dir_all(parent).map_err(mlua::Error::external)?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(path)
                .map_err(mlua::Error::external)?;

            flock(file.as_raw_fd(), FlockArg::LockExclusive).map_err(mlua::Error::external)?;

            let result = f.call::<mlua::Value>(());
            drop(file);
            result
        })?,
    )?;
    Ok(())
}

fn register_setup_worktree(
    lua: &Lua,
    host_root: PathBuf,
    config: ConfigContext,
    rate_pools: RatePoolRegistry,
    runner: Option<MockCommandState>,
) -> Result<()> {
    lua.globals().set(
        "setup_worktree",
        lua.create_function(move |_, prefix: String| {
            let ulid = ulid::Ulid::new().to_string();
            let (candidate_prefix, candidate_from_sep) = candidate_branch_config(&config)?;
            let layout = runtime_context::layout_from_host_root(&host_root)
                .map_err(mlua::Error::external)?;
            let worktrees = layout.runtime_dir(RuntimeKind::Worktrees);
            let path = worktrees.join(format!("{}-{}", prefix, ulid));
            let path = path.to_string_lossy().into_owned();
            let parent_ref =
                current_parent_ref_with_runner(&host_root, &rate_pools, runner.as_ref())?;
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

            let out = run_git_command(
                &host_root,
                ["worktree", "add", "-b", &branch, &path, &parent_ref],
                Some(&rate_pools),
                runner.as_ref(),
            )?;

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

fn candidate_branch_config(config: &ConfigContext) -> Result<(String, String)> {
    let prefix = candidate_branch_config_value(config, ConfigKey::CandidatePrefix)?;
    let from_sep = candidate_branch_config_value(config, ConfigKey::CandidateFromSep)?;
    validate_branch_fragment("FKST_CANDIDATE_PREFIX", &prefix)?;
    validate_branch_fragment("FKST_CANDIDATE_FROM_SEP", &from_sep)?;
    Ok((prefix, from_sep))
}

fn candidate_branch_config_value(config: &ConfigContext, key: ConfigKey) -> Result<String> {
    config.resolved_string(key).map_err(mlua::Error::external)
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

fn current_parent_ref_with_runner(
    host_root: &Path,
    rate_pools: &RatePoolRegistry,
    runner: Option<&MockCommandState>,
) -> Result<String> {
    let branch = run_git_command(
        host_root,
        ["rev-parse", "--abbrev-ref", "HEAD"],
        Some(rate_pools),
        runner,
    )?;
    if !branch.status.success() {
        return Err(read_failure("git-current-branch-failed", &branch.stderr));
    }
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
    if !branch.is_empty() && branch != "HEAD" {
        return Ok(branch);
    }

    let sha = run_git_command(
        host_root,
        ["rev-parse", "--short=12", "HEAD"],
        Some(rate_pools),
        runner,
    )?;
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

fn register_git_log_count(
    lua: &Lua,
    host_root: PathBuf,
    rate_pools: RatePoolRegistry,
    runner: Option<MockCommandState>,
) -> Result<()> {
    lua.globals().set(
        "git_log_count",
        lua.create_function(move |_, (grep, since): (String, String)| {
            let out = run_git_command(
                &host_root,
                [
                    "log",
                    &format!("--grep={}", grep),
                    &format!("--since={}", since),
                    "--oneline",
                ],
                Some(&rate_pools),
                runner.as_ref(),
            )?;

            if !out.status.success() {
                return Err(read_failure("git-log-count-failed", &out.stderr));
            }

            let count = String::from_utf8_lossy(&out.stdout).lines().count();
            Ok(count as i64)
        })?,
    )?;
    Ok(())
}

fn register_git_log_grep(
    lua: &Lua,
    host_root: PathBuf,
    rate_pools: RatePoolRegistry,
    runner: Option<MockCommandState>,
) -> Result<()> {
    lua.globals().set(
        "git_log_grep",
        lua.create_function(move |_, (grep, since): (String, String)| {
            let out = run_git_command(
                &host_root,
                [
                    "log",
                    &format!("--grep={}", grep),
                    &format!("--since={}", since),
                    "--format=%H",
                ],
                Some(&rate_pools),
                runner.as_ref(),
            )?;

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

fn register_count_worktrees(
    lua: &Lua,
    host_root: PathBuf,
    rate_pools: RatePoolRegistry,
    runner: Option<MockCommandState>,
) -> Result<()> {
    lua.globals().set(
        "count_worktrees",
        lua.create_function(move |_, ()| {
            let out = run_git_command(
                &host_root,
                ["worktree", "list", "--porcelain"],
                Some(&rate_pools),
                runner.as_ref(),
            )?;

            if !out.status.success() {
                return Err(read_failure("git-worktree-list-failed", &out.stderr));
            }

            let count = parse_worktree_paths(&out.stdout).len().saturating_sub(1);
            Ok(count as i64)
        })?,
    )?;
    Ok(())
}

fn register_list_orphan_worktrees(
    lua: &Lua,
    host_root: PathBuf,
    rate_pools: RatePoolRegistry,
    runner: Option<MockCommandState>,
) -> Result<()> {
    lua.globals().set(
        "list_orphan_worktrees",
        lua.create_function(move |_, prefix: String| {
            let out = run_git_command(
                &host_root,
                ["worktree", "list", "--porcelain"],
                Some(&rate_pools),
                runner.as_ref(),
            )?;

            if !out.status.success() {
                return Err(read_failure("git-worktree-list-failed", &out.stderr));
            }

            let paths = parse_worktree_paths(&out.stdout);
            let layout = runtime_context::layout_from_host_root(&host_root)
                .map_err(mlua::Error::external)?;
            let target_prefix = runtime_dir_abs(&layout, RuntimeKind::Worktrees)
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

fn run_git_command<'a>(
    host_root: &Path,
    args: impl IntoIterator<Item = &'a str>,
    rate_pools: Option<&RatePoolRegistry>,
    runner: Option<&MockCommandState>,
) -> Result<Output> {
    let mut rendered_args = vec!["-C".to_string(), host_root.to_string_lossy().into_owned()];
    rendered_args.extend(args.into_iter().map(ToOwned::to_owned));
    if let Some(runner) = runner {
        let invocation = MockCommandInvocation {
            rendered: format_command("git", &rendered_args),
            program: "git".to_string(),
            args: rendered_args.clone(),
            stdin: String::new(),
            cwd: None,
            env: Vec::new(),
        };
        match runner.prepare(invocation.clone())? {
            MockCommandPlan::Return(result) => {
                return Ok(mock_output(result.stdout, result.stderr, result.exit_code));
            }
            MockCommandPlan::Record => {
                let audited = crate::external_command::run_audited(CommandSpec {
                    program: "git".into(),
                    args: rendered_args,
                    cwd: None,
                    env: Vec::new(),
                    stdin: crate::external_command::CommandStdin::Null,
                    timeout: None,
                    process_group: false,
                })
                .map_err(mlua::Error::external)?;
                let result = MockCommandResult {
                    stdout: String::from_utf8_lossy(&audited.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&audited.stderr).to_string(),
                    exit_code: audited.exit_code,
                };
                runner.record(invocation, result.clone())?;
                return Ok(mock_output(result.stdout, result.stderr, result.exit_code));
            }
        }
    }

    if let Some(rate_pools) = rate_pools {
        rate_pools
            .acquire_for_program("git")
            .map_err(mlua::Error::external)?;
    }

    let audited = crate::external_command::run_audited(CommandSpec {
        program: "git".into(),
        args: rendered_args,
        cwd: None,
        env: Vec::new(),
        stdin: crate::external_command::CommandStdin::Null,
        timeout: None,
        process_group: false,
    })
    .map_err(mlua::Error::external)?;
    Ok(mock_output(
        String::from_utf8_lossy(&audited.stdout).to_string(),
        String::from_utf8_lossy(&audited.stderr).to_string(),
        audited.exit_code,
    ))
}

#[cfg(unix)]
fn mock_output(stdout: String, stderr: String, exit_code: i32) -> Output {
    use std::os::unix::process::ExitStatusExt;

    Output {
        status: std::process::ExitStatus::from_raw(exit_code << 8),
        stdout: stdout.into_bytes(),
        stderr: stderr.into_bytes(),
    }
}

#[cfg(not(unix))]
fn mock_output(stdout: String, stderr: String, exit_code: i32) -> Output {
    Output {
        status: portable_exit_status(exit_code),
        stdout: stdout.into_bytes(),
        stderr: stderr.into_bytes(),
    }
}

#[cfg(not(unix))]
fn portable_exit_status(exit_code: i32) -> std::process::ExitStatus {
    let code = exit_code.clamp(0, 255).to_string();
    std::process::Command::new(platform_shell())
        .args(platform_shell_exit_args(code))
        .status()
        .expect("platform shell must produce mock command exit status")
}

#[cfg(all(not(unix), windows))]
fn platform_shell() -> &'static str {
    "cmd"
}

#[cfg(all(not(unix), windows))]
fn platform_shell_exit_args(code: String) -> Vec<String> {
    vec!["/C".to_string(), "exit".to_string(), code]
}

#[cfg(all(not(unix), not(windows)))]
fn platform_shell() -> &'static str {
    "sh"
}

#[cfg(all(not(unix), not(windows)))]
fn platform_shell_exit_args(code: String) -> Vec<String> {
    vec!["-c".to_string(), format!("exit {code}")]
}

fn runtime_dir_abs(layout: &RuntimeLayout, kind: RuntimeKind) -> std::path::PathBuf {
    let path = layout.runtime_dir(kind);
    match std::fs::canonicalize(&path) {
        Ok(canonical) => canonical,
        Err(_) => path,
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

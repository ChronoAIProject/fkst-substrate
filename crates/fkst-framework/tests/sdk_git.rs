// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/config_registry.rs"]
mod config_registry;
#[path = "../src/sdk_git.rs"]
mod sdk_git;
mod support;

use mlua::Lua;
use sdk_git::{parse_worktree_paths, register};
use std::path::Path;
use std::process::Command;
use support::process_sandbox::ProcessSandbox;
use tempfile::tempdir;

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn in_sandbox<T>(
    dir: &Path,
    configure: impl FnOnce(&mut ProcessSandbox),
    f: impl FnOnce() -> T,
) -> T {
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(dir);
    configure(&mut sandbox);
    sandbox.run(f)
}

fn repo_with_commit(message: &str) -> tempfile::TempDir {
    let tmp = tempdir().unwrap();
    git(tmp.path(), &["init", "-q"]);
    git(tmp.path(), &["config", "user.email", "test@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test User"]);
    std::fs::write(tmp.path().join("file.txt"), "content\n").unwrap();
    git(tmp.path(), &["add", "file.txt"]);
    git(tmp.path(), &["commit", "-q", "-m", message]);
    tmp
}

fn in_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    in_sandbox(dir, |_| {}, f)
}

#[cfg(unix)]
fn install_git_script(bin_dir: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(bin_dir).unwrap();
    let git = bin_dir.join("git");
    std::fs::write(&git, body).unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn assert_lua_error_contains(err: mlua::Error, parts: &[&str]) {
    let message = err.to_string();
    for part in parts {
        assert!(
            message.contains(part),
            "error {message:?} did not contain {part:?}"
        );
    }
}

#[test]
fn with_lock_runs_fn() {
    let lua = Lua::new();
    register(&lua).unwrap();

    let tmp = tempdir().unwrap();
    let lock = tmp.path().join("x.lock").to_string_lossy().to_string();

    let n: i64 = lua
        .load(format!(
            r#"
            local r = nil
            with_lock("{}", function() r = 42 end)
            return r
            "#,
            lock
        ))
        .eval()
        .unwrap();

    assert_eq!(n, 42);
}

#[test]
fn git_log_count_returns_int() {
    let lua = Lua::new();
    register(&lua).unwrap();

    let r: i64 = lua
        .load(r#"return git_log_count("never-matches-xyzzy", "100 years ago")"#)
        .eval()
        .unwrap();

    assert_eq!(r, 0);
}

#[cfg(unix)]
#[test]
fn git_log_count_failure_raises_lua_error() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let sandbox = ProcessSandbox::new();
    let bin_dir = sandbox.temp_path("bin");
    install_git_script(
        &bin_dir,
        r#"#!/bin/sh
if [ "$1" = "log" ]; then
  printf 'count backend unavailable\n' >&2
  exit 2
fi
exit 99
"#,
    );

    let err = {
        let mut sandbox = sandbox;
        sandbox.prepend_path(&bin_dir);
        sandbox.run(|| {
            lua.load(r#"return git_log_count("x", "y")"#)
                .eval::<i64>()
                .unwrap_err()
        })
    };

    assert_lua_error_contains(err, &["git-log-count-failed", "count backend unavailable"]);
}

#[test]
fn git_log_grep_returns_shas() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("C3 unique message");

    let shas: Vec<String> = in_dir(repo.path(), || {
        lua.load(r#"return git_log_grep("C3 unique", "1970-01-01T00:00:00Z")"#)
            .eval()
            .unwrap()
    });

    assert_eq!(shas.len(), 1);
    assert_eq!(shas[0].len(), 40);
}

#[cfg(unix)]
#[test]
fn git_log_grep_failure_raises_lua_error() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let sandbox = ProcessSandbox::new();
    let bin_dir = sandbox.temp_path("bin");
    install_git_script(
        &bin_dir,
        r#"#!/bin/sh
if [ "$1" = "log" ]; then
  printf 'grep backend unavailable\n' >&2
  exit 1
fi
exit 99
"#,
    );

    let err = {
        let mut sandbox = sandbox;
        sandbox.prepend_path(&bin_dir);
        sandbox.run(|| {
            lua.load(r#"return git_log_grep("x", "y")"#)
                .eval::<Vec<String>>()
                .unwrap_err()
        })
    };

    assert_lua_error_contains(err, &["git-log-grep-failed", "grep backend unavailable"]);
}

#[test]
fn parse_worktree_paths_reads_porcelain_worktree_lines() {
    let paths = parse_worktree_paths(
        b"worktree /repo\nHEAD abc\n\nworktree /repo/.worktrees/a\nHEAD def\n",
    );

    assert_eq!(paths, vec!["/repo", "/repo/.worktrees/a"]);
}

#[test]
fn setup_worktree_creates_under_runtime_worktrees() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree base");
    let path: String = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(".fkst/runtime");
            sandbox.set_env("FKST_CANDIDATE_PREFIX", "host-rc");
            sandbox.set_env("FKST_CANDIDATE_FROM_SEP", "__base__");
        },
        || {
            lua.load(r#"return setup_worktree("c3-test")"#)
                .eval()
                .unwrap()
        },
    );

    assert!(path.starts_with(".fkst/runtime/worktrees/c3-test-"));
    assert!(repo.path().join(&path).is_dir());
    assert!(!repo.path().join(".worktrees").exists());

    let worktree_branch = git_stdout(
        &repo.path().join(&path),
        &["rev-parse", "--abbrev-ref", "HEAD"],
    );
    assert_ne!(worktree_branch, "HEAD");
    assert!(
        worktree_branch.starts_with("host-rc-"),
        "branch={worktree_branch}"
    );
    assert!(
        worktree_branch.contains("-c3-test-") && worktree_branch.contains("__base__"),
        "branch={worktree_branch}"
    );
    let porcelain = git_stdout(repo.path(), &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains(&format!("branch refs/heads/{worktree_branch}")),
        "{porcelain}"
    );
}

#[test]
fn setup_worktree_uses_short_sha_parent_when_detached() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree detached base");
    let parent_sha = git_stdout(repo.path(), &["rev-parse", "--short=12", "HEAD"]);
    git(repo.path(), &["checkout", "--detach", "HEAD"]);

    let path: String = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(".fkst/runtime");
            sandbox.set_env("FKST_CANDIDATE_PREFIX", "host-rc");
            sandbox.set_env("FKST_CANDIDATE_FROM_SEP", "__base__");
        },
        || {
            lua.load(r#"return setup_worktree("detached-test")"#)
                .eval()
                .unwrap()
        },
    );

    let worktree_branch = git_stdout(
        &repo.path().join(&path),
        &["rev-parse", "--abbrev-ref", "HEAD"],
    );
    assert_ne!(worktree_branch, "HEAD");
    assert!(
        worktree_branch.ends_with(&format!("__base__{parent_sha}")),
        "branch={worktree_branch}"
    );
    let porcelain = git_stdout(repo.path(), &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains(&format!("branch refs/heads/{worktree_branch}")),
        "{porcelain}"
    );
}

#[test]
fn setup_worktree_prefers_env_over_fkst_env() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree tunable base");
    std::fs::write(
        repo.path().join("fkst.env"),
        "FKST_CANDIDATE_PREFIX=file-rc\nFKST_CANDIDATE_FROM_SEP=__file__\n",
    )
    .unwrap();

    let path: String = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(".fkst/runtime");
            sandbox.set_env("FKST_CANDIDATE_PREFIX", "env-rc");
            sandbox.set_env("FKST_CANDIDATE_FROM_SEP", "__env__");
        },
        || {
            lua.load(r#"return setup_worktree("tunable-test")"#)
                .eval()
                .unwrap()
        },
    );

    let worktree_branch = git_stdout(
        &repo.path().join(&path),
        &["rev-parse", "--abbrev-ref", "HEAD"],
    );
    assert!(
        worktree_branch.starts_with("env-rc-") && worktree_branch.contains("__env__"),
        "branch={worktree_branch}"
    );
    assert!(!worktree_branch.contains("file-rc"));
    assert!(!worktree_branch.contains("__file__"));
}

#[test]
fn setup_worktree_uses_fkst_env_without_env() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree tunable base");
    std::fs::write(
        repo.path().join("fkst.env"),
        "FKST_CANDIDATE_PREFIX=file-rc\nFKST_CANDIDATE_FROM_SEP=__file__\n",
    )
    .unwrap();

    let path: String = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(".fkst/runtime");
            sandbox.unset_env("FKST_CANDIDATE_PREFIX");
            sandbox.unset_env("FKST_CANDIDATE_FROM_SEP");
        },
        || {
            lua.load(r#"return setup_worktree("tunable-test")"#)
                .eval()
                .unwrap()
        },
    );

    let worktree_branch = git_stdout(
        &repo.path().join(&path),
        &["rev-parse", "--abbrev-ref", "HEAD"],
    );
    assert!(
        worktree_branch.starts_with("file-rc-") && worktree_branch.contains("__file__"),
        "branch={worktree_branch}"
    );
}

#[test]
fn setup_worktree_requires_runtime_root() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree base");
    let err = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.unset_env(fkst_common::runtime_layout::RUNTIME_ROOT_ENV);
            sandbox.set_env("FKST_CANDIDATE_PREFIX", "host-rc");
            sandbox.set_env("FKST_CANDIDATE_FROM_SEP", "__base__");
        },
        || {
            lua.load(r#"return setup_worktree("c3-test")"#)
                .eval::<String>()
                .unwrap_err()
        },
    );

    assert!(err.to_string().contains("FKST_RUNTIME_ROOT must be set"));
}

#[test]
fn setup_worktree_rejects_invalid_candidate_prefix_before_side_effects() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree base");
    std::fs::write(
        repo.path().join("fkst.env"),
        "FKST_CANDIDATE_PREFIX=bad prefix\nFKST_CANDIDATE_FROM_SEP=__base__\n",
    )
    .unwrap();
    let err = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(".fkst/runtime");
            sandbox.unset_env("FKST_CANDIDATE_PREFIX");
            sandbox.unset_env("FKST_CANDIDATE_FROM_SEP");
        },
        || {
            lua.load(r#"return setup_worktree("c3-test")"#)
                .eval::<String>()
                .unwrap_err()
        },
    );

    assert!(err
        .to_string()
        .contains("FKST_CANDIDATE_PREFIX is not a valid branch fragment"));
    assert!(!repo.path().join(".fkst/runtime/worktrees").exists());
}

#[test]
fn setup_worktree_requires_candidate_prefix_before_side_effects() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree base");
    std::fs::write(
        repo.path().join("fkst.env"),
        "FKST_CANDIDATE_FROM_SEP=__base__\n",
    )
    .unwrap();

    let err = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(".fkst/runtime");
            sandbox.unset_env("FKST_CANDIDATE_PREFIX");
            sandbox.unset_env("FKST_CANDIDATE_FROM_SEP");
        },
        || {
            lua.load(r#"return setup_worktree("c3-test")"#)
                .eval::<String>()
                .unwrap_err()
        },
    );

    assert_lua_error_contains(err, &["FKST_CANDIDATE_PREFIX missing"]);
    assert!(!repo.path().join(".fkst/runtime/worktrees").exists());
}

#[test]
fn setup_worktree_requires_candidate_from_separator_before_side_effects() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree base");
    std::fs::write(
        repo.path().join("fkst.env"),
        "FKST_CANDIDATE_PREFIX=file-rc\n",
    )
    .unwrap();

    let err = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(".fkst/runtime");
            sandbox.unset_env("FKST_CANDIDATE_PREFIX");
            sandbox.unset_env("FKST_CANDIDATE_FROM_SEP");
        },
        || {
            lua.load(r#"return setup_worktree("c3-test")"#)
                .eval::<String>()
                .unwrap_err()
        },
    );

    assert_lua_error_contains(err, &["FKST_CANDIDATE_FROM_SEP missing"]);
    assert!(!repo.path().join(".fkst/runtime/worktrees").exists());
}

#[test]
fn setup_worktree_creates_under_configured_runtime_worktrees() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree base");
    let runtime = tempdir().unwrap();

    let path: String = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
            sandbox.set_env("FKST_CANDIDATE_PREFIX", "host-rc");
            sandbox.set_env("FKST_CANDIDATE_FROM_SEP", "__base__");
        },
        || {
            lua.load(r#"return setup_worktree("c3-external")"#)
                .eval()
                .unwrap()
        },
    );

    assert!(path.starts_with(&format!(
        "{}/worktrees/c3-external-",
        runtime.path().display()
    )));
    assert!(Path::new(&path).is_dir());
    assert!(!repo.path().join(".fkst/runtime/worktrees").exists());
    assert!(!repo.path().join(".worktrees").exists());
}

#[test]
fn count_worktrees_returns_linked_worktree_count() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree base");
    let linked = repo.path().join(".fkst/runtime/worktrees/c3-count");
    git(
        repo.path(),
        &["worktree", "add", linked.to_str().unwrap(), "HEAD"],
    );

    let count: i64 = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(".fkst/runtime");
        },
        || lua.load(r#"return count_worktrees()"#).eval().unwrap(),
    );

    assert_eq!(count, 1);
}

#[cfg(unix)]
#[test]
fn count_worktrees_failure_raises_lua_error() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let sandbox = ProcessSandbox::new();
    let bin_dir = sandbox.temp_path("bin");
    install_git_script(
        &bin_dir,
        r#"#!/bin/sh
if [ "$1" = "worktree" ] && [ "$2" = "list" ] && [ "$3" = "--porcelain" ]; then
  printf 'worktree backend unavailable\n' >&2
  exit 2
fi
exit 99
"#,
    );

    let err = {
        let mut sandbox = sandbox;
        sandbox.prepend_path(&bin_dir);
        sandbox.run(|| {
            lua.load(r#"return count_worktrees()"#)
                .eval::<i64>()
                .unwrap_err()
        })
    };

    assert_lua_error_contains(
        err,
        &["git-worktree-list-failed", "worktree backend unavailable"],
    );
}

#[test]
fn list_orphan_worktrees_returns_matching_prefixed_paths() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree base");
    let matching = repo.path().join(".fkst/runtime/worktrees/c3-orphan-a");
    let other = repo.path().join(".fkst/runtime/worktrees/other-orphan-a");
    git(
        repo.path(),
        &["worktree", "add", matching.to_str().unwrap(), "HEAD"],
    );
    git(
        repo.path(),
        &["worktree", "add", other.to_str().unwrap(), "HEAD"],
    );

    let paths: Vec<String> = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(".fkst/runtime");
        },
        || {
            lua.load(r#"return list_orphan_worktrees("c3-orphan")"#)
                .eval()
                .unwrap()
        },
    );

    assert_eq!(
        paths,
        vec![std::fs::canonicalize(matching)
            .unwrap()
            .to_string_lossy()
            .to_string()]
    );
}

#[cfg(unix)]
#[test]
fn list_orphan_worktrees_failure_raises_lua_error() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let sandbox = ProcessSandbox::new();
    let bin_dir = sandbox.temp_path("bin");
    install_git_script(
        &bin_dir,
        r#"#!/bin/sh
if [ "$1" = "worktree" ] && [ "$2" = "list" ] && [ "$3" = "--porcelain" ]; then
  printf 'orphan backend unavailable\n' >&2
  exit 1
fi
exit 99
"#,
    );

    let err = {
        let mut sandbox = sandbox;
        sandbox.prepend_path(&bin_dir);
        sandbox.run(|| {
            lua.load(r#"return list_orphan_worktrees("c3-orphan")"#)
                .eval::<Vec<String>>()
                .unwrap_err()
        })
    };

    assert_lua_error_contains(
        err,
        &["git-worktree-list-failed", "orphan backend unavailable"],
    );
}

#[test]
fn list_orphan_worktrees_filters_configured_runtime_worktrees() {
    let lua = Lua::new();
    register(&lua).unwrap();
    let repo = repo_with_commit("worktree base");
    let runtime = tempdir().unwrap();
    let matching = runtime.path().join("worktrees/c3-external-a");
    let other = repo.path().join(".fkst/runtime/worktrees/c3-external-b");
    git(
        repo.path(),
        &["worktree", "add", matching.to_str().unwrap(), "HEAD"],
    );
    git(
        repo.path(),
        &["worktree", "add", other.to_str().unwrap(), "HEAD"],
    );

    let paths: Vec<String> = in_sandbox(
        repo.path(),
        |sandbox| {
            sandbox.runtime_root(runtime.path());
        },
        || {
            lua.load(r#"return list_orphan_worktrees("c3-external")"#)
                .eval()
                .unwrap()
        },
    );

    assert_eq!(
        paths,
        vec![std::fs::canonicalize(matching)
            .unwrap()
            .to_string_lossy()
            .to_string()]
    );
}

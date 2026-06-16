//! SDK: `now() -> integer`, `exec_sync(cmd | opts) -> {stdout, stderr, exit_code}`.
//!
//! `exec_sync` takes either a string command or an options table with `cwd`,
//! `env`, `timeout`, and optional read coalescing.

use mlua::{Lua, Result, Table, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use crate::config_registry::ConfigContext;
use crate::external_command::{
    CommandSpec, CommandStdin, MockCommandInvocation, MockCommandPlan, MockCommandResult,
    MockCommandState,
};
use crate::rate_pool::RatePoolRegistry;
use crate::read_coalesce::{CoalesceClock, ReadCoalesceOptions, ReadCoalescePlan};

struct ExecOptions {
    cmd: String,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    timeout: Option<Duration>,
    read_coalesce: Option<ReadCoalesceOptions>,
}

pub(crate) struct ExecResult {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
    pub(crate) timed_out: Option<bool>,
    pub(crate) error_class: Option<String>,
}

// Lua SDK registration and self-test match the fixed CLAUDE.md surface exactly; human notification, if needed, is represented through existing git/fs/log facts rather than a new SDK function.
pub fn register(lua: &Lua) -> Result<()> {
    let host_root = std::env::current_dir().map_err(mlua::Error::external)?;
    let config = ConfigContext::from_host_root(&host_root).map_err(mlua::Error::external)?;
    register_with_runner(lua, config, None)
}

pub(crate) fn register_with_runner(
    lua: &Lua,
    config: ConfigContext,
    runner: Option<MockCommandState>,
) -> Result<()> {
    let rate_pools = RatePoolRegistry::from_config(&config).map_err(mlua::Error::external)?;
    let host_root = config.host_root().to_path_buf();
    let coalesce_clock = CoalesceClock::system();
    lua.globals().set(
        "now",
        lua.create_function(|_, ()| {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(mlua::Error::external)?
                .as_secs();
            Ok(secs)
        })?,
    )?;

    lua.globals().set(
        "exec_sync",
        lua.create_function(move |lua, arg: Value| {
            crate::process_tree::ensure_supervisor_parent_alive()?;
            let opts = parse_exec_options(arg)?;
            let out = run_exec_sync_with_context(
                opts,
                runner.as_ref(),
                &rate_pools,
                &host_root,
                &coalesce_clock,
            )?;
            let t = lua.create_table()?;
            t.set("stdout", out.stdout)?;
            t.set("stderr", out.stderr)?;
            t.set("exit_code", out.exit_code)?;
            if let Some(timed_out) = out.timed_out {
                t.set("timed_out", timed_out)?;
            }
            if let Some(error_class) = out.error_class {
                t.set("error_class", error_class)?;
            }
            Ok(t)
        })?,
    )?;

    Ok(())
}

fn parse_exec_options(arg: Value) -> Result<ExecOptions> {
    match arg {
        Value::String(cmd) => Ok(ExecOptions {
            cmd: cmd.to_str()?.to_string(),
            cwd: None,
            env: Vec::new(),
            timeout: None,
            read_coalesce: None,
        }),
        Value::Table(table) => {
            let cmd: String = table.get("cmd")?;
            let cwd: Option<String> = table.get("cwd")?;
            let env = match table.get::<Option<Table>>("env")? {
                Some(env_table) => env_table
                    .pairs::<String, String>()
                    .collect::<Result<Vec<(String, String)>>>()?,
                None => Vec::new(),
            };
            let timeout = table
                .get::<Option<f64>>("timeout")?
                .map(|secs| Duration::from_secs_f64(secs.max(0.0)));
            let read_coalesce = parse_read_coalesce(table.get::<Option<Table>>("read_coalesce")?)?;

            Ok(ExecOptions {
                cmd,
                cwd,
                env,
                timeout,
                read_coalesce,
            })
        }
        other => Err(mlua::Error::external(format!(
            "exec_sync expects string or table, got {}",
            other.type_name()
        ))),
    }
}

fn parse_read_coalesce(table: Option<Table>) -> Result<Option<ReadCoalesceOptions>> {
    let Some(table) = table else {
        return Ok(None);
    };
    let key: String = table.get("key")?;
    let ttl_seconds: f64 = table.get("ttl_seconds")?;
    ReadCoalesceOptions::new(key, ttl_seconds)
        .map(Some)
        .map_err(mlua::Error::external)
}

#[cfg(test)]
fn run_exec_sync(
    opts: ExecOptions,
    runner: Option<&MockCommandState>,
    rate_pools: &RatePoolRegistry,
) -> Result<ExecResult> {
    let host_root = std::env::current_dir().map_err(mlua::Error::external)?;
    run_exec_sync_with_context(
        opts,
        runner,
        rate_pools,
        &host_root,
        &CoalesceClock::system(),
    )
}

fn run_exec_sync_with_context(
    opts: ExecOptions,
    runner: Option<&MockCommandState>,
    rate_pools: &RatePoolRegistry,
    host_root: &Path,
    coalesce_clock: &CoalesceClock,
) -> Result<ExecResult> {
    if let Some(runner) = runner {
        let invocation = MockCommandInvocation {
            rendered: opts.cmd.clone(),
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), opts.cmd.clone()],
            stdin: String::new(),
            cwd: opts.cwd.clone(),
            env: opts.env.clone(),
        };
        match runner.prepare(invocation.clone())? {
            MockCommandPlan::Return(result) => {
                return Ok(ExecResult {
                    stdout: result.stdout,
                    stderr: result.stderr,
                    exit_code: result.exit_code,
                    timed_out: None,
                    error_class: None,
                });
            }
            MockCommandPlan::Record => {
                rate_pools
                    .acquire_for_command_text(&opts.cmd)
                    .map_err(mlua::Error::external)?;
                let output = execute_spec(CommandSpec {
                    program: PathBuf::from("/bin/sh"),
                    args: vec!["-c".to_string(), opts.cmd],
                    cwd: opts.cwd.map(PathBuf::from),
                    env: opts.env,
                    stdin: CommandStdin::Null,
                    timeout: opts.timeout,
                    process_group: opts.timeout.is_some(),
                })?;
                runner.record(
                    invocation,
                    MockCommandResult {
                        stdout: output.stdout.clone(),
                        stderr: output.stderr.clone(),
                        exit_code: output.exit_code,
                    },
                )?;
                return Ok(output);
            }
        }
    }

    let timeout = opts.timeout;
    let command_text = opts.cmd.clone();
    let spec = CommandSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), command_text.clone()],
        cwd: opts.cwd.map(PathBuf::from),
        env: opts.env,
        stdin: CommandStdin::Null,
        timeout,
        process_group: timeout.is_some(),
    };

    if let Some(plan) = coalesce_plan(&opts.read_coalesce, &spec)? {
        if let Some(hit) = plan
            .get(host_root, coalesce_clock)
            .map_err(mlua::Error::external)?
        {
            return Ok(hit);
        }
        let _lock = plan
            .acquire_lock(host_root)
            .map_err(mlua::Error::external)?;
        if let Some(hit) = plan
            .get(host_root, coalesce_clock)
            .map_err(mlua::Error::external)?
        {
            return Ok(hit);
        }
        rate_pools
            .acquire_for_command_text(&command_text)
            .map_err(mlua::Error::external)?;
        let output = execute_spec(spec)?;
        plan.put_success(host_root, &output, coalesce_clock)
            .map_err(mlua::Error::external)?;
        return Ok(output);
    }

    rate_pools
        .acquire_for_command_text(&command_text)
        .map_err(mlua::Error::external)?;
    execute_spec(spec)
}

fn coalesce_plan(
    opts: &Option<ReadCoalesceOptions>,
    spec: &CommandSpec,
) -> Result<Option<ReadCoalescePlan>> {
    let Some(opts) = opts.clone() else {
        return Ok(None);
    };
    if !spec.stdin.is_empty_input() {
        return Ok(None);
    }
    let cwd =
        crate::read_coalesce::execution_cwd(spec.cwd.as_deref()).map_err(mlua::Error::external)?;
    let env: BTreeMap<String, String> = crate::read_coalesce::command_environment(&spec.env);
    ReadCoalescePlan::new(opts, &spec.program, &spec.args, &cwd, env, spec.timeout)
        .map(Some)
        .map_err(mlua::Error::external)
}

fn execute_spec(spec: CommandSpec) -> Result<ExecResult> {
    let timeout = spec.timeout;
    let output = crate::external_command::run_audited(spec).map_err(mlua::Error::external)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok(ExecResult {
        stdout: stdout.clone(),
        stderr: stderr.clone(),
        exit_code: output.exit_code,
        timed_out: timeout.map(|_| output.timed_out),
        error_class: if output.timed_out {
            Some(
                crate::boundary_resource::BoundaryErrorClass::ProviderUnavailable
                    .label()
                    .to_string(),
            )
        } else {
            crate::boundary_resource::classify_process_output(output.exit_code, &stdout, &stderr)
                .map(|class| class.label().to_string())
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_command::{
        CommandCassetteMode, CommandCassetteOptions, MockCommandResult, MockCommandState,
    };
    use crate::rate_pool::{RatePoolConfig, RatePoolRegistry};
    use mlua::Lua;
    #[cfg(unix)]
    use nix::errno::Errno;
    #[cfg(unix)]
    use nix::fcntl::{open, OFlag};
    #[cfg(unix)]
    use nix::sys::stat::Mode;
    #[cfg(unix)]
    use nix::unistd::mkfifo;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Barrier, Mutex, OnceLock,
    };

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(old) = self.old.take() {
                std::env::set_var(self.key, old);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn registry(root: &Path, name: &str) -> RatePoolRegistry {
        registry_with_burst(root, name, 1)
    }

    fn registry_with_burst(root: &Path, name: &str, burst: u64) -> RatePoolRegistry {
        RatePoolRegistry::for_test(
            root.to_path_buf(),
            BTreeMap::from([(
                name.to_string(),
                RatePoolConfig {
                    burst,
                    refill_per_minute: 1,
                },
            )]),
        )
    }

    fn seed_rate_bucket(root: &Path, name: &str, tokens: u64) {
        std::fs::write(
            root.join(format!("{name}.bucket")),
            format!("updated_nanos=0\ntokens={tokens}\nremainder_nanos=0\n"),
        )
        .unwrap();
    }

    fn read_rate_tokens(root: &Path, name: &str) -> u64 {
        std::fs::read_to_string(root.join(format!("{name}.bucket")))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("tokens="))
            .unwrap()
            .parse()
            .unwrap()
    }

    fn count_counter_lines(path: &Path) -> usize {
        std::fs::read_to_string(path).unwrap().lines().count()
    }

    fn install_counting_gh(root: &Path) -> PathBuf {
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
if [ -r "$COUNTER" ]; then
  IFS= read -r count < "$COUNTER"
else
  count=0
fi
count=$((count + 1))
printf '%s\n' "$count" > "$COUNTER"
printf 'value-%s\n' "$count"
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&gh, perms).unwrap();
        }
        bin
    }

    fn install_blocking_counting_gh(root: &Path) -> PathBuf {
        let bin = root.join("blocking-bin");
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
printf 'x\n' >> "$COUNTER"
/bin/sleep 1
printf 'stdout-1\n'
printf 'stderr-1\n' >&2
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&gh, perms).unwrap();
        }
        bin
    }

    fn coalesced_exec(
        cmd: &str,
        key: &str,
        ttl_seconds: f64,
        env: Vec<(String, String)>,
    ) -> ExecOptions {
        ExecOptions {
            cmd: cmd.to_string(),
            cwd: None,
            env,
            timeout: None,
            read_coalesce: Some(ReadCoalesceOptions::new(key.to_string(), ttl_seconds).unwrap()),
        }
    }

    fn test_clock(value: Arc<AtomicU64>) -> CoalesceClock {
        CoalesceClock::for_test(Arc::new(move || u128::from(value.load(Ordering::SeqCst))))
    }

    #[test]
    fn now_returns_positive_int() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let n: i64 = lua.load("return now()").eval().unwrap();
        assert!(n > 1_700_000_000, "now() returned {}", n);
    }

    #[test]
    fn exec_sync_echo_works() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let t: Table = lua
            .load(r#"return exec_sync("echo hello")"#)
            .eval()
            .unwrap();
        let stdout: String = t.get("stdout").unwrap();
        let exit_code: i64 = t.get("exit_code").unwrap();
        assert_eq!(exit_code, 0);
        assert!(stdout.contains("hello"));
    }

    #[test]
    fn exec_sync_nonzero_exit() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let t: Table = lua.load(r#"return exec_sync("exit 7")"#).eval().unwrap();
        let exit_code: i64 = t.get("exit_code").unwrap();
        assert_eq!(exit_code, 7);
    }

    #[test]
    fn exec_sync_table_cwd_works() {
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().to_string_lossy().to_string();
        let lua = Lua::new();
        register(&lua).unwrap();
        lua.globals().set("cwd", expected.clone()).unwrap();
        let t: Table = lua
            .load(r#"return exec_sync({ cmd = "pwd", cwd = cwd })"#)
            .eval()
            .unwrap();
        let stdout: String = t.get("stdout").unwrap();
        let exit_code: i64 = t.get("exit_code").unwrap();
        assert_eq!(exit_code, 0);
        assert_eq!(
            std::fs::canonicalize(stdout.trim()).unwrap(),
            std::fs::canonicalize(expected).unwrap()
        );
    }

    #[test]
    fn exec_sync_table_env_works() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let t: Table = lua
            .load(r#"return exec_sync({ cmd = "echo $X", env = { X = "from-env" } })"#)
            .eval()
            .unwrap();
        let stdout: String = t.get("stdout").unwrap();
        let exit_code: i64 = t.get("exit_code").unwrap();
        assert_eq!(exit_code, 0);
        assert_eq!(stdout.trim(), "from-env");
    }

    #[test]
    fn exec_sync_acquires_matching_rate_pool_token() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\nprintf gh-ok\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&gh, perms).unwrap();
        }
        let rate_pools = registry(dir.path(), "gh");
        std::fs::write(
            dir.path().join("gh.bucket"),
            "updated_nanos=0\ntokens=1\nremainder_nanos=0\n",
        )
        .unwrap();
        let out = run_exec_sync(
            ExecOptions {
                cmd: "gh --version".to_string(),
                cwd: None,
                env: vec![("PATH".to_string(), bin.to_string_lossy().into_owned())],
                timeout: None,
                read_coalesce: None,
            },
            None,
            &rate_pools,
        )
        .unwrap();

        assert_eq!(out.stdout, "gh-ok");
        assert!(dir.path().join("gh.bucket").is_file());
    }

    #[test]
    fn exec_sync_matches_program_basename_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("GH");
        std::fs::write(&gh, "#!/bin/sh\nprintf gh-ok\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&gh, perms).unwrap();
        }
        std::fs::write(
            dir.path().join("gh.bucket"),
            "updated_nanos=0\ntokens=1\nremainder_nanos=0\n",
        )
        .unwrap();
        let rate_pools = registry(dir.path(), "gh");
        let out = run_exec_sync(
            ExecOptions {
                cmd: format!("{} --version", gh.display()),
                cwd: None,
                env: Vec::new(),
                timeout: None,
                read_coalesce: None,
            },
            None,
            &rate_pools,
        )
        .unwrap();

        assert_eq!(out.stdout, "gh-ok");
        let ledger = std::fs::read_to_string(dir.path().join("gh.bucket")).unwrap();
        assert!(ledger.contains("tokens=0\n"), "{ledger}");
    }

    #[test]
    fn exec_sync_leaves_unmatched_command_without_pool_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let rate_pools = registry(dir.path(), "gh");
        let out = run_exec_sync(
            ExecOptions {
                cmd: "printf ok".to_string(),
                cwd: None,
                env: Vec::new(),
                timeout: None,
                read_coalesce: None,
            },
            None,
            &rate_pools,
        )
        .unwrap();

        assert_eq!(out.stdout, "ok");
        assert!(!dir.path().join("gh.bucket").exists());
    }

    #[test]
    fn exec_sync_mock_mode_bypasses_rate_pool() {
        let dir = tempfile::tempdir().unwrap();
        let rate_pools = registry(dir.path(), "gh");
        let runner = MockCommandState::new();
        runner
            .push_mock(
                "gh issue list".to_string(),
                MockCommandResult {
                    stdout: "[]\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .unwrap();

        let out = run_exec_sync(
            ExecOptions {
                cmd: "gh issue list --json number".to_string(),
                cwd: None,
                env: Vec::new(),
                timeout: None,
                read_coalesce: None,
            },
            Some(&runner),
            &rate_pools,
        )
        .unwrap();

        assert_eq!(out.stdout, "[]\n");
        assert!(!dir.path().join("gh.bucket").exists());
    }

    #[test]
    fn exec_sync_cassette_record_mode_consumes_rate_pool_token() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\nprintf '[]\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&gh, perms).unwrap();
        }
        seed_rate_bucket(dir.path(), "gh", 1);
        let rate_pools = registry(dir.path(), "gh");
        let runner = MockCommandState::new();
        runner
            .start_cassette(CommandCassetteOptions {
                path: dir.path().join("cassette.json"),
                mode: CommandCassetteMode::Record,
                redactions: Vec::new(),
            })
            .unwrap();

        let out = run_exec_sync(
            ExecOptions {
                cmd: "gh issue list --json number".to_string(),
                cwd: None,
                env: vec![("PATH".to_string(), bin.to_string_lossy().into_owned())],
                timeout: None,
                read_coalesce: None,
            },
            Some(&runner),
            &rate_pools,
        )
        .unwrap();
        runner.finish_cassette().unwrap();

        assert_eq!(out.stdout, "[]\n");
        let ledger = std::fs::read_to_string(dir.path().join("gh.bucket")).unwrap();
        assert!(ledger.contains("tokens=0\n"), "{ledger}");
    }

    #[test]
    fn exec_sync_read_coalesce_shares_success_and_rate_token_within_ttl() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let _runtime_guard = EnvGuard::set(fkst_common::runtime_layout::RUNTIME_ROOT_ENV, &runtime);
        let bin = install_counting_gh(dir.path());
        let counter = dir.path().join("counter");
        let rate_pools = registry_with_burst(dir.path(), "gh", 5);
        seed_rate_bucket(dir.path(), "gh", 5);
        let now = Arc::new(AtomicU64::new(0));
        let clock = test_clock(now);
        let env = vec![
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            (
                "COUNTER".to_string(),
                counter.to_string_lossy().into_owned(),
            ),
        ];

        let first = run_exec_sync_with_context(
            coalesced_exec(
                "gh issue list --json number",
                "github/issues",
                30.0,
                env.clone(),
            ),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();
        let second = run_exec_sync_with_context(
            coalesced_exec("gh issue list --json number", "github/issues", 30.0, env),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();

        assert_eq!(first.stdout, "value-1\n");
        assert_eq!(second.stdout, "value-1\n");
        assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "1");
        assert_eq!(read_rate_tokens(dir.path(), "gh"), 4);
    }

    #[test]
    fn exec_sync_read_coalesce_concurrent_callers_share_full_result_and_rate_token() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let _runtime_guard = EnvGuard::set(fkst_common::runtime_layout::RUNTIME_ROOT_ENV, &runtime);
        let bin = install_blocking_counting_gh(dir.path());
        let counter = dir.path().join("counter");
        let rate_pools = Arc::new(registry_with_burst(dir.path(), "gh", 2));
        seed_rate_bucket(dir.path(), "gh", 2);
        let now = Arc::new(AtomicU64::new(0));
        let clock = test_clock(now);
        let barrier = Arc::new(Barrier::new(2));
        let env = vec![
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            (
                "COUNTER".to_string(),
                counter.to_string_lossy().into_owned(),
            ),
        ];

        let mut handles = Vec::new();
        for _ in 0..2 {
            let barrier = barrier.clone();
            let rate_pools = rate_pools.clone();
            let host_root = dir.path().to_path_buf();
            let clock = clock.clone();
            let env = env.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                run_exec_sync_with_context(
                    coalesced_exec("gh issue list --json number", "github/issues", 30.0, env),
                    None,
                    &rate_pools,
                    &host_root,
                    &clock,
                )
                .unwrap()
            }));
        }

        let first = handles.remove(0).join().unwrap();
        let second = handles.remove(0).join().unwrap();

        assert_eq!(first.stdout, "stdout-1\n");
        assert_eq!(first.stderr, "stderr-1\n");
        assert_eq!(first.exit_code, 0);
        assert_eq!(first.timed_out, None);
        assert_eq!(first.error_class, None);
        assert_eq!(second.stdout, first.stdout);
        assert_eq!(second.stderr, first.stderr);
        assert_eq!(second.exit_code, first.exit_code);
        assert_eq!(second.timed_out, first.timed_out);
        assert_eq!(second.error_class, first.error_class);
        assert_eq!(count_counter_lines(&counter), 1);
        assert_eq!(read_rate_tokens(dir.path(), "gh"), 1);
    }

    #[test]
    fn exec_sync_read_coalesce_cached_hit_preserves_full_success_result() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let _runtime_guard = EnvGuard::set(fkst_common::runtime_layout::RUNTIME_ROOT_ENV, &runtime);
        let bin = install_blocking_counting_gh(dir.path());
        let counter = dir.path().join("counter");
        let rate_pools = registry_with_burst(dir.path(), "gh", 5);
        seed_rate_bucket(dir.path(), "gh", 5);
        let now = Arc::new(AtomicU64::new(0));
        let clock = test_clock(now);
        let env = vec![
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            (
                "COUNTER".to_string(),
                counter.to_string_lossy().into_owned(),
            ),
        ];
        let mut opts = coalesced_exec("gh issue list --json number", "github/issues", 30.0, env);
        opts.timeout = Some(Duration::from_secs(5));

        let first =
            run_exec_sync_with_context(opts, None, &rate_pools, dir.path(), &clock).unwrap();
        let mut cached_opts = coalesced_exec(
            "gh issue list --json number",
            "github/issues",
            30.0,
            vec![
                ("PATH".to_string(), bin.to_string_lossy().into_owned()),
                (
                    "COUNTER".to_string(),
                    counter.to_string_lossy().into_owned(),
                ),
            ],
        );
        cached_opts.timeout = Some(Duration::from_secs(5));
        let second =
            run_exec_sync_with_context(cached_opts, None, &rate_pools, dir.path(), &clock).unwrap();

        assert_eq!(first.stdout, "stdout-1\n");
        assert_eq!(first.stderr, "stderr-1\n");
        assert_eq!(first.exit_code, 0);
        assert_eq!(first.timed_out, Some(false));
        assert_eq!(first.error_class, None);
        assert_eq!(second.stdout, first.stdout);
        assert_eq!(second.stderr, first.stderr);
        assert_eq!(second.exit_code, first.exit_code);
        assert_eq!(second.timed_out, first.timed_out);
        assert_eq!(second.error_class, first.error_class);
        assert_eq!(count_counter_lines(&counter), 1);
        assert_eq!(read_rate_tokens(dir.path(), "gh"), 4);
    }

    #[test]
    fn exec_sync_read_coalesce_reexecutes_after_ttl_expiry() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let _runtime_guard = EnvGuard::set(fkst_common::runtime_layout::RUNTIME_ROOT_ENV, &runtime);
        let bin = install_counting_gh(dir.path());
        let counter = dir.path().join("counter");
        let rate_pools = registry_with_burst(dir.path(), "gh", 5);
        seed_rate_bucket(dir.path(), "gh", 5);
        let now = Arc::new(AtomicU64::new(0));
        let clock = test_clock(now.clone());
        let env = vec![
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            (
                "COUNTER".to_string(),
                counter.to_string_lossy().into_owned(),
            ),
        ];

        let first = run_exec_sync_with_context(
            coalesced_exec("gh issue list", "github/issues", 1.0, env.clone()),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();
        let second = run_exec_sync_with_context(
            coalesced_exec("gh issue list", "github/issues", 1.0, env.clone()),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();
        now.store(1_000_000_001, Ordering::SeqCst);
        let third = run_exec_sync_with_context(
            coalesced_exec("gh issue list", "github/issues", 1.0, env),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();

        assert_eq!(first.stdout, "value-1\n");
        assert_eq!(second.stdout, "value-1\n");
        assert_eq!(third.stdout, "value-2\n");
        assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "2");
        assert_eq!(read_rate_tokens(dir.path(), "gh"), 3);
    }

    #[test]
    fn exec_sync_read_coalesce_clamps_ttl_seconds_to_300() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let _runtime_guard = EnvGuard::set(fkst_common::runtime_layout::RUNTIME_ROOT_ENV, &runtime);
        let bin = install_counting_gh(dir.path());
        let counter = dir.path().join("counter");
        let rate_pools = registry_with_burst(dir.path(), "gh", 5);
        seed_rate_bucket(dir.path(), "gh", 5);
        let now = Arc::new(AtomicU64::new(0));
        let clock = test_clock(now.clone());
        let env = vec![
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            (
                "COUNTER".to_string(),
                counter.to_string_lossy().into_owned(),
            ),
        ];

        let first = run_exec_sync_with_context(
            coalesced_exec("gh issue list", "github/issues", 999.0, env.clone()),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();
        now.store(299_000_000_000, Ordering::SeqCst);
        let second = run_exec_sync_with_context(
            coalesced_exec("gh issue list", "github/issues", 999.0, env.clone()),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();
        now.store(300_000_000_000, Ordering::SeqCst);
        let third = run_exec_sync_with_context(
            coalesced_exec("gh issue list", "github/issues", 999.0, env),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();

        assert_eq!(first.stdout, "value-1\n");
        assert_eq!(second.stdout, "value-1\n");
        assert_eq!(third.stdout, "value-2\n");
        assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "2");
        assert_eq!(read_rate_tokens(dir.path(), "gh"), 3);
    }

    #[test]
    fn exec_sync_read_coalesce_refuses_specs_with_stdin_input() {
        let opts = Some(ReadCoalesceOptions::new("github/issues".to_string(), 30.0).unwrap());
        let base_spec = CommandSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "cat".to_string()],
            cwd: None,
            env: Vec::new(),
            stdin: CommandStdin::Null,
            timeout: None,
            process_group: false,
        };
        assert!(coalesce_plan(&opts, &base_spec).unwrap().is_some());

        let mut bytes_spec = base_spec.clone();
        bytes_spec.stdin = CommandStdin::Bytes(b"first\n".to_vec());
        assert!(coalesce_plan(&opts, &bytes_spec).unwrap().is_none());

        let mut inherit_spec = base_spec;
        inherit_spec.stdin = CommandStdin::Inherit;
        assert!(coalesce_plan(&opts, &inherit_spec).unwrap().is_none());
    }

    #[test]
    fn exec_sync_read_coalesce_fingerprint_splits_key_args_cwd_and_env() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let _runtime_guard = EnvGuard::set(fkst_common::runtime_layout::RUNTIME_ROOT_ENV, &runtime);
        let bin = install_counting_gh(dir.path());
        let cwd_a = dir.path().join("cwd-a");
        let cwd_b = dir.path().join("cwd-b");
        std::fs::create_dir(&cwd_a).unwrap();
        std::fs::create_dir(&cwd_b).unwrap();
        let counter = dir.path().join("counter");
        let rate_pools = registry_with_burst(dir.path(), "gh", 10);
        seed_rate_bucket(dir.path(), "gh", 10);
        let now = Arc::new(AtomicU64::new(0));
        let clock = test_clock(now);
        let base_env = vec![
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            (
                "COUNTER".to_string(),
                counter.to_string_lossy().into_owned(),
            ),
            ("FINGERPRINT_ENV".to_string(), "a".to_string()),
        ];

        let mut same = coalesced_exec("gh issue list", "github/issues", 30.0, base_env.clone());
        same.cwd = Some(cwd_a.to_string_lossy().into_owned());
        assert_eq!(
            run_exec_sync_with_context(same, None, &rate_pools, dir.path(), &clock)
                .unwrap()
                .stdout,
            "value-1\n"
        );

        let mut different_key =
            coalesced_exec("gh issue list", "github/pulls", 30.0, base_env.clone());
        different_key.cwd = Some(cwd_a.to_string_lossy().into_owned());
        assert_eq!(
            run_exec_sync_with_context(different_key, None, &rate_pools, dir.path(), &clock)
                .unwrap()
                .stdout,
            "value-2\n"
        );

        let mut different_args =
            coalesced_exec("gh pr list", "github/issues", 30.0, base_env.clone());
        different_args.cwd = Some(cwd_a.to_string_lossy().into_owned());
        assert_eq!(
            run_exec_sync_with_context(different_args, None, &rate_pools, dir.path(), &clock)
                .unwrap()
                .stdout,
            "value-3\n"
        );

        let mut different_cwd =
            coalesced_exec("gh issue list", "github/issues", 30.0, base_env.clone());
        different_cwd.cwd = Some(cwd_b.to_string_lossy().into_owned());
        assert_eq!(
            run_exec_sync_with_context(different_cwd, None, &rate_pools, dir.path(), &clock)
                .unwrap()
                .stdout,
            "value-4\n"
        );

        let mut different_env = base_env;
        different_env.retain(|(key, _)| key != "FINGERPRINT_ENV");
        different_env.push(("FINGERPRINT_ENV".to_string(), "b".to_string()));
        let mut env_split = coalesced_exec("gh issue list", "github/issues", 30.0, different_env);
        env_split.cwd = Some(cwd_a.to_string_lossy().into_owned());
        assert_eq!(
            run_exec_sync_with_context(env_split, None, &rate_pools, dir.path(), &clock)
                .unwrap()
                .stdout,
            "value-5\n"
        );
        assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "5");
        assert_eq!(read_rate_tokens(dir.path(), "gh"), 5);
    }

    #[test]
    #[cfg(unix)]
    fn exec_sync_read_coalesce_cwd_fingerprint_uses_raw_exec_cwd() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let _runtime_guard = EnvGuard::set(fkst_common::runtime_layout::RUNTIME_ROOT_ENV, &runtime);
        let bin = install_counting_gh(dir.path());
        let real_cwd = dir.path().join("real-cwd");
        let symlink_cwd = dir.path().join("symlink-cwd");
        std::fs::create_dir(&real_cwd).unwrap();
        std::os::unix::fs::symlink(&real_cwd, &symlink_cwd).unwrap();
        assert_eq!(
            real_cwd.canonicalize().unwrap(),
            symlink_cwd.canonicalize().unwrap()
        );
        let counter = dir.path().join("counter");
        let rate_pools = registry_with_burst(dir.path(), "gh", 5);
        seed_rate_bucket(dir.path(), "gh", 5);
        let now = Arc::new(AtomicU64::new(0));
        let clock = test_clock(now);
        let env = vec![
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            (
                "COUNTER".to_string(),
                counter.to_string_lossy().into_owned(),
            ),
        ];

        let mut real = coalesced_exec("gh issue list", "github/issues", 30.0, env.clone());
        real.cwd = Some(real_cwd.to_string_lossy().into_owned());
        let first =
            run_exec_sync_with_context(real, None, &rate_pools, dir.path(), &clock).unwrap();

        let mut symlink = coalesced_exec("gh issue list", "github/issues", 30.0, env);
        symlink.cwd = Some(symlink_cwd.to_string_lossy().into_owned());
        let second =
            run_exec_sync_with_context(symlink, None, &rate_pools, dir.path(), &clock).unwrap();

        assert_eq!(first.stdout, "value-1\n");
        assert_eq!(second.stdout, "value-2\n");
        assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "2");
        assert_eq!(read_rate_tokens(dir.path(), "gh"), 3);
    }

    #[test]
    fn exec_sync_read_coalesce_does_not_cache_nonzero_exit() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let _runtime_guard = EnvGuard::set(fkst_common::runtime_layout::RUNTIME_ROOT_ENV, &runtime);
        let bin = install_counting_gh(dir.path());
        let counter = dir.path().join("counter");
        let rate_pools = registry_with_burst(dir.path(), "gh", 5);
        seed_rate_bucket(dir.path(), "gh", 5);
        let now = Arc::new(AtomicU64::new(0));
        let clock = test_clock(now);
        let env = vec![
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            (
                "COUNTER".to_string(),
                counter.to_string_lossy().into_owned(),
            ),
        ];

        let first = run_exec_sync_with_context(
            coalesced_exec("gh issue list; exit 7", "github/issues", 30.0, env.clone()),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();
        let second = run_exec_sync_with_context(
            coalesced_exec("gh issue list; exit 7", "github/issues", 30.0, env),
            None,
            &rate_pools,
            dir.path(),
            &clock,
        )
        .unwrap();

        assert_eq!(first.exit_code, 7);
        assert_eq!(second.exit_code, 7);
        assert_eq!(first.stdout, "value-1\n");
        assert_eq!(second.stdout, "value-2\n");
        assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "2");
        assert_eq!(read_rate_tokens(dir.path(), "gh"), 3);
    }

    #[test]
    fn exec_sync_without_read_coalesce_always_runs() {
        let dir = tempfile::tempdir().unwrap();
        let bin = install_counting_gh(dir.path());
        let counter = dir.path().join("counter");
        let rate_pools = registry_with_burst(dir.path(), "gh", 5);
        seed_rate_bucket(dir.path(), "gh", 5);
        let env = vec![
            ("PATH".to_string(), bin.to_string_lossy().into_owned()),
            (
                "COUNTER".to_string(),
                counter.to_string_lossy().into_owned(),
            ),
        ];

        let first = run_exec_sync(
            ExecOptions {
                cmd: "gh issue list".to_string(),
                cwd: None,
                env: env.clone(),
                timeout: None,
                read_coalesce: None,
            },
            None,
            &rate_pools,
        )
        .unwrap();
        let second = run_exec_sync(
            ExecOptions {
                cmd: "gh issue list".to_string(),
                cwd: None,
                env,
                timeout: None,
                read_coalesce: None,
            },
            None,
            &rate_pools,
        )
        .unwrap();

        assert_eq!(first.stdout, "value-1\n");
        assert_eq!(second.stdout, "value-2\n");
        assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "2");
        assert_eq!(read_rate_tokens(dir.path(), "gh"), 3);
    }

    #[test]
    fn exec_sync_mock_mode_bypasses_read_coalesce() {
        let dir = tempfile::tempdir().unwrap();
        let rate_pools = registry(dir.path(), "gh");
        let runner = MockCommandState::new();
        runner
            .push_mock(
                "gh issue list".to_string(),
                MockCommandResult {
                    stdout: "first\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .unwrap();
        runner
            .push_mock(
                "gh issue list".to_string(),
                MockCommandResult {
                    stdout: "second\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .unwrap();

        let first = run_exec_sync(
            coalesced_exec(
                "gh issue list --json number",
                "github/issues",
                30.0,
                Vec::new(),
            ),
            Some(&runner),
            &rate_pools,
        )
        .unwrap();
        let second = run_exec_sync(
            coalesced_exec(
                "gh issue list --json number",
                "github/issues",
                30.0,
                Vec::new(),
            ),
            Some(&runner),
            &rate_pools,
        )
        .unwrap();
        let calls = runner.calls().unwrap();

        assert_eq!(first.stdout, "first\n");
        assert_eq!(second.stdout, "second\n");
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn exec_sync_gh_emits_one_external_command_audit_line() {
        let dir = tempfile::tempdir().unwrap();
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = lines.clone();
        let _sink = crate::external_command::install_audit_sink_for_test(std::sync::Arc::new(
            move |line| captured.lock().unwrap().push(line.to_string()),
        ));
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\nprintf '[]\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&gh, perms).unwrap();
        }
        let rate_pools = registry(dir.path(), "none");
        let out = run_exec_sync(
            ExecOptions {
                cmd: "gh issue list".to_string(),
                cwd: None,
                env: vec![("PATH".to_string(), bin.to_string_lossy().into_owned())],
                timeout: None,
                read_coalesce: None,
            },
            None,
            &rate_pools,
        )
        .unwrap();

        assert_eq!(out.stdout, "[]\n");
        let lines = lines.lock().unwrap();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("EVENT=external_command"))
                .count(),
            1,
            "{lines:?}"
        );
    }

    #[test]
    fn exec_sync_table_timeout_works() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let t: Table = lua
            .load(r#"return exec_sync({ cmd = "sleep 10", timeout = 1 })"#)
            .eval()
            .unwrap();
        let exit_code: i64 = t.get("exit_code").unwrap();
        let timed_out: bool = t.get("timed_out").unwrap();
        assert_eq!(exit_code, 124);
        assert!(timed_out);
    }

    #[cfg(unix)]
    #[test]
    fn exec_sync_timeout_kills_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("probe.fifo");
        let pidfile = dir.path().join("child.pid");
        mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

        let lua = Lua::new();
        register(&lua).unwrap();
        lua.globals()
            .set(
                "cmd",
                format!(
                    "(read _ < '{}') & echo $! > '{}'; wait",
                    fifo.display(),
                    pidfile.display()
                ),
            )
            .unwrap();
        let t: Table = lua
            .load(r#"return exec_sync({ cmd = cmd, timeout = 0.2 })"#)
            .eval()
            .unwrap();
        let exit_code: i64 = t.get("exit_code").unwrap();
        let timed_out: bool = t.get("timed_out").unwrap();
        assert_eq!(exit_code, 124);
        assert!(timed_out);
        assert!(pidfile.exists());

        let err = open(&fifo, OFlag::O_WRONLY | OFlag::O_NONBLOCK, Mode::empty())
            .expect_err("fifo writer should have no live reader after timeout");
        assert_eq!(err, Errno::ENXIO);
    }
}

//! SDK: `now() -> integer`, `exec_sync(cmd | opts) -> {stdout, stderr, exit_code}`.
//!
//! `exec_sync` takes either a string command or an options table with `cwd`,
//! `env`, and `timeout`.

use mlua::{Lua, Result, Table, Value};
use std::path::PathBuf;
use std::time::Duration;

use crate::config_registry::ConfigContext;
use crate::external_command::{CommandSpec, MockCommandState};
use crate::rate_pool::RatePoolRegistry;

struct ExecOptions {
    cmd: String,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    timeout: Option<Duration>,
}

struct ExecResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: Option<bool>,
    error_class: Option<String>,
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
            let out = run_exec_sync(opts, runner.as_ref(), &rate_pools)?;
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

            Ok(ExecOptions {
                cmd,
                cwd,
                env,
                timeout,
            })
        }
        other => Err(mlua::Error::external(format!(
            "exec_sync expects string or table, got {}",
            other.type_name()
        ))),
    }
}

fn run_exec_sync(
    opts: ExecOptions,
    runner: Option<&MockCommandState>,
    rate_pools: &RatePoolRegistry,
) -> Result<ExecResult> {
    if let Some(runner) = runner {
        let result = runner.execute(
            opts.cmd.clone(),
            "/bin/sh".to_string(),
            vec!["-c".to_string(), opts.cmd],
            String::new(),
        )?;
        return Ok(ExecResult {
            stdout: result.stdout,
            stderr: result.stderr,
            exit_code: result.exit_code,
            timed_out: None,
            error_class: None,
        });
    }

    rate_pools
        .acquire_for_command_text(&opts.cmd)
        .map_err(mlua::Error::external)?;

    let timeout = opts.timeout;
    let output = crate::external_command::run_audited(CommandSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), opts.cmd],
        cwd: opts.cwd.map(PathBuf::from),
        env: opts.env,
        timeout,
        process_group: timeout.is_some(),
    })
    .map_err(mlua::Error::external)?;
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
    use crate::external_command::{MockCommandResult, MockCommandState};
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

    fn registry(root: &Path, name: &str) -> RatePoolRegistry {
        RatePoolRegistry::for_test(
            root.to_path_buf(),
            BTreeMap::from([(
                name.to_string(),
                RatePoolConfig {
                    burst: 1,
                    refill_per_minute: 1,
                },
            )]),
        )
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
            },
            Some(&runner),
            &rate_pools,
        )
        .unwrap();

        assert_eq!(out.stdout, "[]\n");
        assert!(!dir.path().join("gh.bucket").exists());
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

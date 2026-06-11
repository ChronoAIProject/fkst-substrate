//! SDK: `now() -> integer`, `exec_sync(cmd | opts) -> {stdout, stderr, exit_code}`.
//!
//! `exec_sync` takes either a string command or an options table with `cwd`,
//! `env`, and `timeout`.

use mlua::{Lua, Result, Table, Value};
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config_registry::ConfigContext;
use crate::external_command::MockCommandState;
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

fn build_command(opts: &ExecOptions) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(&opts.cmd);
    if let Some(cwd) = &opts.cwd {
        command.current_dir(cwd);
    }
    for (key, value) in &opts.env {
        command.env(key, value);
    }
    command
}

#[cfg(unix)]
fn make_process_group_leader(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn make_process_group_leader(_command: &mut Command) {}

#[cfg(unix)]
fn kill_process_group(child_pid: u32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    let _ = killpg(Pid::from_raw(child_pid as i32), Signal::SIGKILL);
}

#[cfg(not(unix))]
fn kill_process_group(_child_pid: u32) {}

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

    match opts.timeout {
        Some(timeout) => run_exec_sync_with_timeout(&opts, timeout),
        None => {
            let out = build_command(&opts)
                .output()
                .map_err(mlua::Error::external)?;
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let exit_code = out.status.code().unwrap_or(-1);
            Ok(ExecResult {
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                exit_code,
                timed_out: None,
                error_class: crate::boundary_resource::classify_process_output(
                    exit_code, &stdout, &stderr,
                )
                .map(|class| class.label().to_string()),
            })
        }
    }
}

fn run_exec_sync_with_timeout(opts: &ExecOptions, timeout: Duration) -> Result<ExecResult> {
    let mut command = build_command(opts);
    make_process_group_leader(&mut command);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(mlua::Error::external)?;
    let child_pid = child.id();
    let stdout_reader = child
        .stdout
        .take()
        .map(read_pipe_in_thread)
        .ok_or_else(|| mlua::Error::external("failed to capture stdout"))?;
    let stderr_reader = child
        .stderr
        .take()
        .map(read_pipe_in_thread)
        .ok_or_else(|| mlua::Error::external("failed to capture stderr"))?;

    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(mlua::Error::external)? {
            let stdout = join_pipe_reader(stdout_reader)?;
            let stderr = join_pipe_reader(stderr_reader)?;
            let stdout = String::from_utf8_lossy(&stdout).to_string();
            let stderr = String::from_utf8_lossy(&stderr).to_string();
            let exit_code = status.code().unwrap_or(-1);
            return Ok(ExecResult {
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                exit_code,
                timed_out: Some(false),
                error_class: crate::boundary_resource::classify_process_output(
                    exit_code, &stdout, &stderr,
                )
                .map(|class| class.label().to_string()),
            });
        }

        if start.elapsed() >= timeout {
            kill_process_group(child_pid);
            let _ = child.wait();
            let stdout = join_pipe_reader(stdout_reader)?;
            let stderr = join_pipe_reader(stderr_reader)?;
            let stdout = String::from_utf8_lossy(&stdout).to_string();
            let stderr = String::from_utf8_lossy(&stderr).to_string();
            return Ok(ExecResult {
                stdout,
                stderr,
                exit_code: 124,
                timed_out: Some(true),
                error_class: Some(
                    crate::boundary_resource::BoundaryErrorClass::ProviderUnavailable
                        .label()
                        .to_string(),
                ),
            });
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_pipe_in_thread<R>(mut pipe: R) -> JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        pipe.read_to_end(&mut buf)?;
        Ok(buf)
    })
}

fn join_pipe_reader(reader: JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| mlua::Error::external("pipe reader thread panicked"))?
        .map_err(mlua::Error::external)
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

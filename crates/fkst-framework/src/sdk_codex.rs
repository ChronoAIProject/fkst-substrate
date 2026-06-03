//! SDK: spawn_codex_sync wraps `codex exec` with an fcntl permit pool.
//!
//! Acquire one slot from `<runtime>/codex-permits/{0..N-1}` by trying each fd's flock until one
//! locks exclusive non-blocking. If none available, block on the first one. Permit
//! holds for the duration of the codex subprocess; released on file drop after exit.

use fkst_common::RuntimeKind;
use mlua::{AnyUserData, Lua, Result, Table, UserData};
use nix::fcntl::{flock, FlockArg};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config_registry::{ConfigContext, ConfigKey};
use crate::runtime_context;

pub(crate) const CODEX_PERMIT_SLOTS_ENV: &str = "FKST_CODEX_PERMIT_SLOTS";
pub const RUNTIME_LOG_DIR_ENV: &str = "FKST_RUNTIME_LOG_DIR";
const DEFAULT_STALL_WINDOW_SECONDS: i64 = 900;
static NEXT_PIPELINE_OWNER_ID: AtomicU64 = AtomicU64::new(1);

// both sync and async SDK calls share one immutable request description.
struct CodexRequest {
    prompt: String,
    context: Option<String>,
    worktree: Option<String>,
    stall_window_seconds: i64,
    log_path: PathBuf,
}

// worker threads return the same result shape before Lua table conversion.
// crate-internal test seams expose only data needed by extracted integration tests.
pub(crate) struct CodexResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
    log_path: String,
    error_kind: Option<String>,
    error: Option<String>,
}

// spawn_codex returns a pipeline-local opaque handle consumed by await_all.
// crate-internal fields remain hidden from Lua while extracted tests can verify handle validation.
pub(crate) struct CodexTaskHandle {
    pub(crate) owner_id: u64,
    pub(crate) task_id: u64,
    pub(crate) join: Arc<Mutex<Option<JoinHandle<CodexResult>>>>,
}

impl UserData for CodexTaskHandle {}

impl Drop for CodexTaskHandle {
    fn drop(&mut self) {
        let Ok(mut join) = self.join.lock() else {
            return;
        };
        if let Some(join) = join.take() {
            let _ = join.join();
        }
    }
}

// each stream is tagged as live activity while the child is still running.
#[derive(Clone, Copy)]
enum CodexStream {
    Stdout,
    Stderr,
}

// output chunks and process exit share one liveness channel.
enum CodexEvent {
    Output(CodexStream, Vec<u8>),
    Exited(std::result::Result<ExitStatus, String>),
}

// wait outcomes distinguish natural exit from no-output stall failure.
enum CodexWaitOutcome {
    Exited {
        stdout: String,
        stderr: String,
        exit_code: i32,
    },
    Failed {
        kind: &'static str,
        message: String,
        stdout: String,
        stderr: String,
        exit_code: i32,
    },
}

impl CodexResult {
    // crate-internal constructors support extracted tests without adding Lua SDK surface.
    pub(crate) fn success(
        stdout: String,
        stderr: String,
        exit_code: i32,
        log_path: String,
    ) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
            log_path,
            error_kind: None,
            error: None,
        }
    }

    fn failure(
        kind: &str,
        message: String,
        stdout: String,
        stderr: String,
        exit_code: i32,
        log_path: String,
    ) -> Self {
        let combined_stderr = if stderr.trim().is_empty() {
            message.clone()
        } else {
            format!("{stderr}\n{message}")
        };
        Self {
            stdout,
            stderr: combined_stderr,
            exit_code,
            log_path,
            error_kind: Some(kind.to_string()),
            error: Some(message),
        }
    }

    fn into_lua_table(self, lua: &Lua) -> Result<Table> {
        let t = result_table(lua, self.stdout, self.stderr, self.exit_code, self.log_path)?;
        if let Some(kind) = self.error_kind {
            t.set("error_kind", kind)?;
        }
        if let Some(message) = self.error {
            t.set("error", message)?;
        }
        Ok(t)
    }
}

fn result_table(
    lua: &Lua,
    stdout: String,
    stderr: String,
    exit_code: i32,
    log_path: String,
) -> Result<Table> {
    let t = lua.create_table()?;
    t.set("stdout", stdout)?;
    t.set("stderr", stderr)?;
    t.set("exit_code", exit_code)?;
    t.set("log_path", log_path)?;
    Ok(t)
}

pub fn register(lua: &Lua, host_root: &Path, config: ConfigContext) -> Result<()> {
    let owner_id = NEXT_PIPELINE_OWNER_ID.fetch_add(1, Ordering::Relaxed);
    let next_task_id = Arc::new(AtomicU64::new(1));
    let host_root = Arc::new(host_root.to_path_buf());
    let config = Arc::new(config);

    lua.globals().set("spawn_codex_sync", {
        let host_root = Arc::clone(&host_root);
        let config = Arc::clone(&config);
        lua.create_function(move |lua, opts: Table| {
            let request = codex_request_from_opts(opts);
            run_codex_request(request, &host_root, &config).into_lua_table(lua)
        })?
    })?;
    lua.globals().set("spawn_codex", {
        let next_task_id = Arc::clone(&next_task_id);
        let host_root = Arc::clone(&host_root);
        let config = Arc::clone(&config);
        lua.create_function(move |_, opts: Table| {
            let request = codex_request_from_opts(opts);
            let task_id = next_task_id.fetch_add(1, Ordering::Relaxed);
            let host_root = Arc::clone(&host_root);
            let config = Arc::clone(&config);
            let join = std::thread::spawn(move || run_codex_request(request, &host_root, &config));
            Ok(CodexTaskHandle {
                owner_id,
                task_id,
                join: Arc::new(Mutex::new(Some(join))),
            })
        })?
    })?;
    lua.globals().set(
        "await_all",
        lua.create_function(move |lua, handles: Table| await_all(lua, handles, owner_id))?,
    )?;
    Ok(())
}

// the same input names a no-output stall window with a bounded default.
fn codex_request_from_opts(opts: Table) -> CodexRequest {
    let prompt: String = opts.get("prompt").unwrap_or_default();
    let context: Option<String> = opts.get("context").ok();
    let worktree: Option<String> = opts.get("worktree").ok();
    let timeout: Option<i64> = opts.get("timeout").ok();
    let stall_window_seconds = timeout
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_STALL_WINDOW_SECONDS);
    let log_path = codex_log_path(worktree.as_deref());
    CodexRequest {
        prompt,
        context,
        worktree,
        stall_window_seconds,
        log_path,
    }
}

fn await_all(lua: &Lua, handles: Table, owner_id: u64) -> Result<Table> {
    let mut seen = HashSet::new();
    let mut tasks: Vec<Arc<Mutex<Option<JoinHandle<CodexResult>>>>> = Vec::new();
    let mut invalid: Option<String> = None;

    for index in 1..=handles.raw_len() {
        let handle_ud = match handles.raw_get::<AnyUserData>(index) {
            Ok(handle_ud) => handle_ud,
            Err(_) => {
                invalid = Some(format!("await_all invalid handle at index {index}"));
                continue;
            }
        };
        let handle = match handle_ud.borrow::<CodexTaskHandle>() {
            Ok(handle) => handle,
            Err(_) => {
                invalid = Some(format!("await_all invalid handle at index {index}"));
                continue;
            }
        };
        if handle.owner_id != owner_id {
            return Err(mlua::Error::external(
                "CodexTaskHandle belongs to a different pipeline",
            ));
        }
        if !seen.insert(handle.task_id) {
            return Err(mlua::Error::external(
                "CodexTaskHandle was reused in the same await_all call",
            ));
        }
        let task = {
            match handle.join.lock() {
                Ok(join) if join.is_some() => Arc::clone(&handle.join),
                Ok(_) => {
                    return Err(mlua::Error::external(
                        "CodexTaskHandle was already consumed by await_all",
                    ));
                }
                Err(_) => {
                    return Err(mlua::Error::external(
                        "CodexTaskHandle state lock is poisoned",
                    ));
                }
            }
        };
        tasks.push(task);
    }

    let results = lua.create_table()?;
    for (index, task) in tasks.into_iter().enumerate() {
        let join = task
            .lock()
            .map_err(|_| mlua::Error::external("CodexTaskHandle state lock is poisoned"))?
            .take()
            .ok_or_else(|| {
                mlua::Error::external("CodexTaskHandle was already consumed by await_all")
            })?;
        let result = match join.join() {
            Ok(result) => result,
            Err(_) => CodexResult::failure(
                "thread",
                "codex task thread panicked".to_string(),
                String::new(),
                String::new(),
                -1,
                String::new(),
            ),
        };
        results.set(index + 1, result.into_lua_table(lua)?)?;
    }
    if let Some(message) = invalid {
        return Err(mlua::Error::external(message));
    }
    Ok(results)
}

// sync and async calls share permit acquisition while preserving P11 lifetime.
fn run_codex_request(
    request: CodexRequest,
    host_root: &Path,
    config: &ConfigContext,
) -> CodexResult {
    if let Err(err) = ensure_pool_with_context(host_root, config) {
        let message = format!("codex permit pool failed: {err}");
        write_codex_log(
            &request.log_path,
            "",
            &message,
            -1,
            &command_line_for_request(&request),
            request.stall_window_seconds,
        );
        return CodexResult::failure(
            "permit",
            message,
            String::new(),
            String::new(),
            -1,
            request.log_path.to_string_lossy().into_owned(),
        );
    }
    let _permit = match acquire_permit_with_context(host_root, config) {
        Ok(permit) => permit,
        Err(err) => {
            let message = format!("codex permit acquire failed: {err}");
            write_codex_log(
                &request.log_path,
                "",
                &message,
                -1,
                &command_line_for_request(&request),
                request.stall_window_seconds,
            );
            return CodexResult::failure(
                "permit",
                message,
                String::new(),
                String::new(),
                -1,
                request.log_path.to_string_lossy().into_owned(),
            );
        }
    };

    run_codex_request_with_permit(request)
}

fn run_codex_request_with_permit(request: CodexRequest) -> CodexResult {
    let (mut cmd, cmd_line) = command_for_request(&request);
    eprintln!(
        "SPAWN: log={} cd={} stall_window={}",
        request.log_path.display(),
        request.worktree.as_deref().unwrap_or("."),
        request.stall_window_seconds
    );

    let child = match spawn_codex_child(&request, &mut cmd, &cmd_line) {
        Ok(child) => child,
        Err(result) => return *result,
    };
    let child = match write_prompt_to_child(child, &request, &cmd_line) {
        Ok(child) => child,
        Err(result) => return *result,
    };

    wait_for_codex_child(child, &request, &cmd_line)
}

// only the rare error path is boxed while preserving the success path shape.
fn spawn_codex_child(
    request: &CodexRequest,
    cmd: &mut Command,
    cmd_line: &str,
) -> std::result::Result<Child, Box<CodexResult>> {
    cmd.spawn().map_err(|err| {
        Box::new(logged_failure(
            request,
            cmd_line,
            "spawn",
            format!("codex spawn failed: {err}"),
            String::new(),
            String::new(),
            -1,
        ))
    })
}

// stdin setup failures keep the same value and are boxed only at the boundary.
fn write_prompt_to_child(
    mut child: Child,
    request: &CodexRequest,
    cmd_line: &str,
) -> std::result::Result<Child, Box<CodexResult>> {
    let Some(mut stdin) = child.stdin.take() else {
        return Err(Box::new(logged_failure(
            request,
            cmd_line,
            "stdin",
            "codex stdin not piped".to_string(),
            String::new(),
            String::new(),
            -1,
        )));
    };
    if let Err(err) = stdin.write_all(request.prompt.as_bytes()) {
        let message = format!("codex stdin write failed: {err}");
        drop(stdin);
        return Err(Box::new(wait_after_stdin_error(
            child, request, cmd_line, message,
        )));
    }
    Ok(child)
}

fn wait_after_stdin_error(
    child: Child,
    request: &CodexRequest,
    cmd_line: &str,
    message: String,
) -> CodexResult {
    match wait_codex_with_stall_window(child, request.stall_window_seconds) {
        CodexWaitOutcome::Exited {
            stdout,
            stderr,
            exit_code,
        } => logged_failure(
            request, cmd_line, "stdin", message, stdout, stderr, exit_code,
        ),
        CodexWaitOutcome::Failed {
            message: wait_error,
            stdout: _,
            stderr: _,
            exit_code: _,
            ..
        } => {
            let message = format!("{message}; {wait_error}");
            logged_failure(
                request,
                cmd_line,
                "stdin",
                message,
                String::new(),
                String::new(),
                -1,
            )
        }
    }
}

fn wait_for_codex_child(child: Child, request: &CodexRequest, cmd_line: &str) -> CodexResult {
    match wait_codex_with_stall_window(child, request.stall_window_seconds) {
        CodexWaitOutcome::Exited {
            stdout,
            stderr,
            exit_code,
        } => {
            write_codex_log(
                &request.log_path,
                &stdout,
                &stderr,
                exit_code,
                cmd_line,
                request.stall_window_seconds,
            );
            CodexResult::success(
                stdout,
                stderr,
                exit_code,
                request.log_path.to_string_lossy().into_owned(),
            )
        }
        CodexWaitOutcome::Failed {
            kind,
            message,
            stdout,
            stderr,
            exit_code,
        } => logged_failure(request, cmd_line, kind, message, stdout, stderr, exit_code),
    }
}

// stdout/stderr output resets a no-output stall window.
fn wait_codex_with_stall_window(mut child: Child, stall_seconds: i64) -> CodexWaitOutcome {
    let child_pid = child.id();
    let (tx, rx) = mpsc::channel();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_stream_reader(stdout, CodexStream::Stdout, tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_stream_reader(stderr, CodexStream::Stderr, tx.clone()));
    }
    let waiter = spawn_waiter(child, tx.clone());
    drop(tx);

    let stall_window = stall_window(stall_seconds);
    let mut last_output = Instant::now();
    let mut killed_for_stall: Option<String> = None;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = loop {
        let event = if killed_for_stall.is_some() {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else if let Some(window) = stall_window {
            let remaining = window.saturating_sub(last_output.elapsed());
            if remaining.is_zero() {
                Err(RecvTimeoutError::Timeout)
            } else {
                rx.recv_timeout(remaining)
            }
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };

        match event {
            Ok(CodexEvent::Output(stream, bytes)) => {
                if !bytes.is_empty() {
                    last_output = Instant::now();
                    match stream {
                        CodexStream::Stdout => stdout.extend(bytes),
                        CodexStream::Stderr => stderr.extend(bytes),
                    }
                }
            }
            Ok(CodexEvent::Exited(result)) => break result,
            Err(RecvTimeoutError::Timeout) => {
                let message = format!(
                    "codex stalled for {stall_seconds}s without stdout/stderr output; sent SIGKILL to process group"
                );
                if let Err(err) = kill_process_group(child_pid) {
                    killed_for_stall = Some(format!("{message}; kill failed: {err}"));
                } else {
                    killed_for_stall = Some(message);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                break Err("codex wait channel closed before process exit".to_string());
            }
        }
    };

    for reader in readers {
        let _ = reader.join();
    }
    let _ = waiter.join();
    drain_codex_events(&rx, &mut stdout, &mut stderr);

    codex_outcome_from_wait(exit, killed_for_stall, stdout, stderr)
}

// no-output stall failures get stable kind, exit 124, and captured output.
fn codex_outcome_from_wait(
    exit: std::result::Result<ExitStatus, String>,
    killed_for_stall: Option<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> CodexWaitOutcome {
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    let exit_code = if killed_for_stall.is_some() {
        124
    } else {
        exit.as_ref().ok().and_then(ExitStatus::code).unwrap_or(-1)
    };

    if let Some(message) = killed_for_stall {
        return CodexWaitOutcome::Failed {
            kind: "stall",
            message,
            stdout,
            stderr,
            exit_code,
        };
    }

    match exit {
        Ok(_) => CodexWaitOutcome::Exited {
            stdout,
            stderr,
            exit_code,
        },
        Err(message) => CodexWaitOutcome::Failed {
            kind: "wait",
            message: format!("codex wait failed: {message}"),
            stdout,
            stderr,
            exit_code,
        },
    }
}

// readers forward chunks immediately so output refreshes liveness.
fn spawn_stream_reader<R: Read + Send + 'static>(
    mut reader: R,
    stream: CodexStream,
    tx: Sender<CodexEvent>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .send(CodexEvent::Output(stream, buf[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

// child exit is another liveness event delivered beside output.
fn spawn_waiter(mut child: Child, tx: Sender<CodexEvent>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let result = child.wait().map_err(|err| err.to_string());
        let _ = tx.send(CodexEvent::Exited(result));
    })
}

// drain queued output events before constructing the final codex result.
fn drain_codex_events(rx: &Receiver<CodexEvent>, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) {
    while let Ok(event) = rx.try_recv() {
        match event {
            CodexEvent::Output(CodexStream::Stdout, bytes) => stdout.extend(bytes),
            CodexEvent::Output(CodexStream::Stderr, bytes) => stderr.extend(bytes),
            CodexEvent::Exited(_) => {}
        }
    }
}

// positive values become no-output windows; invalid values mean unbounded wait.
fn stall_window(stall_seconds: i64) -> Option<Duration> {
    u64::try_from(stall_seconds)
        .ok()
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

#[cfg(unix)]
fn kill_process_group(child_pid: u32) -> std::result::Result<(), String> {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    killpg(Pid::from_raw(child_pid as i32), Signal::SIGKILL).map_err(|err| err.to_string())
}

#[cfg(not(unix))]
fn kill_process_group(_child_pid: u32) -> std::result::Result<(), String> {
    Err("process group kill is not supported on this platform".to_string())
}

fn logged_failure(
    request: &CodexRequest,
    cmd_line: &str,
    kind: &str,
    message: String,
    stdout: String,
    stderr: String,
    exit_code: i32,
) -> CodexResult {
    let logged_stderr = if stderr.trim().is_empty() {
        message.clone()
    } else {
        format!("{stderr}\n{message}")
    };
    write_codex_log(
        &request.log_path,
        &stdout,
        &logged_stderr,
        exit_code,
        cmd_line,
        request.stall_window_seconds,
    );
    CodexResult::failure(
        kind,
        message,
        stdout,
        stderr,
        exit_code,
        request.log_path.to_string_lossy().into_owned(),
    )
}

fn command_for_request(request: &CodexRequest) -> (Command, String) {
    let mut cmd = Command::new("codex");
    let cmd_args = command_args_for_request(request);
    cmd.args(&cmd_args);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let cmd_line = format_command("codex", &cmd_args);
    (cmd, cmd_line)
}

fn command_line_for_request(request: &CodexRequest) -> String {
    format_command("codex", &command_args_for_request(request))
}

fn command_args_for_request(request: &CodexRequest) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
    ];
    if let Some(ctx) = request.context.as_deref() {
        args.push("--context".to_string());
        args.push(ctx.to_string());
    }
    if let Some(wt) = request.worktree.as_deref() {
        args.push("-C".to_string());
        args.push(wt.to_string());
    }
    args.push("-".to_string());
    args
}

fn runtime_log_dir() -> PathBuf {
    std::env::var_os(RUNTIME_LOG_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_runtime_log_dir)
}

fn default_runtime_log_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        let macos_logs = home.join("Library").join("Logs");
        if macos_logs.is_dir() {
            return macos_logs.join("fkst");
        }
        return home.join(".local").join("state").join("fkst");
    }
    PathBuf::from(".fkst-logs")
}

fn codex_log_path(worktree: Option<&str>) -> PathBuf {
    let basename = worktree
        .and_then(|wt| Path::new(wt).file_name())
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("worktree");
    let timestamp = filename_timestamp();
    runtime_log_dir()
        .join("codex")
        .join(format!("{basename}-{timestamp}.log"))
}

fn filename_timestamp() -> String {
    let duration = unix_duration();
    format!("{}-{:09}", duration.as_secs(), duration.subsec_nanos())
}

fn write_codex_log(
    path: &Path,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
    cmd_line: &str,
    stall_window_seconds: i64,
) {
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            eprintln!(
                "WARN: codex log directory creation failed path={} error={err}",
                parent.display()
            );
            return;
        }
    }

    let content = format!(
        "{stdout}{stdout_sep}{stderr}{stderr_sep}EXIT={exit_code}\nDONE_AT={done_at}\nCMD={cmd_line}\nSTALL_WINDOW={stall_window_seconds}\n",
        stdout_sep = if stdout.ends_with('\n') || stdout.is_empty() {
            ""
        } else {
            "\n"
        },
        stderr_sep = if stderr.ends_with('\n') || stderr.is_empty() {
            ""
        } else {
            "\n"
        },
        done_at = unix_seconds_to_iso8601(unix_duration().as_secs()),
    );
    if let Err(err) = std::fs::write(path, content) {
        eprintln!(
            "WARN: codex log write failed path={} error={err}",
            path.display()
        );
    }
}

fn unix_duration() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
}

fn unix_seconds_to_iso8601(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

fn format_command(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_string())
        .chain(args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

// crate-internal visibility keeps permit setup testable after extraction.
#[cfg(test)]
pub(crate) fn ensure_pool() -> mlua::Result<()> {
    let host_root = std::env::current_dir().map_err(mlua::Error::external)?;
    let config = ConfigContext::from_host_root(&host_root).map_err(mlua::Error::external)?;
    ensure_pool_with_context(&host_root, &config)
}

pub(crate) fn ensure_pool_with_context(
    host_root: &Path,
    config: &ConfigContext,
) -> mlua::Result<()> {
    let pool_dir = permit_pool_dir(host_root)?;
    let slot_count = permit_slot_count(config)?;
    // permits live below RuntimeLayout codex-permits dir.
    std::fs::create_dir_all(&pool_dir).map_err(mlua::Error::external)?;
    for i in 0..slot_count {
        let p = pool_dir.join(format!("permit-{}", i));
        if !p.exists() {
            std::fs::write(&p, b"permit\n").map_err(mlua::Error::external)?;
        }
    }
    Ok(())
}

pub(crate) fn self_test_permit_pool(host_root: &Path) -> mlua::Result<()> {
    let config = ConfigContext::from_host_root(&host_root).map_err(mlua::Error::external)?;
    ensure_pool_with_context(&host_root, &config)?;
    let permit = acquire_permit_with_context(&host_root, &config)?;
    drop(permit);
    Ok(())
}

/// Returns a held file (lock active) or blocks until one is available.
// crate-internal visibility keeps permit locking behavior testable after extraction.
#[cfg(test)]
pub(crate) fn acquire_permit() -> mlua::Result<std::fs::File> {
    let host_root = std::env::current_dir().map_err(mlua::Error::external)?;
    let config = ConfigContext::from_host_root(&host_root).map_err(mlua::Error::external)?;
    acquire_permit_with_context(&host_root, &config)
}

fn acquire_permit_with_context(
    host_root: &Path,
    config: &ConfigContext,
) -> mlua::Result<std::fs::File> {
    let pool_dir = permit_pool_dir(host_root)?;
    let slot_count = permit_slot_count(config)?;
    for i in 0..slot_count {
        let p = pool_dir.join(format!("permit-{}", i));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&p)
            .map_err(mlua::Error::external)?;
        if flock(f.as_raw_fd(), FlockArg::LockExclusiveNonblock).is_ok() {
            return Ok(f);
        }
    }

    let p = pool_dir.join("permit-0");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&p)
        .map_err(mlua::Error::external)?;
    flock(f.as_raw_fd(), FlockArg::LockExclusive).map_err(mlua::Error::external)?;
    Ok(f)
}

fn permit_slot_count(config: &ConfigContext) -> mlua::Result<usize> {
    config
        .resolved_positive_usize(ConfigKey::CodexPermitSlots)
        .map_err(mlua::Error::external)
}

fn permit_pool_dir(host_root: &Path) -> mlua::Result<PathBuf> {
    runtime_context::layout_from_host_root(host_root)
        .map(|layout| layout.runtime_dir(RuntimeKind::CodexPermits))
        .map_err(mlua::Error::external)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_seconds_to_iso8601_formats_epoch_and_leap_day() {
        assert_eq!(unix_seconds_to_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            unix_seconds_to_iso8601(1_709_251_199),
            "2024-02-29T23:59:59Z"
        );
    }
}

//! SDK: spawn_codex_sync wraps `codex exec` with an fcntl permit pool.
//!
//! Acquire one slot from `<runtime>/codex-permits/{0..N-1}` by trying each fd's flock until one
//! locks exclusive non-blocking. If none available, block on the first one. Permit
//! holds for the duration of the codex subprocess; released on file drop after exit.

use fkst_common::RuntimeKind;
use mlua::{AnyUserData, Lua, Result, Table, UserData};
use nix::fcntl::{flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt::Write as FmtWrite;
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
use crate::external_command::{
    format_command, MockCommandInvocation, MockCommandPlan, MockCommandResult, MockCommandState,
};
use crate::raise::RaiseBuffer;
use crate::runtime_context;

pub(crate) const CODEX_PERMIT_SLOTS_ENV: &str = "FKST_CODEX_PERMIT_SLOTS";
pub const RUNTIME_LOG_DIR_ENV: &str = "FKST_RUNTIME_LOG_DIR";
const CODEX_LOG_MAX_AGE_ENV: &str = "FKST_CODEX_LOG_MAX_AGE";
const CODEX_LOG_MAX_BYTES_ENV: &str = "FKST_CODEX_LOG_MAX_BYTES";
const CODEX_WORKER_BIN_ENV: &str = "FKST_CODEX_WORKER_BIN";
const DEFAULT_CODEX_TIMEOUT_SECONDS: i64 = 3600;
const DEFAULT_CODEX_LOG_MAX_AGE: Duration = Duration::from_secs(48 * 60 * 60);
const CODEX_STATUS_RECENT_LIMIT: usize = 50;
const CODEX_OUTPUT_TAIL_MAX_LINES: usize = 40;
const CODEX_OUTPUT_TAIL_MAX_BYTES: usize = 4096;
const CODEX_STATUS_LOG_PREFIX: &str = "CODEX_STATUS:";
const CODEX_EFFECT_LOG_PREFIX: &str = "CODEX_EFFECT:";
const CODEX_ADOPTION_DIR: &str = "codex-adoption";
const CODEX_ADOPTION_STATUS_INTENT: &str = "intent";
const CODEX_ADOPTION_STATUS_RUNNING: &str = "running";
const CODEX_ADOPTION_STATUS_COMPLETED: &str = "completed";
const CODEX_ADOPTION_POLL: Duration = Duration::from_millis(100);
const CODEX_ADOPTION_TIMEOUT_GRACE: Duration = Duration::from_secs(2);
const GH_CONFIG_DIR_ENV: &str = "GH_CONFIG_DIR";
const GITHUB_AUTH_ENV_KEYS: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];
static NEXT_PIPELINE_OWNER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CODEX_RUN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexSandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexSandbox {
    fn parse(mode: &str) -> anyhow::Result<Self> {
        match mode {
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "danger-full-access" => Ok(Self::DangerFullAccess),
            _ => anyhow::bail!("codex-sandbox-invalid: unsupported sandbox mode {mode}"),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

// both sync and async SDK calls share one immutable request description.
struct CodexRequest {
    run_id: String,
    prompt: String,
    context: Option<String>,
    worktree: Option<String>,
    sandbox: Option<CodexSandbox>,
    timeout_seconds: i64,
    log_path: PathBuf,
    output_tail_path: PathBuf,
    role: Option<String>,
    label: Option<String>,
    proposal_id: Option<String>,
    dedup_key: Option<String>,
    dept: Option<String>,
    started_at_ms: u64,
    adoption_status_path: Option<PathBuf>,
    effect_key: Option<String>,
    effect_log_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct CodexAdoptionPaths {
    key: String,
    dir: PathBuf,
    prompt: PathBuf,
    stdout: PathBuf,
    stderr: PathBuf,
    effect: PathBuf,
    effect_log: PathBuf,
    result: PathBuf,
    status: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexStatusRecord {
    run_id: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    dept: Option<String>,
    #[serde(default)]
    proposal_id: Option<String>,
    #[serde(default)]
    dedup_key: Option<String>,
    started_at: String,
    started_at_ms: u64,
    #[serde(default)]
    timeout_seconds: Option<i64>,
    #[serde(default)]
    ended_at: Option<String>,
    #[serde(default)]
    ended_at_ms: Option<u64>,
    #[serde(default)]
    elapsed_ms: Option<u64>,
    status: String,
    #[serde(default)]
    exit_code: Option<i32>,
    permit_slot: Option<usize>,
    #[serde(default)]
    output_tail_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexAdoptionRecord {
    key: String,
    run_id: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    dept: Option<String>,
    #[serde(default)]
    proposal_id: Option<String>,
    #[serde(default)]
    dedup_key: Option<String>,
    status: String,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    timeout_seconds: i64,
    worker_pid: Option<u32>,
    codex_pid: Option<u32>,
    exit_code: Option<i32>,
    log_path: String,
    cmd_line: String,
    error_kind: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexAdoptionResultRecord {
    ended_at_ms: u64,
    exit_code: i32,
    error_kind: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexAdoptionEffectRecord {
    effect_key: String,
    ended_at_ms: u64,
    stdout: String,
    stderr: String,
    exit_code: i32,
    error_kind: Option<String>,
    error: Option<String>,
}

// worker threads return the same result shape before Lua table conversion.
// crate-internal test seams expose only data needed by extracted integration tests.
pub(crate) struct CodexResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
    log_path: String,
    error_kind: Option<String>,
    error_class: Option<String>,
    error: Option<String>,
}

// spawn_codex returns a pipeline-local opaque handle consumed by await_all.
// crate-internal fields remain hidden from Lua while extracted tests can verify handle validation.
pub(crate) struct CodexTaskHandle {
    pub(crate) owner_id: u64,
    pub(crate) task_id: u64,
    pub(crate) join: Arc<Mutex<Option<JoinHandle<CodexResult>>>>,
}

pub(crate) struct CodexPermit {
    slot: usize,
    _file: std::fs::File,
}

impl CodexPermit {
    #[cfg(test)]
    pub(crate) fn slot(&self) -> usize {
        self.slot
    }
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

// each stream is tagged so captured output preserves stdout/stderr boundaries.
#[derive(Clone, Copy)]
enum CodexStream {
    Stdout,
    Stderr,
}

// output chunks and process exit share one wait channel.
enum CodexEvent {
    Output(CodexStream, Vec<u8>),
    Exited(std::result::Result<ExitStatus, String>),
}

// wait outcomes distinguish natural exit from timeout and wait failures.
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
        let error_class =
            crate::boundary_resource::classify_process_output(exit_code, &stdout, &stderr)
                .map(|class| class.label().to_string());
        Self {
            stdout,
            stderr,
            exit_code,
            log_path,
            error_kind: None,
            error_class,
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
            error_class: Some(
                crate::boundary_resource::class_for_adapter_failure(kind, &message)
                    .label()
                    .to_string(),
            ),
            error: Some(message),
        }
    }

    fn mock_error(err: mlua::Error) -> Self {
        let message = err.to_string();
        Self {
            stdout: String::new(),
            stderr: message.clone(),
            exit_code: -1,
            log_path: String::new(),
            error_kind: Some("mock".to_string()),
            error_class: Some(
                crate::boundary_resource::BoundaryErrorClass::ProviderUnavailable
                    .label()
                    .to_string(),
            ),
            error: Some(message),
        }
    }

    fn into_lua_table(self, lua: &Lua) -> Result<Table> {
        let t = result_table(lua, self.stdout, self.stderr, self.exit_code, self.log_path)?;
        if let Some(kind) = self.error_kind {
            t.set("error_kind", kind)?;
        }
        if let Some(class) = self.error_class {
            t.set("error_class", class)?;
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

pub fn register(
    lua: &Lua,
    host_root: &Path,
    config: ConfigContext,
    dept: Option<String>,
    raise_buf: RaiseBuffer,
    raised_auth_token: Option<String>,
) -> Result<()> {
    register_with_runner(
        lua,
        host_root,
        config,
        dept,
        None,
        raise_buf,
        raised_auth_token,
    )
}

pub(crate) fn register_with_runner(
    lua: &Lua,
    host_root: &Path,
    config: ConfigContext,
    dept: Option<String>,
    runner: Option<MockCommandState>,
    raise_buf: RaiseBuffer,
    raised_auth_token: Option<String>,
) -> Result<()> {
    let owner_id = NEXT_PIPELINE_OWNER_ID.fetch_add(1, Ordering::Relaxed);
    let next_task_id = Arc::new(AtomicU64::new(1));
    let host_root = Arc::new(host_root.to_path_buf());
    let config = Arc::new(config);
    let dept = Arc::new(dept);
    let runner = Arc::new(runner);
    let raised_auth_token = Arc::new(raised_auth_token);

    lua.globals().set("spawn_codex_sync", {
        let host_root = Arc::clone(&host_root);
        let config = Arc::clone(&config);
        let dept = Arc::clone(&dept);
        let runner = Arc::clone(&runner);
        let raise_buf = raise_buf.clone();
        let raised_auth_token = Arc::clone(&raised_auth_token);
        lua.create_function(move |lua, opts: Table| {
            crate::process_tree::ensure_supervisor_parent_alive()?;
            let request = codex_request_from_opts(opts, dept.as_deref().map(str::to_string))?;
            flush_raises_before_sync_codex(&raise_buf, raised_auth_token.as_deref());
            run_codex_request(request, &host_root, &config, runner.as_ref().as_ref())?
                .into_lua_table(lua)
        })?
    })?;
    lua.globals().set("spawn_codex", {
        let next_task_id = Arc::clone(&next_task_id);
        let host_root = Arc::clone(&host_root);
        let config = Arc::clone(&config);
        let dept = Arc::clone(&dept);
        let runner = Arc::clone(&runner);
        lua.create_function(move |_, opts: Table| {
            crate::process_tree::ensure_supervisor_parent_alive()?;
            let request = codex_request_from_opts(opts, dept.as_deref().map(str::to_string))?;
            let task_id = next_task_id.fetch_add(1, Ordering::Relaxed);
            let host_root = Arc::clone(&host_root);
            let config = Arc::clone(&config);
            let runner = Arc::clone(&runner);
            let join = std::thread::spawn(move || {
                run_codex_request(request, &host_root, &config, runner.as_ref().as_ref())
                    .unwrap_or_else(CodexResult::mock_error)
            });
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
    let fkst = match lua.globals().get::<mlua::Value>("fkst")? {
        mlua::Value::Table(table) => table,
        mlua::Value::Nil => lua.create_table()?,
        _ => {
            return Err(mlua::Error::RuntimeError(
                "global fkst is not a table".to_string(),
            ))
        }
    };
    fkst.set("codex_runs", {
        let host_root = Arc::clone(&host_root);
        lua.create_function(move |lua, ()| codex_status(lua, &host_root))?
    })?;
    lua.globals().set("fkst", fkst)?;
    Ok(())
}

fn flush_raises_before_sync_codex(raise_buf: &RaiseBuffer, raised_auth_token: Option<&str>) {
    if let Some(token) = raised_auth_token {
        raise_buf.drain_emit_authenticated_stdout(token);
    }
}

// the same input names an overall wall-clock timeout with a bounded default.
fn codex_request_from_opts(opts: Table, runtime_dept: Option<String>) -> Result<CodexRequest> {
    let prompt: String = opts.get("prompt").unwrap_or_default();
    let context: Option<String> = opts.get("context").ok();
    let worktree: Option<String> = opts.get("worktree").ok();
    let sandbox = codex_sandbox_from_opts(&opts)?;
    let role: Option<String> = opts.get("role").ok();
    let label: Option<String> = opts.get("label").ok();
    let proposal_id: Option<String> = opts.get("proposal_id").ok();
    let dedup_key: Option<String> = opts.get("dedup_key").ok();
    let dept: Option<String> = opts.get("dept").ok().or(runtime_dept);
    let timeout: Option<i64> = opts.get("timeout").ok();
    let timeout_seconds = timeout
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_CODEX_TIMEOUT_SECONDS);
    let log_path = codex_log_path(worktree.as_deref());
    let output_tail_path = codex_output_tail_path(&log_path);
    let started_at_ms = unix_duration().as_millis() as u64;
    Ok(CodexRequest {
        run_id: codex_run_id(),
        prompt,
        context,
        worktree,
        sandbox,
        timeout_seconds,
        log_path,
        output_tail_path,
        role,
        label,
        proposal_id,
        dedup_key,
        dept,
        started_at_ms,
        adoption_status_path: None,
        effect_key: None,
        effect_log_path: None,
    })
}

fn codex_sandbox_from_opts(opts: &Table) -> Result<Option<CodexSandbox>> {
    let value: mlua::Value = opts.get("sandbox")?;
    match value {
        mlua::Value::Nil => Ok(None),
        mlua::Value::String(value) => {
            let mode = value.to_str().map_err(|_| {
                mlua::Error::external("codex-sandbox-invalid: sandbox must be valid UTF-8")
            })?;
            let mode = mode.as_ref();
            CodexSandbox::parse(mode)
                .map(Some)
                .map_err(mlua::Error::external)
        }
        _ => Err(mlua::Error::external(
            "codex-sandbox-invalid: sandbox must be read-only, workspace-write, or danger-full-access",
        )),
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
        if result.error_kind.as_deref() == Some("mock") {
            return Err(mlua::Error::external(
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "mock command failed".to_string()),
            ));
        }
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
    runner: Option<&MockCommandState>,
) -> Result<CodexResult> {
    prune_codex_logs_for_request(&request);
    if let Some(runner) = runner {
        return run_mocked_codex_request(request, host_root, config, runner);
    }

    if let Some(paths) = adoption_paths_for_request(&request, host_root)? {
        return run_adoptable_codex_request(request, paths, host_root, config);
    }

    let _lifetime_witness = acquire_log_lifetime_witness(&request);
    let mut status = CodexStatusRecord::from_request(&request, None);
    write_codex_status(&request.log_path, &status);
    if let Err(err) = ensure_pool_with_context(host_root, config) {
        let message = format!("codex permit pool failed: {err}");
        write_codex_log(
            &request.log_path,
            "",
            &message,
            -1,
            &command_line_for_request(&request),
            request.timeout_seconds,
        );
        status.finish(-1);
        write_codex_status(&request.log_path, &status);
        return Ok(CodexResult::failure(
            "permit",
            message,
            String::new(),
            String::new(),
            -1,
            request.log_path.to_string_lossy().into_owned(),
        ));
    }
    let _permit = match acquire_permit_with_context(host_root, config) {
        Ok(permit) => {
            status.permit_slot = Some(permit.slot);
            write_codex_status(&request.log_path, &status);
            permit
        }
        Err(err) => {
            let message = format!("codex permit acquire failed: {err}");
            write_codex_log(
                &request.log_path,
                "",
                &message,
                -1,
                &command_line_for_request(&request),
                request.timeout_seconds,
            );
            status.finish(-1);
            write_codex_status(&request.log_path, &status);
            return Ok(CodexResult::failure(
                "permit",
                message,
                String::new(),
                String::new(),
                -1,
                request.log_path.to_string_lossy().into_owned(),
            ));
        }
    };

    let result = run_codex_request_with_permit(request, config);
    status.finish(result.exit_code);
    write_codex_status(Path::new(&result.log_path), &status);
    Ok(result)
}

fn run_adoptable_codex_request(
    request: CodexRequest,
    paths: CodexAdoptionPaths,
    host_root: &Path,
    config: &ConfigContext,
) -> Result<CodexResult> {
    if let Some(result) = read_completed_adoption_result(&paths)? {
        return Ok(result);
    }
    std::fs::create_dir_all(&paths.dir).map_err(mlua::Error::external)?;
    let lock_path = paths.dir.join("run.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(mlua::Error::external)?;
    flock(lock_file.as_raw_fd(), FlockArg::LockExclusive).map_err(mlua::Error::external)?;
    if let Some(result) = read_completed_adoption_result(&paths)? {
        drop(lock_file);
        return Ok(result);
    }
    if let Some(result) = recover_completed_adoption_result_locked(&paths)? {
        drop(lock_file);
        return Ok(result);
    }
    let existing = read_adoption_record(&paths.status)?;
    let running = match existing.as_ref() {
        Some(record) if record.status == CODEX_ADOPTION_STATUS_RUNNING => {
            adoption_effect_alive(&paths.dir, record).map_err(mlua::Error::external)?
        }
        _ => false,
    };
    if !running {
        start_adoption_worker(&request, &paths, host_root, config)?;
    }
    drop(lock_file);
    wait_for_adoption_result(&request, &paths)
}

fn run_mocked_codex_request(
    request: CodexRequest,
    host_root: &Path,
    config: &ConfigContext,
    runner: &MockCommandState,
) -> Result<CodexResult> {
    let _lifetime_witness = acquire_log_lifetime_witness(&request);
    let args = command_args_for_request(&request);
    let cmd_line = format_command("codex", &args);
    let invocation = MockCommandInvocation {
        rendered: cmd_line.clone(),
        program: "codex".to_string(),
        args,
        stdin: request.prompt.clone(),
        cwd: request.worktree.clone(),
        env: Vec::new(),
    };
    let result = match runner.prepare(invocation.clone())? {
        MockCommandPlan::Return(result) => result,
        MockCommandPlan::Record => {
            let result = run_codex_request(request, host_root, config, None)?;
            runner.record(
                invocation,
                MockCommandResult {
                    stdout: result.stdout.clone(),
                    stderr: result.stderr.clone(),
                    exit_code: result.exit_code,
                },
            )?;
            return Ok(result);
        }
    };
    let mut status = CodexStatusRecord::from_request(&request, None);
    write_codex_status(&request.log_path, &status);
    write_live_output_tail(
        Some(&request.output_tail_path),
        result.stdout.as_bytes(),
        result.stderr.as_bytes(),
    );
    write_codex_log(
        &request.log_path,
        &result.stdout,
        &result.stderr,
        result.exit_code,
        &cmd_line,
        request.timeout_seconds,
    );
    status.finish(result.exit_code);
    write_codex_status(&request.log_path, &status);
    Ok(CodexResult::success(
        result.stdout,
        result.stderr,
        result.exit_code,
        request.log_path.to_string_lossy().into_owned(),
    ))
}

fn adoption_paths_for_request(
    request: &CodexRequest,
    host_root: &Path,
) -> Result<Option<CodexAdoptionPaths>> {
    let Some(worktree) = request.worktree.as_deref() else {
        return Ok(None);
    };
    if worktree.trim().is_empty() {
        return Ok(None);
    }
    let key = adoption_key_for_request(request);
    let dir = adoption_runtime_dir(host_root)?.join(&key);
    Ok(Some(CodexAdoptionPaths {
        key,
        prompt: dir.join("prompt.txt"),
        stdout: dir.join("stdout.txt"),
        stderr: dir.join("stderr.txt"),
        effect: dir.join("effect.json"),
        effect_log: dir.join("effect-receipts.log"),
        result: dir.join("result.json"),
        status: dir.join("status.json"),
        dir,
    }))
}

fn adoption_runtime_dir(host_root: &Path) -> Result<PathBuf> {
    runtime_context::layout_from_host_root(host_root)
        .map(|layout| {
            layout
                .runtime_dir(RuntimeKind::Logs)
                .join(CODEX_ADOPTION_DIR)
        })
        .map_err(mlua::Error::external)
}

fn adoption_key_for_request(request: &CodexRequest) -> String {
    let identity = request.dedup_key.as_deref().unwrap_or("prompt");
    let identity = sanitize_adoption_key_segment(identity);
    let hash = adoption_request_hash(request);
    if request.dedup_key.is_some() {
        format!("dedup-{identity}-{hash}")
    } else {
        format!("prompt-{hash}")
    }
}

fn sanitize_adoption_key_segment(value: &str) -> String {
    let mut key = String::new();
    for byte in value.bytes().take(80) {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
            key.push(byte as char);
        } else {
            key.push('-');
        }
    }
    if key.is_empty() {
        "work-unit".to_string()
    } else {
        key
    }
}

fn adoption_request_hash(request: &CodexRequest) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    fn feed(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    feed(
        &mut hash,
        request.dedup_key.as_deref().unwrap_or("").as_bytes(),
    );
    feed(&mut hash, b"\0worktree\0");
    feed(
        &mut hash,
        request.worktree.as_deref().unwrap_or("").as_bytes(),
    );
    if let Some(sandbox) = request.sandbox {
        feed(&mut hash, b"\0sandbox\0");
        feed(&mut hash, sandbox.as_str().as_bytes());
    }
    feed(&mut hash, b"\0prompt\0");
    feed(&mut hash, request.prompt.as_bytes());
    feed(&mut hash, b"\0context\0");
    feed(
        &mut hash,
        request.context.as_deref().unwrap_or("").as_bytes(),
    );
    let mut hex = String::with_capacity(16);
    write!(&mut hex, "{hash:016x}").expect("writing to string should not fail");
    hex
}

fn read_completed_adoption_result(paths: &CodexAdoptionPaths) -> Result<Option<CodexResult>> {
    let Some(record) = read_adoption_record(&paths.status)? else {
        return Ok(None);
    };
    if record.status != CODEX_ADOPTION_STATUS_COMPLETED {
        return Ok(None);
    }
    let stdout = read_optional_string(&paths.stdout).map_err(mlua::Error::external)?;
    let stderr = read_optional_string(&paths.stderr).map_err(mlua::Error::external)?;
    let output_tail_path = codex_output_tail_path(Path::new(&record.log_path));
    write_live_output_tail(
        Some(&output_tail_path),
        stdout.as_bytes(),
        stderr.as_bytes(),
    );
    let exit_code = record.exit_code.unwrap_or(-1);
    if let Some(kind) = record.error_kind.as_deref() {
        return Ok(Some(CodexResult::failure(
            kind,
            record
                .error
                .clone()
                .unwrap_or_else(|| "adopted codex failed".to_string()),
            stdout,
            stderr,
            exit_code,
            record.log_path,
        )));
    }
    Ok(Some(CodexResult::success(
        stdout,
        stderr,
        exit_code,
        record.log_path,
    )))
}

fn recover_completed_adoption_result(paths: &CodexAdoptionPaths) -> Result<Option<CodexResult>> {
    let lock_file = open_adoption_run_lock(&paths.dir).map_err(mlua::Error::external)?;
    flock(lock_file.as_raw_fd(), FlockArg::LockExclusive).map_err(mlua::Error::external)?;
    let result = recover_completed_adoption_result_locked(paths);
    drop(lock_file);
    result
}

fn recover_completed_adoption_result_locked(
    paths: &CodexAdoptionPaths,
) -> Result<Option<CodexResult>> {
    let recovered =
        recover_completed_adoption_result_locked_from_disk(paths).map_err(mlua::Error::external)?;
    if !recovered {
        return Ok(None);
    };
    read_completed_adoption_result(paths)
}

fn recover_completed_adoption_result_locked_from_disk(
    paths: &CodexAdoptionPaths,
) -> anyhow::Result<bool> {
    if let Some(result) = read_adoption_result_record_from_disk(&paths.result)? {
        return promote_completed_adoption_result(paths, result);
    }
    if recover_completed_adoption_effect_by_key(paths)? {
        return Ok(true);
    }
    recover_completed_external_codex_effect_by_key(paths)
}

fn promote_completed_adoption_result(
    paths: &CodexAdoptionPaths,
    result: CodexAdoptionResultRecord,
) -> anyhow::Result<bool> {
    let Some(mut record) = read_adoption_record_from_disk(&paths.status)? else {
        return Ok(false);
    };
    record.status = CODEX_ADOPTION_STATUS_COMPLETED.to_string();
    record.ended_at_ms = Some(result.ended_at_ms);
    record.exit_code = Some(result.exit_code);
    record.error_kind = result.error_kind;
    record.error = result.error;
    write_visible_adoption_record(&paths.status, &record, CODEX_ADOPTION_STATUS_COMPLETED)?;
    Ok(true)
}

fn recover_completed_adoption_effect_by_key(paths: &CodexAdoptionPaths) -> anyhow::Result<bool> {
    let Some(record) = read_adoption_record_from_disk(&paths.status)? else {
        return Ok(false);
    };
    if record.status == CODEX_ADOPTION_STATUS_COMPLETED || record.key != paths.key {
        return Ok(false);
    }
    let Some(effect) = read_adoption_effect_record_from_disk(&paths.effect)? else {
        return Ok(false);
    };
    promote_completed_adoption_effect(paths, effect)
}

fn recover_completed_external_codex_effect_by_key(
    paths: &CodexAdoptionPaths,
) -> anyhow::Result<bool> {
    let Some(record) = read_adoption_record_from_disk(&paths.status)? else {
        return Ok(false);
    };
    if record.status == CODEX_ADOPTION_STATUS_COMPLETED || record.key != paths.key {
        return Ok(false);
    }
    let Some(effect) = read_codex_effect_record_from_log(&paths.effect_log, &paths.key)? else {
        return Ok(false);
    };
    promote_completed_adoption_effect(paths, effect)
}

fn promote_completed_adoption_effect(
    paths: &CodexAdoptionPaths,
    effect: CodexAdoptionEffectRecord,
) -> anyhow::Result<bool> {
    if effect.effect_key != paths.key {
        anyhow::bail!(
            "codex adoption effect key mismatch: expected {} got {}",
            paths.key,
            effect.effect_key
        );
    }
    write_atomic(&paths.stdout, effect.stdout.as_bytes())?;
    write_atomic(&paths.stderr, effect.stderr.as_bytes())?;
    let result = CodexAdoptionResultRecord {
        ended_at_ms: effect.ended_at_ms,
        exit_code: effect.exit_code,
        error_kind: effect.error_kind,
        error: effect.error,
    };
    write_adoption_result_record(&paths.result, &result)?;
    promote_completed_adoption_result(paths, result)
}

fn read_adoption_result_record_from_disk(
    path: &Path,
) -> anyhow::Result<Option<CodexAdoptionResultRecord>> {
    match std::fs::read_to_string(path) {
        Ok(body) => serde_json::from_str(&body).map(Some).map_err(Into::into),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn read_adoption_effect_record_from_disk(
    path: &Path,
) -> anyhow::Result<Option<CodexAdoptionEffectRecord>> {
    match std::fs::read_to_string(path) {
        Ok(body) => serde_json::from_str(&body).map(Some).map_err(Into::into),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn read_codex_effect_record_from_log(
    log_path: &Path,
    effect_key: &str,
) -> anyhow::Result<Option<CodexAdoptionEffectRecord>> {
    let body = match std::fs::read_to_string(log_path) {
        Ok(body) => body,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut matched = None;
    for line in body.lines() {
        let Some(json) = line.strip_prefix(CODEX_EFFECT_LOG_PREFIX) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<CodexAdoptionEffectRecord>(json) else {
            continue;
        };
        if record.effect_key == effect_key {
            matched = Some(record);
        }
    }
    Ok(matched)
}

fn read_adoption_record(path: &Path) -> Result<Option<CodexAdoptionRecord>> {
    read_adoption_record_from_disk(path).map_err(mlua::Error::external)
}

fn read_adoption_record_from_disk(path: &Path) -> anyhow::Result<Option<CodexAdoptionRecord>> {
    match std::fs::read_to_string(path) {
        Ok(body) => serde_json::from_str(&body).map(Some).map_err(Into::into),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn read_optional_string(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(body),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err.into()),
    }
}

fn acquire_log_lifetime_witness(request: &CodexRequest) -> Option<std::fs::File> {
    match acquire_lifetime_witness_from_disk(&request.log_path) {
        Ok(file) => Some(file),
        Err(err) => {
            eprintln!(
                "WARN: codex lifetime witness unavailable run_id={} path={} error={err}",
                request.run_id,
                request.log_path.display()
            );
            None
        }
    }
}

fn acquire_lifetime_witness_from_disk(path: &Path) -> anyhow::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    for _ in 0..8 {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        flock(file.as_raw_fd(), FlockArg::LockExclusive).map_err(anyhow::Error::from)?;
        if lifetime_witness_path_matches(&file, path)? {
            return Ok(file);
        }
    }
    anyhow::bail!(
        "codex lifetime witness path changed repeatedly while locking: {}",
        path.display()
    )
}

#[cfg(unix)]
fn lifetime_witness_path_matches(file: &std::fs::File, path: &Path) -> anyhow::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let locked = file.metadata()?;
    let visible = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(locked.dev() == visible.dev() && locked.ino() == visible.ino())
}

#[cfg(not(unix))]
fn lifetime_witness_path_matches(_file: &std::fs::File, path: &Path) -> anyhow::Result<bool> {
    Ok(path.exists())
}

fn lifetime_witness_held(path: &Path) -> anyhow::Result<bool> {
    match probe_lifetime_witness(path)? {
        LifetimeWitnessProbe::Held => Ok(true),
        LifetimeWitnessProbe::Missing | LifetimeWitnessProbe::Unlocked(_) => Ok(false),
    }
}

enum LifetimeWitnessProbe {
    Missing,
    Unlocked(std::fs::File),
    Held,
}

fn probe_lifetime_witness(path: &Path) -> anyhow::Result<LifetimeWitnessProbe> {
    let file = match std::fs::OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LifetimeWitnessProbe::Missing)
        }
        Err(err) => return Err(err.into()),
    };
    match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(()) => Ok(LifetimeWitnessProbe::Unlocked(file)),
        Err(nix::errno::Errno::EWOULDBLOCK) => Ok(LifetimeWitnessProbe::Held),
        Err(err) => Err(err.into()),
    }
}

fn adoption_worker_witness_path(work_dir: &Path, run_id: &str) -> anyhow::Result<PathBuf> {
    if run_id.is_empty()
        || run_id.len() > 200
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        anyhow::bail!("codex adoption run_id invalid for lifetime witness: {run_id}");
    }
    Ok(work_dir.join(format!("worker-{run_id}.lock")))
}

fn try_acquire_adoption_worker_witness(
    work_dir: &Path,
    run_id: &str,
) -> anyhow::Result<Option<std::fs::File>> {
    let path = adoption_worker_witness_path(work_dir, run_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(()) => Ok(Some(file)),
        Err(nix::errno::Errno::EWOULDBLOCK) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn adoption_effect_alive(work_dir: &Path, record: &CodexAdoptionRecord) -> anyhow::Result<bool> {
    let path = adoption_worker_witness_path(work_dir, &record.run_id)?;
    lifetime_witness_held(&path).map_err(|err| {
        anyhow::anyhow!(
            "codex adoption lifetime witness probe failed run_id={} path={} error={err}",
            record.run_id,
            path.display()
        )
    })
}

fn adoption_intent_alive(work_dir: &Path) -> bool {
    let path = work_dir.join("run.lock");
    match lifetime_witness_held(&path) {
        Ok(held) => held,
        Err(err) => {
            eprintln!(
                "WARN: codex adoption intent witness probe failed path={} error={err}",
                path.display()
            );
            true
        }
    }
}

fn open_adoption_run_lock(work_dir: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(work_dir)?;
    let path = work_dir.join("run.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    Ok(file)
}

fn adoption_record_is_current(record: &CodexAdoptionRecord, run_id: &str, key: &str) -> bool {
    if record.status != CODEX_ADOPTION_STATUS_RUNNING {
        return false;
    }
    record.run_id == run_id && record.key == key
}

fn start_adoption_worker(
    request: &CodexRequest,
    paths: &CodexAdoptionPaths,
    host_root: &Path,
    config: &ConfigContext,
) -> Result<()> {
    ensure_pool_with_context(host_root, config)?;
    std::fs::create_dir_all(&paths.dir).map_err(mlua::Error::external)?;
    write_atomic(&paths.prompt, request.prompt.as_bytes()).map_err(mlua::Error::external)?;
    let intent =
        adoption_record_from_request(request, paths, CODEX_ADOPTION_STATUS_INTENT, None, None);
    write_visible_adoption_record(&paths.status, &intent, CODEX_ADOPTION_STATUS_INTENT)
        .map_err(mlua::Error::external)?;
    wait_for_test_hook("FKST_TEST_AFTER_ADOPTION_INTENT");
    fail_for_test_hook("FKST_TEST_FAIL_AFTER_ADOPTION_INTENT")?;
    let worker_bin = codex_worker_bin().map_err(mlua::Error::external)?;
    let args = codex_worker_args(request, paths, host_root);
    let mut command = Command::new(&worker_bin);
    command.args(&args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().map_err(mlua::Error::external)?;
    drop(child);
    Ok(())
}

fn adoption_record_from_request(
    request: &CodexRequest,
    paths: &CodexAdoptionPaths,
    status: &str,
    worker_pid: Option<u32>,
    codex_pid: Option<u32>,
) -> CodexAdoptionRecord {
    CodexAdoptionRecord {
        key: paths.key.clone(),
        run_id: request.run_id.clone(),
        role: request.role.clone(),
        label: request.label.clone(),
        dept: request.dept.clone(),
        proposal_id: request.proposal_id.clone(),
        dedup_key: request.dedup_key.clone(),
        status: status.to_string(),
        started_at_ms: request.started_at_ms,
        ended_at_ms: None,
        timeout_seconds: request.timeout_seconds,
        worker_pid,
        codex_pid,
        exit_code: None,
        log_path: request.log_path.to_string_lossy().into_owned(),
        cmd_line: command_line_for_request(request),
        error_kind: None,
        error: None,
    }
}

fn codex_worker_bin() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os(CODEX_WORKER_BIN_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe()?;
    if current.file_stem().and_then(OsStr::to_str) == Some("fkst-framework") {
        return Ok(current);
    }
    if let Some(sibling) = current
        .parent()
        .and_then(Path::parent)
        .map(|target| target.join("fkst-framework"))
        .filter(|path| path.is_file())
    {
        return Ok(sibling);
    }
    Ok(current)
}

fn codex_worker_args(
    request: &CodexRequest,
    paths: &CodexAdoptionPaths,
    host_root: &Path,
) -> Vec<String> {
    let mut args = vec![
        "__codex-worker".to_string(),
        "--host-root".to_string(),
        host_root.to_string_lossy().into_owned(),
        "--work-dir".to_string(),
        paths.dir.to_string_lossy().into_owned(),
        "--prompt-file".to_string(),
        paths.prompt.to_string_lossy().into_owned(),
        "--stdout-file".to_string(),
        paths.stdout.to_string_lossy().into_owned(),
        "--stderr-file".to_string(),
        paths.stderr.to_string_lossy().into_owned(),
        "--effect-file".to_string(),
        paths.effect.to_string_lossy().into_owned(),
        "--effect-log-file".to_string(),
        paths.effect_log.to_string_lossy().into_owned(),
        "--result-file".to_string(),
        paths.result.to_string_lossy().into_owned(),
        "--status-file".to_string(),
        paths.status.to_string_lossy().into_owned(),
        "--log-path".to_string(),
        request.log_path.to_string_lossy().into_owned(),
        "--run-id".to_string(),
        request.run_id.clone(),
        "--key".to_string(),
        paths.key.clone(),
        "--started-at-ms".to_string(),
        request.started_at_ms.to_string(),
        "--timeout".to_string(),
        request.timeout_seconds.to_string(),
    ];
    if let Some(context) = request.context.as_deref() {
        args.push("--context".to_string());
        args.push(context.to_string());
    }
    if let Some(worktree) = request.worktree.as_deref() {
        args.push("--worktree".to_string());
        args.push(worktree.to_string());
    }
    if let Some(sandbox) = request.sandbox {
        args.push("--sandbox".to_string());
        args.push(sandbox.as_str().to_string());
    }
    if let Some(label) = request.label.as_deref() {
        args.push("--label".to_string());
        args.push(label.to_string());
    }
    if let Some(role) = request.role.as_deref() {
        args.push("--role".to_string());
        args.push(role.to_string());
    }
    if let Some(proposal_id) = request.proposal_id.as_deref() {
        args.push("--proposal-id".to_string());
        args.push(proposal_id.to_string());
    }
    if let Some(dedup_key) = request.dedup_key.as_deref() {
        args.push("--dedup-key".to_string());
        args.push(dedup_key.to_string());
    }
    if let Some(dept) = request.dept.as_deref() {
        args.push("--dept".to_string());
        args.push(dept.to_string());
    }
    args
}

fn wait_for_adoption_result(
    request: &CodexRequest,
    paths: &CodexAdoptionPaths,
) -> Result<CodexResult> {
    let deadline = Instant::now()
        + overall_timeout(request.timeout_seconds)
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_CODEX_TIMEOUT_SECONDS as u64))
        + CODEX_ADOPTION_TIMEOUT_GRACE;
    loop {
        if let Some(result) = read_completed_adoption_result(paths)? {
            return Ok(result);
        }
        if let Some(result) = recover_completed_adoption_result(paths)? {
            return Ok(result);
        }
        if Instant::now() >= deadline {
            if let Some(record) = read_adoption_record(&paths.status)? {
                kill_adoption_worker_group(&record);
            }
            let message = format!(
                "adopted codex timed out waiting for result after {}s wall clock",
                request.timeout_seconds
            );
            return Ok(CodexResult::failure(
                "timeout",
                message,
                read_optional_string(&paths.stdout).map_err(mlua::Error::external)?,
                read_optional_string(&paths.stderr).map_err(mlua::Error::external)?,
                124,
                request.log_path.to_string_lossy().into_owned(),
            ));
        }
        std::thread::sleep(CODEX_ADOPTION_POLL);
    }
}

#[cfg(unix)]
fn kill_adoption_worker_group(record: &CodexAdoptionRecord) {
    if let Some(pid) = record.codex_pid {
        kill_process_group_by_pid(pid);
    }
    let Some(pid) = record.worker_pid else {
        return;
    };
    kill_process_group_by_pid(pid);
}

#[cfg(unix)]
fn kill_process_group_by_pid(pid: u32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
}

#[cfg(not(unix))]
fn kill_adoption_worker_group(_record: &CodexAdoptionRecord) {}

fn write_adoption_child_pid(request: &CodexRequest, child_pid: u32) {
    let Some(path) = request.adoption_status_path.as_deref() else {
        return;
    };
    let Some(work_dir) = path.parent() else {
        return;
    };
    let Some(key) = request.effect_key.as_deref() else {
        return;
    };
    let Ok(lock_file) = open_adoption_run_lock(work_dir) else {
        return;
    };
    if flock(lock_file.as_raw_fd(), FlockArg::LockExclusive).is_err() {
        return;
    }
    let Ok(Some(mut record)) = read_adoption_record(path) else {
        return;
    };
    if !adoption_record_is_current(&record, &request.run_id, key) {
        return;
    }
    record.codex_pid = Some(child_pid);
    let expected_status = record.status.clone();
    if let Err(err) = write_visible_adoption_record(path, &record, &expected_status) {
        eprintln!(
            "WARN: codex adoption child pid write failed run_id={} error={err}",
            request.run_id
        );
    }
}

#[derive(Debug)]
pub(crate) struct CodexWorkerOptions {
    host_root: PathBuf,
    work_dir: PathBuf,
    prompt_file: PathBuf,
    stdout_file: PathBuf,
    stderr_file: PathBuf,
    effect_file: PathBuf,
    effect_log_file: PathBuf,
    result_file: PathBuf,
    status_file: PathBuf,
    log_path: PathBuf,
    run_id: String,
    key: String,
    started_at_ms: u64,
    timeout_seconds: i64,
    context: Option<String>,
    worktree: Option<String>,
    sandbox: Option<CodexSandbox>,
    role: Option<String>,
    label: Option<String>,
    proposal_id: Option<String>,
    dedup_key: Option<String>,
    dept: Option<String>,
}

pub(crate) fn parse_worker_args(args: Vec<String>) -> anyhow::Result<CodexWorkerOptions> {
    let mut parser = WorkerArgParser::new(args);
    let host_root = parser.required_path("--host-root")?;
    let work_dir = parser.required_path("--work-dir")?;
    let prompt_file = parser.required_path("--prompt-file")?;
    let stdout_file = parser.required_path("--stdout-file")?;
    let stderr_file = parser.required_path("--stderr-file")?;
    let effect_file = parser.required_path("--effect-file")?;
    let effect_log_file = parser.required_path("--effect-log-file")?;
    let result_file = parser.required_path("--result-file")?;
    let status_file = parser.required_path("--status-file")?;
    let log_path = parser.required_path("--log-path")?;
    let run_id = parser.required_string("--run-id")?;
    let key = parser.required_string("--key")?;
    let started_at_ms = parser.required_string("--started-at-ms")?.parse::<u64>()?;
    let timeout_seconds = parser.required_string("--timeout")?.parse::<i64>()?;
    let context = parser.optional_string("--context");
    let worktree = parser.optional_string("--worktree");
    let sandbox = parser
        .optional_string("--sandbox")
        .map(|mode| CodexSandbox::parse(&mode))
        .transpose()?;
    let role = parser.optional_string("--role");
    let label = parser.optional_string("--label");
    let proposal_id = parser.optional_string("--proposal-id");
    let dedup_key = parser.optional_string("--dedup-key");
    let dept = parser.optional_string("--dept");
    parser.finish()?;
    Ok(CodexWorkerOptions {
        host_root,
        work_dir,
        prompt_file,
        stdout_file,
        stderr_file,
        effect_file,
        effect_log_file,
        result_file,
        status_file,
        log_path,
        run_id,
        key,
        started_at_ms,
        timeout_seconds,
        context,
        worktree,
        sandbox,
        role,
        label,
        proposal_id,
        dedup_key,
        dept,
    })
}

pub(crate) fn run_codex_worker(options: CodexWorkerOptions) -> anyhow::Result<i32> {
    std::fs::create_dir_all(&options.work_dir)?;
    let prompt = std::fs::read_to_string(&options.prompt_file)?;
    let request = CodexRequest {
        run_id: options.run_id.clone(),
        prompt,
        context: options.context.clone(),
        worktree: options.worktree.clone(),
        sandbox: options.sandbox,
        timeout_seconds: options.timeout_seconds,
        log_path: options.log_path.clone(),
        output_tail_path: codex_output_tail_path(&options.log_path),
        role: options.role.clone(),
        label: options.label.clone(),
        proposal_id: options.proposal_id.clone(),
        dedup_key: options.dedup_key.clone(),
        dept: options.dept.clone(),
        started_at_ms: options.started_at_ms,
        adoption_status_path: Some(options.status_file.clone()),
        effect_key: Some(options.key.clone()),
        effect_log_path: Some(options.effect_log_file.clone()),
    };
    let config = ConfigContext::from_host_root(&options.host_root)?;
    let mut adoption = CodexAdoptionRecord {
        key: options.key.clone(),
        run_id: options.run_id.clone(),
        role: options.role.clone(),
        label: options.label.clone(),
        dept: options.dept.clone(),
        proposal_id: options.proposal_id.clone(),
        dedup_key: options.dedup_key.clone(),
        status: CODEX_ADOPTION_STATUS_RUNNING.to_string(),
        started_at_ms: options.started_at_ms,
        ended_at_ms: None,
        timeout_seconds: options.timeout_seconds,
        worker_pid: Some(std::process::id()),
        codex_pid: None,
        exit_code: None,
        log_path: options.log_path.to_string_lossy().into_owned(),
        cmd_line: command_line_for_request(&request),
        error_kind: None,
        error: None,
    };
    let Some(_adoption_lifetime_witness) = claim_adoption_worker(&options, &mut adoption)? else {
        return Ok(0);
    };
    let _status_lifetime_witness = acquire_log_lifetime_witness(&request);
    let mut status = CodexStatusRecord::from_request(&request, None);
    write_codex_status(&request.log_path, &status);

    let result = match acquire_permit_with_context(&options.host_root, &config) {
        Ok(permit) => {
            status.permit_slot = Some(permit.slot);
            write_codex_status(&request.log_path, &status);
            let result = run_codex_request_with_permit(request, &config);
            drop(permit);
            result
        }
        Err(err) => {
            let message = format!("codex permit acquire failed: {err}");
            let cmd_line = adoption.cmd_line.clone();
            logged_failure(
                &request,
                &cmd_line,
                "permit",
                message,
                String::new(),
                String::new(),
                -1,
            )
        }
    };

    let ended_at_ms = unix_duration().as_millis() as u64;
    let paths = adoption_paths_from_worker_options(&options);
    if !publish_adoption_result_if_current(&paths, &mut adoption, &result, ended_at_ms)? {
        eprintln!(
            "WARN: codex adoption result publication fenced run_id={} key={}",
            adoption.run_id, adoption.key
        );
    }
    status.finish(result.exit_code);
    write_codex_status(Path::new(&result.log_path), &status);
    Ok(0)
}

fn claim_adoption_worker(
    options: &CodexWorkerOptions,
    adoption: &mut CodexAdoptionRecord,
) -> anyhow::Result<Option<std::fs::File>> {
    let lock_file = open_adoption_run_lock(&options.work_dir)?;
    flock(lock_file.as_raw_fd(), FlockArg::LockExclusive).map_err(anyhow::Error::from)?;
    let paths = adoption_paths_from_worker_options(options);
    if recover_completed_adoption_result_locked_from_disk(&paths)? {
        drop(lock_file);
        return Ok(None);
    }
    let Some(lifetime_witness) =
        try_acquire_adoption_worker_witness(&options.work_dir, &options.run_id)?
    else {
        drop(lock_file);
        return Ok(None);
    };
    let current_pid = std::process::id();
    let existing = read_adoption_record_from_disk(&options.status_file)?;
    if existing
        .as_ref()
        .is_some_and(|record| record.status == CODEX_ADOPTION_STATUS_COMPLETED)
    {
        drop(lock_file);
        return Ok(None);
    }
    if let Some(record) = existing.as_ref().filter(|record| {
        record.status == CODEX_ADOPTION_STATUS_RUNNING && record.worker_pid != Some(current_pid)
    }) {
        if adoption_effect_alive(&options.work_dir, record)? {
            drop(lock_file);
            return Ok(None);
        }
    }
    adoption.worker_pid = Some(current_pid);
    adoption.status = CODEX_ADOPTION_STATUS_RUNNING.to_string();
    write_visible_adoption_record(
        &options.status_file,
        adoption,
        CODEX_ADOPTION_STATUS_RUNNING,
    )?;
    drop(lock_file);
    Ok(Some(lifetime_witness))
}

fn adoption_paths_from_worker_options(options: &CodexWorkerOptions) -> CodexAdoptionPaths {
    CodexAdoptionPaths {
        key: options.key.clone(),
        dir: options.work_dir.clone(),
        prompt: options.prompt_file.clone(),
        stdout: options.stdout_file.clone(),
        stderr: options.stderr_file.clone(),
        effect: options.effect_file.clone(),
        effect_log: options.effect_log_file.clone(),
        result: options.result_file.clone(),
        status: options.status_file.clone(),
    }
}

fn publish_adoption_result_if_current(
    paths: &CodexAdoptionPaths,
    adoption: &mut CodexAdoptionRecord,
    result: &CodexResult,
    ended_at_ms: u64,
) -> anyhow::Result<bool> {
    let lock_file = open_adoption_run_lock(&paths.dir)?;
    flock(lock_file.as_raw_fd(), FlockArg::LockExclusive).map_err(anyhow::Error::from)?;
    let Some(current) = read_adoption_record_from_disk(&paths.status)? else {
        return Ok(false);
    };
    if !adoption_record_is_current(&current, &adoption.run_id, &adoption.key) {
        return Ok(false);
    }

    write_adoption_effect_record(
        &paths.effect,
        &CodexAdoptionEffectRecord {
            effect_key: adoption.key.clone(),
            ended_at_ms,
            stdout: result.stdout.clone(),
            stderr: result.stderr.clone(),
            exit_code: result.exit_code,
            error_kind: result.error_kind.clone(),
            error: result.error.clone(),
        },
    )?;
    write_atomic(&paths.stdout, result.stdout.as_bytes())?;
    write_atomic(&paths.stderr, result.stderr.as_bytes())?;
    write_adoption_result_record(
        &paths.result,
        &CodexAdoptionResultRecord {
            ended_at_ms,
            exit_code: result.exit_code,
            error_kind: result.error_kind.clone(),
            error: result.error.clone(),
        },
    )?;
    adoption.status = CODEX_ADOPTION_STATUS_COMPLETED.to_string();
    adoption.ended_at_ms = Some(ended_at_ms);
    adoption.exit_code = Some(result.exit_code);
    adoption.error_kind = result.error_kind.clone();
    adoption.error = result.error.clone();
    write_visible_adoption_record(&paths.status, adoption, CODEX_ADOPTION_STATUS_COMPLETED)?;
    Ok(true)
}

struct WorkerArgParser {
    args: Vec<String>,
}

impl WorkerArgParser {
    fn new(args: Vec<String>) -> Self {
        Self { args }
    }

    fn required_path(&mut self, flag: &str) -> anyhow::Result<PathBuf> {
        self.required_string(flag).map(PathBuf::from)
    }

    fn required_string(&mut self, flag: &str) -> anyhow::Result<String> {
        self.take(flag)
            .ok_or_else(|| anyhow::anyhow!("missing {flag}"))
    }

    fn optional_string(&mut self, flag: &str) -> Option<String> {
        self.take(flag)
    }

    fn take(&mut self, flag: &str) -> Option<String> {
        let index = self.args.iter().position(|arg| arg == flag)?;
        if index + 1 >= self.args.len() {
            return None;
        }
        let value = self.args.remove(index + 1);
        self.args.remove(index);
        Some(value)
    }

    fn finish(self) -> anyhow::Result<()> {
        if self.args.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("unknown __codex-worker argument: {}", self.args[0])
        }
    }
}

fn write_adoption_record(path: &Path, record: &CodexAdoptionRecord) -> anyhow::Result<()> {
    write_atomic(path, serde_json::to_string(record)?.as_bytes())
}

fn write_adoption_result_record(
    path: &Path,
    record: &CodexAdoptionResultRecord,
) -> anyhow::Result<()> {
    write_atomic(path, serde_json::to_string(record)?.as_bytes())
}

fn write_adoption_effect_record(
    path: &Path,
    record: &CodexAdoptionEffectRecord,
) -> anyhow::Result<()> {
    write_atomic(path, serde_json::to_string(record)?.as_bytes())
}

fn write_visible_adoption_record(
    path: &Path,
    record: &CodexAdoptionRecord,
    expected_status: &str,
) -> anyhow::Result<()> {
    write_adoption_record(path, record)?;
    let visible = read_adoption_record_from_disk(path)?
        .ok_or_else(|| anyhow::anyhow!("codex adoption record not visible after write"))?;
    if visible.key != record.key {
        anyhow::bail!(
            "codex adoption record key mismatch after write: expected {} got {}",
            record.key,
            visible.key
        );
    }
    if visible.status != expected_status {
        anyhow::bail!(
            "codex adoption record status mismatch after write: expected {} got {}",
            expected_status,
            visible.status
        );
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        filename_timestamp()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
fn wait_for_test_hook(env: &str) {
    let Some(path) = std::env::var_os(env).filter(|value| !value.is_empty()) else {
        return;
    };
    while Path::new(&path).exists() {
        std::thread::sleep(CODEX_ADOPTION_POLL);
    }
}

#[cfg(not(test))]
fn wait_for_test_hook(_env: &str) {}

#[cfg(test)]
fn fail_for_test_hook(env: &str) -> Result<()> {
    if std::env::var_os(env)
        .filter(|value| !value.is_empty())
        .is_some()
    {
        return Err(mlua::Error::external(format!(
            "{env} requested failure after adoption intent"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn fail_for_test_hook(_env: &str) -> Result<()> {
    Ok(())
}

impl CodexStatusRecord {
    fn from_request(request: &CodexRequest, permit_slot: Option<usize>) -> Self {
        Self {
            run_id: request.run_id.clone(),
            role: request.role.clone().or_else(|| request.label.clone()),
            label: request.label.clone(),
            dept: request.dept.clone(),
            proposal_id: request.proposal_id.clone(),
            dedup_key: request.dedup_key.clone(),
            started_at: unix_millis_to_iso8601(request.started_at_ms),
            started_at_ms: request.started_at_ms,
            timeout_seconds: Some(request.timeout_seconds),
            ended_at: None,
            ended_at_ms: None,
            elapsed_ms: None,
            status: "running".to_string(),
            exit_code: None,
            permit_slot,
            output_tail_path: Some(request.output_tail_path.to_string_lossy().into_owned()),
        }
    }

    fn finish(&mut self, exit_code: i32) {
        let ended_at_ms = unix_duration().as_millis() as u64;
        self.ended_at = Some(unix_millis_to_iso8601(ended_at_ms));
        self.ended_at_ms = Some(ended_at_ms);
        self.elapsed_ms = Some(ended_at_ms.saturating_sub(self.started_at_ms));
        self.status = "completed".to_string();
        self.exit_code = Some(exit_code);
    }
}

fn codex_status(lua: &Lua, host_root: &Path) -> Result<Table> {
    let mut records = read_codex_status_records(host_root).map_err(mlua::Error::external)?;
    let now_ms = unix_duration().as_millis() as u64;
    records.sort_by(|left, right| {
        right
            .started_at_ms
            .cmp(&left.started_at_ms)
            .then_with(|| right.run_id.cmp(&left.run_id))
    });

    let result = lua.create_table()?;
    let running = lua.create_table()?;
    let recent = lua.create_table()?;
    let mut running_index = 1;
    let mut recent_index = 1;
    for record in records {
        let item = codex_status_record_table(lua, &record, now_ms)?;
        if record.status == "running" {
            running.set(running_index, item)?;
            running_index += 1;
        } else if recent_index <= CODEX_STATUS_RECENT_LIMIT {
            recent.set(recent_index, item)?;
            recent_index += 1;
        }
    }
    result.set("running", running)?;
    result.set("recent", recent)?;
    Ok(result)
}

fn codex_status_record_table(lua: &Lua, record: &CodexStatusRecord, now_ms: u64) -> Result<Table> {
    let table = lua.create_table()?;
    table.set("run_id", record.run_id.clone())?;
    if let Some(role) = record.role.as_ref().or(record.label.as_ref()) {
        table.set("role", role.as_str())?;
    }
    set_optional_string(&table, "label", &record.label)?;
    set_optional_string(&table, "dept", &record.dept)?;
    set_optional_string(&table, "proposal_id", &record.proposal_id)?;
    set_optional_string(&table, "dedup_key", &record.dedup_key)?;
    if let Some(key) = record.proposal_id.as_ref().or(record.dedup_key.as_ref()) {
        table.set("proposal_id_or_key", key.as_str())?;
    }
    table.set("started_at", record.started_at.clone())?;
    table.set("started_at_ms", record.started_at_ms)?;
    if let Some(timeout_seconds) = record.timeout_seconds {
        table.set("timeout_seconds", timeout_seconds)?;
        if let Some(lease_expires_at_ms) =
            lease_expires_at_ms(record.started_at_ms, timeout_seconds)
        {
            table.set("lease_expires_at_ms", lease_expires_at_ms)?;
            table.set(
                "lease_expires_at",
                unix_millis_to_iso8601(lease_expires_at_ms),
            )?;
        }
    }
    set_optional_string(&table, "ended_at", &record.ended_at)?;
    if let Some(ended_at_ms) = record.ended_at_ms {
        table.set("ended_at_ms", ended_at_ms)?;
    }
    let elapsed_ms = record
        .elapsed_ms
        .unwrap_or_else(|| now_ms.saturating_sub(record.started_at_ms));
    table.set("elapsed_ms", elapsed_ms)?;
    table.set("status", sdk_status(record))?;
    if let Some(exit_code) = record.exit_code {
        table.set("exit_code", exit_code)?;
    }
    if let Some(permit_slot) = record.permit_slot {
        table.set("permit_slot", permit_slot)?;
    }
    table.set("output_tail", read_codex_output_tail(record))?;
    Ok(table)
}

fn sdk_status(record: &CodexStatusRecord) -> &'static str {
    if record.status == "running" {
        "running"
    } else if record.exit_code == Some(0) {
        "done"
    } else {
        "failed"
    }
}

fn read_codex_output_tail(record: &CodexStatusRecord) -> String {
    let Some(path) = record.output_tail_path.as_deref() else {
        return String::new();
    };
    match std::fs::read(path) {
        Ok(bytes) => bounded_output_tail(&bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            eprintln!(
                "WARN: codex output tail read failed path={} error={err}",
                path
            );
            String::new()
        }
    }
}

fn set_optional_string(table: &Table, key: &str, value: &Option<String>) -> Result<()> {
    if let Some(value) = value {
        table.set(key, value.as_str())?;
    }
    Ok(())
}

fn write_codex_status(log_path: &Path, record: &CodexStatusRecord) {
    if let Err(err) = append_codex_status_log(log_path, record) {
        eprintln!(
            "WARN: codex status write failed run_id={} error={err}",
            record.run_id
        );
    }
}

fn append_codex_status_log(log_path: &Path, record: &CodexStatusRecord) -> anyhow::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(
        file,
        "{CODEX_STATUS_LOG_PREFIX}{}",
        serde_json::to_string(record)?
    )?;
    Ok(())
}

fn append_codex_effect_log(
    log_path: &Path,
    record: &CodexAdoptionEffectRecord,
) -> anyhow::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(
        file,
        "{CODEX_EFFECT_LOG_PREFIX}{}",
        serde_json::to_string(record)?
    )?;
    Ok(())
}

fn read_codex_status_records(host_root: &Path) -> anyhow::Result<Vec<CodexStatusRecord>> {
    let _ = host_root;
    let log_dir = runtime_log_dir().join("codex");
    let mut records = Vec::new();
    match std::fs::read_dir(&log_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(OsStr::to_str) != Some("log") {
                    continue;
                }
                let body = std::fs::read_to_string(&path)?;
                let witness_held = match lifetime_witness_held(&path) {
                    Ok(held) => held,
                    Err(err) => {
                        eprintln!(
                            "WARN: codex lifetime witness probe failed path={} error={err}",
                            path.display()
                        );
                        true
                    }
                };
                for line in body.lines() {
                    let Some(json) = line.strip_prefix(CODEX_STATUS_LOG_PREFIX) else {
                        continue;
                    };
                    match serde_json::from_str::<CodexStatusRecord>(json) {
                        Ok(mut record) => {
                            if record.status == "running" && !witness_held {
                                record.status = "abandoned".to_string();
                                record.exit_code = Some(-1);
                            }
                            records.push(record);
                        }
                        Err(err) => eprintln!(
                            "WARN: codex status log line skipped path={} error={err}",
                            path.display()
                        ),
                    }
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    records.extend(read_adoption_status_records(host_root)?);
    Ok(latest_codex_status_records(records))
}

fn read_adoption_status_records(host_root: &Path) -> anyhow::Result<Vec<CodexStatusRecord>> {
    let adoption_dir = runtime_context::layout_from_host_root(host_root)?
        .runtime_dir(RuntimeKind::Logs)
        .join(CODEX_ADOPTION_DIR);
    let entries = match std::fs::read_dir(&adoption_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        let work_dir = entry.path();
        let status_path = work_dir.join("status.json");
        match read_adoption_record(&status_path) {
            Ok(Some(record)) => records.push(codex_status_record_from_adoption(record, &work_dir)),
            Ok(None) => {}
            Err(err) => {
                eprintln!(
                    "WARN: codex adoption status skipped path={} error={err}",
                    status_path.display()
                );
                continue;
            }
        }
    }
    Ok(records)
}

fn codex_status_record_from_adoption(
    record: CodexAdoptionRecord,
    work_dir: &Path,
) -> CodexStatusRecord {
    let ended_at_ms = record.ended_at_ms;
    let status = adoption_status_for_observe(&record, work_dir);
    let exit_code = adoption_status_exit_code(&record, status);
    CodexStatusRecord {
        run_id: record.run_id,
        role: record.role,
        label: record.label,
        dept: record.dept,
        proposal_id: record.proposal_id,
        dedup_key: record.dedup_key.or(Some(record.key)),
        started_at: unix_millis_to_iso8601(record.started_at_ms),
        started_at_ms: record.started_at_ms,
        timeout_seconds: Some(record.timeout_seconds),
        ended_at: ended_at_ms.map(unix_millis_to_iso8601),
        ended_at_ms,
        elapsed_ms: ended_at_ms.map(|ended| ended.saturating_sub(record.started_at_ms)),
        status: status.to_string(),
        exit_code,
        permit_slot: None,
        output_tail_path: Some(
            codex_output_tail_path(Path::new(&record.log_path))
                .to_string_lossy()
                .into_owned(),
        ),
    }
}

fn adoption_status_exit_code(record: &CodexAdoptionRecord, observed_status: &str) -> Option<i32> {
    if observed_status == CODEX_ADOPTION_STATUS_RUNNING {
        None
    } else {
        Some(record.exit_code.unwrap_or(-1))
    }
}

fn adoption_status_for_observe(record: &CodexAdoptionRecord, work_dir: &Path) -> &'static str {
    match record.status.as_str() {
        CODEX_ADOPTION_STATUS_COMPLETED => "completed",
        CODEX_ADOPTION_STATUS_INTENT if adoption_intent_alive(work_dir) => {
            CODEX_ADOPTION_STATUS_RUNNING
        }
        CODEX_ADOPTION_STATUS_RUNNING => match adoption_effect_alive(work_dir, record) {
            Ok(true) => CODEX_ADOPTION_STATUS_RUNNING,
            Ok(false) => "completed",
            Err(err) => {
                eprintln!(
                    "WARN: codex adoption lifetime witness indeterminate run_id={} error={err}",
                    record.run_id
                );
                CODEX_ADOPTION_STATUS_RUNNING
            }
        },
        _ => "completed",
    }
}

fn latest_codex_status_records(records: Vec<CodexStatusRecord>) -> Vec<CodexStatusRecord> {
    let mut latest = std::collections::HashMap::<String, CodexStatusRecord>::new();
    for record in records {
        let key = record.run_id.clone();
        match latest.get(&key) {
            Some(existing) if status_record_time(existing) > status_record_time(&record) => {}
            _ => {
                latest.insert(key, record);
            }
        }
    }
    latest.into_values().collect()
}

fn status_record_time(record: &CodexStatusRecord) -> u64 {
    record.ended_at_ms.unwrap_or(record.started_at_ms)
}

fn lease_expires_at_ms(started_at_ms: u64, timeout_seconds: i64) -> Option<u64> {
    u64::try_from(timeout_seconds)
        .ok()
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|timeout_ms| started_at_ms.checked_add(timeout_ms))
}

fn run_codex_request_with_permit(mut request: CodexRequest, config: &ConfigContext) -> CodexResult {
    let shim_dir = match prepare_rate_shims_for_codex(config) {
        Ok(shim_dir) => shim_dir,
        Err(err) => {
            let cmd_line = command_line_for_request(&request);
            return logged_failure(
                &request,
                &cmd_line,
                "rate_pool",
                format!("codex rate shim setup failed: {err}"),
                String::new(),
                String::new(),
                -1,
            );
        }
    };
    let codex_program = resolve_codex_program(shim_dir.as_deref());
    let (mut cmd, cmd_line) = command_for_request(&request, &codex_program);
    if let Some(shim_dir) = shim_dir.as_deref() {
        crate::rate_shim::prepend_shim_dir_to_path(&mut cmd, shim_dir);
    }
    if let Err(err) = validate_codex_program(&codex_program) {
        return logged_failure(
            &request,
            &cmd_line,
            "spawn",
            format!("codex spawn failed: {err}"),
            String::new(),
            String::new(),
            -1,
        );
    }
    eprintln!(
        "SPAWN: log={} cd={} timeout={}",
        request.log_path.display(),
        request.worktree.as_deref().unwrap_or("."),
        request.timeout_seconds
    );

    let child = match spawn_codex_child(&request, &mut cmd, &cmd_line) {
        Ok(child) => child,
        Err(result) => return *result,
    };
    let child_pid = child.id();
    write_adoption_child_pid(&request, child_pid);
    let registration = crate::process_tree::sdk_process_groups().register(child_pid);
    let prompt = std::mem::take(&mut request.prompt);
    let (child, stdin_writer) = match spawn_stdin_writer(child, prompt, &request, &cmd_line) {
        Ok(parts) => parts,
        Err(result) => return *result,
    };

    wait_for_codex_child(child, stdin_writer, &request, &cmd_line, registration)
}

fn prepare_rate_shims_for_codex(config: &ConfigContext) -> Result<Option<PathBuf>> {
    let registry =
        crate::rate_pool::RatePoolRegistry::from_config(config).map_err(mlua::Error::external)?;
    if registry.is_empty() {
        return Ok(None);
    }
    let framework_bin = std::env::current_exe().map_err(mlua::Error::external)?;
    let shim_dir = crate::rate_shim::ensure_rate_shims(&registry, &framework_bin)
        .map_err(mlua::Error::external)?;
    Ok(Some(shim_dir))
}

fn resolve_codex_program(shim_dir: Option<&Path>) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let shim_dir = shim_dir.unwrap_or_else(|| Path::new(""));
    crate::rate_shim::resolve_program_on_path("codex", Some(&path), shim_dir)
        .unwrap_or_else(|| PathBuf::from("codex"))
}

fn validate_codex_program(program: &Path) -> std::result::Result<(), String> {
    if program == Path::new("codex") || program.is_file() {
        return Ok(());
    }
    Err(format!(
        "resolved codex path is not a file: {}",
        program.display()
    ))
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
fn spawn_stdin_writer(
    mut child: Child,
    prompt: String,
    request: &CodexRequest,
    cmd_line: &str,
) -> std::result::Result<(Child, JoinHandle<std::io::Result<()>>), Box<CodexResult>> {
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
    let writer = std::thread::spawn(move || {
        let result = stdin.write_all(prompt.as_bytes());
        drop(stdin);
        result
    });
    Ok((child, writer))
}

fn join_stdin_writer(writer: JoinHandle<std::io::Result<()>>) -> std::result::Result<(), String> {
    writer
        .join()
        .map_err(|_| "codex stdin writer thread panicked".to_string())?
        .map_err(|err| format!("codex stdin write failed: {err}"))
}

fn wait_for_codex_child(
    child: Child,
    stdin_writer: JoinHandle<std::io::Result<()>>,
    request: &CodexRequest,
    cmd_line: &str,
    _registration: crate::process_tree::ProcessGroupRegistration,
) -> CodexResult {
    let wait_outcome = wait_codex_with_timeout_and_tail(
        child,
        request.timeout_seconds,
        Some(request.output_tail_path.clone()),
    );
    let stdin_result = join_stdin_writer(stdin_writer);
    match wait_outcome {
        CodexWaitOutcome::Exited {
            stdout,
            stderr,
            exit_code,
        } => {
            if exit_code == 0 {
                if let Err(message) = stdin_result {
                    let result = logged_failure(
                        request, cmd_line, "stdin", message, stdout, stderr, exit_code,
                    );
                    return codex_result_with_effect_receipt(request, result);
                }
            }
            let result = CodexResult::success(
                stdout,
                stderr,
                exit_code,
                request.log_path.to_string_lossy().into_owned(),
            );
            let result = codex_result_with_effect_receipt(request, result);
            write_codex_log(
                &request.log_path,
                &result.stdout,
                &result.stderr,
                result.exit_code,
                cmd_line,
                request.timeout_seconds,
            );
            result
        }
        CodexWaitOutcome::Failed {
            kind,
            message,
            stdout,
            stderr,
            exit_code,
        } => {
            let result =
                logged_failure(request, cmd_line, kind, message, stdout, stderr, exit_code);
            codex_result_with_effect_receipt(request, result)
        }
    }
}

fn codex_result_with_effect_receipt(request: &CodexRequest, result: CodexResult) -> CodexResult {
    if let Err(err) = write_codex_effect_receipt(request, &result) {
        return CodexResult::failure(
            "effect_receipt",
            format!("codex effect receipt write failed: {err}"),
            result.stdout,
            result.stderr,
            -1,
            result.log_path,
        );
    }
    result
}

fn write_codex_effect_receipt(request: &CodexRequest, result: &CodexResult) -> anyhow::Result<()> {
    let Some(effect_key) = request.effect_key.as_deref() else {
        return Ok(());
    };
    let Some(effect_log_path) = request.effect_log_path.as_deref() else {
        anyhow::bail!("codex effect receipt path missing for keyed adoption");
    };
    let status_path = request
        .adoption_status_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("codex adoption status path missing for effect receipt"))?;
    let work_dir = status_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("codex adoption status path has no parent"))?;
    let lock_file = open_adoption_run_lock(work_dir)?;
    flock(lock_file.as_raw_fd(), FlockArg::LockExclusive).map_err(anyhow::Error::from)?;
    let current = read_adoption_record_from_disk(status_path)?;
    if !current
        .as_ref()
        .is_some_and(|record| adoption_record_is_current(record, &request.run_id, effect_key))
    {
        eprintln!(
            "WARN: codex adoption effect receipt publication fenced run_id={} key={effect_key}",
            request.run_id
        );
        return Ok(());
    }
    let receipt = CodexAdoptionEffectRecord {
        effect_key: effect_key.to_string(),
        ended_at_ms: unix_duration().as_millis() as u64,
        stdout: result.stdout.clone(),
        stderr: result.stderr.clone(),
        exit_code: result.exit_code,
        error_kind: result.error_kind.clone(),
        error: result.error.clone(),
    };
    append_codex_effect_log(effect_log_path, &receipt)
}

fn wait_codex_with_timeout_and_tail(
    mut child: Child,
    timeout_seconds: i64,
    output_tail_path: Option<PathBuf>,
) -> CodexWaitOutcome {
    let start = Instant::now();
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

    let timeout = overall_timeout(timeout_seconds);
    let mut killed_for_timeout: Option<String> = None;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = loop {
        let event = if killed_for_timeout.is_some() {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else if let Some(timeout) = timeout {
            let remaining = timeout.saturating_sub(start.elapsed());
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
                    match stream {
                        CodexStream::Stdout => stdout.extend(bytes),
                        CodexStream::Stderr => stderr.extend(bytes),
                    }
                    write_live_output_tail(output_tail_path.as_deref(), &stdout, &stderr);
                }
            }
            Ok(CodexEvent::Exited(result)) => break result,
            Err(RecvTimeoutError::Timeout) => {
                let message = format!(
                    "codex timed out after {timeout_seconds}s wall clock; sent SIGKILL to process group"
                );
                if let Err(err) = kill_process_group(child_pid) {
                    killed_for_timeout = Some(format!("{message}; kill failed: {err}"));
                } else {
                    killed_for_timeout = Some(message);
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
    write_live_output_tail(output_tail_path.as_deref(), &stdout, &stderr);

    codex_outcome_from_wait(exit, killed_for_timeout, stdout, stderr)
}

// overall timeout failures get stable kind, exit 124, and captured output.
fn codex_outcome_from_wait(
    exit: std::result::Result<ExitStatus, String>,
    killed_for_timeout: Option<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> CodexWaitOutcome {
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    let exit_code = if killed_for_timeout.is_some() {
        124
    } else {
        exit.as_ref().ok().and_then(ExitStatus::code).unwrap_or(-1)
    };

    if let Some(message) = killed_for_timeout {
        return CodexWaitOutcome::Failed {
            kind: "timeout",
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

// readers forward chunks immediately for output capture.
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

// child exit is delivered beside output.
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

fn write_live_output_tail(path: Option<&Path>, stdout: &[u8], stderr: &[u8]) {
    let Some(path) = path else {
        return;
    };
    let mut combined = Vec::with_capacity(stdout.len().saturating_add(stderr.len()));
    combined.extend_from_slice(stdout);
    combined.extend_from_slice(stderr);
    let tail = bounded_output_tail(&combined);
    if let Err(err) = write_atomic(path, tail.as_bytes()) {
        eprintln!(
            "WARN: codex output tail write failed path={} error={err}",
            path.display()
        );
    }
}

fn bounded_output_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text
        .lines()
        .rev()
        .take(CODEX_OUTPUT_TAIL_MAX_LINES)
        .collect::<Vec<_>>();
    lines.reverse();
    let mut tail = lines.join("\n");
    if text.ends_with('\n') && !tail.is_empty() {
        tail.push('\n');
    }
    truncate_tail_utf8(&tail, CODEX_OUTPUT_TAIL_MAX_BYTES)
}

fn truncate_tail_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_string()
}

// positive values become overall timeout caps; invalid values mean unbounded wait.
fn overall_timeout(timeout_seconds: i64) -> Option<Duration> {
    u64::try_from(timeout_seconds)
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
    write_live_output_tail(
        Some(&request.output_tail_path),
        stdout.as_bytes(),
        logged_stderr.as_bytes(),
    );
    write_codex_log(
        &request.log_path,
        &stdout,
        &logged_stderr,
        exit_code,
        cmd_line,
        request.timeout_seconds,
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

fn command_for_request(request: &CodexRequest, program: &Path) -> (Command, String) {
    let mut cmd = Command::new(program);
    let cmd_args = command_args_for_request(request);
    cmd.args(&cmd_args);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    scrub_github_auth_for_codex(&mut cmd, request);
    let cmd_line = format_command("codex", &cmd_args);
    (cmd, cmd_line)
}

fn scrub_github_auth_for_codex(cmd: &mut Command, request: &CodexRequest) {
    for key in GITHUB_AUTH_ENV_KEYS {
        cmd.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        if is_github_credential_env_key(&key) {
            cmd.env_remove(key);
        }
    }

    let gh_config_dir = codex_gh_config_dir(request);
    if let Err(err) = std::fs::create_dir_all(&gh_config_dir) {
        eprintln!(
            "WARN: codex gh config dir create failed path={} error={err}",
            gh_config_dir.display()
        );
    }
    // Complete gh banning also needs OS-enforced egress denial, tracked in substrate#256.
    // This strips credentials only; codex may still invoke gh, but it does not inherit bot auth.
    cmd.env(GH_CONFIG_DIR_ENV, gh_config_dir);
}

fn is_github_credential_env_key(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    if key == GH_CONFIG_DIR_ENV {
        return false;
    }
    if GITHUB_AUTH_ENV_KEYS.contains(&key) {
        return true;
    }
    let Some(suffix) = key
        .strip_prefix("GH_")
        .or_else(|| key.strip_prefix("GITHUB_"))
    else {
        return false;
    };
    let suffix = suffix.to_ascii_uppercase();
    suffix.split('_').any(|part| {
        matches!(
            part,
            "AUTH"
                | "OAUTH"
                | "TOKEN"
                | "SECRET"
                | "PASSWORD"
                | "CREDENTIAL"
                | "CREDENTIALS"
                | "PAT"
                | "KEY"
        )
    })
}

fn codex_gh_config_dir(request: &CodexRequest) -> PathBuf {
    request
        .log_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(runtime_log_dir)
        .join("gh-config")
        .join(&request.run_id)
}

fn command_line_for_request(request: &CodexRequest) -> String {
    format_command("codex", &command_args_for_request(request))
}

fn command_args_for_request(request: &CodexRequest) -> Vec<String> {
    let mut args = vec!["exec".to_string()];
    if let Some(sandbox) = request.sandbox {
        args.push("--sandbox".to_string());
        args.push(sandbox.as_str().to_string());
    } else {
        args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    }
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

fn codex_output_tail_path(log_path: &Path) -> PathBuf {
    let stem = log_path
        .file_stem()
        .and_then(OsStr::to_str)
        .filter(|stem| !stem.is_empty())
        .unwrap_or("codex");
    log_path.with_file_name(format!("{stem}.tail"))
}

fn prune_codex_logs_for_request(request: &CodexRequest) {
    let Some(log_dir) = request.log_path.parent() else {
        return;
    };
    let policy = CodexLogRetentionPolicy::from_env();
    if let Err(err) = prune_codex_logs(log_dir, &request.log_path, policy, SystemTime::now()) {
        eprintln!(
            "WARN: codex log prune failed dir={} current_log={} error={err}",
            log_dir.display(),
            request.log_path.display()
        );
    }
}

#[derive(Clone, Copy)]
struct CodexLogRetentionPolicy {
    max_age: Option<Duration>,
    max_bytes: Option<u64>,
}

impl CodexLogRetentionPolicy {
    fn from_env() -> Self {
        Self {
            max_age: retention_max_age_from_env(),
            max_bytes: retention_max_bytes_from_env(),
        }
    }
}

struct CodexLogEntry {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
    unlocked_witness: Option<std::fs::File>,
}

fn prune_codex_logs(
    log_dir: &Path,
    current_log_path: &Path,
    policy: CodexLogRetentionPolicy,
    now: SystemTime,
) -> anyhow::Result<()> {
    if policy.max_age.is_none() && policy.max_bytes.is_none() {
        return Ok(());
    }
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    let current_log_path = current_log_path.to_path_buf();
    let mut retained = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!(
                    "WARN: codex log prune entry skipped dir={} error={err}",
                    log_dir.display()
                );
                continue;
            }
        };
        let path = entry.path();
        if path == current_log_path || path.extension().and_then(OsStr::to_str) != Some("log") {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                eprintln!(
                    "WARN: codex log prune metadata skipped path={} error={err}",
                    path.display()
                );
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let unlocked_witness = match probe_lifetime_witness(&path) {
            Ok(LifetimeWitnessProbe::Unlocked(file)) => Some(file),
            Ok(LifetimeWitnessProbe::Held) => None,
            Ok(LifetimeWitnessProbe::Missing) => continue,
            Err(err) => {
                eprintln!(
                    "WARN: codex log prune witness probe failed path={} error={err}",
                    path.display()
                );
                None
            }
        };
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        if unlocked_witness.is_some() {
            if let Some(max_age) = policy.max_age {
                if now.duration_since(modified).unwrap_or(Duration::ZERO) > max_age {
                    remove_codex_log(&path);
                    continue;
                }
            }
        }
        retained.push(CodexLogEntry {
            path,
            len: metadata.len(),
            modified,
            unlocked_witness,
        });
    }

    if let Some(max_bytes) = policy.max_bytes {
        prune_codex_logs_by_size(retained, max_bytes);
    }
    Ok(())
}

fn prune_codex_logs_by_size(mut entries: Vec<CodexLogEntry>, max_bytes: u64) {
    let mut total = entries
        .iter()
        .fold(0_u64, |sum, entry| sum.saturating_add(entry.len));
    if total <= max_bytes {
        return;
    }
    entries.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    for entry in entries {
        if total <= max_bytes {
            break;
        }
        if entry.unlocked_witness.is_none() {
            continue;
        }
        remove_codex_log(&entry.path);
        total = total.saturating_sub(entry.len);
    }
}

fn remove_codex_log(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        eprintln!(
            "WARN: codex log prune remove failed path={} error={err}",
            path.display()
        );
    }
    let tail_path = codex_output_tail_path(path);
    if let Err(err) = std::fs::remove_file(&tail_path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "WARN: codex tail prune remove failed path={} error={err}",
                tail_path.display()
            );
        }
    }
}

fn retention_max_age_from_env() -> Option<Duration> {
    match std::env::var(CODEX_LOG_MAX_AGE_ENV) {
        Ok(raw) => parse_retention_duration(&raw).unwrap_or_else(|err| {
            eprintln!("WARN: {CODEX_LOG_MAX_AGE_ENV} ignored value={raw:?} error={err}");
            Some(DEFAULT_CODEX_LOG_MAX_AGE)
        }),
        Err(std::env::VarError::NotPresent) => Some(DEFAULT_CODEX_LOG_MAX_AGE),
        Err(std::env::VarError::NotUnicode(_)) => {
            eprintln!("WARN: {CODEX_LOG_MAX_AGE_ENV} ignored non-unicode value");
            Some(DEFAULT_CODEX_LOG_MAX_AGE)
        }
    }
}

fn retention_max_bytes_from_env() -> Option<u64> {
    match std::env::var(CODEX_LOG_MAX_BYTES_ENV) {
        Ok(raw) => parse_optional_u64(&raw).unwrap_or_else(|err| {
            eprintln!("WARN: {CODEX_LOG_MAX_BYTES_ENV} ignored value={raw:?} error={err}");
            None
        }),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            eprintln!("WARN: {CODEX_LOG_MAX_BYTES_ENV} ignored non-unicode value");
            None
        }
    }
}

fn parse_retention_duration(raw: &str) -> std::result::Result<Option<Duration>, String> {
    let value = raw.trim();
    if value.is_empty() || value == "0" {
        return Ok(None);
    }
    let (number, multiplier) = match value.as_bytes().last().copied() {
        Some(b's') => (&value[..value.len() - 1], 1_u64),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 60 * 60),
        Some(b'd') => (&value[..value.len() - 1], 24 * 60 * 60),
        Some(byte) if byte.is_ascii_digit() => (value, 1),
        _ => return Err("expected integer seconds or s/m/h/d suffix".to_string()),
    };
    let count = number
        .parse::<u64>()
        .map_err(|_| "expected positive integer duration".to_string())?;
    if count == 0 {
        return Ok(None);
    }
    count
        .checked_mul(multiplier)
        .map(Duration::from_secs)
        .map(Some)
        .ok_or_else(|| "duration overflow".to_string())
}

fn parse_optional_u64(raw: &str) -> std::result::Result<Option<u64>, String> {
    let value = raw.trim();
    if value.is_empty() || value == "0" {
        return Ok(None);
    }
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| "expected non-negative integer bytes".to_string())
}

fn codex_run_id() -> String {
    let sequence = NEXT_CODEX_RUN_ID.fetch_add(1, Ordering::Relaxed);
    format!("codex-{}-{sequence}", filename_timestamp())
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
    timeout_seconds: i64,
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
        "{stdout}{stdout_sep}{stderr}{stderr_sep}EXIT={exit_code}\nDONE_AT={done_at}\nCMD={cmd_line}\nTIMEOUT_SECONDS={timeout_seconds}\n",
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
    if let Err(err) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(content.as_bytes()))
    {
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

fn unix_millis_to_iso8601(millis: u64) -> String {
    unix_seconds_to_iso8601(millis / 1000)
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
pub(crate) fn acquire_permit() -> mlua::Result<CodexPermit> {
    let host_root = std::env::current_dir().map_err(mlua::Error::external)?;
    let config = ConfigContext::from_host_root(&host_root).map_err(mlua::Error::external)?;
    acquire_permit_with_context(&host_root, &config)
}

fn acquire_permit_with_context(
    host_root: &Path,
    config: &ConfigContext,
) -> mlua::Result<CodexPermit> {
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
            return Ok(CodexPermit { slot: i, _file: f });
        }
    }

    let p = pool_dir.join("permit-0");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&p)
        .map_err(mlua::Error::external)?;
    flock(f.as_raw_fd(), FlockArg::LockExclusive).map_err(mlua::Error::external)?;
    Ok(CodexPermit { slot: 0, _file: f })
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

    fn command_test_request() -> CodexRequest {
        let run_id = codex_run_id();
        let log_path = std::env::temp_dir()
            .join("fkst-sdk-codex-command-test")
            .join(&run_id)
            .join("run.log");
        CodexRequest {
            run_id,
            prompt: String::new(),
            context: None,
            worktree: None,
            sandbox: None,
            timeout_seconds: DEFAULT_CODEX_TIMEOUT_SECONDS,
            output_tail_path: codex_output_tail_path(&log_path),
            log_path,
            role: None,
            label: None,
            proposal_id: None,
            dedup_key: None,
            dept: None,
            started_at_ms: 0,
            adoption_status_path: None,
            effect_key: None,
            effect_log_path: None,
        }
    }

    #[test]
    fn unix_seconds_to_iso8601_formats_epoch_and_leap_day() {
        assert_eq!(unix_seconds_to_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            unix_seconds_to_iso8601(1_709_251_199),
            "2024-02-29T23:59:59Z"
        );
    }

    #[test]
    fn codex_command_uses_requested_sandbox_without_bypass_flag() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("prompt", "judge").unwrap();
        opts.set("sandbox", "read-only").unwrap();
        let request = codex_request_from_opts(opts, None).unwrap();

        let args = command_args_for_request(&request);

        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--sandbox".to_string(),
                "read-only".to_string(),
                "-".to_string()
            ]
        );
        assert!(!args
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn codex_command_without_sandbox_preserves_default_argv() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("prompt", "implement").unwrap();
        let request = codex_request_from_opts(opts, None).unwrap();

        let args = command_args_for_request(&request);

        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "-".to_string()
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--sandbox"));
    }

    #[test]
    fn codex_request_rejects_invalid_sandbox_mode() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("prompt", "judge").unwrap();
        opts.set("sandbox", "bogus").unwrap();

        let err = match codex_request_from_opts(opts, None) {
            Ok(_) => panic!("invalid sandbox mode should fail closed"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("codex-sandbox-invalid"));
    }

    #[test]
    fn adoption_request_hash_includes_sandbox_mode() {
        let mut default = command_test_request();
        default.prompt = "same prompt".to_string();
        default.worktree = Some("same-worktree".to_string());
        let mut read_only = command_test_request();
        read_only.prompt = default.prompt.clone();
        read_only.worktree = default.worktree.clone();
        read_only.sandbox = Some(CodexSandbox::ReadOnly);
        let mut workspace_write = command_test_request();
        workspace_write.prompt = default.prompt.clone();
        workspace_write.worktree = default.worktree.clone();
        workspace_write.sandbox = Some(CodexSandbox::WorkspaceWrite);
        let mut danger_full_access = command_test_request();
        danger_full_access.prompt = default.prompt.clone();
        danger_full_access.worktree = default.worktree.clone();
        danger_full_access.sandbox = Some(CodexSandbox::DangerFullAccess);

        let hashes = [
            adoption_request_hash(&default),
            adoption_request_hash(&read_only),
            adoption_request_hash(&workspace_write),
            adoption_request_hash(&danger_full_access),
        ];

        assert_eq!(hashes.iter().collect::<HashSet<_>>().len(), hashes.len());
    }

    #[test]
    fn worker_args_round_trip_sandbox_mode() {
        let mut request = command_test_request();
        let paths = CodexAdoptionPaths {
            key: "key".to_string(),
            dir: PathBuf::from("adoption-dir"),
            prompt: PathBuf::from("prompt"),
            stdout: PathBuf::from("stdout"),
            stderr: PathBuf::from("stderr"),
            effect: PathBuf::from("effect"),
            effect_log: PathBuf::from("effect-log"),
            result: PathBuf::from("result"),
            status: PathBuf::from("status"),
        };

        for sandbox in [
            CodexSandbox::ReadOnly,
            CodexSandbox::WorkspaceWrite,
            CodexSandbox::DangerFullAccess,
        ] {
            request.sandbox = Some(sandbox);
            let args = codex_worker_args(&request, &paths, Path::new("host-root"));
            let options = parse_worker_args(args.into_iter().skip(1).collect()).unwrap();

            assert_eq!(options.sandbox, Some(sandbox));
        }
    }

    #[test]
    fn stale_adoption_incarnation_cannot_publish_shared_result() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("adoption");
        let paths = CodexAdoptionPaths {
            key: "same-key".to_string(),
            prompt: dir.join("prompt.txt"),
            stdout: dir.join("stdout.txt"),
            stderr: dir.join("stderr.txt"),
            effect: dir.join("effect.json"),
            effect_log: dir.join("effect-receipts.log"),
            result: dir.join("result.json"),
            status: dir.join("status.json"),
            dir,
        };
        let mut request = command_test_request();
        request.run_id = "codex-old-incarnation".to_string();
        let mut old = adoption_record_from_request(
            &request,
            &paths,
            CODEX_ADOPTION_STATUS_RUNNING,
            Some(std::process::id()),
            None,
        );
        let mut current = old.clone();
        current.run_id = "codex-new-incarnation".to_string();
        write_adoption_record(&paths.status, &current).unwrap();
        let result = CodexResult::success(
            "old-output".to_string(),
            String::new(),
            0,
            request.log_path.to_string_lossy().into_owned(),
        );

        let published = publish_adoption_result_if_current(&paths, &mut old, &result, 123).unwrap();

        assert!(!published);
        assert!(!paths.effect.exists());
        assert!(!paths.result.exists());
        assert!(!paths.stdout.exists());
        let visible = read_adoption_record_from_disk(&paths.status)
            .unwrap()
            .unwrap();
        assert_eq!(visible.run_id, "codex-new-incarnation");
        assert_eq!(visible.status, CODEX_ADOPTION_STATUS_RUNNING);
    }

    #[test]
    fn stale_adoption_incarnation_cannot_publish_effect_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("adoption");
        let paths = CodexAdoptionPaths {
            key: "same-key".to_string(),
            prompt: dir.join("prompt.txt"),
            stdout: dir.join("stdout.txt"),
            stderr: dir.join("stderr.txt"),
            effect: dir.join("effect.json"),
            effect_log: dir.join("effect-receipts.log"),
            result: dir.join("result.json"),
            status: dir.join("status.json"),
            dir,
        };
        let mut old_request = command_test_request();
        old_request.run_id = "codex-old-incarnation".to_string();
        old_request.adoption_status_path = Some(paths.status.clone());
        old_request.effect_key = Some(paths.key.clone());
        old_request.effect_log_path = Some(paths.effect_log.clone());
        let old = adoption_record_from_request(
            &old_request,
            &paths,
            CODEX_ADOPTION_STATUS_RUNNING,
            Some(std::process::id()),
            None,
        );
        let mut current = old.clone();
        current.run_id = "codex-new-incarnation".to_string();
        write_adoption_record(&paths.status, &current).unwrap();
        let result = CodexResult::success(
            "old-output".to_string(),
            String::new(),
            0,
            old_request.log_path.to_string_lossy().into_owned(),
        );

        write_codex_effect_receipt(&old_request, &result).unwrap();

        assert!(!paths.effect_log.exists());
    }

    #[test]
    fn invalid_adoption_incarnation_is_not_evidence_of_death() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("adoption");
        let paths = CodexAdoptionPaths {
            key: "same-key".to_string(),
            prompt: dir.join("prompt.txt"),
            stdout: dir.join("stdout.txt"),
            stderr: dir.join("stderr.txt"),
            effect: dir.join("effect.json"),
            effect_log: dir.join("effect-receipts.log"),
            result: dir.join("result.json"),
            status: dir.join("status.json"),
            dir,
        };
        let mut request = command_test_request();
        request.run_id = "invalid/incarnation".to_string();
        let record = adoption_record_from_request(
            &request,
            &paths,
            CODEX_ADOPTION_STATUS_RUNNING,
            Some(std::process::id()),
            None,
        );

        let probe = adoption_effect_alive(&paths.dir, &record);

        assert!(probe.is_err());
        assert_eq!(
            adoption_status_for_observe(&record, &paths.dir),
            CODEX_ADOPTION_STATUS_RUNNING
        );
    }

    #[test]
    fn codex_command_strips_github_token_env() {
        let request = command_test_request();
        let (cmd, _) = command_for_request(&request, Path::new("codex"));
        let envs = cmd.get_envs().collect::<Vec<_>>();

        assert!(envs
            .iter()
            .any(|(key, value)| *key == OsStr::new("GH_TOKEN") && value.is_none()));
        assert!(envs
            .iter()
            .any(|(key, value)| *key == OsStr::new("GITHUB_TOKEN") && value.is_none()));
        assert!(!envs.iter().any(|(key, value)| {
            matches!(
                (*key, value),
                (key, Some(_)) if key == OsStr::new("GH_TOKEN")
                    || key == OsStr::new("GITHUB_TOKEN")
            )
        }));
        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(GH_CONFIG_DIR_ENV)
                && value.is_some_and(|path| {
                    Path::new(path)
                        .file_name()
                        .is_some_and(|name| name == OsStr::new(&request.run_id))
                })
        }));
    }
}

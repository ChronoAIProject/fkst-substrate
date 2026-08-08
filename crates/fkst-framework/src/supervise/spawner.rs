//! Framework spawn with new process group, output capture, and child log.
//!
//! Spawn `fkst-framework run <lua_path> --project-root <path> --package-root <path> ... --owner-namespace <id> --event <json>` with setsid.

use crate::process_tree::{ProcessGroupRegistration, ProcessGroupRegistry};
use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::info;

static NEXT_FRAMEWORK_CHILD_LOG_ID: AtomicU64 = AtomicU64::new(1);
const RAISED_AUTH_TOKEN_ENV: &str = "FKST_RAISED_AUTH_TOKEN";
const CACHE_REPLAY_BYPASS_ENV: &str = "FKST_INTERNAL_CACHE_REPLAY_BYPASS";
const ONCE_REPLAY_BYPASS_ENV: &str = "FKST_INTERNAL_ONCE_REPLAY_BYPASS";
const ENGINE_BINARY_PATH_ENV_KEYS: &[&str] =
    &["BIN", "FKST_FRAMEWORK_BIN", "FKST_CODEX_WORKER_BIN"];

pub struct SpawnResult {
    pub pid: u32,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub spawn_return_ms: u128,
    pub first_pipe_read_ms: Option<u128>,
    pub wait_complete_ms: u128,
    pub capture_complete_ms: u128,
    pub elapsed_ms: u128,
    pub log_path: Option<PathBuf>,
}

pub type StdoutLineObserver = Arc<dyn Fn(&str) -> Result<()> + Send + Sync + 'static>;

// each stream is tagged so captured output preserves stdout/stderr boundaries.
#[derive(Clone, Copy)]
enum FrameworkStream {
    Stdout,
    Stderr,
}

// output chunks and process exit share one wait channel.
enum FrameworkEvent {
    Output(FrameworkStream, Vec<u8>, u128),
    Exited(std::result::Result<std::process::ExitStatus, String>, u128),
}

// Framework children run until natural exit; Department stall_window is a
// delivery lease owned by consumer.rs, not a subprocess kill deadline.
pub async fn spawn_framework(
    binary: &Path,
    lua_path: &Path,
    host_root: &Path,
    package_roots: &[PathBuf],
    owner_namespace: &str,
    event_json: &str,
    codex_permit_slots: usize,
    child_label: &str,
    log_dir: &Path,
    process_groups: ProcessGroupRegistry,
) -> Result<SpawnResult> {
    spawn_framework_with_stdout_observer(
        binary,
        lua_path,
        host_root,
        package_roots,
        owner_namespace,
        event_json,
        codex_permit_slots,
        child_label,
        log_dir,
        process_groups,
        None,
        false,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn spawn_framework_with_stdout_observer(
    binary: &Path,
    lua_path: &Path,
    host_root: &Path,
    package_roots: &[PathBuf],
    owner_namespace: &str,
    event_json: &str,
    codex_permit_slots: usize,
    child_label: &str,
    log_dir: &Path,
    process_groups: ProcessGroupRegistry,
    raised_auth_token: Option<&str>,
    replay_scratch_bypass: bool,
    stdout_observer: Option<StdoutLineObserver>,
) -> Result<SpawnResult> {
    let start = std::time::Instant::now();
    let cmd_line = format!(
        "{} run {} --project-root {} {} --owner-namespace {} --event <json>",
        binary.display(),
        lua_path.display(),
        host_root.display(),
        package_root_flags(package_roots),
        owner_namespace,
    );
    let mut log = FrameworkChildLog::open(log_dir, child_label);
    log.write_line(&format!("CMD={cmd_line}"));
    log.write_line(&format!("LUA={}", lua_path.display()));
    log.write_line(&format!("HOST_ROOT={}", host_root.display()));
    log.write_line(&format!(
        "PACKAGE_ROOTS={}",
        package_root_list(package_roots)
    ));
    log.write_line(&format!("OWNER_NAMESPACE={owner_namespace}"));
    log.write_line(&format!(
        "ENGINE_VER={}",
        crate::provenance::current_engine_ver()
    ));
    log.write_line(&format!(
        "PKG_VER={}",
        crate::provenance::pkg_ver_for_namespace(owner_namespace)
    ));
    log.write_line(&format!(
        "PKG_VERS={}",
        crate::provenance::current_pkg_versions_summary()
    ));
    log.write_line(&format!("DEPT={child_label}"));
    if raised_auth_token.is_some() {
        log.write_line("RAISED_AUTH=enabled");
    }
    if replay_scratch_bypass {
        log.write_line("REPLAY_SCRATCH_BYPASS=enabled");
    }

    let mut cmd = Command::new(binary);
    cmd.arg("run")
        .arg(lua_path)
        .arg("--project-root")
        .arg(host_root);
    for package_root in package_roots {
        cmd.arg("--package-root").arg(package_root);
    }
    cmd.arg("--owner-namespace")
        .arg(owner_namespace)
        .arg("--event")
        .arg(event_json)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.env(
        crate::sdk_codex::CODEX_PERMIT_SLOTS_ENV,
        codex_permit_slots.to_string(),
    );
    cmd.env(
        "FKST_SUPERVISOR_PID",
        crate::process_tree::current_pid().to_string(),
    );
    if let Some(token) = raised_auth_token {
        cmd.env(RAISED_AUTH_TOKEN_ENV, token);
    }
    if replay_scratch_bypass {
        cmd.env(CACHE_REPLAY_BYPASS_ENV, "1");
        cmd.env(ONCE_REPLAY_BYPASS_ENV, "1");
    } else {
        cmd.env_remove(CACHE_REPLAY_BYPASS_ENV);
        cmd.env_remove(ONCE_REPLAY_BYPASS_ENV);
    }
    scrub_engine_binary_path_env(&mut cmd);
    cmd.current_dir(host_root);

    // Set a new process group before exec so framework becomes its own group leader.
    // tokio::process exposes `process_group(0)` to call setpgid(0,0); equivalent for our purposes.
    cmd.process_group(0);

    let (child, spawn_return_ms) = match cmd.spawn() {
        Ok(child) => (child, start.elapsed().as_millis()),
        Err(err) => {
            log.write_line(&format!("SPAWN_ERROR={err}"));
            return Err(err).context("spawn fkst-framework");
        }
    };
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("no pid after spawn"))?;
    log.write_line(&format!("PID={pid}"));
    info!(pid = pid, lua = %lua_path.display(), "framework spawned");
    let registration = process_groups.register(pid);

    wait_for_framework_child(
        child,
        pid,
        start,
        spawn_return_ms,
        log,
        registration,
        stdout_observer,
    )
    .await
}

pub(crate) fn scrub_current_engine_binary_path_env() {
    for key in ENGINE_BINARY_PATH_ENV_KEYS {
        std::env::remove_var(key);
    }
}

fn scrub_engine_binary_path_env(cmd: &mut Command) {
    for key in ENGINE_BINARY_PATH_ENV_KEYS {
        cmd.env_remove(key);
    }
}

fn package_root_flags(package_roots: &[PathBuf]) -> String {
    package_roots
        .iter()
        .map(|root| format!("--package-root {}", root.display()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn package_root_list(package_roots: &[PathBuf]) -> String {
    package_roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(";")
}

async fn wait_for_framework_child(
    mut child: Child,
    pid: u32,
    start: Instant,
    spawn_return_ms: u128,
    mut log: FrameworkChildLog,
    _registration: ProcessGroupRegistration,
    stdout_observer: Option<StdoutLineObserver>,
) -> Result<SpawnResult> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_stream_reader(
            stdout,
            FrameworkStream::Stdout,
            start,
            tx.clone(),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_stream_reader(
            stderr,
            FrameworkStream::Stderr,
            start,
            tx.clone(),
        ));
    }
    let waiter = tokio::spawn(async move {
        let result = child.wait().await;
        let wait_complete_ms = start.elapsed().as_millis();
        let _ = tx.send(FrameworkEvent::Exited(
            result.map_err(|err| err.to_string()),
            wait_complete_ms,
        ));
    });

    let mut output = FrameworkOutput::default();
    let mut line_buffer = StdoutLineBuffer::default();
    let (status, wait_complete_ms) = loop {
        match rx.recv().await {
            Some(FrameworkEvent::Output(stream, bytes, read_complete_ms)) => {
                if !bytes.is_empty() {
                    log.write_chunk(stream, &bytes);
                    if matches!(stream, FrameworkStream::Stdout) {
                        line_buffer.push(&bytes, stdout_observer.as_deref())?;
                    }
                    output.push(stream, bytes, read_complete_ms);
                }
            }
            Some(FrameworkEvent::Exited(result, wait_complete_ms)) => {
                break (result, Some(wait_complete_ms));
            }
            None => {
                break (
                    Err("framework wait channel closed before process exit".to_string()),
                    None,
                );
            }
        }
    };

    for reader in readers {
        let _ = reader.await;
    }
    let _ = waiter.await;
    drain_framework_events(
        &mut rx,
        &mut output,
        &mut log,
        &mut line_buffer,
        stdout_observer.as_deref(),
    )?;
    let capture_complete_ms = start.elapsed().as_millis();

    spawn_result_from_status(
        pid,
        status,
        output,
        spawn_return_ms,
        wait_complete_ms,
        capture_complete_ms,
        log,
    )
}

// buffers accumulate incrementally as output events arrive.
#[derive(Default)]
struct FrameworkOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    first_pipe_read_ms: Option<u128>,
}

impl FrameworkOutput {
    fn push(&mut self, stream: FrameworkStream, bytes: Vec<u8>, read_complete_ms: u128) {
        self.first_pipe_read_ms = Some(
            self.first_pipe_read_ms
                .map_or(read_complete_ms, |first| first.min(read_complete_ms)),
        );
        match stream {
            FrameworkStream::Stdout => self.stdout.extend(bytes),
            FrameworkStream::Stderr => self.stderr.extend(bytes),
        }
    }
}

// drain queued output events before constructing the final spawn result.
fn drain_framework_events(
    rx: &mut mpsc::UnboundedReceiver<FrameworkEvent>,
    output: &mut FrameworkOutput,
    log: &mut FrameworkChildLog,
    line_buffer: &mut StdoutLineBuffer,
    stdout_observer: Option<&(dyn Fn(&str) -> Result<()> + Send + Sync)>,
) -> Result<()> {
    while let Ok(event) = rx.try_recv() {
        if let FrameworkEvent::Output(stream, bytes, read_complete_ms) = event {
            log.write_chunk(stream, &bytes);
            if matches!(stream, FrameworkStream::Stdout) {
                line_buffer.push(&bytes, stdout_observer)?;
            }
            output.push(stream, bytes, read_complete_ms);
        }
    }
    Ok(())
}

#[derive(Default)]
struct StdoutLineBuffer {
    pending: Vec<u8>,
}

impl StdoutLineBuffer {
    fn push(
        &mut self,
        bytes: &[u8],
        observer: Option<&(dyn Fn(&str) -> Result<()> + Send + Sync)>,
    ) -> Result<()> {
        self.pending.extend_from_slice(bytes);
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=newline).collect::<Vec<_>>();
            if line.ends_with(b"\n") {
                line.pop();
            }
            if line.ends_with(b"\r") {
                line.pop();
            }
            if let Some(observer) = observer {
                observer(&String::from_utf8_lossy(&line))?;
            }
        }
        Ok(())
    }
}

// natural exits keep their child status.
fn spawn_result_from_status(
    pid: u32,
    status: std::result::Result<std::process::ExitStatus, String>,
    output: FrameworkOutput,
    spawn_return_ms: u128,
    wait_complete_ms: Option<u128>,
    capture_complete_ms: u128,
    mut log: FrameworkChildLog,
) -> Result<SpawnResult> {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = status.map_err(anyhow::Error::msg)?.code().unwrap_or(-1);
    let wait_complete_ms = wait_complete_ms
        .ok_or_else(|| anyhow::anyhow!("framework wait channel closed before process exit"))?;
    log.write_line(&format!("EXIT={exit_code}"));
    log.write_line(&format!("ELAPSED_MS={capture_complete_ms}"));
    let log_path = log.path().map(Path::to_path_buf);
    Ok(SpawnResult {
        pid,
        exit_code,
        stdout,
        stderr,
        spawn_return_ms,
        first_pipe_read_ms: output.first_pipe_read_ms,
        wait_complete_ms,
        capture_complete_ms,
        elapsed_ms: capture_complete_ms,
        log_path,
    })
}

// readers forward each chunk immediately for output capture.
fn spawn_stream_reader<R>(
    mut reader: R,
    stream: FrameworkStream,
    start: Instant,
    tx: mpsc::UnboundedSender<FrameworkEvent>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let read_complete_ms = start.elapsed().as_millis();
                    if tx
                        .send(FrameworkEvent::Output(
                            stream,
                            buf[..n].to_vec(),
                            read_complete_ms,
                        ))
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

// each framework child owns a best-effort durable log that records command
// metadata, streamed stdout/stderr chunks, and final exit metadata
// without changing RAISED parsing or spawn success semantics.
struct FrameworkChildLog {
    path: Option<PathBuf>,
    file: Option<std::fs::File>,
}

impl FrameworkChildLog {
    fn open(log_dir: &Path, child_label: &str) -> Self {
        let path = framework_child_log_path(log_dir, child_label);
        let file = path
            .parent()
            .and_then(|parent| std::fs::create_dir_all(parent).ok())
            .and_then(|_| {
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                    .ok()
            });
        Self {
            path: file.as_ref().map(|_| path),
            file,
        }
    }

    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn write_line(&mut self, line: &str) {
        self.write_all(line.as_bytes());
        self.write_all(b"\n");
    }

    fn write_chunk(&mut self, stream: FrameworkStream, bytes: &[u8]) {
        let header = match stream {
            FrameworkStream::Stdout => b"STDOUT\n".as_slice(),
            FrameworkStream::Stderr => b"STDERR\n".as_slice(),
        };
        self.write_all(header);
        self.write_all(bytes);
        if !bytes.ends_with(b"\n") {
            self.write_all(b"\n");
        }
    }

    fn write_all(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.write_all(bytes).and_then(|_| file.flush()).is_err() {
            self.file = None;
            self.path = None;
        }
    }
}

fn framework_child_log_path(log_dir: &Path, child_label: &str) -> PathBuf {
    let basename = Path::new(child_label)
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("framework-child");
    let id = NEXT_FRAMEWORK_CHILD_LOG_ID.fetch_add(1, Ordering::Relaxed);
    log_dir.join(format!("{basename}-{}-{id}.log", filename_timestamp()))
}

fn filename_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    format!("{}-{:09}", duration.as_secs(), duration.subsec_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn replay_scratch_bypass_is_private_child_process_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("fkst-framework");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '%s:%s' \"${}\" \"${}\"\n",
                CACHE_REPLAY_BYPASS_ENV, ONCE_REPLAY_BYPASS_ENV,
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let lua = temp.path().join("dept.lua");
        std::fs::write(&lua, "return {}\n").unwrap();

        let result = spawn_framework_with_stdout_observer(
            &binary,
            &lua,
            temp.path(),
            &[temp.path().to_path_buf()],
            "pkg",
            "{}",
            1,
            "worker",
            &temp.path().join("logs"),
            ProcessGroupRegistry::default(),
            None,
            true,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "1:1");
    }
}

//! Framework spawn with new process group + SIGKILL -pgid on stall.
//!
//! Spawn `fkst-framework run <lua_path> --project-root <path> --package-root <path> ... --event <json>` with setsid.
//! On no-output stall, send SIGKILL to -pgid so codex subprocess children are collected too.

use anyhow::{Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

static NEXT_FRAMEWORK_CHILD_LOG_ID: AtomicU64 = AtomicU64::new(1);

// results expose no-output stalls and the age of the last observed output.
pub struct SpawnResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stalled: bool,
    pub elapsed_ms: u128,
    pub last_output_age_ms: u128,
    pub log_path: Option<PathBuf>,
}

// each stream is tagged as live activity while the child is still running.
#[derive(Clone, Copy)]
enum FrameworkStream {
    Stdout,
    Stderr,
}

// output chunks and process exit share one activity channel.
enum FrameworkEvent {
    Output(FrameworkStream, Vec<u8>),
    Exited(std::result::Result<std::process::ExitStatus, String>),
}

// stdout/stderr activity keeps the process alive until natural exit.
// supervise only forwards configured slot width to each framework
// child; SDK fcntl permit locks remain the only codex concurrency authority.
pub async fn spawn_framework(
    binary: &Path,
    lua_path: &Path,
    host_root: &Path,
    package_roots: &[PathBuf],
    event_json: &str,
    stall_window: Duration,
    codex_permit_slots: usize,
    child_label: &str,
    log_dir: &Path,
) -> Result<SpawnResult> {
    let start = std::time::Instant::now();
    let cmd_line = format!(
        "{} run {} --project-root {} {} --event <json>",
        binary.display(),
        lua_path.display(),
        host_root.display(),
        package_root_flags(package_roots)
    );
    let mut log = FrameworkChildLog::open(log_dir, child_label);
    log.write_line(&format!("CMD={cmd_line}"));
    log.write_line(&format!("LUA={}", lua_path.display()));
    log.write_line(&format!("HOST_ROOT={}", host_root.display()));
    log.write_line(&format!("PACKAGE_ROOTS={}", package_root_list(package_roots)));
    log.write_line(&format!("DEPT={child_label}"));
    log.write_line(&format!("STALL_WINDOW_MS={}", stall_window.as_millis()));

    let mut cmd = Command::new(binary);
    cmd.arg("run")
        .arg(lua_path)
        .arg("--project-root")
        .arg(host_root);
    for package_root in package_roots {
        cmd.arg("--package-root").arg(package_root);
    }
    cmd.arg("--event")
        .arg(event_json)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.env(
        crate::sdk_codex::CODEX_PERMIT_SLOTS_ENV,
        codex_permit_slots.to_string(),
    );
    cmd.current_dir(host_root);

    // Set a new process group before exec so framework becomes its own group leader.
    // tokio::process exposes `process_group(0)` to call setpgid(0,0); equivalent for our purposes.
    cmd.process_group(0);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            log.write_line(&format!("SPAWN_ERROR={err}"));
            return Err(err).context("spawn fkst-framework");
        }
    };
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("no pid after spawn"))?;
    log.write_line(&format!("PID={pid}"));
    info!(pid = pid, lua = %lua_path.display(), stall_window_ms = stall_window.as_millis(), "framework spawned");

    wait_with_stall_window(child, pid, stall_window, start, log).await
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

// stdout/stderr output resets a no-output stall window.
async fn wait_with_stall_window(
    mut child: Child,
    pid: u32,
    stall_window: Duration,
    start: Instant,
    mut log: FrameworkChildLog,
) -> Result<SpawnResult> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_stream_reader(
            stdout,
            FrameworkStream::Stdout,
            tx.clone(),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_stream_reader(
            stderr,
            FrameworkStream::Stderr,
            tx.clone(),
        ));
    }
    let waiter = tokio::spawn(async move {
        let result = child.wait().await.map_err(|err| err.to_string());
        let _ = tx.send(FrameworkEvent::Exited(result));
    });

    let mut output = FrameworkOutput::default();
    let mut last_output = Instant::now();
    let mut stalled = false;
    let status = loop {
        let event = if stalled {
            rx.recv().await
        } else {
            let remaining = stall_window.saturating_sub(last_output.elapsed());
            if remaining.is_zero() {
                None
            } else {
                tokio::time::timeout(remaining, rx.recv())
                    .await
                    .unwrap_or(None)
            }
        };

        match event {
            Some(FrameworkEvent::Output(stream, bytes)) => {
                if !bytes.is_empty() {
                    last_output = Instant::now();
                    log.write_chunk(stream, &bytes);
                    output.push(stream, bytes);
                }
            }
            Some(FrameworkEvent::Exited(result)) => break result,
            None if !stalled => {
                stalled = true;
                log.write_line(&format!("STALL_KILL_PID={pid}"));
                warn!(
                    pid = pid,
                    stall_window_ms = stall_window.as_millis(),
                    "framework stalled, SIGKILL -pgid"
                );
                let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
            }
            None => break Err("framework wait channel closed before process exit".to_string()),
        }
    };

    for reader in readers {
        let _ = reader.await;
    }
    let _ = waiter.await;
    drain_framework_events(&mut rx, &mut output, &mut log);

    spawn_result_from_status(status, stalled, output, start, last_output, log)
}

// buffers accumulate incrementally as liveness events arrive.
#[derive(Default)]
struct FrameworkOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl FrameworkOutput {
    fn push(&mut self, stream: FrameworkStream, bytes: Vec<u8>) {
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
) {
    while let Ok(event) = rx.try_recv() {
        if let FrameworkEvent::Output(stream, bytes) = event {
            log.write_chunk(stream, &bytes);
            output.push(stream, bytes);
        }
    }
}

// stall kill maps to exit 124 while natural exits keep their child status.
fn spawn_result_from_status(
    status: std::result::Result<std::process::ExitStatus, String>,
    stalled: bool,
    output: FrameworkOutput,
    start: Instant,
    last_output: Instant,
    mut log: FrameworkChildLog,
) -> Result<SpawnResult> {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = if stalled {
        124
    } else {
        status.map_err(anyhow::Error::msg)?.code().unwrap_or(-1)
    };
    let elapsed_ms = start.elapsed().as_millis();
    let last_output_age_ms = last_output.elapsed().as_millis();
    log.write_line(&format!("EXIT={exit_code}"));
    log.write_line(&format!("STALLED={stalled}"));
    log.write_line(&format!("ELAPSED_MS={elapsed_ms}"));
    log.write_line(&format!("LAST_OUTPUT_AGE_MS={last_output_age_ms}"));
    let log_path = log.path().map(Path::to_path_buf);
    Ok(SpawnResult {
        exit_code,
        stdout,
        stderr,
        stalled,
        elapsed_ms,
        last_output_age_ms,
        log_path,
    })
}

// readers forward each chunk immediately so output refreshes liveness.
fn spawn_stream_reader<R>(
    mut reader: R,
    stream: FrameworkStream,
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
                    if tx
                        .send(FrameworkEvent::Output(stream, buf[..n].to_vec()))
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
// metadata, streamed stdout/stderr chunks, stall kills, and final exit metadata
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

//! Framework spawn with new process group, output capture, and child log.
//!
//! Spawn `fkst-framework run <lua_path> --project-root <path> --package-root <path> ... --owner-namespace <id> --event <json>` with setsid.

use crate::process_tree::{ProcessGroupRegistration, ProcessGroupRegistry};
use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle as ThreadJoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::info;

static NEXT_FRAMEWORK_CHILD_LOG_ID: AtomicU64 = AtomicU64::new(1);

pub struct SpawnResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u128,
    pub log_path: Option<PathBuf>,
}

// each stream is tagged so captured output preserves stdout/stderr boundaries.
#[derive(Clone, Copy)]
enum FrameworkStream {
    Stdout,
    Stderr,
}

// output chunks and process exit share one wait channel.
enum FrameworkEvent {
    Output(FrameworkStream, Vec<u8>),
    Exited(std::result::Result<std::process::ExitStatus, String>),
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
    log.write_line(&format!("DEPT={child_label}"));

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
        crate::process_tree::SUPERVISOR_PID_ENV,
        crate::process_tree::current_pid().to_string(),
    );
    cmd.env(crate::process_tree::SUPERVISED_RUN_ENV, "1");
    cmd.current_dir(host_root);

    // Set a new process group before exec so framework becomes its own group leader.
    cmd.process_group(0);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            log.write_line(&format!("SPAWN_ERROR={err}"));
            return Err(err).context("spawn fkst-framework");
        }
    };
    let pid = child.id();
    log.write_line(&format!("PID={pid}"));
    info!(pid = pid, lua = %lua_path.display(), "framework spawned");
    let registration = process_groups.register(pid);

    wait_for_framework_child(child, start, log, registration).await
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
    start: Instant,
    mut log: FrameworkChildLog,
    _registration: ProcessGroupRegistration,
) -> Result<SpawnResult> {
    let (tx, mut rx) = tokio_mpsc::unbounded_channel();
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
    let waiter = std::thread::spawn(move || {
        let result = child.wait().map_err(|err| err.to_string());
        let _ = tx.send(FrameworkEvent::Exited(result));
    });

    let mut output = FrameworkOutput::default();
    let status = loop {
        match rx.recv().await {
            Some(FrameworkEvent::Output(stream, bytes)) => {
                if !bytes.is_empty() {
                    log.write_chunk(stream, &bytes);
                    output.push(stream, bytes);
                }
            }
            Some(FrameworkEvent::Exited(result)) => break result,
            None => break Err("framework wait channel closed before process exit".to_string()),
        }
    };

    for reader in readers {
        let _ = tokio::task::spawn_blocking(move || reader.join()).await;
    }
    let _ = tokio::task::spawn_blocking(move || waiter.join()).await;
    drain_framework_events(&mut rx, &mut output, &mut log);

    spawn_result_from_status(status, output, start, log).await
}

// buffers accumulate incrementally as output events arrive.
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
    rx: &mut tokio_mpsc::UnboundedReceiver<FrameworkEvent>,
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

// natural exits keep their child status.
async fn spawn_result_from_status(
    status: std::result::Result<std::process::ExitStatus, String>,
    output: FrameworkOutput,
    start: Instant,
    mut log: FrameworkChildLog,
) -> Result<SpawnResult> {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = status.map_err(anyhow::Error::msg)?.code().unwrap_or(-1);
    let elapsed_ms = start.elapsed().as_millis();
    log.write_line(&format!("EXIT={exit_code}"));
    log.write_line(&format!("ELAPSED_MS={elapsed_ms}"));
    let log_path = log.finish().await;
    Ok(SpawnResult {
        exit_code,
        stdout,
        stderr,
        elapsed_ms,
        log_path,
    })
}

// readers forward each chunk immediately for output capture.
fn spawn_stream_reader<R>(
    mut reader: R,
    stream: FrameworkStream,
    tx: tokio_mpsc::UnboundedSender<FrameworkEvent>,
) -> ThreadJoinHandle<()>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
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
// metadata, streamed stdout/stderr chunks, and final exit metadata
// without changing RAISED parsing or spawn success semantics.
struct FrameworkChildLog {
    path: Option<PathBuf>,
    tx: Option<std_mpsc::Sender<Vec<u8>>>,
    writer: Option<std::thread::JoinHandle<()>>,
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
        let (tx, writer) = match file {
            Some(mut file) => {
                let (tx, rx) = std_mpsc::channel::<Vec<u8>>();
                let writer = std::thread::spawn(move || {
                    while let Ok(bytes) = rx.recv() {
                        if file.write_all(&bytes).and_then(|_| file.flush()).is_err() {
                            break;
                        }
                    }
                });
                (Some(tx), Some(writer))
            }
            None => (None, None),
        };
        Self {
            path: tx.as_ref().map(|_| path),
            tx,
            writer,
        }
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
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        if tx.send(bytes.to_vec()).is_err() {
            self.tx = None;
            self.path = None;
        }
    }

    async fn finish(mut self) -> Option<PathBuf> {
        drop(self.tx.take());
        if let Some(writer) = self.writer.take() {
            let _ = tokio::task::spawn_blocking(move || writer.join()).await;
        }
        self.path
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

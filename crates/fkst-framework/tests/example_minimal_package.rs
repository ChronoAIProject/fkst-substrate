use base64::Engine;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::Value;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output};
use std::thread;
use std::time::{Duration, Instant};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).unwrap();
        }
    }
}

fn copy_minimal_package(host: &Path) {
    let package = repo_root().join("examples/minimal-package");
    copy_dir(&package, host);
}

fn run_department(host: &Path, runtime: &Path, lua: &str, event: &str) -> Output {
    Command::new(framework_bin())
        .arg("run")
        .arg(host.join(lua))
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(host)
        .arg("--event")
        .arg(event)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", runtime)
        .output()
        .unwrap()
}

fn run_conformance(host: &Path, runtime: &Path) -> Output {
    Command::new(framework_bin())
        .arg("conformance")
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(host)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", runtime)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn decode_raised(stdout: &str) -> Value {
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("RAISED: "))
        .unwrap_or_else(|| panic!("missing RAISED line in stdout: {stdout}"));
    let encoded = line.trim_start().trim_start_matches("RAISED: ").trim();
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(encoded)
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn read_framework_child_logs(runtime: &Path) -> String {
    let dir = runtime.join("logs/framework-child");
    let mut body = String::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return body;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Ok(log) = fs::read_to_string(path) {
                body.push_str(&log);
                body.push('\n');
            }
        }
    }
    body
}

fn read_framework_child_log_bodies(runtime: &Path) -> Vec<String> {
    let dir = runtime.join("logs/framework-child");
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_file() {
                fs::read_to_string(path).ok()
            } else {
                None
            }
        })
        .collect()
}

fn consumer_log_completed(runtime: &Path) -> Option<String> {
    read_framework_child_log_bodies(runtime)
        .into_iter()
        .find(|log| {
            log.contains("consumer received Event{queue=example_event") && log.contains("EXIT=0\n")
        })
}

fn consumer_ts(logs: &str) -> Option<u64> {
    logs.lines()
        .find(|line| line.contains("consumer received Event{queue=example_event"))
        .and_then(|line| line.split("ts=").nth(1))
        .and_then(|tail| tail.trim_end_matches('}').parse::<u64>().ok())
}

fn stop_supervise(child: &mut Child) {
    let pid = child.id() as i32;
    let pgid = Pid::from_raw(-pid);
    let _ = kill(pgid, Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = kill(pgid, Signal::SIGKILL);
    let _ = child.wait();
}

struct SuperviseGuard {
    child: Child,
}

impl SuperviseGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for SuperviseGuard {
    fn drop(&mut self) {
        stop_supervise(&mut self.child);
    }
}

#[test]
fn minimal_package_loads_producer_consumer_graph() {
    let host = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    copy_minimal_package(host.path());

    let conformance = run_conformance(host.path(), runtime.path());
    assert_success(&conformance);
    let out = stdout(&conformance);
    assert!(
        out.contains("loaded 2 departments, 1 raisers, 2 queues"),
        "stdout: {out}"
    );
}

#[test]
fn producer_raises_example_event_from_tick() {
    let host = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    copy_minimal_package(host.path());

    let producer = run_department(
        host.path(),
        runtime.path(),
        "departments/producer/main.lua",
        r#"{"queue":"tick","payload":{"raiser":"tick"}}"#,
    );
    assert_success(&producer);

    let raised = decode_raised(&stdout(&producer));
    assert_eq!(raised.as_array().unwrap().len(), 1, "raised={raised}");
    assert_eq!(raised[0]["queue"], "example_event");
    assert_eq!(raised[0]["payload"]["from"], "producer");
    assert_eq!(raised[0]["payload"]["source_queue"], "tick");
    assert_eq!(raised[0]["payload"]["source_raiser"], "tick");
    assert!(raised[0].get("ts").is_none(), "raised={raised}");
}

#[test]
fn consumer_logs_complete_standard_event() {
    let host = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    copy_minimal_package(host.path());

    let consumer = run_department(
        host.path(),
        runtime.path(),
        "departments/consumer/main.lua",
        r#"{"queue":"example_event","payload":{"from":"producer","note":"opaque example payload","source_queue":"tick","source_raiser":"tick"},"ts":123}"#,
    );
    assert_success(&consumer);

    let err = stderr(&consumer);
    assert!(
        err.contains("consumer received Event{queue=example_event"),
        "stderr: {err}"
    );
    assert!(err.contains("payload={from=producer"), "stderr: {err}");
    assert!(err.contains("note=opaque example payload"), "stderr: {err}");
    assert!(err.contains("source_queue=tick"), "stderr: {err}");
    assert!(err.contains("source_raiser=tick"), "stderr: {err}");
    assert!(err.contains("ts=123"), "stderr: {err}");
}

#[test]
fn supervise_routes_raised_event_to_consumer() {
    let host = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    copy_minimal_package(host.path());

    let child = Command::new(framework_bin())
        .arg("supervise")
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--framework-bin")
        .arg(framework_bin())
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", runtime.path())
        .process_group(0)
        .spawn()
        .unwrap();
    let mut supervise = SuperviseGuard::new(child);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut logs = String::new();
    while Instant::now() < deadline {
        logs = read_framework_child_logs(runtime.path());
        if let Some(log) = consumer_log_completed(runtime.path()) {
            logs = log;
            break;
        }
        if let Some(status) = supervise.try_wait().unwrap() {
            panic!("supervise exited early with {status}; logs={logs}");
        }
        thread::sleep(Duration::from_millis(100));
    }

    assert!(
        logs.contains("consumer received Event{queue=example_event"),
        "logs={logs}"
    );
    assert!(logs.contains("from=producer"), "logs={logs}");
    assert!(logs.contains("note=opaque example payload"), "logs={logs}");
    assert!(logs.contains("source_queue=tick"), "logs={logs}");
    assert!(logs.contains("source_raiser=tick"), "logs={logs}");
    assert!(!logs.contains("ts=nil"), "logs={logs}");
    let ts = consumer_ts(&logs).unwrap_or_else(|| panic!("missing numeric ts in logs={logs}"));
    assert!(ts > 0, "logs={logs}");
    assert!(logs.contains("EXIT=0\n"), "logs={logs}");
}

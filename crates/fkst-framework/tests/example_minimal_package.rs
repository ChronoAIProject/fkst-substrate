use base64::Engine;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        .env_remove("FKST_SUPERVISOR_PID")
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
        .env_remove("FKST_SUPERVISOR_PID")
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
fn producer_raised_payload_is_consumable_by_consumer_event() {
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
    let payload = raised[0]["payload"].clone();
    let ts = 1_717_000_000_u64;
    let consumer_event = serde_json::json!({
        "queue": "example_event",
        "payload": payload,
        "ts": ts,
    });

    // Direct producer-to-consumer contract check: producer output is consumable by
    // consumer as a standard Event. Dispatcher routing is covered by framework tests.
    let consumer = run_department(
        host.path(),
        runtime.path(),
        "departments/consumer/main.lua",
        &consumer_event.to_string(),
    );
    assert_success(&consumer);

    let logs = stderr(&consumer);
    assert!(
        logs.contains("consumer received Event{queue=example_event"),
        "logs={logs}"
    );
    assert!(logs.contains("from=producer"), "logs={logs}");
    assert!(logs.contains("note=opaque example payload"), "logs={logs}");
    assert!(logs.contains("source_queue=tick"), "logs={logs}");
    assert!(logs.contains("source_raiser=tick"), "logs={logs}");
    assert!(logs.contains("ts=1717000000"), "logs={logs}");
}

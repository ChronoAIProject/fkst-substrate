use base64::Engine;
use redb::{Database, TableDefinition};
use serde_json::json;
use std::fs;
use std::io::ErrorKind;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod support;

use support::manifest_fixture::{unit_name, write_single_package_workspace};

const DELIVERY_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("delivery_by_id");
const DEAD_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("dead_by_id");

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn framework_command() -> Command {
    let mut command = Command::new(framework_bin());
    command.env_remove("FKST_SUPERVISOR_PID");
    command
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
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

fn raised_payload(output: &Output) -> serde_json::Value {
    let out = stdout(output);
    let line = out
        .lines()
        .find_map(|line| line.strip_prefix("RAISED: "))
        .unwrap_or_else(|| panic!("missing RAISED line in stdout: {out}"));
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(line)
        .unwrap();
    let raises: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
    raises[0]["payload"].clone()
}

fn observe_socket_path(durable_root: &Path) -> PathBuf {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in durable_root.as_os_str().to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    PathBuf::from("/tmp").join(format!("fkst-observe-{hash:016x}.sock"))
}

fn write_observe_fixture(durable_root: &Path) {
    let db = Database::create(durable_root.join("delivery.redb")).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut deliveries = write.open_table(DELIVERY_BY_ID).unwrap();
        let mut dead = write.open_table(DEAD_BY_ID).unwrap();
        let delivery = json!({
            "delivery_id": "delivery-one",
            "queue": "input",
            "dept": "worker",
            "payload": {
                "schema": "github.issue",
                "dedup_key": "issue-81",
                "body": "do not print this issue body"
            },
            "source": {"kind": "External", "reference": "issue/81"},
            "cron_payload": null,
            "observed_at_ms": 1000,
            "attempt": 0,
            "redrive_count": 0,
            "lease_generation": 0,
            "lease_until_ms": null,
            "not_before_ms": 1000,
            "last_error_excerpt": null
        });
        deliveries
            .insert(
                "delivery-one",
                serde_json::to_vec(&delivery).unwrap().as_slice(),
            )
            .unwrap();
        let dead_record = json!({
            "delivery_id": "dead-one",
            "queue": "input",
            "dept": "worker",
            "source": null,
            "observed_at_ms": 900,
            "not_before_ms": 900,
            "dead_at_ms": 1200,
            "attempts": 3,
            "redrive_count": 0,
            "replayable": false,
            "permanent": true,
            "error_excerpt": "final failure",
            "record": null
        });
        dead.insert(
            "dead-one",
            serde_json::to_vec(&dead_record).unwrap().as_slice(),
        )
        .unwrap();
    }
    write.commit().unwrap();
    drop(db);
}

#[test]
fn observe_json_reports_snapshot_without_payload_body() {
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    write_observe_fixture(durable.path());

    let output = framework_command()
        .arg("observe")
        .arg("--durable-root")
        .arg(durable.path())
        .arg("--json")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("\"schema_version\": 1"), "{out}");
    assert!(out.contains("\"queue\": \"input\""), "{out}");
    assert!(out.contains("\"schema\": \"github.issue\""), "{out}");
    assert!(out.contains("\"dedup_key\": \"issue-81\""), "{out}");
    assert!(out.contains("\"delivery_id\": \"dead-one\""), "{out}");
    assert!(!out.contains("do not print this issue body"), "{out}");
}

#[test]
fn fkst_observe_returns_cli_snapshot_in_process() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    write_observe_fixture(durable.path());
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    let probe = host.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { consumes = { "tick" }, produces = { "seen" }, ephemeral = { "tick" } }

function M.pipeline(event)
  local snapshot = fkst.observe()
  raise("seen", {
    schema_version = snapshot.schema_version,
    queue = snapshot.queues[1].queue,
    deliveries = #snapshot.deliveries,
    dead_letters = #snapshot.dead_letters,
    digest = snapshot.deliveries[1].payload.digest,
  })
end

return M
"#,
    )
    .unwrap();
    write_single_package_workspace(host.path());

    let cli = framework_command()
        .arg("observe")
        .arg("--durable-root")
        .arg(durable.path())
        .arg("--json")
        .output()
        .unwrap();
    assert_exit(&cli, 0);
    let cli_json: serde_json::Value = serde_json::from_slice(&cli.stdout).unwrap();

    let run = framework_command()
        .arg("run")
        .arg(&probe)
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--owner-namespace")
        .arg(unit_name(host.path()))
        .arg("--event")
        .arg(r#"{"queue":"tick","payload":{}}"#)
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .env("FKST_DURABLE_ROOT", durable.path())
        .output()
        .unwrap();

    assert_exit(&run, 0);
    let raised = raised_payload(&run);
    assert_eq!(raised["schema_version"], cli_json["schema_version"]);
    assert_eq!(raised["queue"], cli_json["queues"][0]["queue"]);
    assert_eq!(
        raised["deliveries"].as_u64().unwrap(),
        cli_json["deliveries"].as_array().unwrap().len() as u64
    );
    assert_eq!(
        raised["dead_letters"].as_u64().unwrap(),
        cli_json["dead_letters"].as_array().unwrap().len() as u64
    );
    assert_eq!(
        raised["digest"],
        cli_json["deliveries"][0]["payload"]["digest"]
    );
    assert!(
        !stdout(&run).contains("do not print this issue body"),
        "stdout: {}",
        stdout(&run)
    );
}

#[test]
fn fkst_test_mock_observe_injects_deterministic_snapshot() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = { consumes = { "tick" }, produces = { "seen" }, ephemeral = { "tick" } }

function M.pipeline(event)
  local snapshot = fkst.observe({ limit = 1 })
  raise("seen", {
    generated_at_ms = snapshot.generated_at_ms,
    queue = snapshot.queues[1].queue,
    depth = snapshot.queues[1].depth,
    deliveries = #snapshot.deliveries,
    max_deliveries = snapshot.limits.max_deliveries,
    truncated_deliveries = snapshot.truncated.deliveries,
  })
end

return M
"#,
    )
    .unwrap();
    fs::write(
        host.path().join("tests/observe_test.lua"),
        r#"
local t = fkst.test

return {
  test_mock_observe_feeds_run_department = function()
    t.mock_observe({
      schema_version = 1,
      generated_at_ms = 4242,
      queues = {
        { queue = "work", depth = 7 },
      },
      limits = { max_deliveries = 10, max_dead_letters = 10 },
      truncated = { deliveries = false, dead_letters = false },
      deliveries = {
        { delivery_id = "delivery-one" },
        { delivery_id = "delivery-two" },
      },
    })
    local result = t.run_department("departments/probe/main.lua", { queue = "tick", payload = {} })
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].payload.generated_at_ms, 4242)
    t.eq(result.raises[1].payload.queue, "work")
    t.eq(result.raises[1].payload.depth, 7)
    t.eq(result.raises[1].payload.deliveries, 1)
    t.eq(result.raises[1].payload.max_deliveries, 1)
    t.eq(result.raises[1].payload.truncated_deliveries, true)
  end,
}
"#,
    )
    .unwrap();
    write_single_package_workspace(host.path());

    let output = framework_command()
        .arg("test")
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("PASS tests/observe_test.lua::test_mock_observe_feeds_run_department"),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn fkst_test_observe_fails_closed_without_mock() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    write_observe_fixture(durable.path());
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("tests/observe_test.lua"),
        r#"
return {
  test_unmocked_observe_fails_closed = function()
    fkst.observe()
  end,
}
"#,
    )
    .unwrap();
    write_single_package_workspace(host.path());

    let output = framework_command()
        .arg("test")
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .env("FKST_DURABLE_ROOT", durable.path())
        .output()
        .unwrap();

    assert_exit(&output, 1);
    let out = stdout(&output);
    assert!(
        out.contains("FAIL tests/observe_test.lua::test_unmocked_observe_fails_closed"),
        "stdout: {out}"
    );
    assert!(
        out.contains("fkst.observe is not mocked in test mode"),
        "stdout: {out}"
    );
}

#[test]
fn observe_rejects_missing_database_without_creating_it() {
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();

    let output = framework_command()
        .arg("observe")
        .arg("--durable-root")
        .arg(durable.path())
        .arg("--json")
        .output()
        .unwrap();

    assert_exit(&output, 2);
    assert!(
        !durable.path().join("delivery.redb").exists(),
        "observe must not create a delivery database"
    );
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("open existing durable delivery database"),
        "{err}"
    );
}

#[test]
fn observe_json_uses_live_socket_when_database_is_open() {
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    let db_path = durable.path().join("delivery.redb");
    let db = Database::create(&db_path).unwrap();
    let write = db.begin_write().unwrap();
    {
        write.open_table(DELIVERY_BY_ID).unwrap();
        write.open_table(DEAD_BY_ID).unwrap();
    }
    write.commit().unwrap();
    let socket_path = observe_socket_path(durable.path());
    let _ = std::fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(err)
            if err.kind() == ErrorKind::PermissionDenied && socket_path.starts_with("/tmp") =>
        {
            return;
        }
        Err(err) => panic!(
            "bind observe socket `{}` failed: {err}",
            socket_path.display()
        ),
    };
    let durable_root = durable.path().display().to_string();
    let database = db_path.display().to_string();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap();
        assert!(request.contains("\"limit\":500"), "{request}");
        let response = json!({
            "status": "ok",
            "snapshot": {
                "schema_version": 1,
                "generated_at_ms": 2000,
                "source": {
                    "durable_root": durable_root,
                    "database": database,
                    "read_semantics": "single read transaction over the owner redb handle for live supervise snapshots or over an offline database open",
                    "history_semantics": "delivery queue snapshot only; acked deliveries are removed and historical timelines require a journal"
                },
                "limits": {"max_deliveries": 500, "max_dead_letters": 500},
                "truncated": {"deliveries": false, "dead_letters": false},
                "queues": [{
                    "queue": "input",
                    "depth": 1,
                    "pending": 1,
                    "in_flight": 0,
                    "retrying": 0,
                    "oldest_pending_age_ms": 0
                }],
                "deliveries": [{
                    "delivery_id": "live-one",
                    "queue": "input",
                    "dept": "worker",
                    "source": null,
                    "status": "pending",
                    "observed_at_ms": 1000,
                    "not_before_ms": 1000,
                    "attempt": 0,
                    "redrive_count": 0,
                    "lease_generation": 0,
                    "lease_until_ms": null,
                    "fence_token": "live-one#0",
                    "payload": {
                        "schema": "github.issue",
                        "dedup_key": "issue-81",
                        "digest": "00",
                        "bytes": 2
                    },
                    "last_error_excerpt": null
                }],
                "dead_letters": []
            }
        });
        writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
    });

    let output = framework_command()
        .arg("observe")
        .arg("--durable-root")
        .arg(durable.path())
        .arg("--json")
        .output()
        .unwrap();
    server.join().unwrap();
    let _ = std::fs::remove_file(&socket_path);

    assert_exit(&output, 0);
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("\"delivery_id\": \"live-one\""), "{out}");
    assert!(out.contains("owner redb handle"), "{out}");
}

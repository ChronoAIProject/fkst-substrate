use redb::{Database, TableDefinition};
use serde_json::json;
use std::io::ErrorKind;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DELIVERY_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("delivery_by_id");
const DEAD_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("dead_by_id");

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
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

fn run_command() -> Command {
    let mut command = Command::new(framework_bin());
    command.env_remove("FKST_SUPERVISOR_PID");
    command
}

fn observe_socket_path(durable_root: &Path) -> PathBuf {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in durable_root.as_os_str().to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    PathBuf::from("/tmp").join(format!("fkst-observe-{hash:016x}.sock"))
}

fn write_single_package_workspace(root: &Path) {
    std::fs::write(
        root.join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["."]
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("fkst.toml"),
        r#"
kind = "package"
name = "host"

[code]
root = "."
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("fkst.env"),
        "FKST_QUEUE_CAPACITY=8\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=30s\nFKST_CODEX_PERMIT_SLOTS=1\n",
    )
    .unwrap();
}

fn write_observe_fixture(durable: &Path) {
    let db = Database::create(durable.join("delivery.redb")).unwrap();
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
                "dedup_key": "issue-169",
                "body": "do not expose this body"
            },
            "source": {"kind": "External", "reference": "issue/169"},
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
}

#[test]
fn observe_json_reports_snapshot_without_payload_body() {
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    let db = Database::create(durable.path().join("delivery.redb")).unwrap();
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

    let output = Command::new(framework_bin())
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
fn lua_observe_returns_existing_snapshot_model_without_shelling_out() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    write_single_package_workspace(host.path());
    write_observe_fixture(durable.path());
    let dept = host.path().join("departments/probe/main.lua");
    std::fs::create_dir_all(dept.parent().unwrap()).unwrap();
    std::fs::write(
        &dept,
        r#"
local M = {}
M.spec = { consumes = { "tick" }, produces = { "seen" }, ephemeral = { "tick" }, stall_window = "30s" }
function M.pipeline(_)
  local snapshot = fkst.observe({ limit = 1 })
  assert(snapshot.schema_version == 1, "schema_version")
  assert(snapshot.limits.max_deliveries == 1, "limit")
  assert(snapshot.queues[1].queue == "input", "queue")
  assert(snapshot.deliveries[1].delivery_id == "delivery-one", "delivery")
  assert(snapshot.deliveries[1].payload.schema == "github.issue", "schema")
  assert(snapshot.deliveries[1].payload.dedup_key == "issue-169", "dedup_key")
  assert(snapshot.deliveries[1].payload.body == nil, "payload body leaked")
  assert(snapshot.dead_letters[1].delivery_id == "dead-one", "dead letter")
  raise("seen", { observed = true, schema_version = snapshot.schema_version })
end
return M
"#,
    )
    .unwrap();

    let output = run_command()
        .arg("run")
        .arg(&dept)
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--event")
        .arg(r#"{"queue":"tick","payload":{},"ts":1}"#)
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .env("FKST_DURABLE_ROOT", durable.path())
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("RAISED:"), "{out}");
    assert!(
        !out.contains("do not expose this body"),
        "observe must not expose full payload bodies: {out}"
    );
}

#[test]
fn lua_observe_fails_closed_without_durable_root() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_single_package_workspace(host.path());
    let dept = host.path().join("departments/probe/main.lua");
    std::fs::create_dir_all(dept.parent().unwrap()).unwrap();
    std::fs::write(
        &dept,
        r#"
local M = {}
M.spec = { consumes = { "tick" }, produces = {}, ephemeral = { "tick" }, stall_window = "30s" }
function M.pipeline(_)
  fkst.observe()
end
return M
"#,
    )
    .unwrap();

    let output = run_command()
        .arg("run")
        .arg(&dept)
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--event")
        .arg(r#"{"queue":"tick","payload":{},"ts":1}"#)
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .env_remove("FKST_DURABLE_ROOT")
        .output()
        .unwrap();

    assert_exit(&output, 1);
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("FKST_DURABLE_ROOT must be set"), "{err}");
}

#[test]
fn observe_rejects_missing_database_without_creating_it() {
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();

    let output = Command::new(framework_bin())
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

    let output = Command::new(framework_bin())
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

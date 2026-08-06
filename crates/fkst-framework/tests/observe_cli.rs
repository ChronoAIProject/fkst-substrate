use base64::Engine;
use redb::{Database, TableDefinition};
use serde_json::json;
use std::fs;
use std::io::ErrorKind;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

mod support;

use support::manifest_fixture::{unit_name, write_single_package_workspace};

const DELIVERY_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("delivery_by_id");
const DELIVERY_BY_OBSERVE_ORDER: TableDefinition<&str, &str> =
    TableDefinition::new("delivery_by_observe_order");
const DEAD_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("dead_by_id");
const DEAD_BY_TIME: TableDefinition<&str, ()> = TableDefinition::new("dead_by_time");
const MAX_OBSERVE_LIMIT: usize = 10_000;

fn delivery_observe_key(queue: &str, dept: &str, not_before_ms: u64, delivery_id: &str) -> String {
    format!("{queue}\0{dept}\0{not_before_ms:020}\0{delivery_id}")
}

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
        let mut delivery_order = write.open_table(DELIVERY_BY_OBSERVE_ORDER).unwrap();
        let mut dead = write.open_table(DEAD_BY_ID).unwrap();
        let mut dead_by_time = write.open_table(DEAD_BY_TIME).unwrap();
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
            "subscriber_absent_since_ms": null,
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
        delivery_order
            .insert(
                delivery_observe_key("input", "worker", 1000, "delivery-one").as_str(),
                "delivery-one",
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
        dead_by_time
            .insert(format!("{:020}/{}", 1200, "dead-one").as_str(), &())
            .unwrap();
    }
    write.commit().unwrap();
    drop(db);
}

fn write_dead_letter_page_fixture(durable_root: &Path) {
    let db = Database::create(durable_root.join("delivery.redb")).unwrap();
    let write = db.begin_write().unwrap();
    {
        write.open_table(DELIVERY_BY_ID).unwrap();
        write.open_table(DELIVERY_BY_OBSERVE_ORDER).unwrap();
        let mut dead = write.open_table(DEAD_BY_ID).unwrap();
        let mut dead_by_time = write.open_table(DEAD_BY_TIME).unwrap();
        for (delivery_id, dept, dead_at_ms) in [
            ("dead-c", "middle", 1_201_u64),
            ("dead-b", "alpha", 1_200_u64),
            ("dead-a", "zeta", 1_200_u64),
        ] {
            let record = json!({
                "delivery_id": delivery_id,
                "queue": "input",
                "dept": dept,
                "source": null,
                "observed_at_ms": 900,
                "not_before_ms": 900,
                "dead_at_ms": dead_at_ms,
                "attempts": 3,
                "redrive_count": 0,
                "replayable": false,
                "permanent": true,
                "error_excerpt": "final failure",
                "record": null
            });
            dead.insert(delivery_id, serde_json::to_vec(&record).unwrap().as_slice())
                .unwrap();
            dead_by_time
                .insert(format!("{dead_at_ms:020}/{delivery_id}").as_str(), &())
                .unwrap();
        }
    }
    write.commit().unwrap();
    drop(db);
}

fn write_large_dead_letter_page_fixture(durable_root: &Path) {
    let db = Database::create(durable_root.join("delivery.redb")).unwrap();
    let write = db.begin_write().unwrap();
    {
        write.open_table(DELIVERY_BY_ID).unwrap();
        write.open_table(DELIVERY_BY_OBSERVE_ORDER).unwrap();
        let mut dead = write.open_table(DEAD_BY_ID).unwrap();
        let mut dead_by_time = write.open_table(DEAD_BY_TIME).unwrap();
        for index in 0..=MAX_OBSERVE_LIMIT {
            let delivery_id = format!("dead-{index:05}");
            let dead_at_ms = if index >= MAX_OBSERVE_LIMIT - 1 {
                MAX_OBSERVE_LIMIT as u64
            } else {
                index as u64
            };
            let record = json!({
                "delivery_id": delivery_id,
                "queue": "input",
                "dept": "worker",
                "source": null,
                "observed_at_ms": 900,
                "not_before_ms": 900,
                "dead_at_ms": dead_at_ms,
                "attempts": 3,
                "redrive_count": 0,
                "replayable": false,
                "permanent": true,
                "error_excerpt": "final failure",
                "record": null
            });
            dead.insert(
                delivery_id.as_str(),
                serde_json::to_vec(&record).unwrap().as_slice(),
            )
            .unwrap();
            dead_by_time
                .insert(format!("{dead_at_ms:020}/{delivery_id}").as_str(), &())
                .unwrap();
        }
    }
    write.commit().unwrap();
    drop(db);
}

fn write_subscriber_absence_fixture(durable_root: &Path) {
    let db = Database::create(durable_root.join("delivery.redb")).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut deliveries = write.open_table(DELIVERY_BY_ID).unwrap();
        let mut delivery_order = write.open_table(DELIVERY_BY_OBSERVE_ORDER).unwrap();
        let mut dead = write.open_table(DEAD_BY_ID).unwrap();
        let mut dead_by_time = write.open_table(DEAD_BY_TIME).unwrap();
        let pending = json!({
            "delivery_id": "pending-absent",
            "queue": "orphan",
            "dept": "old_worker",
            "payload": {
                "schema": "test.absent",
                "dedup_key": "pending-absent"
            },
            "source": {"kind": "External", "reference": "fixture/pending-absent"},
            "cron_payload": null,
            "observed_at_ms": 1000,
            "attempt": 0,
            "redrive_count": 0,
            "subscriber_absent_since_ms": 1100,
            "lease_generation": 0,
            "lease_until_ms": null,
            "not_before_ms": 4000000000000_u64,
            "last_error_excerpt": null
        });
        deliveries
            .insert(
                "pending-absent",
                serde_json::to_vec(&pending).unwrap().as_slice(),
            )
            .unwrap();
        delivery_order
            .insert(
                delivery_observe_key("orphan", "old_worker", 4000000000000_u64, "pending-absent")
                    .as_str(),
                "pending-absent",
            )
            .unwrap();
        let original = json!({
            "delivery_id": "dead-absent",
            "queue": "orphan",
            "dept": "old_worker",
            "payload": {
                "schema": "test.absent",
                "dedup_key": "dead-absent"
            },
            "source": {"kind": "External", "reference": "fixture/dead-absent"},
            "cron_payload": null,
            "observed_at_ms": 1000,
            "attempt": 0,
            "redrive_count": 0,
            "subscriber_absent_since_ms": 1100,
            "lease_generation": 0,
            "lease_until_ms": null,
            "not_before_ms": 1600,
            "last_error_excerpt": "subscriber-absent"
        });
        let dead_record = json!({
            "delivery_id": "dead-absent",
            "queue": "orphan",
            "dept": "old_worker",
            "source": {"kind": "External", "reference": "fixture/dead-absent"},
            "observed_at_ms": 1000,
            "not_before_ms": 1600,
            "dead_at_ms": 1600,
            "attempts": 0,
            "redrive_count": 0,
            "replayable": true,
            "permanent": false,
            "error_excerpt": "subscriber-absent",
            "record": original
        });
        dead.insert(
            "dead-absent",
            serde_json::to_vec(&dead_record).unwrap().as_slice(),
        )
        .unwrap();
        dead_by_time
            .insert(format!("{:020}/{}", 1600, "dead-absent").as_str(), &())
            .unwrap();
    }
    write.commit().unwrap();
    drop(db);
}

fn write_pending_delivery_fixture(durable_root: &Path, rows: &[(&str, &str, &str)]) {
    let db = Database::create(durable_root.join("delivery.redb")).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut deliveries = write.open_table(DELIVERY_BY_ID).unwrap();
        let mut delivery_order = write.open_table(DELIVERY_BY_OBSERVE_ORDER).unwrap();
        write.open_table(DEAD_BY_ID).unwrap();
        write.open_table(DEAD_BY_TIME).unwrap();
        for (delivery_id, queue, dept) in rows {
            let delivery = json!({
                "delivery_id": delivery_id,
                "queue": queue,
                "dept": dept,
                "payload": {
                    "schema": "test.pending",
                    "dedup_key": delivery_id
                },
                "source": {"kind": "External", "reference": format!("fixture/{delivery_id}")},
                "cron_payload": null,
                "observed_at_ms": 1000,
                "attempt": 0,
                "redrive_count": 0,
                "subscriber_absent_since_ms": null,
                "lease_generation": 0,
                "lease_until_ms": null,
                "not_before_ms": 4000000000000_u64,
                "last_error_excerpt": null
            });
            deliveries
                .insert(
                    *delivery_id,
                    serde_json::to_vec(&delivery).unwrap().as_slice(),
                )
                .unwrap();
            delivery_order
                .insert(
                    delivery_observe_key(queue, dept, 4000000000000_u64, delivery_id).as_str(),
                    *delivery_id,
                )
                .unwrap();
        }
    }
    write.commit().unwrap();
    drop(db);
}

fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn terminate_child(child: &mut Child) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = child.wait();
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
fn fkst_observe_traverses_durable_dead_letters_in_two_pages() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    write_dead_letter_page_fixture(durable.path());
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    let probe = host.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { consumes = { "tick" }, produces = { "seen" }, ephemeral = { "tick" } }

function M.pipeline(event)
  local first = fkst.observe({
    limit = 2,
    include = { "errors", "entities" },
    page = { section = "dead_letters" },
  })
  local malformed_ok, malformed_error = pcall(function()
    return fkst.observe({
      limit = 2,
      include = { "errors", "entities" },
      page = { section = "dead_letters", after = "malformed" },
    })
  end)
  local second = fkst.observe({
    limit = 2,
    include = { "errors", "entities" },
    page = { section = "dead_letters", after = first.page.next },
  })
  raise("seen", {
    delivery_ids = {
      first.dead_letters[1].delivery_id,
      first.dead_letters[2].delivery_id,
      second.dead_letters[1].delivery_id,
    },
    first_section = first.page.section,
    first_has_next = first.page.next ~= nil,
    first_count = #first.dead_letters,
    malformed_failed_closed = not malformed_ok
      and string.find(tostring(malformed_error), "observe dead-letter cursor invalid", 1, true) ~= nil,
    second_section = second.page.section,
    second_has_next = second.page.next ~= nil,
    second_count = #second.dead_letters,
  })
end

return M
"#,
    )
    .unwrap();
    write_single_package_workspace(host.path());

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
    assert_eq!(
        raised["delivery_ids"],
        json!(["dead-a", "dead-b", "dead-c"])
    );
    assert_eq!(raised["first_section"], "dead_letters");
    assert_eq!(raised["first_has_next"], true);
    assert_eq!(raised["first_count"], 2, "{raised}");
    assert_eq!(raised["malformed_failed_closed"], true, "{raised}");
    assert_eq!(raised["second_section"], "dead_letters");
    assert_eq!(raised["second_has_next"], false, "{raised}");
    assert_eq!(raised["second_count"], 1, "{raised}");
}

#[test]
fn fkst_observe_traverses_stable_dead_letter_fixture_beyond_max_limit() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    write_large_dead_letter_page_fixture(durable.path());
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    let probe = host.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { consumes = { "tick" }, produces = { "seen" }, ephemeral = { "tick" } }

function M.pipeline(event)
  local limit = 10000
  local expected_total = limit + 1
  local legacy = fkst.observe({
    limit = limit,
    include = { "errors", "entities" },
  })
  local total = 0
  local page_count = 0
  local first_page_count = 0
  local first_has_next = false
  local final_page_count = 0
  local duplicate_free = true
  local ordered = true
  local seen = {}
  local previous_dead_at_ms = nil
  local previous_delivery_id = nil
  local boundary_dead_at_ms = nil
  local first_after_boundary_dead_at_ms = nil
  local after = nil

  repeat
    local page = fkst.observe({
      limit = limit,
      include = { "errors", "entities" },
      page = { section = "dead_letters", after = after },
    })
    page_count = page_count + 1
    if page_count > 2 then
      error("unexpected additional dead-letter page")
    end
    if page_count == 1 then
      first_page_count = #page.dead_letters
    end
    for _, entry in ipairs(page.dead_letters) do
      local expected_delivery_id = string.format("dead-%05d", total)
      if seen[entry.delivery_id] then
        duplicate_free = false
      end
      seen[entry.delivery_id] = true
      if entry.delivery_id ~= expected_delivery_id then
        ordered = false
      end
      if previous_dead_at_ms ~= nil then
        if entry.dead_at_ms < previous_dead_at_ms
          or (entry.dead_at_ms == previous_dead_at_ms
            and entry.delivery_id <= previous_delivery_id) then
          ordered = false
        end
      end
      previous_dead_at_ms = entry.dead_at_ms
      previous_delivery_id = entry.delivery_id
      if total == limit - 1 then
        boundary_dead_at_ms = entry.dead_at_ms
      elseif total == limit then
        first_after_boundary_dead_at_ms = entry.dead_at_ms
      end
      total = total + 1
    end
    after = page.page.next
    if page_count == 1 then
      first_has_next = after ~= nil
    end
    if after == nil then
      final_page_count = #page.dead_letters
    end
  until after == nil

  raise("seen", {
    total = total,
    expected_total = expected_total,
    duplicate_free = duplicate_free,
    ordered = ordered,
    equal_timestamp_boundary = boundary_dead_at_ms == first_after_boundary_dead_at_ms,
    page_count = page_count,
    first_page_count = first_page_count,
    first_has_next = first_has_next,
    final_page_count = final_page_count,
    terminal_has_next = after ~= nil,
    legacy_count = #legacy.dead_letters,
    legacy_truncated = legacy.truncated.dead_letters,
    legacy_has_page = legacy.page ~= nil,
  })
end

return M
"#,
    )
    .unwrap();
    write_single_package_workspace(host.path());

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
    assert_eq!(raised["total"], MAX_OBSERVE_LIMIT + 1, "{raised}");
    assert_eq!(raised["expected_total"], MAX_OBSERVE_LIMIT + 1, "{raised}");
    assert_eq!(raised["duplicate_free"], true, "{raised}");
    assert_eq!(raised["ordered"], true, "{raised}");
    assert_eq!(raised["equal_timestamp_boundary"], true, "{raised}");
    assert_eq!(raised["page_count"], 2, "{raised}");
    assert_eq!(raised["first_page_count"], MAX_OBSERVE_LIMIT, "{raised}");
    assert_eq!(raised["first_has_next"], true, "{raised}");
    assert_eq!(raised["final_page_count"], 1, "{raised}");
    assert_eq!(raised["terminal_has_next"], false, "{raised}");
    assert_eq!(raised["legacy_count"], MAX_OBSERVE_LIMIT, "{raised}");
    assert_eq!(raised["legacy_truncated"], true, "{raised}");
    assert_eq!(raised["legacy_has_page"], false, "{raised}");
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
fn observe_reports_actionable_error_when_live_owner_socket_is_unavailable() {
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
    let _ = fs::remove_file(&socket_path);

    let output = framework_command()
        .arg("observe")
        .arg("--durable-root")
        .arg(durable.path())
        .arg("--json")
        .output()
        .unwrap();

    assert_exit(&output, 2);
    let err = stderr(&output);
    assert!(err.contains("observe-live-owner-unavailable"), "{err}");
    assert!(err.contains(&socket_path.display().to_string()), "{err}");
    assert!(
        err.contains("restart the database-owning `supervise` process to restore the live endpoint, or stop that process before offline inspection"),
        "{err}"
    );
    drop(db);
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
                    "oldest_pending_age_ms": 0,
                    "subscriber_status": "current",
                    "has_current_subscriber": true
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
                    "subscriber_absent_since_ms": null,
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
    assert!(out.contains("\"subscriber_status\": \"current\""), "{out}");
    assert!(out.contains("owner redb handle"), "{out}");
}

#[test]
fn observe_json_reports_live_subscriber_status_for_pending_queues() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    fs::create_dir_all(host.path().join("departments/worker")).unwrap();
    fs::create_dir_all(host.path().join("raisers")).unwrap();
    fs::write(
        host.path().join("fkst.env"),
        "FKST_QUEUE_CAPACITY=100\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=30s\nFKST_CODEX_PERMIT_SLOTS=20\n",
    )
    .unwrap();
    fs::write(
        host.path().join("departments/worker/main.lua"),
        r#"
local M = {}
M.spec = { consumes = { "active" }, stall_window = "30s" }
function M.pipeline(event)
end
return M
"#,
    )
    .unwrap();
    fs::write(
        host.path().join("raisers/active.lua"),
        format!(
            r#"return {{ type = "file_watch", glob = {:?}, produces = "active" }}"#,
            host.path()
                .join("no-matches")
                .join("*.txt")
                .to_string_lossy()
        ),
    )
    .unwrap();
    write_single_package_workspace(host.path());
    write_pending_delivery_fixture(
        durable.path(),
        &[
            ("active-one", "active", "worker"),
            ("orphan-one", "orphan", "removed_worker"),
        ],
    );

    let socket_path = observe_socket_path(durable.path());
    let _ = std::fs::remove_file(&socket_path);
    let mut supervise = framework_command()
        .current_dir(host.path())
        .arg("supervise")
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--framework-bin")
        .arg(framework_bin())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .env("FKST_DURABLE_ROOT", durable.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    if !wait_for_path(&socket_path, Duration::from_secs(10)) {
        terminate_child(&mut supervise);
        panic!(
            "timed out waiting for observe socket {}",
            socket_path.display()
        );
    }

    let output = framework_command()
        .arg("observe")
        .arg("--durable-root")
        .arg(durable.path())
        .arg("--json")
        .output()
        .unwrap();
    terminate_child(&mut supervise);

    assert_exit(&output, 0);
    let snapshot: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let queues = snapshot["queues"].as_array().unwrap();
    let active = queues
        .iter()
        .find(|queue| queue["queue"] == "active")
        .unwrap();
    let orphan = queues
        .iter()
        .find(|queue| queue["queue"] == "orphan")
        .unwrap();

    assert_eq!(active["pending"], 1);
    assert_eq!(orphan["pending"], 1);
    assert_eq!(active["subscriber_status"], "current");
    assert_eq!(orphan["subscriber_status"], "absent");
    assert_eq!(active["has_current_subscriber"], true);
    assert_eq!(orphan["has_current_subscriber"], false);
    let orphan_delivery = snapshot["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|delivery| delivery["delivery_id"] == "orphan-one")
        .unwrap();
    assert!(
        orphan_delivery["subscriber_absent_since_ms"].is_u64(),
        "{orphan_delivery}"
    );
}

#[test]
fn observe_json_distinguishes_pending_absent_from_dead_lettered_absent() {
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    write_subscriber_absence_fixture(durable.path());

    let output = framework_command()
        .arg("observe")
        .arg("--durable-root")
        .arg(durable.path())
        .arg("--json")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let snapshot: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let pending = snapshot["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|delivery| delivery["delivery_id"] == "pending-absent")
        .unwrap();
    let dead = snapshot["dead_letters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|delivery| delivery["delivery_id"] == "dead-absent")
        .unwrap();

    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["subscriber_absent_since_ms"], 1100);
    assert_eq!(dead["error_excerpt"], "subscriber-absent");
    assert_eq!(dead["replayable"], true);
    assert_eq!(dead["permanent"], false);
}

#[test]
fn observe_json_reports_unknown_subscriber_status_without_live_graph_authority() {
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    write_pending_delivery_fixture(
        durable.path(),
        &[
            ("active-one", "active", "worker"),
            ("orphan-one", "orphan", "removed_worker"),
        ],
    );

    let output = framework_command()
        .arg("observe")
        .arg("--durable-root")
        .arg(durable.path())
        .arg("--json")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let snapshot: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let queues = snapshot["queues"].as_array().unwrap();
    let active = queues
        .iter()
        .find(|queue| queue["queue"] == "active")
        .unwrap();
    let orphan = queues
        .iter()
        .find(|queue| queue["queue"] == "orphan")
        .unwrap();

    assert_eq!(active["subscriber_status"], "unknown");
    assert_eq!(orphan["subscriber_status"], "unknown");
    assert!(active.get("has_current_subscriber").is_none());
    assert!(orphan.get("has_current_subscriber").is_none());
}

#[test]
fn observe_human_output_includes_subscriber_status() {
    let durable = tempfile::Builder::new()
        .prefix("fkst-durable")
        .tempdir()
        .unwrap();
    write_pending_delivery_fixture(durable.path(), &[("pending-one", "input", "worker")]);

    let output = framework_command()
        .arg("observe")
        .arg("--durable-root")
        .arg(durable.path())
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("queue=input"), "{out}");
    assert!(out.contains("subscriber_status=unknown"), "{out}");
}

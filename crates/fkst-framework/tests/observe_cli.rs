use redb::{Database, TableDefinition};
use serde_json::json;
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

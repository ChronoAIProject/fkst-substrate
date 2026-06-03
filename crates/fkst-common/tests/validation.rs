// crate-level integration tests own behavior coverage while runtime modules keep runtime code.

use fkst_common::config::{Config, DepartmentDecl, LimitsDecl, QueueDecl, RaiserDecl};
use fkst_common::validation::validate;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn touch(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, "-- empty\n").unwrap();
    p
}

fn cfg_minimal(lua_file: &Path) -> Config {
    let mut queue = BTreeMap::new();
    queue.insert(
        "tick".into(),
        QueueDecl {
            capacity: 10,
            fanout: false,
        },
    );
    let mut raiser = BTreeMap::new();
    raiser.insert(
        "cron_x".into(),
        RaiserDecl::Cron {
            interval: "10s".into(),
            produces: "tick".into(),
        },
    );
    let mut department = BTreeMap::new();
    department.insert(
        "d".into(),
        DepartmentDecl {
            lua: lua_file.into(),
            consumes: vec!["tick".into()],
            produces: vec![],
            stall_window: "30m".into(),
        },
    );
    Config {
        queue,
        raiser,
        department,
        limits: LimitsDecl {
            global_codex_processes: 1,
        },
    }
}

#[test]
fn direct_deserialize_requires_department_stall_window() {
    let err = serde_json::from_value::<DepartmentDecl>(serde_json::json!({
        "lua": "d.lua",
        "consumes": ["tick"]
    }))
    .unwrap_err();

    assert!(err.to_string().contains("stall_window"), "{err}");
}

#[test]
fn direct_deserialize_requires_global_codex_processes() {
    let err = serde_json::from_value::<LimitsDecl>(serde_json::json!({})).unwrap_err();

    assert!(err.to_string().contains("global_codex_processes"), "{err}");
}

#[test]
fn valid_config_passes() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let cfg = cfg_minimal(&lua);
    let warnings = validate(&cfg, tmp.path()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn unknown_produces_queue_rejected() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.raiser.insert(
        "bad".into(),
        RaiserDecl::Cron {
            interval: "10s".into(),
            produces: "nonexistent".into(),
        },
    );
    let e = validate(&cfg, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("nonexistent"), "{}", e);
}

#[test]
fn unknown_consumes_queue_rejected() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.department
        .get_mut("d")
        .unwrap()
        .consumes
        .push("ghost".into());
    let e = validate(&cfg, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("ghost"), "{}", e);
}

#[test]
fn missing_lua_file_rejected() {
    let tmp = tempdir().unwrap();
    let lua = tmp.path().join("does_not_exist.lua");
    let cfg = cfg_minimal(&lua);
    let e = validate(&cfg, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("does not exist"), "{}", e);
}

#[test]
fn capacity_zero_rejected() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.queue.get_mut("tick").unwrap().capacity = 0;
    let e = validate(&cfg, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("capacity 0"), "{}", e);
}

#[test]
fn global_codex_processes_zero_rejected() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.limits.global_codex_processes = 0;
    let e = validate(&cfg, tmp.path()).unwrap_err();
    let message = e.to_string();
    assert!(
        message.contains("limits.global_codex_processes"),
        "{message}"
    );
    assert!(message.contains("must be > 0"), "{message}");
}

#[test]
fn bad_stall_window_rejected() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.department.get_mut("d").unwrap().stall_window = "30x".into();
    let e = validate(&cfg, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("stall_window"), "{}", e);
}

#[test]
fn isolated_queue_rejected() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.queue.insert(
        "orphan".into(),
        QueueDecl {
            capacity: 10,
            fanout: false,
        },
    );
    let e = validate(&cfg, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("orphan"), "{}", e);
}

#[test]
fn producer_without_consumer_warns() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.queue.insert(
        "produced_only".into(),
        QueueDecl {
            capacity: 10,
            fanout: false,
        },
    );
    cfg.department
        .get_mut("d")
        .unwrap()
        .produces
        .push("produced_only".into());

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("produced_only"), "{warnings:?}");
    assert!(warnings[0].contains("department 'd'"), "{warnings:?}");
    assert!(warnings[0].contains("has no consumer"), "{warnings:?}");
}

#[test]
fn consumer_without_producer_warns() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.queue.insert(
        "consumed_only".into(),
        QueueDecl {
            capacity: 10,
            fanout: false,
        },
    );
    cfg.department
        .get_mut("d")
        .unwrap()
        .consumes
        .push("consumed_only".into());

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("consumed_only"), "{warnings:?}");
    assert!(warnings[0].contains("department 'd'"), "{warnings:?}");
    assert!(warnings[0].contains("has no producer"), "{warnings:?}");
}

#[test]
fn duplicate_consumers_without_fanout_rejected() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.department.insert(
        "other".into(),
        DepartmentDecl {
            lua: lua.clone(),
            consumes: vec!["tick".into()],
            produces: vec![],
            stall_window: "30m".into(),
        },
    );

    let e = validate(&cfg, tmp.path()).unwrap_err();

    assert!(e.to_string().contains("multiple consumers"), "{}", e);
    assert!(e.to_string().contains("not declared fanout"), "{}", e);
}

#[test]
fn duplicate_consumers_with_fanout_pass() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.queue.get_mut("tick").unwrap().fanout = true;
    cfg.department.insert(
        "other".into(),
        DepartmentDecl {
            lua: lua.clone(),
            consumes: vec!["tick".into()],
            produces: vec![],
            stall_window: "30m".into(),
        },
    );

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn consume_produce_same_queue_without_fanout_rejected() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.department
        .get_mut("d")
        .unwrap()
        .produces
        .push("tick".into());

    let e = validate(&cfg, tmp.path()).unwrap_err();

    assert!(e.to_string().contains("consumed and produced"), "{}", e);
    assert!(e.to_string().contains("not declared fanout"), "{}", e);
}

#[test]
fn consume_produce_same_queue_with_fanout_pass() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.queue.get_mut("tick").unwrap().fanout = true;
    cfg.department
        .get_mut("d")
        .unwrap()
        .produces
        .push("tick".into());

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert!(warnings.is_empty(), "{warnings:?}");
}

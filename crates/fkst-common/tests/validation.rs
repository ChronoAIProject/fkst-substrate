// crate-level integration tests own behavior coverage while runtime modules keep runtime code.

use fkst_common::config::{Config, DepartmentDecl, LimitsDecl, QueueDecl, RaiserDecl, RetryDecl};
use fkst_common::validation::{validate, validate_runtime_key};
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
            owner_root: lua_file.parent().unwrap().into(),
            owner_namespace: "pkg".to_string(),
            consumes: vec!["tick".into()],
            produces: vec![],
            ephemeral: vec![],
            stall_window: "30m".into(),
            graph_json: false,
            retry: None,
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

fn add_queue(cfg: &mut Config, name: &str) {
    cfg.queue.insert(
        name.into(),
        QueueDecl {
            capacity: 10,
            fanout: false,
        },
    );
}

fn department_decl(lua_file: &Path, consumes: Vec<&str>, produces: Vec<&str>) -> DepartmentDecl {
    DepartmentDecl {
        lua: lua_file.into(),
        owner_root: lua_file.parent().unwrap().into(),
        owner_namespace: "pkg".to_string(),
        consumes: consumes.into_iter().map(String::from).collect(),
        produces: produces.into_iter().map(String::from).collect(),
        ephemeral: vec![],
        stall_window: "30m".into(),
        graph_json: false,
        retry: None,
    }
}

#[test]
fn direct_deserialize_requires_department_stall_window() {
    let err = serde_json::from_value::<DepartmentDecl>(serde_json::json!({
        "lua": "d.lua",
        "owner_root": ".",
        "owner_namespace": "pkg",
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
fn runtime_key_accepts_readable_relative_paths() {
    assert_eq!(
        validate_runtime_key("github-proxy/issue/owner/repo/42").unwrap(),
        "github-proxy/issue/owner/repo/42"
    );
    assert_eq!(validate_runtime_key("a.B_0-1").unwrap(), "a.B_0-1");
}

#[test]
fn runtime_key_rejects_traversal_and_invalid_segments() {
    let too_long_segment = "a".repeat(256);
    for key in [
        "..",
        "a/../b",
        "/abs",
        "a//b",
        "a/",
        "",
        "bad key",
        "bad:key",
        "a\\b",
        "a/.../b",
        too_long_segment.as_str(),
    ] {
        let err = validate_runtime_key(key).unwrap_err();
        assert!(err.to_string().contains("runtime key"), "{key:?}: {err}");
    }
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
            owner_root: tmp.path().into(),
            owner_namespace: "pkg".to_string(),
            consumes: vec!["tick".into()],
            produces: vec![],
            ephemeral: vec![],
            stall_window: "30m".into(),
            graph_json: false,
            retry: None,
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
            owner_root: tmp.path().into(),
            owner_namespace: "pkg".to_string(),
            consumes: vec!["tick".into()],
            produces: vec![],
            ephemeral: vec![],
            stall_window: "30m".into(),
            graph_json: false,
            retry: None,
        },
    );

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn mixed_retry_consumers_on_fanout_queue_pass() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.queue.get_mut("tick").unwrap().fanout = true;
    cfg.department.get_mut("d").unwrap().retry = Some(RetryDecl {
        max_attempts: 5,
        base: "60s".into(),
        cap: "30m".into(),
    });
    cfg.department.insert(
        "other".into(),
        DepartmentDecl {
            lua: lua.clone(),
            owner_root: tmp.path().into(),
            owner_namespace: "pkg".to_string(),
            consumes: vec!["tick".into()],
            produces: vec![],
            ephemeral: vec![],
            stall_window: "30m".into(),
            graph_json: false,
            retry: None,
        },
    );

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn ephemeral_queue_must_be_consumed_by_department() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.department
        .get_mut("d")
        .unwrap()
        .ephemeral
        .push("ghost".into());

    let e = validate(&cfg, tmp.path()).unwrap_err();

    assert!(
        e.to_string().contains("marks queue 'ghost' ephemeral"),
        "{e}"
    );
}

#[test]
fn all_ephemeral_producer_to_reliable_consumer_rejected() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    add_queue(&mut cfg, "proposal");
    let producer = cfg.department.get_mut("d").unwrap();
    producer.consumes = vec!["tick".into()];
    producer.ephemeral = vec!["tick".into()];
    producer.produces = vec!["proposal".into()];
    cfg.department.insert(
        "reliable_worker".into(),
        department_decl(&lua, vec!["proposal"], vec![]),
    );

    let err = validate(&cfg, tmp.path()).unwrap_err();
    let message = err.to_string();

    assert!(message.contains("department 'd'"), "{message}");
    assert!(message.contains("queue 'proposal'"), "{message}");
    assert!(message.contains("ephemeral"), "{message}");
}

#[test]
fn reliable_input_can_raise_to_reliable_consumer() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    add_queue(&mut cfg, "proposal");
    cfg.department.get_mut("d").unwrap().produces = vec!["proposal".into()];
    cfg.department.insert(
        "reliable_worker".into(),
        department_decl(&lua, vec!["proposal"], vec![]),
    );

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn all_ephemeral_input_can_raise_to_ephemeral_consumer() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    add_queue(&mut cfg, "proposal");
    let producer = cfg.department.get_mut("d").unwrap();
    producer.ephemeral = vec!["tick".into()];
    producer.produces = vec!["proposal".into()];
    let mut downstream = department_decl(&lua, vec!["proposal"], vec![]);
    downstream.ephemeral = vec!["proposal".into()];
    cfg.department.insert("ephemeral_worker".into(), downstream);

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn mixed_ephemeral_and_reliable_inputs_can_raise_to_reliable_consumer() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    add_queue(&mut cfg, "seed");
    add_queue(&mut cfg, "proposal");
    cfg.raiser.insert(
        "cron_seed".into(),
        RaiserDecl::Cron {
            interval: "10s".into(),
            produces: "seed".into(),
        },
    );
    let producer = cfg.department.get_mut("d").unwrap();
    producer.consumes = vec!["tick".into(), "seed".into()];
    producer.ephemeral = vec!["tick".into()];
    producer.produces = vec!["proposal".into()];
    cfg.department.insert(
        "reliable_worker".into(),
        department_decl(&lua, vec!["proposal"], vec![]),
    );

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn all_ephemeral_input_can_raise_to_queue_without_consumer() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    add_queue(&mut cfg, "proposal");
    let producer = cfg.department.get_mut("d").unwrap();
    producer.ephemeral = vec!["tick".into()];
    producer.produces = vec!["proposal".into()];

    let warnings = validate(&cfg, tmp.path()).unwrap();

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("proposal"), "{warnings:?}");
    assert!(warnings[0].contains("has no consumer"), "{warnings:?}");
}

#[test]
fn retry_decl_validates_attempts_and_durations() {
    let tmp = tempdir().unwrap();
    let lua = touch(tmp.path(), "d.lua");
    let mut cfg = cfg_minimal(&lua);
    cfg.department.get_mut("d").unwrap().retry = Some(RetryDecl {
        max_attempts: 0,
        base: "60s".into(),
        cap: "30m".into(),
    });
    let e = validate(&cfg, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("retry.max_attempts"), "{}", e);

    cfg.department.get_mut("d").unwrap().retry = Some(RetryDecl {
        max_attempts: 5,
        base: "0s".into(),
        cap: "30m".into(),
    });
    let e = validate(&cfg, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("retry.base"), "{}", e);

    cfg.department.get_mut("d").unwrap().retry = Some(RetryDecl {
        max_attempts: 5,
        base: "30m".into(),
        cap: "60s".into(),
    });
    let e = validate(&cfg, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("retry.cap"), "{}", e);
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

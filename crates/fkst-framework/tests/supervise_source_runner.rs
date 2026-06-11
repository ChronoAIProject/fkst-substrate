// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/supervise/delivery_index.rs"]
mod delivery_index;
#[path = "../src/supervise/delivery_retry.rs"]
mod delivery_retry;
#[path = "../src/supervise/delivery_router.rs"]
mod delivery_router;
#[path = "../src/supervise/delivery_store.rs"]
mod delivery_store;
#[path = "../src/supervise/delivery_transition.rs"]
mod delivery_transition;
#[path = "../src/supervise/delivery_types.rs"]
mod delivery_types;
#[path = "../src/supervise/delivery_watch.rs"]
mod delivery_watch;
#[path = "../src/supervise/event_fanout.rs"]
mod event_fanout;
#[allow(dead_code)]
#[path = "../src/supervise/source_runner.rs"]
mod source_runner;

use delivery_router::DeliveryRouter;
use event_fanout::Fanout;
use fkst_common::config::{Config, DepartmentDecl, LimitsDecl, QueueDecl};
use source_runner::spawn_file_watch;
use std::collections::BTreeMap;
use tokio::time::{timeout, Duration};

fn fanout_router(queue_name: &str) -> (Fanout, DeliveryRouter) {
    let fanout = Fanout::new();
    let mut queue = BTreeMap::new();
    queue.insert(
        queue_name.to_string(),
        QueueDecl {
            capacity: 10,
            fanout: false,
        },
    );
    let mut department = BTreeMap::new();
    department.insert(
        "test".to_string(),
        DepartmentDecl {
            lua: "departments/test/main.lua".into(),
            consumes: vec![queue_name.to_string()],
            produces: Vec::new(),
            ephemeral: vec![queue_name.to_string()],
            stall_window: "30s".to_string(),
            graph_json: false,
            retry: None,
            owner_root: std::path::PathBuf::from("."),
            owner_namespace: "pkg".to_string(),
        },
    );
    let cfg = Config {
        queue,
        raiser: BTreeMap::new(),
        department,
        limits: LimitsDecl {
            global_codex_processes: 1,
        },
    };
    let router = DeliveryRouter::new(&cfg, fanout.clone(), None);
    (fanout, router)
}

#[tokio::test]
async fn file_watch_existing_file_emits_event() {
    let tmp = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let file = tmp.path().join("existing.txt");
    std::fs::write(&file, "ready").unwrap();
    let glob = tmp.path().join("*.txt");

    let (fanout, router) = fanout_router("files");
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        tmp.path(),
        "files".to_string(),
        router,
    )
    .unwrap();

    let got = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.queue, "files");
    assert_eq!(
        got.payload,
        serde_json::json!({"path": file.canonicalize().unwrap().to_string_lossy()})
    );
    handle.abort();
}

#[tokio::test]
async fn file_watch_new_file_emits_event() {
    let tmp = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let glob = tmp.path().join("*.txt");

    let (fanout, router) = fanout_router("files");
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        tmp.path(),
        "files".to_string(),
        router,
    )
    .unwrap();

    let file = tmp.path().join("new.txt");
    std::fs::write(&file, "ready").unwrap();

    let got = timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        got.payload,
        serde_json::json!({"path": file.canonicalize().unwrap().to_string_lossy()})
    );
    handle.abort();
}

#[tokio::test]
async fn file_watch_periodic_scan_dedupes_unchanged_file() {
    let tmp = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let file = tmp.path().join("stable.txt");
    std::fs::write(&file, "ready").unwrap();
    let glob = tmp.path().join("*.txt");

    let (fanout, router) = fanout_router("files");
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        tmp.path(),
        "files".to_string(),
        router,
    )
    .unwrap();

    let got = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        got.payload,
        serde_json::json!({"path": file.canonicalize().unwrap().to_string_lossy()})
    );
    assert!(timeout(Duration::from_secs(7), rx.recv()).await.is_err());
    handle.abort();
}

#[tokio::test]
async fn file_watch_periodic_scan_reemits_changed_file() {
    let tmp = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let file = tmp.path().join("stable.txt");
    std::fs::write(&file, "ready").unwrap();
    let glob = tmp.path().join("*.txt");

    let (fanout, router) = fanout_router("files");
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        tmp.path(),
        "files".to_string(),
        router,
    )
    .unwrap();

    let got = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        got.payload,
        serde_json::json!({"path": file.canonicalize().unwrap().to_string_lossy()})
    );

    std::fs::write(&file, "ready changed").unwrap();
    let changed = timeout(Duration::from_secs(7), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        changed.payload,
        serde_json::json!({"path": file.canonicalize().unwrap().to_string_lossy()})
    );
    handle.abort();
}

#[tokio::test]
async fn file_watch_restart_startup_scan_replays_existing_file() {
    let tmp = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let file = tmp.path().join("stable.txt");
    std::fs::write(&file, "ready").unwrap();
    let glob = tmp.path().join("*.txt");

    let (fanout, router) = fanout_router("files");
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        tmp.path(),
        "files".to_string(),
        router.clone(),
    )
    .unwrap();

    let got = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        got.payload,
        serde_json::json!({"path": file.canonicalize().unwrap().to_string_lossy()})
    );
    handle.abort();

    let restarted = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        tmp.path(),
        "files".to_string(),
        router,
    )
    .unwrap();
    let replayed = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        replayed.payload,
        serde_json::json!({"path": file.canonicalize().unwrap().to_string_lossy()})
    );
    restarted.abort();
}

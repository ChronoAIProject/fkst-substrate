// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/supervise/event_fanout.rs"]
mod event_fanout;
#[allow(dead_code)]
#[path = "../src/supervise/source_runner.rs"]
mod source_runner;

use event_fanout::Fanout;
use source_runner::spawn_file_watch;
use tokio::time::{timeout, Duration};

#[tokio::test]
async fn file_watch_existing_file_emits_event() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("existing.txt");
    std::fs::write(&file, "ready").unwrap();
    let glob = tmp.path().join("*.txt");

    let fanout = Fanout::new();
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        "files".to_string(),
        fanout,
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
    let tmp = tempfile::tempdir().unwrap();
    let glob = tmp.path().join("*.txt");

    let fanout = Fanout::new();
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        "files".to_string(),
        fanout,
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
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("stable.txt");
    std::fs::write(&file, "ready").unwrap();
    let glob = tmp.path().join("*.txt");

    let fanout = Fanout::new();
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        "files".to_string(),
        fanout,
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
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("stable.txt");
    std::fs::write(&file, "ready").unwrap();
    let glob = tmp.path().join("*.txt");

    let fanout = Fanout::new();
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        "files".to_string(),
        fanout,
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
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("stable.txt");
    std::fs::write(&file, "ready").unwrap();
    let glob = tmp.path().join("*.txt");

    let fanout = Fanout::new();
    let mut rx = fanout.subscribe("files", 10).await;
    let handle = spawn_file_watch(
        "watch".to_string(),
        glob.to_str().unwrap(),
        "files".to_string(),
        fanout.clone(),
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
        "files".to_string(),
        fanout,
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

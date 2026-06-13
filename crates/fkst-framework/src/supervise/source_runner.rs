//! Raisers produce events into queues.

use super::delivery_router::{DeliveryRouter, PublishEnvelope};
use super::delivery_types::{SourceKind, SourceRef};
use fkst_common::Event;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;
use tracing::{info, warn};

/// Spawn a Cron raiser as a tokio task. First fire is start plus deterministic jitter.
/// Missed ticks during sleep/pause coalesce to one tick.
///
/// `interval_str` formats: `Ns`, `Nm`, or `Nh`, for example "10s", "5m", or "1h".
pub fn spawn_cron(
    name: String,
    interval_str: &str,
    produces: String,
    router: DeliveryRouter,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let interval = parse_duration(interval_str)?;
    let handle = tokio::spawn(async move {
        info!(raiser = %name, interval_s = interval.as_secs(), "cron raiser starting");
        let mut next = Instant::now() + cron_first_fire_jitter(&name, interval);
        loop {
            tokio::time::sleep_until(next).await;
            let payload = cron_payload(&name);
            let ev = Event::new(&produces, payload);
            let slot = cron_slot_unix_millis(interval);
            if let Err(e) = router.publish(PublishEnvelope {
                event: ev,
                source: Some(SourceRef {
                    kind: SourceKind::Cron,
                    reference: cron_source_reference(&name, slot),
                }),
                cron_payload: Some(cron_payload(&name)),
                derived: None,
            }) {
                warn!(raiser = %name, error = %e, "delivery publish failed");
            }
            // Monotonic next-fire; missed ticks coalesce to one.
            let now = Instant::now();
            next = std::cmp::max(next + interval, now);
        }
    });
    Ok(handle)
}

fn cron_payload(name: &str) -> serde_json::Value {
    serde_json::json!({"raiser": name})
}

fn cron_first_fire_jitter(name: &str, interval: Duration) -> Duration {
    let bound = std::cmp::min(interval, Duration::from_secs(30));
    let bound_ms = bound.as_millis().min(u64::MAX as u128) as u64;
    if bound_ms == 0 {
        return Duration::from_millis(0);
    }
    Duration::from_millis(deterministic_hash_u64(name.as_bytes()) % bound_ms)
}

fn deterministic_hash_u64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Spawn a file-watch raiser as a tokio task. Emits matching filesystem observations.
///
/// The payload is `{ "path": "<abs path>" }`. Consumer departments own durable
/// idempotence across process restarts.
pub fn spawn_file_watch(
    name: String,
    glob: &str,
    host_root: &Path,
    produces: String,
    router: DeliveryRouter,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let glob = absolutize_glob(glob, host_root)?;
    let handle = tokio::spawn(async move {
        info!(raiser = %name, glob = %glob, "file_watch raiser starting");

        let mut seen = HashMap::new();
        emit_scan(&name, &glob, &produces, &router, &mut seen);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx_for_watcher = tx.clone();
        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                let _ = tx_for_watcher.send(res);
            },
            Config::default(),
        ) {
            Ok(watcher) => Some(watcher),
            Err(e) => {
                warn!(raiser = %name, error = %e, "file_watch notify watcher init failed; using polling only");
                None
            }
        };
        let _tx_guard = tx;

        if let Some(watcher) = watcher.as_mut() {
            let root = watch_root_for_glob(&glob);
            let mode = if glob_has_recursive_wildcard(&glob) {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if let Err(e) = watcher.watch(&root, mode) {
                warn!(
                    raiser = %name,
                    root = %root.display(),
                    error = %e,
                    "file_watch notify watch failed; polling remains active"
                );
            }
        }

        let mut poll = tokio::time::interval(Duration::from_secs(5));
        poll.tick().await;
        loop {
            tokio::select! {
                maybe_event = rx.recv() => {
                    match maybe_event {
                        Some(Ok(event)) => {
                            if is_file_creation_like(&event.kind) {
                                for path in event.paths {
                                    emit_if_match(&name, &glob, &produces, &router, &path, &mut seen);
                                }
                            }
                        }
                        Some(Err(e)) => {
                            warn!(raiser = %name, error = %e, "file_watch notify event error");
                        }
                        None => {
                            emit_scan(&name, &glob, &produces, &router, &mut seen);
                        }
                    }
                }
                _ = poll.tick() => {
                    emit_scan(&name, &glob, &produces, &router, &mut seen);
                }
            }
        }
    });
    Ok(handle)
}

fn is_file_creation_like(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(notify::event::ModifyKind::Name(_))
            | EventKind::Any
            | EventKind::Other
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn emit_scan(
    name: &str,
    glob: &str,
    produces: &str,
    router: &DeliveryRouter,
    seen: &mut HashMap<PathBuf, FileIdentity>,
) {
    for path in existing_matches(glob) {
        emit_changed_path(name, produces, router, path, seen);
    }
}

fn emit_if_match(
    name: &str,
    glob: &str,
    produces: &str,
    router: &DeliveryRouter,
    path: &Path,
    seen: &mut HashMap<PathBuf, FileIdentity>,
) {
    if !path.is_file() {
        return;
    }
    let Ok(path) = path.canonicalize() else {
        return;
    };
    if glob_matches_path(glob, &path) {
        emit_changed_path(name, produces, router, path, seen);
    }
}

fn emit_changed_path(
    name: &str,
    produces: &str,
    router: &DeliveryRouter,
    path: PathBuf,
    seen: &mut HashMap<PathBuf, FileIdentity>,
) {
    let Ok(identity) = file_identity(&path) else {
        return;
    };
    if seen.get(&path) == Some(&identity) {
        return;
    }
    seen.insert(path.clone(), identity);
    emit_abs_path(name, produces, router, path, identity);
}

fn file_identity(path: &Path) -> std::io::Result<FileIdentity> {
    let metadata = path.metadata()?;
    Ok(FileIdentity {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn emit_abs_path(
    name: &str,
    produces: &str,
    router: &DeliveryRouter,
    path: PathBuf,
    identity: FileIdentity,
) {
    let payload = serde_json::json!({"path": path.to_string_lossy()});
    let ev = Event::new(produces, payload);
    if let Err(e) = router.publish(PublishEnvelope {
        event: ev,
        source: Some(SourceRef {
            kind: SourceKind::File,
            reference: file_source_reference(&path, identity),
        }),
        cron_payload: None,
        derived: None,
    }) {
        warn!(raiser = %name, error = %e, "delivery publish failed");
    }
}

fn cron_slot_unix_millis(interval: Duration) -> u64 {
    let interval_ms = interval.as_millis().min(u64::MAX as u128) as u64;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    if interval_ms == 0 {
        now_ms
    } else {
        (now_ms / interval_ms) * interval_ms
    }
}

fn cron_source_reference(name: &str, slot_unix_ms: u64) -> String {
    format!("{name}/slot/{slot_unix_ms}")
}

fn file_source_reference(path: &Path, identity: FileIdentity) -> String {
    let modified_ms = identity
        .modified
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64);
    match modified_ms {
        Some(modified_ms) => format!(
            "{}/len/{}/mtime/{}",
            path.to_string_lossy(),
            identity.len,
            modified_ms
        ),
        None => format!("{}/len/{}", path.to_string_lossy(), identity.len),
    }
}

fn existing_matches(glob: &str) -> Vec<PathBuf> {
    let literal = Path::new(glob);
    if !glob_has_magic(glob) {
        return if literal.is_file() {
            literal.canonicalize().into_iter().collect()
        } else {
            Vec::new()
        };
    }

    let root = scan_root_for_glob(glob);
    let mut matches = Vec::new();
    visit_files(&root, &mut |path| {
        if glob_matches_path(glob, path) {
            matches.push(path.to_path_buf());
        }
    });
    matches
}

fn visit_files(root: &Path, f: &mut impl FnMut(&Path)) {
    if root.is_file() {
        if let Ok(path) = root.canonicalize() {
            f(&path);
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_files(&path, f);
        } else if path.is_file() {
            if let Ok(path) = path.canonicalize() {
                f(&path);
            }
        }
    }
}

fn absolutize_glob(glob: &str, host_root: &Path) -> anyhow::Result<String> {
    let path = Path::new(glob);
    let path = if path.is_absolute() {
        PathBuf::from(glob)
    } else {
        host_root.join(glob)
    };
    let path = path.to_string_lossy().into_owned();
    if !glob_has_magic(&path) {
        return Ok(path);
    }

    let root = scan_root_for_glob(&path);
    let Ok(canonical_root) = root.canonicalize() else {
        return Ok(path);
    };
    let root = root.to_string_lossy();
    let suffix = path.strip_prefix(root.as_ref()).unwrap_or("");
    Ok(format!("{}{}", canonical_root.to_string_lossy(), suffix))
}

fn scan_root_for_glob(glob: &str) -> PathBuf {
    let prefix = literal_prefix_before_magic(glob);
    let path = Path::new(&prefix);
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf()
    }
}

fn watch_root_for_glob(glob: &str) -> PathBuf {
    let mut root = scan_root_for_glob(glob);
    while !root.exists() {
        if !root.pop() {
            return PathBuf::from("/");
        }
    }
    root
}

fn literal_prefix_before_magic(glob: &str) -> String {
    let magic_idx = glob.find(['*', '?', '[']).unwrap_or(glob.len());
    let prefix = &glob[..magic_idx];
    match prefix.rfind(std::path::MAIN_SEPARATOR) {
        Some(0) => std::path::MAIN_SEPARATOR.to_string(),
        Some(idx) => prefix[..idx].to_string(),
        None => ".".to_string(),
    }
}

fn glob_has_magic(glob: &str) -> bool {
    glob.contains('*') || glob.contains('?') || glob.contains('[')
}

fn glob_has_recursive_wildcard(glob: &str) -> bool {
    split_path_segments(glob).contains(&"**")
}

fn glob_matches_path(glob: &str, path: &Path) -> bool {
    let path = path.to_string_lossy();
    let glob = normalize_separators(glob);
    let path = normalize_separators(&path);
    let glob_segments = split_path_segments(&glob);
    let path_segments = split_path_segments(&path);
    glob_segments_match(&glob_segments, &path_segments)
}

fn normalize_separators(s: &str) -> String {
    s.replace('\\', "/")
}

fn split_path_segments(s: &str) -> Vec<&str> {
    s.split('/').collect()
}

fn glob_segments_match(glob: &[&str], path: &[&str]) -> bool {
    if glob.is_empty() {
        return path.is_empty();
    }
    if glob[0] == "**" {
        return glob_segments_match(&glob[1..], path)
            || (!path.is_empty() && glob_segments_match(glob, &path[1..]));
    }
    !path.is_empty()
        && glob_segment_match(glob[0], path[0])
        && glob_segments_match(&glob[1..], &path[1..])
}

fn glob_segment_match(glob: &str, text: &str) -> bool {
    let glob: Vec<char> = glob.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut gi = 0;
    let mut ti = 0;
    let mut star = None;
    let mut star_text = 0;

    while ti < text.len() {
        if gi < glob.len() && (glob[gi] == '?' || glob[gi] == text[ti]) {
            gi += 1;
            ti += 1;
        } else if gi < glob.len() && glob[gi] == '*' {
            star = Some(gi);
            gi += 1;
            star_text = ti;
        } else if let Some(star_idx) = star {
            gi = star_idx + 1;
            star_text += 1;
            ti = star_text;
        } else {
            return false;
        }
    }

    while gi < glob.len() && glob[gi] == '*' {
        gi += 1;
    }

    gi == glob.len()
}

/// Parse `Ns`, `Nm`, or `Nh` into Duration.
pub fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration");
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let n: u64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("not a number: '{}'", num_str))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => anyhow::bail!("unit must be s/m/h, got '{}'", unit),
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::super::event_fanout::Fanout;
    use super::*;
    use fkst_common::config::{Config, DepartmentDecl, LimitsDecl, QueueDecl};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use tokio::time::{timeout, Duration};

    static CURRENT_DIR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct CurrentDirGuard {
        original: PathBuf,
    }

    impl CurrentDirGuard {
        fn enter(path: &Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { original }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

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
                owner_root: std::path::PathBuf::from("."),
                owner_namespace: "pkg".to_string(),
                consumes: vec![queue_name.to_string()],
                produces: Vec::new(),
                ephemeral: vec![queue_name.to_string()],
                stall_window: "30s".to_string(),
                graph_json: false,
                retry: None,
            },
        );
        let cfg = Config {
            package: BTreeMap::new(),
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

    #[test]
    fn relative_file_watch_glob_is_host_root_anchored_when_cwd_differs() {
        let _lock = CURRENT_DIR_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let host = tempfile::tempdir().unwrap();
        let unrelated_cwd = tempfile::tempdir().unwrap();
        let input = host.path().join("input");
        std::fs::create_dir(&input).unwrap();

        let _cwd = CurrentDirGuard::enter(unrelated_cwd.path());
        let resolved = absolutize_glob("input/*.txt", host.path());

        let resolved = resolved.unwrap();
        let expected_prefix = input.canonicalize().unwrap().to_string_lossy().into_owned();
        assert_eq!(resolved, format!("{expected_prefix}/*.txt"));
    }

    #[test]
    fn parses_seconds() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
    }
    #[test]
    fn parses_minutes() {
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
    }
    #[test]
    fn parses_hours() {
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }
    #[test]
    fn rejects_no_unit() {
        assert!(parse_duration("60").is_err());
    }
    #[test]
    fn rejects_bad_unit() {
        assert!(parse_duration("10x").is_err());
    }
    #[test]
    fn rejects_cron_expression_prefix() {
        assert!(parse_duration("*/5m").is_err());
        assert!(parse_duration("*/5 m").is_err());
    }
    #[test]
    fn rejects_empty() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn cron_payload_carries_no_process_local_counter() {
        let payload = cron_payload("tick");
        assert_eq!(payload, serde_json::json!({"raiser": "tick"}));
        assert!(payload.get("n").is_none());
    }

    #[test]
    fn cron_source_reference_uses_logical_slot() {
        assert_eq!(cron_source_reference("tick", 1_000), "tick/slot/1000");
        let slot = cron_slot_unix_millis(Duration::from_secs(60));
        assert_eq!(slot % 60_000, 0);
    }

    #[test]
    fn cron_first_fire_jitter_is_before_interval() {
        let interval = Duration::from_secs(600);
        let jitter = cron_first_fire_jitter("tick", interval);

        assert_eq!(jitter, cron_first_fire_jitter("tick", interval));
        assert_ne!(jitter, cron_first_fire_jitter("other", interval));
        assert!(jitter < Duration::from_secs(30));
        assert!(jitter < interval);
    }

    #[test]
    fn cron_first_fire_jitter_is_bounded_by_short_interval() {
        let interval = Duration::from_secs(5);
        let jitter = cron_first_fire_jitter("tick", interval);

        assert!(jitter < interval);
    }

    #[test]
    fn file_source_reference_includes_stable_change_version() {
        let identity = FileIdentity {
            len: 4,
            modified: Some(UNIX_EPOCH + Duration::from_millis(1_234)),
        };
        let reference = file_source_reference(Path::new("/tmp/input.txt"), identity);

        assert_eq!(reference, "/tmp/input.txt/len/4/mtime/1234");
    }

    #[tokio::test]
    async fn file_watch_existing_file_emits_event() {
        let tmp = tempfile::tempdir().unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
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
}

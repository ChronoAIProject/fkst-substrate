//! Per-department consumer task. Subscribes to validated queues, pops events,
//! hands static codex permit slots to framework, parses RAISED, fans produced events back into
//! the physical Fanout.

use super::event_fanout::Fanout;
use super::raised::parse_raised;
use super::retry_state::{self, CompletionStatus, RetryPolicy, StartDecision, StartDecisionError};
use super::retry_sweep::SWEEP_INTERVAL;
use super::source_runner::parse_duration;
use super::spawner::{spawn_framework, SpawnResult};
use fkst_common::config::DepartmentDecl;
use fkst_common::Event;
use fkst_common::{RuntimeKind, RuntimeLayout};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

#[allow(clippy::too_many_arguments)]
pub async fn spawn_consumer(
    name: String,
    decl: DepartmentDecl,
    project_root: PathBuf,
    package_root: PathBuf,
    framework_binary: PathBuf,
    fanout: Fanout,
    queue_capacity: usize,
    codex_permit_slots: usize,
) -> JoinHandle<()> {
    let mut receivers: Vec<mpsc::Receiver<Event>> = Vec::new();
    for q in &decl.consumes {
        let rx = fanout.subscribe(q, queue_capacity).await;
        receivers.push(rx);
    }
    tokio::spawn(async move {
        let stall_window =
            parse_duration(&decl.stall_window).expect("validation already accepted stall_window");
        let layout = crate::runtime_context::layout_from_host_root(&project_root)
            .expect("runtime layout should be valid");
        let framework_child_log_dir = layout
            .runtime_dir(RuntimeKind::Logs)
            .join("framework-child");
        let retry_policy = decl
            .retry
            .as_ref()
            .map(retry_state::policy_from_decl)
            .transpose()
            .expect("validation already accepted retry");
        let (inbox_tx, mut inbox_rx) = mpsc::channel::<Event>(queue_capacity);
        for mut rx in receivers {
            let tx = inbox_tx.clone();
            let dept_name = name.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if tx.send(ev).await.is_err() {
                        warn!(dept = %dept_name, "consumer inbox closed");
                        return;
                    }
                }
                warn!(dept = %dept_name, "queue receiver disconnected");
            });
        }
        drop(inbox_tx);

        info!(dept = %name, queues = ?decl.consumes, "consumer started");

        while let Some(ev) = inbox_rx.recv().await {
            let lua_full = project_root.join(&decl.lua);
            let event_json = match serde_json::to_string(&ev) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "serialize event");
                    continue;
                }
            };
            let fanout_clone = fanout.clone();
            let dept_name = name.clone();
            let framework_bin = framework_binary.clone();
            let project_root = project_root.clone();
            let package_root = package_root.clone();
            let log_dir = framework_child_log_dir.clone();
            let layout = layout.clone();
            let retry_policy = retry_policy.clone();
            tokio::spawn(async move {
                if let Some(policy) = retry_policy {
                    run_reliable_event(
                        policy,
                        &layout,
                        &fanout_clone,
                        &dept_name,
                        ev,
                        SpawnArgs {
                            framework_bin,
                            lua_full,
                            project_root,
                            package_root,
                            event_json,
                            stall_window,
                            codex_permit_slots,
                            log_dir,
                        },
                    )
                    .await;
                } else {
                    run_untracked_event(
                        &fanout_clone,
                        &dept_name,
                        SpawnArgs {
                            framework_bin,
                            lua_full,
                            project_root,
                            package_root,
                            event_json,
                            stall_window,
                            codex_permit_slots,
                            log_dir,
                        },
                    )
                    .await;
                }
            });
        }
        warn!(dept = %name, "consumer inbox disconnected");
    })
}

struct SpawnArgs {
    framework_bin: PathBuf,
    lua_full: PathBuf,
    project_root: PathBuf,
    package_root: PathBuf,
    event_json: String,
    stall_window: std::time::Duration,
    codex_permit_slots: usize,
    log_dir: PathBuf,
}

async fn run_untracked_event(fanout: &Fanout, dept_name: &str, args: SpawnArgs) {
    match spawn_and_report(dept_name, &args).await {
        Ok(result) => fanout_raised(fanout, &result.stdout),
        Err(err) => {
            error!(
                dept = %dept_name,
                log_dir = %args.log_dir.display(),
                error = %err,
                "framework spawn error"
            );
        }
    }
}

async fn run_reliable_event(
    policy: RetryPolicy,
    layout: &RuntimeLayout,
    fanout: &Fanout,
    dept_name: &str,
    event: Event,
    args: SpawnArgs,
) {
    let lease = retry_lease(args.stall_window);
    let run = match retry_state::start_decision(layout, dept_name, &event, lease) {
        Ok(StartDecision::SkipMarked(key)) => {
            info!(dept = %dept_name, key = %key.as_str(), "reliable_retry decision=skip-marked");
            return;
        }
        Ok(StartDecision::SkipPending(key)) => {
            info!(dept = %dept_name, key = %key.as_str(), "reliable_retry decision=skip-pending");
            return;
        }
        Ok(StartDecision::Run { key, generation }) => Some((key, generation)),
        Ok(StartDecision::RunUntracked) => None,
        Err(StartDecisionError::InvalidDedupKey(err)) => {
            warn!(
                dept = %dept_name,
                queue = %event.queue,
                error = %err,
                "reliable_retry invalid_dedup_key action=run-untracked"
            );
            None
        }
        Err(StartDecisionError::State(err)) => {
            error!(
                dept = %dept_name,
                queue = %event.queue,
                error = %err,
                "reliable_retry state error action=drop"
            );
            return;
        }
    };

    let keeper = run.as_ref().map(|(key, generation)| {
        spawn_lease_keeper(layout.clone(), key.clone(), *generation, lease)
    });
    let result = spawn_and_report(dept_name, &args).await;
    if let Some(keeper) = keeper {
        stop_lease_keeper(keeper).await;
    }
    match (&run, result) {
        (Some((key, generation)), Ok(result)) => {
            fanout_raised(fanout, &result.stdout);
            let status = if result.exit_code == 0 {
                CompletionStatus::Success
            } else {
                CompletionStatus::Failure {
                    error: format!(
                        "exit={} stalled={} stderr={}",
                        result.exit_code, result.stalled, result.stderr
                    ),
                }
            };
            if let Err(err) = retry_state::complete(
                layout,
                fanout,
                &policy,
                dept_name,
                &event,
                key,
                *generation,
                status,
            ) {
                error!(
                    dept = %dept_name,
                    key = %key.as_str(),
                    generation = *generation,
                    error = %err,
                    "reliable_retry completion update failed"
                );
            }
        }
        (None, Ok(result)) => {
            fanout_raised(fanout, &result.stdout);
        }
        (Some((key, generation)), Err(err)) => {
            if let Err(state_err) = retry_state::complete(
                layout,
                fanout,
                &policy,
                dept_name,
                &event,
                key,
                *generation,
                CompletionStatus::Failure {
                    error: format!("spawn error: {err}"),
                },
            ) {
                error!(
                    dept = %dept_name,
                    key = %key.as_str(),
                    generation = *generation,
                    error = %state_err,
                    "reliable_retry spawn-error update failed"
                );
            }
            error!(
                dept = %dept_name,
                log_dir = %args.log_dir.display(),
                error = %err,
                "framework spawn error"
            );
        }
        (None, Err(err)) => {
            error!(
                dept = %dept_name,
                log_dir = %args.log_dir.display(),
                error = %err,
                "framework spawn error"
            );
        }
    }
}

fn retry_lease(stall_window: Duration) -> Duration {
    stall_window
        .saturating_add(SWEEP_INTERVAL)
        .saturating_add(Duration::from_secs(5))
}

fn lease_renew_interval(lease: Duration) -> Duration {
    std::cmp::min(SWEEP_INTERVAL, lease / 2).max(Duration::from_millis(1))
}

fn spawn_lease_keeper(
    layout: RuntimeLayout,
    key: retry_state::ReliableKey,
    generation: u64,
    lease: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let renew_interval = lease_renew_interval(lease);
        loop {
            tokio::time::sleep(renew_interval).await;
            match retry_state::renew_lease(&layout, &key, generation, lease) {
                Ok(true) => {}
                Ok(false) => return,
                Err(err) => {
                    warn!(
                        key = %key.as_str(),
                        generation = generation,
                        error = %err,
                        "reliable_retry lease renewal failed"
                    );
                    return;
                }
            }
        }
    })
}

async fn stop_lease_keeper(keeper: JoinHandle<()>) {
    keeper.abort();
    let _ = keeper.await;
}

async fn spawn_and_report(dept_name: &str, args: &SpawnArgs) -> anyhow::Result<SpawnResult> {
    let result = spawn_framework(
        &args.framework_bin,
        &args.lua_full,
        &args.project_root,
        &args.package_root,
        &args.event_json,
        args.stall_window,
        args.codex_permit_slots,
        dept_name,
        &args.log_dir,
    )
    .await?;

    if result.exit_code != 0 {
        warn!(dept = %dept_name, exit = result.exit_code, stalled = result.stalled,
              stall_window_ms = args.stall_window.as_millis(),
              elapsed_ms = result.elapsed_ms,
              last_output_age_ms = result.last_output_age_ms,
              log_path = ?result.log_path,
              stderr = %result.stderr,
              "framework failed");
    } else {
        info!(dept = %dept_name,
              stall_window_ms = args.stall_window.as_millis(),
              elapsed_ms = result.elapsed_ms,
              last_output_age_ms = result.last_output_age_ms,
              log_path = ?result.log_path,
              "framework ok");
    }
    Ok(result)
}

fn fanout_raised(fanout: &Fanout, stdout: &str) {
    for raised_ev in parse_raised(stdout) {
        let queue = raised_ev.queue.clone();
        let _ = fanout.send(&queue, raised_ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::event_fanout::Fanout;
    use base64::Engine;
    use fkst_common::config::RetryDecl;
    use std::path::PathBuf;
    use std::time::Duration;

    struct RuntimeRootGuard(Option<String>);

    impl RuntimeRootGuard {
        fn set(value: String) -> Self {
            let previous = std::env::var("FKST_RUNTIME_ROOT").ok();
            std::env::set_var("FKST_RUNTIME_ROOT", value);
            Self(previous)
        }
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var("FKST_RUNTIME_ROOT", value),
                None => std::env::remove_var("FKST_RUNTIME_ROOT"),
            }
        }
    }

    fn raised_line(queue: &str, payload: serde_json::Value) -> String {
        let entries = serde_json::json!([{ "queue": queue, "payload": payload }]);
        let json = serde_json::to_string(&entries).unwrap();
        base64::engine::general_purpose::URL_SAFE.encode(json.as_bytes())
    }

    fn executable(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn retry_decl(max_attempts: u64) -> RetryDecl {
        RetryDecl {
            max_attempts,
            base: "1s".to_string(),
            cap: "5s".to_string(),
        }
    }

    async fn wait_for_count(path: &std::path::Path, expected: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let count = std::fs::read_to_string(path)
                .map(|body| body.lines().count())
                .unwrap_or(0);
            if count >= expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "count file did not reach {expected}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_for_path(path: &std::path::Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "path did not appear: {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn consumer_wires_runtime_log_dir_and_handles_multiple_input_queues() {
        // Serialize with every other test that mutates the process-global
        // FKST_RUNTIME_ROOT via the shared crate lock; otherwise parallel test
        // threads stomp each other's runtime root.
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _runtime_guard = RuntimeRootGuard::set(runtime.path().to_string_lossy().into_owned());
        let project = tempfile::tempdir().unwrap();
        let lua_path = project.path().join("departments/test.lua");
        std::fs::create_dir_all(lua_path.parent().unwrap()).unwrap();
        std::fs::write(&lua_path, "-- test department\n").unwrap();
        let framework_dir = tempfile::tempdir().unwrap();
        let framework_path = framework_dir.path().join("fkst-framework");
        std::fs::write(
            &framework_path,
            format!(
                "#!/bin/sh\nprintf 'consumer-stdout\\n'; printf 'consumer-stderr\\n' >&2; printf 'RAISED: {}\\n'\n",
                raised_line("done", serde_json::json!({"status": "complete"}))
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&framework_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fanout = Fanout::new();
        let mut done_rx = fanout.subscribe("done", 8).await;
        let handle = spawn_consumer(
            "custom-dept".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/test.lua"),
                consumes: vec!["input".to_string(), "second-input".to_string()],
                produces: Vec::new(),
                stall_window: "5s".to_string(),
                retry: None,
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path,
            fanout.clone(),
            8,
            20,
        )
        .await;

        fanout
            .send(
                "input",
                Event::new("input", serde_json::json!({"kind": "test"})),
            )
            .unwrap();
        fanout
            .send(
                "second-input",
                Event::new("second-input", serde_json::json!({"kind": "test"})),
            )
            .unwrap();

        let log_dir = runtime.path().join("logs/framework-child");
        let done = tokio::time::timeout(Duration::from_secs(5), done_rx.recv())
            .await
            .expect("framework child did not raise completion event")
            .expect("completion receiver closed before framework child completed");
        assert_eq!(done.queue, "done");
        assert_eq!(done.payload, serde_json::json!({"status": "complete"}));
        let second_done = tokio::time::timeout(Duration::from_secs(5), done_rx.recv())
            .await
            .expect("second input queue did not raise completion event")
            .expect("completion receiver closed before second input completed");
        assert_eq!(second_done.queue, "done");
        assert_eq!(
            second_done.payload,
            serde_json::json!({"status": "complete"})
        );
        let log_path = std::fs::read_dir(&log_dir)
            .unwrap_or_else(|err| panic!("failed reading log dir {}: {err}", log_dir.display()))
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("custom-dept-")
            })
            .expect("framework child log should be written before completion event");
        let body = std::fs::read_to_string(&log_path).unwrap();

        assert!(body.contains("DEPT=custom-dept\n"));
        assert!(body.contains("EXIT=0\n"));
        assert!(body.contains("STALLED=false\n"));
        assert!(body.contains("consumer-stdout"));
        assert!(body.contains("consumer-stderr"));

        handle.abort();
    }

    #[tokio::test]
    async fn reliable_fanout_marker_skip_is_per_department() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _runtime_guard = RuntimeRootGuard::set(runtime.path().to_string_lossy().into_owned());
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("departments/a")).unwrap();
        std::fs::create_dir_all(project.path().join("departments/b")).unwrap();
        std::fs::write(project.path().join("departments/a/main.lua"), "-- a\n").unwrap();
        std::fs::write(project.path().join("departments/b/main.lua"), "-- b\n").unwrap();
        let framework_dir = tempfile::tempdir().unwrap();
        let framework_path = framework_dir.path().join("fkst-framework");
        let counts = project.path().join("counts.log");
        executable(
            &framework_path,
            &format!(
                "#!/bin/sh\ncase \"$2\" in\n*/departments/a/main.lua) echo a >> {}; exit 1 ;;\n*/departments/b/main.lua) echo b >> {}; exit 0 ;;\nesac\nexit 0\n",
                counts.display(),
                counts.display()
            ),
        );
        let fanout = Fanout::new();
        let a = spawn_consumer(
            "a".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/a/main.lua"),
                consumes: vec!["input".to_string()],
                produces: Vec::new(),
                stall_window: "5s".to_string(),
                retry: Some(retry_decl(3)),
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path.clone(),
            fanout.clone(),
            8,
            20,
        )
        .await;
        let b = spawn_consumer(
            "b".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/b/main.lua"),
                consumes: vec!["input".to_string()],
                produces: Vec::new(),
                stall_window: "5s".to_string(),
                retry: Some(retry_decl(3)),
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path,
            fanout.clone(),
            8,
            20,
        )
        .await;

        let event = Event::new(
            "input",
            serde_json::json!({"dedup_key": "owner/repo#pr#4@2026"}),
        );
        fanout.send("input", event.clone()).unwrap();
        wait_for_count(&counts, 2).await;
        let retry_path = runtime.path().join("retry/a/owner/repo-pr-4-2026");
        let mut retry: retry_state::RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(&retry_path).unwrap()).unwrap();
        retry.due_at = 0;
        retry_state::write_json_atomic(&retry_path, &retry).unwrap();
        fanout.send("input", event).unwrap();
        wait_for_count(&counts, 3).await;

        let body = std::fs::read_to_string(&counts).unwrap();
        assert_eq!(body.lines().filter(|line| *line == "a").count(), 2);
        assert_eq!(body.lines().filter(|line| *line == "b").count(), 1);
        assert!(runtime.path().join("marks/b/owner/repo-pr-4-2026").exists());
        assert!(retry_path.exists());

        a.abort();
        b.abort();
    }

    #[tokio::test]
    async fn reliable_failure_writes_retry_attempt_and_due_at() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _runtime_guard = RuntimeRootGuard::set(runtime.path().to_string_lossy().into_owned());
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("departments/a")).unwrap();
        std::fs::write(project.path().join("departments/a/main.lua"), "-- a\n").unwrap();
        let framework_dir = tempfile::tempdir().unwrap();
        let framework_path = framework_dir.path().join("fkst-framework");
        executable(
            &framework_path,
            "#!/bin/sh\nprintf 'temporary failure\\n' >&2\nexit 1\n",
        );
        let fanout = Fanout::new();
        let handle = spawn_consumer(
            "a".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/a/main.lua"),
                consumes: vec!["input".to_string()],
                produces: Vec::new(),
                stall_window: "5s".to_string(),
                retry: Some(retry_decl(3)),
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path,
            fanout.clone(),
            8,
            20,
        )
        .await;

        fanout
            .send(
                "input",
                Event::new("input", serde_json::json!({"dedup_key": "job#1"})),
            )
            .unwrap();
        let retry_path = runtime.path().join("retry/a/job-1");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let retry = loop {
            if retry_path.exists() {
                let retry: retry_state::RetryRecord =
                    serde_json::from_str(&std::fs::read_to_string(&retry_path).unwrap()).unwrap();
                if retry.attempt == 1 {
                    break retry;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "retry file missing completed attempt"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };

        assert_eq!(retry.dept, "a");
        assert_eq!(retry.dedup_key, "a/job-1");
        assert_eq!(retry.attempt, 1);
        assert!(retry.due_at >= retry_state::now_unix_millis());
        assert!(retry.last_error_excerpt.contains("temporary failure"));

        handle.abort();
    }

    #[tokio::test]
    async fn reliable_write_ahead_retry_record_exists_before_spawn_completes() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _runtime_guard = RuntimeRootGuard::set(runtime.path().to_string_lossy().into_owned());
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("departments/a")).unwrap();
        std::fs::write(project.path().join("departments/a/main.lua"), "-- a\n").unwrap();
        let framework_dir = tempfile::tempdir().unwrap();
        let framework_path = framework_dir.path().join("fkst-framework");
        let gate = project.path().join("gate");
        executable(
            &framework_path,
            &format!(
                "#!/bin/sh\nwhile [ ! -f {} ]; do sleep 0.05; done\nexit 0\n",
                gate.display()
            ),
        );
        let fanout = Fanout::new();
        let handle = spawn_consumer(
            "a".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/a/main.lua"),
                consumes: vec!["input".to_string()],
                produces: Vec::new(),
                stall_window: "1s".to_string(),
                retry: Some(retry_decl(3)),
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path,
            fanout.clone(),
            8,
            20,
        )
        .await;

        let before = retry_state::now_unix_millis();
        fanout
            .send(
                "input",
                Event::new("input", serde_json::json!({"dedup_key": "job#1"})),
            )
            .unwrap();
        let retry_path = runtime.path().join("retry/a/job-1");
        wait_for_path(&retry_path).await;
        let retry: retry_state::RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(&retry_path).unwrap()).unwrap();

        assert_eq!(retry.attempt, 0);
        assert_eq!(retry.last_error_excerpt, "");
        assert!(retry.due_at >= before + 36_000);
        assert!(retry.due_at <= before + 38_000);

        std::fs::write(gate, "go\n").unwrap();
        wait_for_path(&runtime.path().join("marks/a/job-1")).await;
        assert!(!retry_path.exists());

        handle.abort();
    }

    #[tokio::test]
    async fn reliable_failure_at_max_writes_dead_and_sends_dead_letter() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _runtime_guard = RuntimeRootGuard::set(runtime.path().to_string_lossy().into_owned());
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("departments/a")).unwrap();
        std::fs::write(project.path().join("departments/a/main.lua"), "-- a\n").unwrap();
        let framework_dir = tempfile::tempdir().unwrap();
        let framework_path = framework_dir.path().join("fkst-framework");
        executable(
            &framework_path,
            "#!/bin/sh\nprintf 'final failure\\n' >&2\nexit 1\n",
        );
        let fanout = Fanout::new();
        let mut dead_rx = fanout.subscribe("dead_letter", 8).await;
        let handle = spawn_consumer(
            "a".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/a/main.lua"),
                consumes: vec!["input".to_string()],
                produces: Vec::new(),
                stall_window: "5s".to_string(),
                retry: Some(retry_decl(1)),
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path,
            fanout.clone(),
            8,
            20,
        )
        .await;

        fanout
            .send(
                "input",
                Event::new("input", serde_json::json!({"dedup_key": "job#1"})),
            )
            .unwrap();
        let dead = tokio::time::timeout(Duration::from_secs(5), dead_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(dead.queue, "dead_letter");
        assert_eq!(dead.payload["dept"], "a");
        assert_eq!(dead.payload["dedup_key"], "a/job-1");
        assert_eq!(dead.payload["original_queue"], "input");
        assert_eq!(dead.payload["attempts"], 1);
        assert!(runtime.path().join("dead/a/job-1").exists());
        assert!(!runtime.path().join("retry/a/job-1").exists());

        handle.abort();
    }

    #[tokio::test]
    async fn reliable_dead_letter_failure_at_max_does_not_re_emit_dead_letter() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _runtime_guard = RuntimeRootGuard::set(runtime.path().to_string_lossy().into_owned());
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("departments/a")).unwrap();
        std::fs::write(project.path().join("departments/a/main.lua"), "-- a\n").unwrap();
        let framework_dir = tempfile::tempdir().unwrap();
        let framework_path = framework_dir.path().join("fkst-framework");
        executable(
            &framework_path,
            "#!/bin/sh\nprintf 'dead fail\\n' >&2\nexit 1\n",
        );
        let fanout = Fanout::new();
        let mut dead_rx = fanout.subscribe("dead_letter", 8).await;
        let handle = spawn_consumer(
            "a".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/a/main.lua"),
                consumes: vec!["dead_letter".to_string()],
                produces: Vec::new(),
                stall_window: "5s".to_string(),
                retry: Some(retry_decl(1)),
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path,
            fanout.clone(),
            8,
            20,
        )
        .await;

        fanout
            .send(
                "dead_letter",
                Event::new("dead_letter", serde_json::json!({"dedup_key": "dead#1"})),
            )
            .unwrap();
        let original = tokio::time::timeout(Duration::from_secs(1), dead_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.queue, "dead_letter");
        assert_eq!(original.payload["dedup_key"], "dead#1");
        wait_for_path(&runtime.path().join("dead/a/dead-1")).await;

        assert!(
            tokio::time::timeout(Duration::from_millis(100), dead_rx.recv())
                .await
                .is_err()
        );

        handle.abort();
    }

    #[tokio::test]
    async fn reliable_missing_dedup_key_runs_once_without_tracking() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _runtime_guard = RuntimeRootGuard::set(runtime.path().to_string_lossy().into_owned());
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("departments/a")).unwrap();
        std::fs::write(project.path().join("departments/a/main.lua"), "-- a\n").unwrap();
        let framework_dir = tempfile::tempdir().unwrap();
        let framework_path = framework_dir.path().join("fkst-framework");
        let counts = project.path().join("counts.log");
        executable(
            &framework_path,
            &format!("#!/bin/sh\necho a >> {}\nexit 0\n", counts.display()),
        );
        let fanout = Fanout::new();
        let handle = spawn_consumer(
            "a".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/a/main.lua"),
                consumes: vec!["input".to_string()],
                produces: Vec::new(),
                stall_window: "5s".to_string(),
                retry: Some(retry_decl(3)),
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path,
            fanout.clone(),
            8,
            20,
        )
        .await;

        fanout
            .send("input", Event::new("input", serde_json::json!({"id": 1})))
            .unwrap();
        wait_for_count(&counts, 1).await;

        assert!(!runtime.path().join("marks").exists());
        assert!(!runtime.path().join("retry").exists());

        handle.abort();
    }

    #[tokio::test]
    async fn reliable_non_string_dedup_key_runs_once_without_tracking() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _runtime_guard = RuntimeRootGuard::set(runtime.path().to_string_lossy().into_owned());
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("departments/a")).unwrap();
        std::fs::write(project.path().join("departments/a/main.lua"), "-- a\n").unwrap();
        let framework_dir = tempfile::tempdir().unwrap();
        let framework_path = framework_dir.path().join("fkst-framework");
        let counts = project.path().join("counts.log");
        executable(
            &framework_path,
            &format!("#!/bin/sh\necho a >> {}\nexit 0\n", counts.display()),
        );
        let fanout = Fanout::new();
        let handle = spawn_consumer(
            "a".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/a/main.lua"),
                consumes: vec!["input".to_string()],
                produces: Vec::new(),
                stall_window: "5s".to_string(),
                retry: Some(retry_decl(3)),
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path,
            fanout.clone(),
            8,
            20,
        )
        .await;

        fanout
            .send(
                "input",
                Event::new("input", serde_json::json!({"dedup_key": 7})),
            )
            .unwrap();
        wait_for_count(&counts, 1).await;

        assert!(!runtime.path().join("marks").exists());
        assert!(!runtime.path().join("retry").exists());

        handle.abort();
    }

    #[tokio::test]
    async fn reliable_swept_expired_lease_can_complete_through_consumer() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _runtime_guard = RuntimeRootGuard::set(runtime.path().to_string_lossy().into_owned());
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("departments/a")).unwrap();
        std::fs::write(project.path().join("departments/a/main.lua"), "-- a\n").unwrap();
        let framework_dir = tempfile::tempdir().unwrap();
        let framework_path = framework_dir.path().join("fkst-framework");
        let gate = project.path().join("gate");
        executable(
            &framework_path,
            &format!(
                "#!/bin/sh\nif [ ! -f {} ]; then sleep 10; fi\nexit 0\n",
                gate.display()
            ),
        );
        let fanout = Fanout::new();
        let handle = spawn_consumer(
            "a".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/a/main.lua"),
                consumes: vec!["input".to_string()],
                produces: Vec::new(),
                stall_window: "1s".to_string(),
                retry: Some(retry_decl(3)),
            },
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            framework_path,
            fanout.clone(),
            8,
            20,
        )
        .await;

        fanout
            .send(
                "input",
                Event::new("input", serde_json::json!({"dedup_key": "job#1"})),
            )
            .unwrap();
        let retry_path = runtime.path().join("retry/a/job-1");
        wait_for_path(&retry_path).await;
        let mut retry: retry_state::RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(&retry_path).unwrap()).unwrap();
        retry.due_at = 0;
        retry_state::write_json_atomic(&retry_path, &retry).unwrap();
        std::fs::write(&gate, "go\n").unwrap();

        assert_eq!(
            crate::supervise::retry_sweep::sweep_once(
                &RuntimeLayout::new(runtime.path()).unwrap(),
                &fanout
            )
            .unwrap(),
            1
        );
        wait_for_path(&runtime.path().join("marks/a/job-1")).await;
        assert!(!retry_path.exists());

        handle.abort();
    }

    #[tokio::test]
    async fn lease_keeper_prevents_sweeper_reinjecting_healthy_long_run() {
        let runtime = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("input", 8).await;
        let event = Event::new("input", serde_json::json!({"dedup_key": "job#1"}));
        let lease = Duration::from_millis(80);
        let StartDecision::Run { key, generation } =
            retry_state::start_decision(&layout, "a", &event, lease).unwrap()
        else {
            panic!("expected run");
        };
        let keeper = spawn_lease_keeper(layout.clone(), key.clone(), generation, lease);

        tokio::time::sleep(Duration::from_millis(180)).await;
        let sent = crate::supervise::retry_sweep::sweep_once(&layout, &fanout).unwrap();

        assert_eq!(sent, 0);
        assert!(tokio::time::timeout(Duration::from_millis(30), rx.recv())
            .await
            .is_err());
        let retry_path = runtime.path().join("retry/a/job-1");
        let retry: retry_state::RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(retry_path).unwrap()).unwrap();
        assert_eq!(retry.generation, generation);
        assert_eq!(retry.attempt, 0);
        assert!(retry.due_at > retry_state::now_unix_millis());

        keeper.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopped_lease_keeper_cannot_overwrite_failure_backoff_due_at() {
        let runtime = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let fanout = Fanout::new();
        let event = Event::new("input", serde_json::json!({"dedup_key": "job#1"}));
        let lease = Duration::from_secs(60);
        let StartDecision::Run { key, generation } =
            retry_state::start_decision(&layout, "a", &event, lease).unwrap()
        else {
            panic!("expected run");
        };
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let keeper_layout = layout.clone();
        let keeper_key = key.clone();
        let keeper = tokio::spawn(async move {
            entered_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            retry_state::renew_lease(&keeper_layout, &keeper_key, generation, lease).unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("keeper did not enter synchronous renewal segment");
        stop_lease_keeper(keeper).await;

        let before_complete = retry_state::now_unix_millis();
        retry_state::complete(
            &layout,
            &fanout,
            &RetryPolicy {
                max_attempts: 3,
                base: Duration::from_secs(1),
                cap: Duration::from_secs(1),
            },
            "a",
            &event,
            &key,
            generation,
            CompletionStatus::Failure {
                error: "child failed".to_string(),
            },
        )
        .unwrap();

        let retry_path = runtime.path().join("retry/a/job-1");
        let retry: retry_state::RetryRecord =
            serde_json::from_str(&std::fs::read_to_string(retry_path).unwrap()).unwrap();
        assert_eq!(retry.attempt, 1);
        assert_eq!(retry.generation, generation);
        assert!(
            retry.due_at >= before_complete + 900,
            "failure backoff was overwritten by lease due_at: {} < {}",
            retry.due_at,
            before_complete + 900
        );
        assert!(
            retry.due_at <= before_complete + 1_500,
            "failure backoff due_at is outside expected window: {}",
            retry.due_at
        );
    }

    #[tokio::test]
    async fn expired_lease_reinjects_after_keeper_stops() {
        let runtime = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(runtime.path()).unwrap();
        let fanout = Fanout::new();
        let mut rx = fanout.subscribe("input", 8).await;
        let event = Event::new("input", serde_json::json!({"dedup_key": "job#1"}));
        let lease = Duration::from_millis(80);
        let StartDecision::Run { key, generation } =
            retry_state::start_decision(&layout, "a", &event, lease).unwrap()
        else {
            panic!("expected run");
        };
        let keeper = spawn_lease_keeper(layout.clone(), key, generation, lease);
        tokio::time::sleep(Duration::from_millis(100)).await;
        keeper.abort();

        tokio::time::sleep(Duration::from_millis(120)).await;
        let sent = crate::supervise::retry_sweep::sweep_once(&layout, &fanout).unwrap();

        assert_eq!(sent, 1);
        let reinjected = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reinjected.queue, "input");
        assert_eq!(reinjected.payload, event.payload);
    }
}

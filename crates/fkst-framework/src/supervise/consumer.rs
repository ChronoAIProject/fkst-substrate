//! Per-department consumer task. Subscribes to validated queues, pops events,
//! hands static codex permit slots to framework, parses RAISED, fans produced events back into
//! the physical Fanout.

use super::event_fanout::Fanout;
use super::raised::parse_raised;
use super::source_runner::parse_duration;
use super::spawner::spawn_framework;
use fkst_common::config::DepartmentDecl;
use fkst_common::Event;
use fkst_common::RuntimeKind;
use std::path::PathBuf;
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
            parse_duration(&decl.timeout).expect("validation already accepted timeout");
        let framework_child_log_dir = crate::runtime_context::layout_from_host_root(&project_root)
            .expect("runtime layout should be valid")
            .runtime_dir(RuntimeKind::Logs)
            .join("framework-child");
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
            tokio::spawn(async move {
                match spawn_framework(
                    &framework_bin,
                    &lua_full,
                    &project_root,
                    &package_root,
                    &event_json,
                    stall_window,
                    codex_permit_slots,
                    &dept_name,
                    &log_dir,
                )
                .await
                {
                    Ok(r) => {
                        if r.exit_code != 0 {
                            warn!(dept = %dept_name, exit = r.exit_code, stalled = r.stalled,
                                  stall_window_ms = stall_window.as_millis(),
                                  elapsed_ms = r.elapsed_ms,
                                  last_output_age_ms = r.last_output_age_ms,
                                  log_path = ?r.log_path,
                                  stderr = %r.stderr,
                                  "framework failed");
                        } else {
                            info!(dept = %dept_name,
                                  stall_window_ms = stall_window.as_millis(),
                                  elapsed_ms = r.elapsed_ms,
                                  last_output_age_ms = r.last_output_age_ms,
                                  log_path = ?r.log_path,
                                  "framework ok");
                        }
                        // Parse RAISED regardless of exit code (partial raise OK).
                        for raised_ev in parse_raised(&r.stdout) {
                            let queue = raised_ev.queue.clone();
                            let _ = fanout_clone.send(&queue, raised_ev);
                        }
                    }
                    Err(e) => {
                        error!(dept = %dept_name, log_dir = %log_dir.display(), error = %e, "framework spawn error");
                    }
                }
            });
        }
        warn!(dept = %name, "consumer inbox disconnected");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::event_fanout::Fanout;
    use base64::Engine;
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

    #[tokio::test]
    async fn consumer_wires_runtime_log_dir_and_handles_multiple_input_queues() {
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
                timeout: "5s".to_string(),
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
}

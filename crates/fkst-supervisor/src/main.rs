//! fkst-supervisor process root.

use anyhow::{Context, Result};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

mod process_tree;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let project_root = std::env::current_dir().context("get cwd")?;
    let framework_bin = framework_binary()?;
    let code = run(&project_root, &framework_bin).await?;
    std::process::exit(code);
}

async fn run(project_root: &Path, framework_bin: &Path) -> Result<i32> {
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut child = Command::new(framework_bin)
        .arg("supervise")
        .arg("--project-root")
        .arg(project_root)
        .arg("--framework-bin")
        .arg(framework_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .with_context(|| format!("spawn {}", framework_bin.display()))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("spawned runtime has no pid"))?;
    info!(pid = pid, framework = %framework_bin.display(), "event runtime spawned");

    let code = loop {
        if let Some(status) = child.try_wait().context("poll event runtime")? {
            let code = status.code().unwrap_or(128);
            info!(pid = pid, exit_code = code, "event runtime exited");
            break code;
        }

        tokio::select! {
            _ = sigint.recv() => {
                warn!(pid = pid, signal = "interrupt", "terminating event runtime process group");
                process_tree::terminate_process_group(&mut child, pid, "event runtime").await;
                break 130;
            }
            _ = sigterm.recv() => {
                warn!(pid = pid, signal = "terminate", "terminating event runtime process group");
                process_tree::terminate_process_group(&mut child, pid, "event runtime").await;
                break 143;
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    };

    reap_children();
    Ok(code)
}

fn framework_binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("FKST_FRAMEWORK_BIN") {
        return Ok(PathBuf::from(path));
    }
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("current executable has no parent"))?
        .to_path_buf();
    let framework_bin = exe_dir.join("fkst-framework");
    if !framework_bin.is_file() {
        anyhow::bail!(
            "fkst-framework not found at {}; build the workspace or set FKST_FRAMEWORK_BIN",
            framework_bin.display()
        );
    }
    Ok(framework_bin)
}

fn reap_children() {
    loop {
        match waitpid(
            Pid::from_raw(-1),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        ) {
            Ok(WaitStatus::StillAlive) => break,
            Ok(_) => {}
            Err(nix::errno::Errno::ECHILD) => break,
            Err(err) => {
                warn!(error = %err, "child reap failed");
                break;
            }
        }
    }
}

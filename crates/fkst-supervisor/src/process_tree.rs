use nix::errno::Errno;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::time::{Duration, Instant};
use tokio::process::Child;
use tracing::warn;

const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

pub async fn terminate_process_group(child: &mut Child, pid: u32, label: &str) {
    let pgid = Pid::from_raw(pid as i32);
    if let Err(err) = killpg(pgid, Signal::SIGTERM) {
        if err != Errno::ESRCH {
            warn!(pid = pid, label = label, error = %err, "process group SIGTERM failed");
        }
    }

    let deadline = Instant::now() + TERMINATION_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                warn!(pid = pid, label = label, status = %status, "process group exited after SIGTERM");
                return;
            }
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Ok(None) => break,
            Err(err) => {
                warn!(pid = pid, label = label, error = %err, "process wait failed during termination");
                break;
            }
        }
    }

    if let Err(err) = killpg(pgid, Signal::SIGKILL) {
        if err != Errno::ESRCH {
            warn!(pid = pid, label = label, error = %err, "process group SIGKILL failed");
        }
    }
    let _ = child.wait().await;
}

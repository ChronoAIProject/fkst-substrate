use nix::{errno::Errno, sys::signal::killpg, sys::signal::Signal, unistd::Pid};
use std::time::{Duration, Instant};
use tokio::process::Child;
use tracing::warn;

const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
pub async fn terminate_process_group(child: &mut Child, pid: u32, label: &str) {
    let pgid = Pid::from_raw(pid as i32);
    send_group_signal(pgid, Signal::SIGTERM, pid, label);

    let deadline = Instant::now() + TERMINATION_GRACE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                warn!(pid = pid, label = label, status = %status, "process group exited after SIGTERM");
                return;
            }
            Ok(None) => tokio::time::sleep(POLL_INTERVAL).await,
            Err(err) => {
                warn!(pid = pid, label = label, error = %err, "process wait failed during termination");
                break;
            }
        }
    }

    send_group_signal(pgid, Signal::SIGKILL, pid, label);
    let _ = child.wait().await;
}
fn send_group_signal(pgid: Pid, signal: Signal, pid: u32, label: &str) {
    match killpg(pgid, signal) {
        Ok(()) => {}
        Err(err) if group_signal_cleanup_tolerated(err) => {
            warn!(pid = pid, label = label, signal = ?signal, "process group signal cleanup tolerated");
        }
        Err(err) => {
            warn!(pid = pid, label = label, signal = ?signal, error = %err, "process group signal cleanup failed");
        }
    }
}
pub(crate) fn group_signal_cleanup_tolerated(err: Errno) -> bool {
    matches!(err, Errno::EPERM | Errno::ESRCH)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn group_signal_cleanup_tolerates_eperm_and_esrch() {
        let tolerated = group_signal_cleanup_tolerated;
        assert!(tolerated(Errno::EPERM) && tolerated(Errno::ESRCH));
    }
}

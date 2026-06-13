use nix::errno::Errno;
use nix::sys::signal::{killpg, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::Pid;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
static SDK_PROCESS_GROUPS: OnceLock<ProcessGroupRegistry> = OnceLock::new();
static SIGNAL_WATCH_INSTALLED: OnceLock<()> = OnceLock::new();
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_SIGNAL: AtomicI32 = AtomicI32::new(0);

#[derive(Clone, Default)]
pub(crate) struct ProcessGroupRegistry {
    pgids: Arc<Mutex<BTreeSet<u32>>>,
}

impl ProcessGroupRegistry {
    pub(crate) fn register(&self, pgid: u32) -> ProcessGroupRegistration {
        self.pgids
            .lock()
            .expect("process group registry poisoned")
            .insert(pgid);
        ProcessGroupRegistration {
            pgid,
            registry: self.pgids.clone(),
        }
    }

    pub(crate) async fn terminate_all(&self, label: &str) {
        let pgids = self.snapshot();
        if pgids.is_empty() {
            return;
        }
        info!(
            groups = pgids.len(),
            label = label,
            "terminating process groups"
        );
        for pgid in &pgids {
            send_group_signal(*pgid, Signal::SIGTERM, label);
        }
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline {
            if pgids.iter().all(|pgid| !process_group_exists(*pgid)) {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        for pgid in pgids {
            if process_group_exists(pgid) {
                send_group_signal(pgid, Signal::SIGKILL, label);
            }
        }
    }

    pub(crate) fn terminate_all_blocking(&self, label: &str) {
        let pgids = self.snapshot();
        if pgids.is_empty() {
            return;
        }
        for pgid in &pgids {
            send_group_signal(*pgid, Signal::SIGTERM, label);
        }
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline {
            if pgids.iter().all(|pgid| !process_group_exists(*pgid)) {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        for pgid in pgids {
            if process_group_exists(pgid) {
                send_group_signal(pgid, Signal::SIGKILL, label);
            }
        }
    }

    fn snapshot(&self) -> Vec<u32> {
        self.pgids
            .lock()
            .expect("process group registry poisoned")
            .iter()
            .copied()
            .collect()
    }
}

pub(crate) struct ProcessGroupRegistration {
    pgid: u32,
    registry: Arc<Mutex<BTreeSet<u32>>>,
}

impl Drop for ProcessGroupRegistration {
    fn drop(&mut self) {
        self.registry
            .lock()
            .expect("process group registry poisoned")
            .remove(&self.pgid);
    }
}

pub(crate) fn process_group_exists(pgid: u32) -> bool {
    match killpg(Pid::from_raw(pgid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

pub(crate) fn current_parent_pid() -> u32 {
    nix::unistd::getppid().as_raw() as u32
}

pub(crate) fn current_pid() -> u32 {
    nix::unistd::getpid().as_raw() as u32
}

pub(crate) fn parent_changed(expected_parent_pid: u32) -> bool {
    current_parent_pid() != expected_parent_pid
}

pub(crate) fn sdk_process_groups() -> &'static ProcessGroupRegistry {
    SDK_PROCESS_GROUPS.get_or_init(ProcessGroupRegistry::default)
}

pub(crate) fn install_sdk_shutdown_watch() {
    SIGNAL_WATCH_INSTALLED.get_or_init(|| {
        install_signal_handler(Signal::SIGTERM);
        install_signal_handler(Signal::SIGINT);
        std::thread::spawn(|| loop {
            if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                sdk_process_groups().terminate_all_blocking("sdk child");
                let signal = SHUTDOWN_SIGNAL.load(Ordering::SeqCst);
                std::process::exit(128 + signal);
            }
            std::thread::sleep(POLL_INTERVAL);
        });
    });
}

#[cfg(not(test))]
pub(crate) fn ensure_supervisor_parent_alive() -> mlua::Result<()> {
    #[cfg(test)]
    {
        Ok(())
    }
    #[cfg(not(test))]
    {
        let Some(expected_parent_pid) = std::env::var("FKST_SUPERVISOR_PID")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            return Ok(());
        };
        if parent_changed(expected_parent_pid) {
            return Err(mlua::Error::external(format!(
                "supervisor parent lost: expected parent pid {expected_parent_pid}, current parent pid {}",
                current_parent_pid()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn ensure_supervisor_parent_alive() -> mlua::Result<()> {
    Ok(())
}

fn install_signal_handler(signal: Signal) {
    let action = SigAction::new(
        SigHandler::Handler(record_shutdown_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        if let Err(err) = nix::sys::signal::sigaction(signal, &action) {
            warn!(signal = ?signal, error = %err, "signal handler install failed");
        }
    }
}

extern "C" fn record_shutdown_signal(signal: i32) {
    SHUTDOWN_SIGNAL.store(signal, Ordering::SeqCst);
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

fn send_group_signal(pgid: u32, signal: Signal, label: &str) {
    if let Err(err) = killpg(Pid::from_raw(pgid as i32), signal) {
        if err != Errno::ESRCH {
            warn!(pgid = pgid, label = label, signal = ?signal, error = %err, "process group signal failed");
        }
    }
}

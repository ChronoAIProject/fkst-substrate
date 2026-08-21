use nix::errno::Errno;
use nix::sys::signal::{killpg, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::Pid;
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
pub(crate) const SUPERVISOR_PID_ENV: &str = "FKST_SUPERVISOR_PID";
static SDK_PROCESS_GROUPS: OnceLock<ProcessGroupRegistry> = OnceLock::new();
static SIGNAL_WATCH_INSTALLED: OnceLock<()> = OnceLock::new();
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_SIGNAL: AtomicI32 = AtomicI32::new(0);

#[derive(Clone, Default)]
pub(crate) struct ProcessGroupRegistry {
    state: Arc<Mutex<ProcessGroupState>>,
}

#[derive(Default)]
struct ProcessGroupState {
    pgids: BTreeSet<u32>,
    spawns_in_progress: usize,
    terminating: bool,
}

impl ProcessGroupRegistry {
    pub(crate) fn begin_spawn(&self) -> Option<ProcessGroupSpawnGuard> {
        let mut state = self.state.lock().expect("process group registry poisoned");
        if state.terminating {
            return None;
        }
        state.spawns_in_progress += 1;
        Some(ProcessGroupSpawnGuard {
            state: self.state.clone(),
            active: true,
        })
    }

    fn begin_termination(&self) -> Vec<u32> {
        let mut state = self.state.lock().expect("process group registry poisoned");
        state.terminating = true;
        state.pgids.iter().copied().collect()
    }

    fn is_idle(&self) -> bool {
        let state = self.state.lock().expect("process group registry poisoned");
        state.pgids.is_empty() && state.spawns_in_progress == 0
    }

    pub(crate) async fn terminate_all(&self, label: &str) {
        let pgids = self.begin_termination();
        if pgids.is_empty() && self.is_idle() {
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
            if self.is_idle() {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        for pgid in self.snapshot() {
            if process_group_exists(pgid) {
                send_group_signal(pgid, Signal::SIGKILL, label);
            }
        }
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline {
            if self.is_idle() {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        self.warn_survivors(label);
    }

    pub(crate) fn terminate_all_blocking(&self, label: &str) {
        let pgids = self.begin_termination();
        if pgids.is_empty() && self.is_idle() {
            return;
        }
        for pgid in &pgids {
            send_group_signal(*pgid, Signal::SIGTERM, label);
        }
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline {
            if self.is_idle() {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        for pgid in self.snapshot() {
            if process_group_exists(pgid) {
                send_group_signal(pgid, Signal::SIGKILL, label);
            }
        }
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline {
            if self.is_idle() {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        self.warn_survivors(label);
    }

    fn snapshot(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("process group registry poisoned")
            .pgids
            .iter()
            .copied()
            .collect()
    }

    fn warn_survivors(&self, label: &str) {
        let state = self.state.lock().expect("process group registry poisoned");
        for pgid in &state.pgids {
            if process_group_exists(*pgid) {
                warn!(
                    pgid = pgid,
                    label = label,
                    "process group survived SIGKILL grace"
                );
            }
        }
        if state.spawns_in_progress > 0 {
            warn!(
                spawns = state.spawns_in_progress,
                label = label,
                "process group spawns did not finish during termination grace"
            );
        }
    }
}

pub(crate) struct ProcessGroupSpawnGuard {
    state: Arc<Mutex<ProcessGroupState>>,
    active: bool,
}

impl ProcessGroupSpawnGuard {
    pub(crate) fn register(mut self, pgid: u32) -> ProcessGroupRegistration {
        let terminating = {
            let mut state = self.state.lock().expect("process group registry poisoned");
            state.spawns_in_progress = state
                .spawns_in_progress
                .checked_sub(1)
                .expect("process group spawn guard underflow");
            state.pgids.insert(pgid);
            state.terminating
        };
        self.active = false;
        if terminating {
            send_group_signal(pgid, Signal::SIGKILL, "process spawned during shutdown");
        }
        ProcessGroupRegistration {
            pgid,
            state: self.state.clone(),
        }
    }
}

impl Drop for ProcessGroupSpawnGuard {
    fn drop(&mut self) {
        if self.active {
            let mut state = self.state.lock().expect("process group registry poisoned");
            state.spawns_in_progress = state
                .spawns_in_progress
                .checked_sub(1)
                .expect("process group spawn guard underflow");
        }
    }
}

pub(crate) struct ProcessGroupRegistration {
    pgid: u32,
    state: Arc<Mutex<ProcessGroupState>>,
}

impl Drop for ProcessGroupRegistration {
    fn drop(&mut self) {
        self.state
            .lock()
            .expect("process group registry poisoned")
            .pgids
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
        let expected_parent_pid = supervisor_parent_pid();
        std::thread::spawn(move || loop {
            if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                sdk_process_groups().terminate_all_blocking("sdk child");
                let signal = SHUTDOWN_SIGNAL.load(Ordering::SeqCst);
                std::process::exit(128 + signal);
            }
            if let Some(expected_parent_pid) =
                expected_parent_pid.filter(|pid| parent_changed(*pid))
            {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[framework] process owner parent lost: expected parent pid {}, current parent pid {}",
                    expected_parent_pid,
                    current_parent_pid()
                );
                sdk_process_groups().terminate_all_blocking("sdk child after parent loss");
                std::process::exit(125);
            }
            std::thread::sleep(POLL_INTERVAL);
        });
    });
}

fn supervisor_parent_pid() -> Option<u32> {
    std::env::var(SUPERVISOR_PID_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
}

#[cfg(not(test))]
pub(crate) fn ensure_supervisor_parent_alive() -> mlua::Result<()> {
    let Some(expected_parent_pid) = supervisor_parent_pid() else {
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

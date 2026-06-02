use nix::errno::Errno;
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;
use std::fs;
use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const RUNTIME_ROOT_ENV: &str = "FKST_RUNTIME_ROOT";
const PACKAGE_ROOT_ENV: &str = "FKST_PACKAGE_ROOT";
const WITNESS_TIMEOUT: Duration = Duration::from_secs(60);
const LOG_TAIL_BYTES: usize = 12 * 1024;

fn supervisor_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fkst-supervisor"))
}

fn framework_bin() -> PathBuf {
    let supervisor = supervisor_bin();
    let Some(target_dir) = supervisor.parent() else {
        panic!("supervisor binary has no parent: {}", supervisor.display());
    };
    let framework = target_dir.join("fkst-framework");
    assert!(
        framework.is_file(),
        "fkst-framework binary missing at {}; run cargo build --workspace before this test",
        framework.display()
    );
    framework
}

fn write_release_probe(root: &Path) {
    fs::create_dir_all(root.join("departments/release_probe")).unwrap();
    fs::write(
        root.join("departments/release_probe/main.lua"),
        r#"
local M = {}

M.spec = {
    consumes = { "release_probe" },
    produces = { "release_probe_done" },
    timeout = "5s",
}

function pipeline(event)
    raise("release_probe_done", {
        marker = "release-readiness-raised",
        source_path = event.payload.path,
    })
end

_G.pipeline = pipeline
return M
"#,
    )
    .unwrap();
}

fn write_release_sink(root: &Path, witness: &Path) {
    fs::create_dir_all(root.join("departments/release_sink")).unwrap();
    fs::write(
        root.join("departments/release_sink/main.lua"),
        format!(
            r#"
local M = {{}}

M.spec = {{
    consumes = {{ "release_probe_done" }},
    timeout = "5s",
}}

function pipeline(event)
    local payload = event.payload or {{}}
    assert(payload.marker == "release-readiness-raised", "unexpected marker")
    assert(type(payload.source_path) == "string", "missing source path")
    file.write({:?}, "marker=" .. payload.marker .. "\nsource_path=" .. payload.source_path .. "\nqueue=" .. tostring(event.queue) .. "\n")
end

_G.pipeline = pipeline
return M
"#,
            witness.to_string_lossy()
        ),
    )
    .unwrap();
}

fn write_release_raiser(root: &Path) {
    fs::create_dir_all(root.join("raisers")).unwrap();
    fs::write(
        root.join("raisers/release_probe.lua"),
        r#"return { type = "file_watch", glob = "runtime://pipeline/release_probe/*.md", produces = "release_probe" }"#,
    )
    .unwrap();
}

fn write_host_defaults(root: &Path) {
    fs::create_dir_all(root.join("tunables")).unwrap();
    fs::write(root.join("tunables/queue_capacity.txt"), "100\n").unwrap();
    fs::write(
        root.join("tunables/department_default_timeout.txt"),
        "30m\n",
    )
    .unwrap();
    fs::write(root.join("tunables/codex_permit_slots.txt"), "20\n").unwrap();
}

fn wait_for_durable_fact(
    deadline: Instant,
    child: &mut Child,
    supervisor_log: &Path,
    child_log_dir: &Path,
    label: &str,
    fact: impl Fn() -> Option<String>,
) -> String {
    loop {
        if let Some(value) = fact() {
            return value;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "supervisor exited before {label} appeared: {status}\n{}",
                diagnostic_tail(supervisor_log, child_log_dir)
            );
        }
        assert!(
            Instant::now() < deadline,
            "{}",
            timeout_message(label, supervisor_log, child_log_dir)
        );
        std::thread::yield_now();
    }
}

fn wait_for_witness(
    deadline: Instant,
    child: &mut Child,
    supervisor_log: &Path,
    child_log_dir: &Path,
    witness: &Path,
) -> String {
    wait_for_durable_fact(
        deadline,
        child,
        supervisor_log,
        child_log_dir,
        "witness content",
        || {
            let content = fs::read_to_string(witness).ok()?;
            if content.is_empty() {
                return None;
            }
            Some(content)
        },
    )
}

fn wait_for_child_log_with_raised(
    deadline: Instant,
    child: &mut Child,
    supervisor_log: &Path,
    child_log_dir: &Path,
) -> String {
    wait_for_durable_fact(
        deadline,
        child,
        supervisor_log,
        child_log_dir,
        "RAISED child log",
        || {
            let entries = fs::read_dir(child_log_dir).ok()?;
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };
                if content.contains("RAISED: ") {
                    return Some(content);
                }
            }
            None
        },
    )
}

fn timeout_message(label: &str, supervisor_log: &Path, child_log_dir: &Path) -> String {
    format!(
        "timed out waiting for {label}\n{}",
        diagnostic_tail(supervisor_log, child_log_dir)
    )
}

fn diagnostic_tail(supervisor_log: &Path, child_log_dir: &Path) -> String {
    format!(
        "supervisor_log:\n{}\nchild_log:\n{}",
        tail_file(supervisor_log),
        tail_child_logs(child_log_dir)
    )
}

fn tail_file(path: &Path) -> String {
    let Ok(content) = fs::read_to_string(path) else {
        return format!("missing {}", path.display());
    };
    tail_text(&content, LOG_TAIL_BYTES)
}

fn tail_child_logs(child_log_dir: &Path) -> String {
    let Ok(entries) = fs::read_dir(child_log_dir) else {
        return format!("missing {}", child_log_dir.display());
    };
    let mut logs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            logs.push(format!("== {} ==\n{}", path.display(), tail_file(&path)));
        }
    }
    if logs.is_empty() {
        format!("empty {}", child_log_dir.display())
    } else {
        logs.join("\n")
    }
}

fn tail_text(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }
    let mut start = content.len() - max_bytes;
    while !content.is_char_boundary(start) {
        start += 1;
    }
    content[start..].to_string()
}

// refactor helper, no behavior change
struct SupervisorHarness {
    _tmp: TempDir,
    child: Child,
    pgid: Pid,
    root: PathBuf,
    framework_pid_file: PathBuf,
}

impl SupervisorHarness {
    fn spawn(
        tmp: TempDir,
        runtime_root: &Path,
        supervisor_log: &Path,
        framework_wrapper: &Path,
        framework_pid_file: &Path,
    ) -> Self {
        let root = tmp.path().to_path_buf();
        let child = Command::new(supervisor_bin())
            .current_dir(&root)
            .env("FKST_FRAMEWORK_BIN", framework_wrapper)
            .env("FKST_TEST_REAL_FRAMEWORK", framework_bin())
            .env("FKST_TEST_FRAMEWORK_PID_FILE", framework_pid_file)
            .env(RUNTIME_ROOT_ENV, runtime_root)
            .env(PACKAGE_ROOT_ENV, &root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(File::create(supervisor_log).unwrap()))
            .stderr(Stdio::from(
                File::options().append(true).open(supervisor_log).unwrap(),
            ))
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = Pid::from_raw(child.id() as i32);
        Self {
            _tmp: tmp,
            child,
            pgid,
            root,
            framework_pid_file: framework_pid_file.to_path_buf(),
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for SupervisorHarness {
    fn drop(&mut self) {
        kill_framework_process_groups(&self.framework_pid_file);
        kill_process_groups_for_root(&self.root);
        let _ = killpg(self.pgid, Signal::SIGKILL);
        let _ = self.child.wait();
        wait_for_no_framework_processes(
            &self.root,
            &self.framework_pid_file,
            Instant::now() + Duration::from_secs(3),
        );
    }
}

fn write_framework_wrapper(root: &Path) -> (PathBuf, PathBuf) {
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let wrapper = bin_dir.join("fkst-framework-wrapper");
    let pid_file = root.join(".fkst/runtime/framework-pids.txt");
    fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' \"$$\" >> \"$FKST_TEST_FRAMEWORK_PID_FILE\"\nexec \"$FKST_TEST_REAL_FRAMEWORK\" \"$@\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();
    (wrapper, pid_file)
}

fn kill_process_groups_for_root(root: &Path) {
    let mut pgids = process_groups_for_root(root);
    pgids.sort_by_key(|pid| pid.as_raw());
    pgids.dedup_by_key(|pid| pid.as_raw());
    for pgid in pgids {
        let _ = killpg(pgid, Signal::SIGKILL);
    }
}

fn kill_framework_process_groups(pid_file: &Path) {
    let mut pgids = process_groups_from_pid_file(pid_file);
    pgids.sort_by_key(|pid| pid.as_raw());
    pgids.dedup_by_key(|pid| pid.as_raw());
    for pgid in pgids {
        let _ = killpg(pgid, Signal::SIGKILL);
    }
}

fn process_groups_from_pid_file(pid_file: &Path) -> Vec<Pid> {
    let Ok(content) = fs::read_to_string(pid_file) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .map(Pid::from_raw)
        .collect()
}

fn process_groups_for_root(root: &Path) -> Vec<Pid> {
    let root = root.to_string_lossy();
    let Ok(output) = Command::new("ps")
        .args(["-axo", "pid=,pgid=,command="])
        .output()
    else {
        return Vec::new();
    };
    let listing = String::from_utf8_lossy(&output.stdout);
    listing
        .lines()
        .filter_map(|line| process_group_from_ps_line(line, &root))
        .collect()
}

fn process_group_from_ps_line(line: &str, root: &str) -> Option<Pid> {
    if !(line.contains(root) && line.contains("fkst-")) {
        return None;
    }
    let mut fields = line.split_whitespace();
    let _pid = fields.next()?;
    let pgid = fields.next()?.parse::<i32>().ok()?;
    Some(Pid::from_raw(pgid))
}

fn live_framework_pids(pid_file: &Path) -> Vec<Pid> {
    process_groups_from_pid_file(pid_file)
        .into_iter()
        .filter(|pid| match kill(*pid, None) {
            Ok(()) => true,
            Err(Errno::ESRCH) => false,
            Err(_) => true,
        })
        .collect()
}

fn wait_for_no_framework_processes(root: &Path, pid_file: &Path, deadline: Instant) {
    loop {
        let root_pgids = process_groups_for_root(root);
        let live_pids = live_framework_pids(pid_file);
        if root_pgids.is_empty() && live_pids.is_empty() {
            return;
        }
        kill_framework_process_groups(pid_file);
        kill_process_groups_for_root(root);
        assert!(
            Instant::now() < deadline,
            "supervisor harness left processes behind for {}: root_pgids={:?} live_pids={:?}",
            root.display(),
            root_pgids,
            live_pids
        );
        std::thread::yield_now();
    }
}

#[test]
fn supervisor_framework_completes_release_probe_raised_cycle() {
    let scratch_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/fkst-test-runtime");
    fs::create_dir_all(&scratch_root).unwrap();
    let scratch_root = scratch_root.canonicalize().unwrap();
    let tmp = tempfile::Builder::new()
        .prefix("self-host-witness-")
        .tempdir_in(scratch_root)
        .unwrap();
    let root = tmp.path().to_path_buf();
    let runtime_root = root.join(".fkst/runtime");
    let probe_dir = runtime_root.join("pipeline/release_probe");
    let witness = runtime_root.join("pipeline/release_sink/witness.txt");
    let supervisor_log = runtime_root.join("logs/supervisor.log");
    let child_log_dir = runtime_root.join("logs/framework-child");

    fs::create_dir_all(&probe_dir).unwrap();
    fs::create_dir_all(witness.parent().unwrap()).unwrap();
    fs::create_dir_all(supervisor_log.parent().unwrap()).unwrap();
    let (framework_wrapper, framework_pid_file) = write_framework_wrapper(&root);
    write_host_defaults(&root);
    write_release_probe(&root);
    write_release_sink(&root, &witness);
    write_release_raiser(&root);

    let probe_file = probe_dir.join("probe.md");
    fs::write(&probe_file, "# release probe\n").unwrap();

    {
        let mut supervisor = SupervisorHarness::spawn(
            tmp,
            &runtime_root,
            &supervisor_log,
            &framework_wrapper,
            &framework_pid_file,
        );

        let deadline = Instant::now() + WITNESS_TIMEOUT;
        let witness_content = wait_for_witness(
            deadline,
            supervisor.child_mut(),
            &supervisor_log,
            &child_log_dir,
            &witness,
        );
        let child_log = wait_for_child_log_with_raised(
            deadline,
            supervisor.child_mut(),
            &supervisor_log,
            &child_log_dir,
        );

        assert!(
            witness_content.contains("marker=release-readiness-raised"),
            "witness={witness_content}"
        );
        assert!(
            witness_content.contains(&format!("source_path={}", probe_file.display())),
            "witness={witness_content}"
        );
        assert!(
            witness_content.contains("queue=release_probe_done"),
            "witness={witness_content}"
        );
        assert!(
            child_log.contains("DEPT=release_probe\n"),
            "log={child_log}"
        );
        assert!(child_log.contains("RAISED: "), "log={child_log}");
        assert!(child_log.contains("EXIT=0\n"), "log={child_log}");
    }

    wait_for_no_framework_processes(
        &root,
        &framework_pid_file,
        Instant::now() + Duration::from_secs(3),
    );
    assert!(
        !root.exists(),
        "supervisor harness left temp dir behind: {}",
        root.display()
    );
}

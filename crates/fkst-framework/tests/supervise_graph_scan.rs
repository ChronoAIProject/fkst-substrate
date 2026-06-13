// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/config_registry.rs"]
mod config_registry;
#[path = "../src/supervise/graph_scan.rs"]
mod graph_scan;
#[path = "../src/path_resolver.rs"]
mod path_resolver;
#[path = "../src/rate_pool.rs"]
mod rate_pool;
#[path = "../src/sdk_i18n.rs"]
mod sdk_i18n;
#[path = "../src/sdk_log.rs"]
mod sdk_log;
#[path = "../src/sdk_strings.rs"]
mod sdk_strings;

use fkst_common::config::RaiserDecl;
use fkst_common::validation::validate;
use graph_scan::load as graph_load;
use path_resolver::PackageRoots;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

const RUNTIME_ROOT_ENV: &str = "FKST_RUNTIME_ROOT";
const QUEUE_CAPACITY_ENV: &str = "FKST_QUEUE_CAPACITY";
const DEPARTMENT_DEFAULT_STALL_WINDOW_ENV: &str = "FKST_DEPARTMENT_DEFAULT_STALL_WINDOW";
const CODEX_PERMIT_SLOTS_ENV: &str = "FKST_CODEX_PERMIT_SLOTS";
const DEPARTMENT_CHILD_PROCESS_LIMIT_ENV: &str = "FKST_DEPARTMENT_CHILD_PROCESS_LIMIT";
const DEPARTMENT_CHILD_PROCESS_LIMIT_PER_DEPT_ENV: &str =
    "FKST_DEPARTMENT_CHILD_PROCESS_LIMIT_PER_DEPT";
const RETRY_DEFAULT_MAX_ATTEMPTS_ENV: &str = "FKST_RETRY_DEFAULT_MAX_ATTEMPTS";
const RETRY_DEFAULT_BASE_ENV: &str = "FKST_RETRY_DEFAULT_BASE";
const RETRY_DEFAULT_CAP_ENV: &str = "FKST_RETRY_DEFAULT_CAP";
const PACKAGE_ROOT_ENV: &str = "FKST_PACKAGE_ROOT";
const PACKAGE_ROOTS_ENV: &str = "FKST_PACKAGE_ROOTS";

static CURRENT_DIR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
thread_local! {
    static ENV_LOCK_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct EnvLockGuard {
    _guard: Option<MutexGuard<'static, ()>>,
}

impl Drop for EnvLockGuard {
    fn drop(&mut self) {
        ENV_LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

fn env_lock() -> EnvLockGuard {
    let already_held = ENV_LOCK_DEPTH.with(|depth| depth.get() > 0);
    if already_held {
        ENV_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        return EnvLockGuard { _guard: None };
    }
    let guard = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    ENV_LOCK_DEPTH.with(|depth| depth.set(1));
    EnvLockGuard {
        _guard: Some(guard),
    }
}

struct EnvGuard {
    key: &'static str,
    old: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let old = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, old }
    }

    fn unset(key: &'static str) -> Self {
        let old = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, old }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(old) = self.old.take() {
            std::env::set_var(self.key, old);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn load(path: &std::path::Path) -> anyhow::Result<fkst_common::config::Config> {
    let _env_lock = env_lock();
    let _root = if std::env::var_os(RUNTIME_ROOT_ENV).is_none() {
        Some(EnvGuard::set(RUNTIME_ROOT_ENV, ".fkst/runtime"))
    } else {
        None
    };
    graph_load(path)
}

fn write_host_defaults(root: &std::path::Path, queue: &str, stall_window: &str, slots: &str) {
    fs::write(
        root.join("fkst.env"),
        format!(
            "FKST_QUEUE_CAPACITY={}FKST_DEPARTMENT_DEFAULT_STALL_WINDOW={}FKST_CODEX_PERMIT_SLOTS={}",
            queue, stall_window, slots
        ),
    )
    .unwrap();
}

fn write_repo(depts: &[(&str, &str)], raisers: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(dir.path(), "100\n", "30m\n", "20\n");
    let depts_root = dir.path().join("departments");
    for (name, content) in depts {
        let d = depts_root.join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("main.lua"), content).unwrap();
    }
    let raisers_root = dir.path().join("raisers");
    for (name, content) in raisers {
        fs::create_dir_all(&raisers_root).unwrap();
        fs::write(raisers_root.join(format!("{}.lua", name)), content).unwrap();
    }
    dir
}

fn ns(root: &std::path::Path) -> String {
    root.canonicalize()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn q(root: &std::path::Path, queue: &str) -> String {
    format!("{}.{}", ns(root), queue)
}

fn dept(consumes: &str, produces: &str) -> String {
    format!(
        r#"
local M = {{}}
M.spec = {{ consumes = {{{}}}, produces = {{{}}}, stall_window = "30s" }}
function pipeline(_) end
return M
"#,
        consumes, produces
    )
}

fn dept_with_fanout(consumes: &str, produces: &str, fanout: &str) -> String {
    format!(
        r#"
local M = {{}}
M.spec = {{ consumes = {{{}}}, produces = {{{}}}, fanout = {{{}}}, stall_window = "30s" }}
function pipeline(_) end
return M
"#,
        consumes, produces, fanout
    )
}

fn dept_with_retry(consumes: &str, retry: &str) -> String {
    format!(
        r#"
local M = {{}}
M.spec = {{ consumes = {{{}}}, stall_window = "30s", retry = {{{}}} }}
function pipeline(_) end
return M
"#,
        consumes, retry
    )
}

fn dept_with_raw_retry(consumes: &str, retry: &str) -> String {
    format!(
        r#"
local M = {{}}
M.spec = {{ consumes = {{{}}}, stall_window = "30s", retry = {} }}
function pipeline(_) end
return M
"#,
        consumes, retry
    )
}

fn write_package_helper(root: &std::path::Path) {
    fs::create_dir_all(root.join("fkst")).unwrap();
    fs::write(
        root.join("fkst/spec_helper.lua"),
        r#"return { stall_window = function() return "45s" end }"#,
    )
    .unwrap();
}

#[test]
fn scans_minimal_repo() {
    let dir = write_repo(
        &[(
            "hello",
            r#"
local M = {}
M.spec = { consumes = {"tick"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );
    let cfg = load(dir.path()).unwrap();
    assert_eq!(cfg.queue.get("tick").unwrap().capacity, 100);
    assert!(!cfg.queue.get("tick").unwrap().fanout);
    assert_eq!(cfg.limits.global_codex_processes, 20);
    assert_eq!(cfg.department.get("hello").unwrap().consumes, vec!["tick"]);
    let retry = cfg.department.get("hello").unwrap().retry.as_ref().unwrap();
    assert_eq!(retry.max_attempts, 5);
    assert_eq!(retry.base, "60s");
    assert_eq!(retry.cap, "30m");
    assert_eq!(
        cfg.department.get("hello").unwrap().lua,
        PathBuf::from("departments/hello/main.lua")
    );
    match cfg.raiser.get("cron_a").unwrap() {
        RaiserDecl::Cron {
            interval, produces, ..
        } => {
            assert_eq!(interval, "10s");
            assert_eq!(produces, "tick");
        }
        _ => panic!("expected Cron"),
    }
}

#[test]
fn graph_scan_accepts_engine_failure_fact_queue() {
    let dir = write_repo(
        &[(
            "triage",
            r#"
local M = {}
M.spec = { consumes = {"fkst.failure_fact"}, ephemeral = {"fkst.failure_fact"}, stall_window = "30s", retry = false }
function pipeline(_) end
return M
"#,
        )],
        &[],
    );

    let cfg = load(dir.path()).unwrap();

    assert!(cfg.queue.contains_key("fkst.failure_fact"));
    assert_eq!(
        cfg.department.get("triage").unwrap().consumes,
        vec!["fkst.failure_fact"]
    );
}

#[test]
fn graph_scan_spec_eval_exposes_truncate_utf8() {
    let dir = write_repo(
        &[(
            "hello",
            r#"
local M = {}
local consumed = truncate_utf8("tick-long", 4)
M.spec = { consumes = {consumed}, produces = {"done"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = truncate_utf8("tick-long", 4) }"#,
        )],
    );

    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.department.get("hello").unwrap().consumes, vec!["tick"]);
    match cfg.raiser.get("cron_a").unwrap() {
        RaiserDecl::Cron { produces, .. } => assert_eq!(produces, "tick"),
        _ => panic!("expected Cron"),
    }
}

#[test]
fn graph_scan_rejects_department_producing_engine_failure_fact_queue() {
    let dir = write_repo(
        &[(
            "forger",
            r#"
local M = {}
M.spec = { produces = {"fkst.failure_fact"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[],
    );

    let err = load(dir.path()).unwrap_err();

    assert!(
        err.to_string().contains("resolve `forger.spec.produces`"),
        "got: {err:#}"
    );
}

#[test]
fn graph_scan_spec_eval_does_not_expose_runtime_primitives() {
    let dir = write_repo(
        &[(
            "hello",
            r#"
local M = {}
assert(exec_sync == nil, "exec_sync graph-scan global must not be injected")
local _ = exec_sync("echo no")
M.spec = { consumes = {"tick"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let err = load(dir.path()).unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("exec_sync"), "got: {msg}");
    assert!(msg.contains("eval department `hello`"), "got: {msg}");
}

#[test]
fn scans_department_retry_with_defaults() {
    let dir = write_repo(
        &[(
            "hello",
            &dept_with_retry(r#""tick""#, r#"max_attempts = 3"#),
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let cfg = load(dir.path()).unwrap();
    let retry = cfg.department.get("hello").unwrap().retry.as_ref().unwrap();

    assert_eq!(retry.max_attempts, 3);
    assert_eq!(retry.base, "60s");
    assert_eq!(retry.cap, "30m");
}

#[test]
fn department_retry_false_disables_default_tracking() {
    let dir = write_repo(
        &[("hello", &dept_with_raw_retry(r#""tick""#, "false"))],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.department.get("hello").unwrap().retry, None);
}

#[test]
fn department_retry_table_merges_global_defaults() {
    let _env_lock = env_lock();
    let _attempts = EnvGuard::set(RETRY_DEFAULT_MAX_ATTEMPTS_ENV, "7");
    let _base = EnvGuard::set(RETRY_DEFAULT_BASE_ENV, "11s");
    let _cap = EnvGuard::set(RETRY_DEFAULT_CAP_ENV, "9m");
    let dir = write_repo(
        &[(
            "hello",
            &dept_with_retry(r#""tick""#, r#"max_attempts = 3"#),
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let cfg = load(dir.path()).unwrap();
    let retry = cfg.department.get("hello").unwrap().retry.as_ref().unwrap();

    assert_eq!(retry.max_attempts, 3);
    assert_eq!(retry.base, "11s");
    assert_eq!(retry.cap, "9m");
}

#[test]
fn dead_letter_consumer_defaults_to_retry_disabled() {
    let dir = write_repo(
        &[(
            "dead_handler",
            r#"
local M = {}
M.spec = { consumes = {"dead_letter"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[],
    );

    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.department.get("dead_handler").unwrap().retry, None);
}

#[test]
fn dead_letter_mixed_with_normal_queue_requires_explicit_retry() {
    let dir = write_repo(
        &[(
            "dead_handler",
            r#"
local M = {}
M.spec = { consumes = {"dead_letter", "tick"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let err = load(dir.path()).unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("dead_letter"), "got: {msg}");
    assert!(msg.contains("another queue"), "got: {msg}");
    assert!(msg.contains("M.spec.retry"), "got: {msg}");
    assert!(msg.contains("table or false"), "got: {msg}");
}

#[test]
fn host_graph_defaults_use_operational_defaults_when_env_and_fkst_env_are_absent() {
    let _env_lock = env_lock();
    let _queue = EnvGuard::unset(QUEUE_CAPACITY_ENV);
    let _stall_window = EnvGuard::unset(DEPARTMENT_DEFAULT_STALL_WINDOW_ENV);
    let _slots = EnvGuard::unset(CODEX_PERMIT_SLOTS_ENV);
    let _child_limit = EnvGuard::unset(DEPARTMENT_CHILD_PROCESS_LIMIT_ENV);
    let _child_limit_per_dept = EnvGuard::unset(DEPARTMENT_CHILD_PROCESS_LIMIT_PER_DEPT_ENV);
    let _attempts = EnvGuard::unset(RETRY_DEFAULT_MAX_ATTEMPTS_ENV);
    let _retry_base = EnvGuard::unset(RETRY_DEFAULT_BASE_ENV);
    let _retry_cap = EnvGuard::unset(RETRY_DEFAULT_CAP_ENV);
    let dir = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let depts_root = dir.path().join("departments");
    fs::create_dir_all(depts_root.join("hello")).unwrap();
    fs::write(
        depts_root.join("hello/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"tick"} }
function pipeline(_) end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("raisers")).unwrap();
    fs::write(
        dir.path().join("raisers/cron_a.lua"),
        r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
    )
    .unwrap();

    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.queue.get("tick").unwrap().capacity, 16);
    assert_eq!(cfg.department.get("hello").unwrap().stall_window, "30s");
    assert_eq!(cfg.limits.global_codex_processes, 20);
    let retry = cfg.department.get("hello").unwrap().retry.as_ref().unwrap();
    assert_eq!(retry.max_attempts, 5);
    assert_eq!(retry.base, "60s");
    assert_eq!(retry.cap, "30m");
}

#[test]
fn host_graph_defaults_use_fkst_env() {
    let _env_lock = env_lock();
    let _queue = EnvGuard::unset(QUEUE_CAPACITY_ENV);
    let _stall_window = EnvGuard::unset(DEPARTMENT_DEFAULT_STALL_WINDOW_ENV);
    let _slots = EnvGuard::unset(CODEX_PERMIT_SLOTS_ENV);
    let _attempts = EnvGuard::unset(RETRY_DEFAULT_MAX_ATTEMPTS_ENV);
    let _retry_base = EnvGuard::unset(RETRY_DEFAULT_BASE_ENV);
    let _retry_cap = EnvGuard::unset(RETRY_DEFAULT_CAP_ENV);
    let dir = write_repo(
        &[(
            "hello",
            r#"
local M = {}
M.spec = { consumes = {"tick"} }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );
    write_host_defaults(dir.path(), "11\n", "44s\n", "12\n");

    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.queue.get("tick").unwrap().capacity, 11);
    assert_eq!(cfg.department.get("hello").unwrap().stall_window, "44s");
    assert_eq!(cfg.limits.global_codex_processes, 12);
    assert_eq!(cfg.limits.global_department_child_processes, 16);
    assert_eq!(cfg.limits.department_child_processes_per_dept, 4);
}

#[test]
fn host_graph_defaults_use_env_before_fkst_env() {
    let _env_lock = env_lock();
    let _queue = EnvGuard::set(QUEUE_CAPACITY_ENV, "31");
    let _stall_window = EnvGuard::set(DEPARTMENT_DEFAULT_STALL_WINDOW_ENV, "66h");
    let _slots = EnvGuard::set(CODEX_PERMIT_SLOTS_ENV, "32");
    let _child_limit = EnvGuard::set(DEPARTMENT_CHILD_PROCESS_LIMIT_ENV, "9");
    let _child_limit_per_dept = EnvGuard::set(DEPARTMENT_CHILD_PROCESS_LIMIT_PER_DEPT_ENV, "3");
    let dir = write_repo(
        &[(
            "hello",
            r#"
local M = {}
M.spec = { consumes = {"tick"} }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );
    fs::write(
        dir.path().join("fkst.env"),
        "FKST_QUEUE_CAPACITY=21\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=55m\nFKST_CODEX_PERMIT_SLOTS=22\nFKST_DEPARTMENT_CHILD_PROCESS_LIMIT=7\nFKST_DEPARTMENT_CHILD_PROCESS_LIMIT_PER_DEPT=2\nFKST_RETRY_DEFAULT_MAX_ATTEMPTS=8\nFKST_RETRY_DEFAULT_BASE=2m\nFKST_RETRY_DEFAULT_CAP=1h\n",
    )
    .unwrap();

    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.queue.get("tick").unwrap().capacity, 31);
    assert_eq!(cfg.department.get("hello").unwrap().stall_window, "66h");
    assert_eq!(cfg.limits.global_codex_processes, 32);
    assert_eq!(cfg.limits.global_department_child_processes, 9);
    assert_eq!(cfg.limits.department_child_processes_per_dept, 3);
    let retry = cfg.department.get("hello").unwrap().retry.as_ref().unwrap();
    assert_eq!(retry.max_attempts, 8);
    assert_eq!(retry.base, "2m");
    assert_eq!(retry.cap, "1h");
}

#[test]
fn host_graph_retry_defaults_use_env_and_validate() {
    let _env_lock = env_lock();
    let _attempts = EnvGuard::set(RETRY_DEFAULT_MAX_ATTEMPTS_ENV, "12");
    let _base = EnvGuard::set(RETRY_DEFAULT_BASE_ENV, "4s");
    let _cap = EnvGuard::set(RETRY_DEFAULT_CAP_ENV, "2m");
    let dir = write_repo(
        &[(
            "hello",
            r#"
local M = {}
M.spec = { consumes = {"tick"} }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let cfg = load(dir.path()).unwrap();
    let retry = cfg.department.get("hello").unwrap().retry.as_ref().unwrap();

    assert_eq!(retry.max_attempts, 12);
    assert_eq!(retry.base, "4s");
    assert_eq!(retry.cap, "2m");
}

#[test]
fn explicit_department_stall_window_overrides_host_default_stall_window() {
    let dir = write_repo(
        &[(
            "hello",
            r#"
local M = {}
M.spec = { consumes = {"tick"}, stall_window = "9s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );
    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.department.get("hello").unwrap().stall_window, "9s");
}

#[test]
fn dept_spec_rejects_removed_timeout_field_fail_closed() {
    let dir = write_repo(
        &[(
            "hello",
            r#"
local M = {}
M.spec = { consumes = {"tick"}, timeout = "9s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let err = load(dir.path()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("parse `hello.spec`"), "got: {msg}");
    assert!(msg.contains("unknown field"), "got: {msg}");
    assert!(msg.contains("timeout"), "got: {msg}");
}

#[test]
fn operational_defaults_ignore_removed_txt_files() {
    let _env_lock = env_lock();
    let _queue = EnvGuard::unset(QUEUE_CAPACITY_ENV);
    let _stall_window = EnvGuard::unset(DEPARTMENT_DEFAULT_STALL_WINDOW_ENV);
    let _slots = EnvGuard::unset(CODEX_PERMIT_SLOTS_ENV);
    let dir = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(dir.path().join("departments/hello")).unwrap();
    fs::create_dir_all(dir.path().join("raisers")).unwrap();
    fs::create_dir_all(dir.path().join("tunables")).unwrap();
    fs::write(dir.path().join("tunables/queue_capacity.txt"), "99\n").unwrap();
    fs::write(
        dir.path()
            .join("tunables/department_default_stall_window.txt"),
        "99m\n",
    )
    .unwrap();
    fs::write(dir.path().join("tunables/codex_permit_slots.txt"), "99\n").unwrap();
    fs::write(
        dir.path().join("departments/hello/main.lua"),
        r#"
local M = {}
M.spec = { consumes = {"tick"} }
function pipeline(_) end
return M
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("raisers/cron_a.lua"),
        r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
    )
    .unwrap();

    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.queue.get("tick").unwrap().capacity, 16);
    assert_eq!(cfg.department.get("hello").unwrap().stall_window, "30s");
    assert_eq!(cfg.limits.global_codex_processes, 20);
}

#[test]
fn host_graph_defaults_fail_closed_when_configured_value_is_invalid() {
    let _env_lock = env_lock();
    let _queue = EnvGuard::unset(QUEUE_CAPACITY_ENV);
    let _stall_window = EnvGuard::unset(DEPARTMENT_DEFAULT_STALL_WINDOW_ENV);
    let _slots = EnvGuard::unset(CODEX_PERMIT_SLOTS_ENV);
    let _attempts = EnvGuard::unset(RETRY_DEFAULT_MAX_ATTEMPTS_ENV);
    let _retry_base = EnvGuard::unset(RETRY_DEFAULT_BASE_ENV);
    let _retry_cap = EnvGuard::unset(RETRY_DEFAULT_CAP_ENV);
    let cases = [
        (QUEUE_CAPACITY_ENV, "not-a-number"),
        (DEPARTMENT_DEFAULT_STALL_WINDOW_ENV, "30x"),
        (CODEX_PERMIT_SLOTS_ENV, "0"),
        (RETRY_DEFAULT_MAX_ATTEMPTS_ENV, "0"),
        (RETRY_DEFAULT_BASE_ENV, "0s"),
        (RETRY_DEFAULT_CAP_ENV, "bad"),
    ];

    for (key, value) in cases {
        let dir = write_repo(
            &[(
                "hello",
                r#"
local M = {}
M.spec = { consumes = {"tick"} }
function pipeline(_) end
return M
"#,
            )],
            &[(
                "cron_a",
                r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
            )],
        );
        fs::write(dir.path().join("fkst.env"), format!("{key}={value}\n")).unwrap();

        let err = load(dir.path()).unwrap_err();
        let msg = format!("{:#}", err);

        assert!(msg.contains(key), "got: {msg}");
    }

    let dir = write_repo(
        &[(
            "hello",
            r#"
local M = {}
M.spec = { consumes = {"tick"} }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );
    fs::write(
        dir.path().join("fkst.env"),
        "FKST_RETRY_DEFAULT_BASE=30m\nFKST_RETRY_DEFAULT_CAP=60s\n",
    )
    .unwrap();

    let err = load(dir.path()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("FKST_RETRY_DEFAULT_CAP"), "got: {msg}");
}

#[test]
fn graph_scan_loads_root_modules() {
    let repo = write_repo(
        &[(
            "hello",
            r#"
local helper = require("fkst.spec_helper")
local M = {}
M.spec = { consumes = {"in"}, produces = {"out"}, stall_window = helper.stall_window() }
function pipeline(_) end
return M
"#,
        )],
        &[],
    );
    fs::create_dir_all(repo.path().join("fkst")).unwrap();
    fs::write(
        repo.path().join("fkst/spec_helper.lua"),
        r#"return { stall_window = function() return "45s" end }"#,
    )
    .unwrap();

    let cfg = load(repo.path()).unwrap();
    assert_eq!(cfg.department["hello"].stall_window, "45s");
}

#[test]
fn package_root_assets_and_host_departments_form_one_graph() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package = write_repo(
        &[],
        &[(
            "standard_tick",
            r#"return { type = "cron", interval = "10s", produces = "standard_tick" }"#,
        )],
    );
    write_package_helper(package.path());
    let host_worker = r#"
local helper = require("fkst.spec_helper")
local M = {}
M.spec = { consumes = {"PACKAGE_NS.standard_tick"}, produces = {"host_done"}, stall_window = helper.stall_window() }
function pipeline(_) end
return M
"#
    .replace("PACKAGE_NS", &ns(package.path()));
    let host = write_repo(&[("host_worker", &host_worker)], &[]);
    write_package_helper(host.path());

    let roots = PackageRoots::resolve(host.path(), vec![package.path().to_path_buf()]).unwrap();
    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert!(cfg.raiser.contains_key(&q(package.path(), "standard_tick")));
    assert!(cfg.department.contains_key("host.host_worker"));
    assert_eq!(cfg.department["host.host_worker"].stall_window, "45s");
    assert_eq!(
        cfg.department["host.host_worker"].lua,
        host.path()
            .canonicalize()
            .unwrap()
            .join("departments/host_worker/main.lua")
    );
    validate(&cfg, host.path()).unwrap();
    assert!(cfg.queue.contains_key(&q(package.path(), "standard_tick")));
    assert!(cfg.queue.contains_key("host.host_done"));
}

#[test]
fn multiple_package_roots_and_host_form_one_graph() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package_a = write_repo(
        &[("producer", &dept("", r#""handoff""#))],
        &[(
            "tick_a",
            r#"return { type = "cron", interval = "10s", produces = "start_a" }"#,
        )],
    );
    let package_b = write_repo(
        &[(
            "consumer",
            &dept(
                &format!(r#""{}""#, q(package_a.path(), "handoff")),
                r#""done_b""#,
            ),
        )],
        &[(
            "tick_b",
            r#"return { type = "cron", interval = "20s", produces = "start_b" }"#,
        )],
    );
    let host = write_repo(
        &[(
            "host_worker",
            &dept(&format!(r#""{}""#, q(package_b.path(), "done_b")), ""),
        )],
        &[],
    );

    let roots = PackageRoots::resolve(
        host.path(),
        vec![
            package_a.path().to_path_buf(),
            package_b.path().to_path_buf(),
        ],
    )
    .unwrap();
    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert!(cfg.raiser.contains_key(&q(package_a.path(), "tick_a")));
    assert!(cfg.raiser.contains_key(&q(package_b.path(), "tick_b")));
    assert!(cfg
        .department
        .contains_key(&q(package_a.path(), "producer")));
    assert!(cfg
        .department
        .contains_key(&q(package_b.path(), "consumer")));
    assert!(cfg.department.contains_key("host.host_worker"));
    validate(&cfg, host.path()).unwrap();
}

#[test]
fn host_department_uses_package_standard_asset_in_multi_package_graph() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package_a = write_repo(
        &[],
        &[(
            "tick_a",
            r#"return { type = "cron", interval = "10s", produces = "tick_a" }"#,
        )],
    );
    fs::create_dir_all(package_a.path().join("fkst")).unwrap();
    fs::write(
        package_a.path().join("fkst/standard_asset.lua"),
        r#"return { stall_window = function() return "45s" end }"#,
    )
    .unwrap();
    let package_b = write_repo(
        &[],
        &[(
            "tick_b",
            r#"return { type = "cron", interval = "20s", produces = "tick_b" }"#,
        )],
    );
    let host_worker = r#"
local standard = require("fkst.standard_asset")
local M = {}
M.spec = { consumes = {"PACKAGE_NS.tick_a"}, produces = {"done"}, stall_window = standard.stall_window() }
function pipeline(_) end
return M
"#
    .replace("PACKAGE_NS", &ns(package_a.path()));
    let host = write_repo(&[("host_worker", &host_worker)], &[]);

    let roots = PackageRoots::resolve(
        host.path(),
        vec![
            package_a.path().to_path_buf(),
            package_b.path().to_path_buf(),
        ],
    )
    .unwrap();
    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert_eq!(cfg.department["host.host_worker"].stall_window, "45s");
    validate(&cfg, host.path()).unwrap();
}

#[test]
fn package_department_cannot_require_another_package_private_module() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package_a = write_repo(
        &[(
            "alpha",
            r#"
local core = require("core")
local M = {}
M.spec = { consumes = {"tick_a"}, produces = {core.queue()}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "tick_a",
            r#"return { type = "cron", interval = "10s", produces = "tick_a" }"#,
        )],
    );
    let package_b = write_repo(&[], &[]);
    fs::write(
        package_b.path().join("core.lua"),
        r#"return { queue = function() return "b_done" end }"#,
    )
    .unwrap();
    let host = write_repo(&[], &[]);
    let roots = PackageRoots::resolve(
        host.path(),
        vec![
            package_a.path().to_path_buf(),
            package_b.path().to_path_buf(),
        ],
    )
    .unwrap();

    let err = graph_scan::load_roots(&roots).unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("module 'core' not found"), "got: {msg}");
    assert!(!msg.contains("b_done"), "got: {msg}");
}

#[test]
fn graph_scan_uses_fresh_lua_state_and_root_local_require_path() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package_a = write_repo(
        &[(
            "alpha",
            r#"
local core = require("core")
local M = {}
M.spec = { consumes = {"tick_a"}, produces = {core.queue()}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "tick_a",
            r#"return { type = "cron", interval = "10s", produces = "tick_a" }"#,
        )],
    );
    fs::write(
        package_a.path().join("core.lua"),
        r#"return { queue = function() return "a_done" end }"#,
    )
    .unwrap();
    let package_b = write_repo(
        &[(
            "beta",
            r#"
local core = require("core")
local M = {}
M.spec = { consumes = {"tick_b"}, produces = {core.queue()}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "tick_b",
            r#"return { type = "cron", interval = "10s", produces = "tick_b" }"#,
        )],
    );
    fs::write(
        package_b.path().join("core.lua"),
        r#"return { queue = function() return "b_done" end }"#,
    )
    .unwrap();
    let host = write_repo(&[], &[]);

    let roots = PackageRoots::resolve(
        host.path(),
        vec![
            package_a.path().to_path_buf(),
            package_b.path().to_path_buf(),
        ],
    )
    .unwrap();
    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert_eq!(
        cfg.department[&q(package_a.path(), "alpha")].produces,
        vec![q(package_a.path(), "a_done")]
    );
    assert_eq!(
        cfg.department[&q(package_b.path(), "beta")].produces,
        vec![q(package_b.path(), "b_done")]
    );
}

#[test]
fn graph_scan_does_not_fall_back_to_host_or_cwd_require_path() {
    let _env_lock = env_lock();
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package = write_repo(
        &[(
            "alpha",
            r#"
local core = require("core")
local M = {}
M.spec = { consumes = {"tick"}, produces = {core.queue()}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "tick",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );
    let host = write_repo(&[], &[]);
    fs::write(
        host.path().join("core.lua"),
        r#"return { queue = function() return "host_done" end }"#,
    )
    .unwrap();
    let cwd = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::write(
        cwd.path().join("core.lua"),
        r#"return { queue = function() return "cwd_done" end }"#,
    )
    .unwrap();
    let roots = PackageRoots::resolve(host.path(), vec![package.path().to_path_buf()]).unwrap();

    let prior_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(cwd.path()).unwrap();
    let err = graph_scan::load_roots(&roots).unwrap_err();
    std::env::set_current_dir(prior_cwd).unwrap();
    let msg = format!("{err:#}");

    assert!(msg.contains("module 'core' not found"), "got: {msg}");
    assert!(
        !msg.contains("host_done") && !msg.contains("cwd_done"),
        "got: {msg}"
    );
}

#[test]
fn same_department_name_across_package_roots_is_namespaced() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package_a = write_repo(&[("same", &dept(r#""tick""#, ""))], &[]);
    let package_b = write_repo(&[("same", &dept(r#""tick""#, ""))], &[]);
    let host = write_repo(&[], &[]);
    let roots = PackageRoots::resolve(
        host.path(),
        vec![
            package_a.path().to_path_buf(),
            package_b.path().to_path_buf(),
        ],
    )
    .unwrap();

    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert!(cfg.department.contains_key(&q(package_a.path(), "same")));
    assert!(cfg.department.contains_key(&q(package_b.path(), "same")));
    validate(&cfg, host.path()).unwrap();
}

#[test]
fn same_raiser_name_across_package_roots_is_namespaced() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package_a = write_repo(
        &[],
        &[(
            "same",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );
    let package_b = write_repo(
        &[],
        &[(
            "same",
            r#"return { type = "cron", interval = "20s", produces = "tick" }"#,
        )],
    );
    let host = write_repo(&[], &[]);
    let roots = PackageRoots::resolve(
        host.path(),
        vec![
            package_a.path().to_path_buf(),
            package_b.path().to_path_buf(),
        ],
    )
    .unwrap();

    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert!(cfg.raiser.contains_key(&q(package_a.path(), "same")));
    assert!(cfg.raiser.contains_key(&q(package_b.path(), "same")));
}

#[test]
fn package_root_env_is_used_when_flag_is_absent() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package = write_repo(
        &[],
        &[(
            "standard_tick",
            r#"return { type = "cron", interval = "10s", produces = "standard_tick" }"#,
        )],
    );
    let host = write_repo(
        &[(
            "host_worker",
            &dept(&format!(r#""{}""#, q(package.path(), "standard_tick")), ""),
        )],
        &[],
    );
    let _env = EnvGuard::set(PACKAGE_ROOT_ENV, package.path());

    let _plural_env = EnvGuard::unset(PACKAGE_ROOTS_ENV);
    let roots = PackageRoots::resolve(host.path(), Vec::new()).unwrap();
    assert_eq!(
        roots.package_roots()[0],
        package.path().canonicalize().unwrap()
    );
    assert_eq!(roots.host_root(), host.path().canonicalize().unwrap());
    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert!(cfg.raiser.contains_key(&q(package.path(), "standard_tick")));
    assert!(cfg.department.contains_key("host.host_worker"));
}

#[test]
fn missing_package_root_fails_closed_even_when_share_tree_exists() {
    let _env_lock = env_lock();
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("share/fkst/departments")).unwrap();
    let _package_root = EnvGuard::unset(PACKAGE_ROOT_ENV);
    let _stdlib = EnvGuard::unset("FKST_STDLIB_ROOT");
    let _runtime_package = EnvGuard::unset("FKST_RUNTIME_PACKAGE_ROOT");
    let _graph_roots = EnvGuard::unset("FKST_GRAPH_ROOTS");

    let _plural_env = EnvGuard::unset(PACKAGE_ROOTS_ENV);
    let err = PackageRoots::resolve(host.path(), Vec::new()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(
        msg.contains("FKST_PACKAGE_ROOTS, FKST_PACKAGE_ROOT, or --package-root is required"),
        "got: {msg}"
    );
}

#[test]
fn removed_package_root_envs_fail_closed() {
    let host = write_repo(&[("host_worker", &dept(r#""tick""#, ""))], &[]);
    let _env = EnvGuard::set("FKST_STDLIB_ROOT", host.path());

    let err = PackageRoots::resolve(host.path(), Vec::new()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("FKST_STDLIB_ROOT"), "got: {}", msg);
    assert!(msg.contains("removed package root surface"), "got: {}", msg);
}

#[test]
fn duplicate_package_root_basename_fails_closed() {
    let parent_a = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let parent_b = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package_a = parent_a.path().join("pkg");
    let package_b = parent_b.path().join("pkg");
    fs::create_dir_all(&package_a).unwrap();
    fs::create_dir_all(&package_b).unwrap();
    write_host_defaults(&package_a, "100\n", "30m\n", "20\n");
    write_host_defaults(&package_b, "100\n", "30m\n", "20\n");
    let host = write_repo(&[], &[]);

    let err = PackageRoots::resolve(host.path(), vec![package_a, package_b]).unwrap_err();
    let msg = format!("{err:#}");

    assert!(
        msg.contains("duplicate package root basename `pkg`"),
        "got: {msg}"
    );
}

#[test]
fn package_basename_host_with_independent_host_fails_closed() {
    let parent = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = parent.path().join("host");
    fs::create_dir_all(&package).unwrap();
    write_host_defaults(&package, "100\n", "30m\n", "20\n");
    let host = write_repo(&[], &[]);

    let err = PackageRoots::resolve(host.path(), vec![package]).unwrap_err();
    let msg = format!("{err:#}");

    assert!(
        msg.contains("conflicts with independent host namespace"),
        "got: {msg}"
    );
}

#[test]
fn folded_single_package_same_namespace_qualified_queue_stays_flat() {
    let dir = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(dir.path(), "100\n", "30m\n", "20\n");
    let namespace = ns(dir.path());
    let dept_dir = dir.path().join("departments/hello");
    fs::create_dir_all(&dept_dir).unwrap();
    fs::write(
        dept_dir.join("main.lua"),
        dept(
            &format!(r#""{namespace}.tick""#),
            &format!(r#""{namespace}.done""#),
        ),
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("raisers")).unwrap();
    fs::write(
        dir.path().join("raisers/cron_a.lua"),
        format!(r#"return {{ type = "cron", interval = "10s", produces = "{namespace}.tick" }}"#),
    )
    .unwrap();
    let roots = PackageRoots::resolve(dir.path(), vec![dir.path().to_path_buf()]).unwrap();
    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert!(cfg.queue.contains_key("tick"));
    assert!(cfg.queue.contains_key("done"));
    assert!(!cfg.queue.contains_key(&format!("{namespace}.tick")));
    assert_eq!(cfg.department["hello"].consumes, vec!["tick"]);
    match &cfg.raiser["cron_a"] {
        RaiserDecl::Cron { produces, .. } => assert_eq!(produces, "tick"),
        _ => panic!("expected cron"),
    }
}

#[test]
fn same_department_name_across_package_and_host_is_namespaced() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package = write_repo(&[("same", &dept(r#""tick""#, ""))], &[]);
    let host = write_repo(&[("same", &dept(r#""tick""#, ""))], &[]);
    let roots = PackageRoots::resolve(host.path(), vec![package.path().to_path_buf()]).unwrap();

    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert!(cfg.department.contains_key(&q(package.path(), "same")));
    assert!(cfg.department.contains_key("host.same"));
    validate(&cfg, host.path()).unwrap();
}

#[test]
fn derives_queues_from_dept_consumes_produces() {
    let dir = write_repo(
        &[(
            "evolve",
            r#"
local M = {}
M.spec = {
consumes = {"evolve_request"},
produces = {"evolve_done", "evolve_failed"},
stall_window = "30m",
}
function pipeline(_) end
return M
"#,
        )],
        &[],
    );
    let cfg = load(dir.path()).unwrap();
    assert!(cfg.queue.contains_key("evolve_request"));
    assert!(cfg.queue.contains_key("evolve_done"));
    assert!(cfg.queue.contains_key("evolve_failed"));
    for q in cfg.queue.values() {
        assert_eq!(q.capacity, 100);
        assert!(!q.fanout);
    }
}

#[test]
fn missing_spec_errors() {
    let dir = write_repo(
        &[(
            "bad",
            r#"
local M = {}
function pipeline(_) end
return M
"#,
        )],
        &[],
    );
    let err = load(dir.path()).unwrap_err();
    let msg = format!("{:#}", err);
    assert!(msg.contains("missing `M.spec`"), "got: {}", msg);
}

#[test]
fn graph_scan_rejects_fkst_paths_global() {
    let _env = env_lock();
    let _root = EnvGuard::set(RUNTIME_ROOT_ENV, ".fkst/runtime");
    let dir = write_repo(
        &[(
            "runtime_paths",
            r#"
local M = {}
assert(fkst_paths == nil, "fkst_paths graph-scan global must not be injected")
local _ = fkst_paths.runtime_root()
M.spec = { consumes = {"tick"}, stall_window = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "runtime_watch",
            r#"return { type = "file_watch", glob = "host_inbox/*/meta.md", produces = "tick" }"#,
        )],
    );
    let err = load(dir.path()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("fkst_paths"), "got: {msg}");
    assert!(
        msg.contains("eval department `runtime_paths`"),
        "got: {msg}"
    );
}

#[test]
fn runtime_file_watch_glob_is_removed_surface() {
    let _env = env_lock();
    let _root = EnvGuard::set(RUNTIME_ROOT_ENV, ".fkst/runtime");
    let dir = write_repo(
        &[],
        &[(
            "runtime_watch",
            concat!(
                r#"return { type = "file_watch", glob = "runtime"#,
                r#"://arti"#,
                r#"facts/"#,
                r#"pipeline/inbox/*.md", produces = "tick" }"#
            ),
        )],
    );

    let err = load(dir.path()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(
        msg.contains(concat!(
            "runtime",
            ":// file_watch glob is a removed surface"
        )),
        "got: {msg}"
    );
    assert!(
        msg.contains("host-root relative or absolute glob"),
        "got: {msg}"
    );
}

#[test]
fn skips_dept_without_main_lua() {
    let dir = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    write_host_defaults(dir.path(), "100\n", "30m\n", "20\n");
    fs::create_dir_all(dir.path().join("departments/empty")).unwrap();
    let cfg = load(dir.path()).unwrap();
    assert!(cfg.department.is_empty());
}

#[test]
fn dept_spec_fanout_marks_declared_queue() {
    let dir = write_repo(
        &[
            ("alpha", &dept_with_fanout(r#""tick""#, "", r#""tick""#)),
            ("beta", &dept(r#""tick""#, "")),
        ],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let cfg = load(dir.path()).unwrap();

    assert!(cfg.queue.get("tick").unwrap().fanout);
}

#[test]
fn dept_spec_fanout_rejects_unreferenced_queue() {
    let dir = write_repo(
        &[("alpha", &dept_with_fanout(r#""tick""#, "", r#""ghost""#))],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let err = load(dir.path()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("fanout queue `ghost`"), "got: {}", msg);
    assert!(msg.contains("does not consume or produce"), "got: {}", msg);
}

#[test]
fn dept_spec_fanout_duplicate_declarations_are_idempotent() {
    let dir = write_repo(
        &[
            ("alpha", &dept_with_fanout(r#""tick""#, "", r#""tick""#)),
            ("beta", &dept_with_fanout(r#""tick""#, "", r#""tick""#)),
        ],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let cfg = load(dir.path()).unwrap();

    assert!(cfg.queue.get("tick").unwrap().fanout);
}

#[test]
fn fanout_queue_allows_default_retry_mixed_with_retry_false() {
    let dir = write_repo(
        &[
            ("alpha", &dept_with_fanout(r#""tick""#, "", r#""tick""#)),
            ("beta", &dept_with_raw_retry(r#""tick""#, "false")),
        ],
        &[(
            "cron_a",
            r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
        )],
    );

    let cfg = load(dir.path()).unwrap();
    let warnings = validate(&cfg, dir.path()).unwrap();

    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(cfg.queue.get("tick").unwrap().fanout);
}

#[test]
fn root_package_lua_is_removed_surface() {
    let dir = write_repo(&[("alpha", &dept(r#""tick""#, ""))], &[]);
    fs::write(dir.path().join("package.lua"), "return {}\n").unwrap();

    let err = load(dir.path()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("package.lua"), "got: {}", msg);
    assert!(msg.contains("removed graph surface"), "got: {}", msg);
}

#[cfg(unix)]
#[test]
fn duplicate_department_name_fails_closed() {
    use fkst_common::config::DepartmentDecl;
    use std::collections::BTreeMap;

    let mut departments = BTreeMap::new();
    let path = PathBuf::from("departments/alpha/main.lua");
    graph_scan::insert_department_decl(
        &mut departments,
        "alpha",
        DepartmentDecl {
            lua: path.clone(),
            owner_root: PathBuf::from("first-root"),
            owner_namespace: "pkg".to_string(),
            consumes: vec!["tick".into()],
            produces: Vec::new(),
            ephemeral: Vec::new(),
            stall_window: "30s".into(),
            graph_json: false,
            retry: None,
        },
        &path,
    )
    .unwrap();
    let err = graph_scan::insert_department_decl(
        &mut departments,
        "alpha",
        DepartmentDecl {
            lua: path.clone(),
            owner_root: PathBuf::from("second-root"),
            owner_namespace: "pkg".to_string(),
            consumes: vec!["tick".into()],
            produces: Vec::new(),
            ephemeral: Vec::new(),
            stall_window: "30s".into(),
            graph_json: false,
            retry: None,
        },
        &path,
    )
    .unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("duplicate department `alpha`"), "got: {}", msg);
}

#[cfg(unix)]
#[test]
fn duplicate_raiser_name_fails_closed() {
    use std::collections::BTreeMap;

    let mut raisers = BTreeMap::new();
    let path = PathBuf::from("raisers/cron_a.lua");
    graph_scan::insert_raiser_decl(
        &mut raisers,
        "cron_a",
        RaiserDecl::Cron {
            interval: "10s".into(),
            produces: "tick".into(),
        },
        &path,
    )
    .unwrap();
    let second_path = PathBuf::from("other/raisers/cron_a.lua");
    let err = graph_scan::insert_raiser_decl(
        &mut raisers,
        "cron_a",
        RaiserDecl::Cron {
            interval: "20s".into(),
            produces: "tick".into(),
        },
        &second_path,
    )
    .unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("duplicate raiser `cron_a`"), "got: {}", msg);
}

// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/config_registry.rs"]
mod config_registry;
#[path = "../src/supervise/graph_scan.rs"]
mod graph_scan;
#[path = "../src/path_resolver.rs"]
mod path_resolver;
#[path = "../src/runtime_context.rs"]
mod runtime_context;

use fkst_common::config::RaiserDecl;
use fkst_common::validation::validate;
use graph_scan::load as graph_load;
use path_resolver::PackageRoots;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;

const RUNTIME_ROOT_ENV: &str = "FKST_RUNTIME_ROOT";
const QUEUE_CAPACITY_ENV: &str = "FKST_QUEUE_CAPACITY";
const DEPARTMENT_DEFAULT_TIMEOUT_ENV: &str = "FKST_DEPARTMENT_DEFAULT_TIMEOUT";
const CODEX_PERMIT_SLOTS_ENV: &str = "FKST_CODEX_PERMIT_SLOTS";
const PACKAGE_ROOT_ENV: &str = "FKST_PACKAGE_ROOT";

static CURRENT_DIR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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
    let _root = if std::env::var_os(RUNTIME_ROOT_ENV).is_none() {
        Some(EnvGuard::set(RUNTIME_ROOT_ENV, ".fkst/runtime"))
    } else {
        None
    };
    graph_load(path)
}

fn write_host_defaults(root: &std::path::Path, queue: &str, timeout: &str, slots: &str) {
    fs::write(
        root.join("fkst.env"),
        format!(
            "FKST_QUEUE_CAPACITY={}FKST_DEPARTMENT_DEFAULT_TIMEOUT={}FKST_CODEX_PERMIT_SLOTS={}",
            queue, timeout, slots
        ),
    )
    .unwrap();
}

fn write_repo(depts: &[(&str, &str)], raisers: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
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

fn dept(consumes: &str, produces: &str) -> String {
    format!(
        r#"
local M = {{}}
M.spec = {{ consumes = {{{}}}, produces = {{{}}}, timeout = "30s" }}
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
M.spec = {{ consumes = {{{}}}, produces = {{{}}}, fanout = {{{}}}, timeout = "30s" }}
function pipeline(_) end
return M
"#,
        consumes, produces, fanout
    )
}

fn write_package_helper(root: &std::path::Path) {
    fs::create_dir_all(root.join("fkst")).unwrap();
    fs::write(
        root.join("fkst/spec_helper.lua"),
        r#"return { timeout = function() return "45s" end }"#,
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
M.spec = { consumes = {"tick"}, timeout = "30s" }
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
fn host_graph_defaults_use_operational_defaults_when_env_and_fkst_env_are_absent() {
    let _env_lock = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let _queue = EnvGuard::unset(QUEUE_CAPACITY_ENV);
    let _timeout = EnvGuard::unset(DEPARTMENT_DEFAULT_TIMEOUT_ENV);
    let _slots = EnvGuard::unset(CODEX_PERMIT_SLOTS_ENV);
    let dir = TempDir::new().unwrap();
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
    assert_eq!(cfg.department.get("hello").unwrap().timeout, "30s");
    assert_eq!(cfg.limits.global_codex_processes, 20);
}

#[test]
fn host_graph_defaults_use_fkst_env() {
    let _env_lock = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let _queue = EnvGuard::unset(QUEUE_CAPACITY_ENV);
    let _timeout = EnvGuard::unset(DEPARTMENT_DEFAULT_TIMEOUT_ENV);
    let _slots = EnvGuard::unset(CODEX_PERMIT_SLOTS_ENV);
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
    assert_eq!(cfg.department.get("hello").unwrap().timeout, "44s");
    assert_eq!(cfg.limits.global_codex_processes, 12);
}

#[test]
fn host_graph_defaults_use_env_before_fkst_env() {
    let _env_lock = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let _queue = EnvGuard::set(QUEUE_CAPACITY_ENV, "31");
    let _timeout = EnvGuard::set(DEPARTMENT_DEFAULT_TIMEOUT_ENV, "66h");
    let _slots = EnvGuard::set(CODEX_PERMIT_SLOTS_ENV, "32");
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
        "FKST_QUEUE_CAPACITY=21\nFKST_DEPARTMENT_DEFAULT_TIMEOUT=55m\nFKST_CODEX_PERMIT_SLOTS=22\n",
    )
    .unwrap();

    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.queue.get("tick").unwrap().capacity, 31);
    assert_eq!(cfg.department.get("hello").unwrap().timeout, "66h");
    assert_eq!(cfg.limits.global_codex_processes, 32);
}

#[test]
fn explicit_department_timeout_overrides_host_default_timeout() {
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
    let cfg = load(dir.path()).unwrap();

    assert_eq!(cfg.department.get("hello").unwrap().timeout, "9s");
}

#[test]
fn operational_defaults_ignore_removed_txt_files() {
    let _env_lock = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let _queue = EnvGuard::unset(QUEUE_CAPACITY_ENV);
    let _timeout = EnvGuard::unset(DEPARTMENT_DEFAULT_TIMEOUT_ENV);
    let _slots = EnvGuard::unset(CODEX_PERMIT_SLOTS_ENV);
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("departments/hello")).unwrap();
    fs::create_dir_all(dir.path().join("raisers")).unwrap();
    fs::create_dir_all(dir.path().join("tunables")).unwrap();
    fs::write(dir.path().join("tunables/queue_capacity.txt"), "99\n").unwrap();
    fs::write(
        dir.path().join("tunables/department_default_timeout.txt"),
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
    assert_eq!(cfg.department.get("hello").unwrap().timeout, "30s");
    assert_eq!(cfg.limits.global_codex_processes, 20);
}

#[test]
fn host_graph_defaults_fail_closed_when_configured_value_is_invalid() {
    let _env_lock = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let _queue = EnvGuard::unset(QUEUE_CAPACITY_ENV);
    let _timeout = EnvGuard::unset(DEPARTMENT_DEFAULT_TIMEOUT_ENV);
    let _slots = EnvGuard::unset(CODEX_PERMIT_SLOTS_ENV);
    let cases = [
        (QUEUE_CAPACITY_ENV, "not-a-number"),
        (DEPARTMENT_DEFAULT_TIMEOUT_ENV, "30x"),
        (CODEX_PERMIT_SLOTS_ENV, "0"),
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
}

#[test]
fn graph_scan_loads_root_modules() {
    let repo = write_repo(
        &[(
            "hello",
            r#"
local helper = require("fkst.spec_helper")
local M = {}
M.spec = { consumes = {"in"}, produces = {"out"}, timeout = helper.timeout() }
function pipeline(_) end
return M
"#,
        )],
        &[],
    );
    fs::create_dir_all(repo.path().join("fkst")).unwrap();
    fs::write(
        repo.path().join("fkst/spec_helper.lua"),
        r#"return { timeout = function() return "45s" end }"#,
    )
    .unwrap();

    let cfg = load(repo.path()).unwrap();
    assert_eq!(cfg.department["hello"].timeout, "45s");
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
    let host = write_repo(
        &[(
            "host_worker",
            r#"
local helper = require("fkst.spec_helper")
local M = {}
M.spec = { consumes = {"standard_tick"}, produces = {"host_done"}, timeout = helper.timeout() }
function pipeline(_) end
return M
"#,
        )],
        &[],
    );

    let roots = PackageRoots::resolve(host.path(), Some(package.path().to_path_buf())).unwrap();
    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert!(cfg.raiser.contains_key("standard_tick"));
    assert!(cfg.department.contains_key("host_worker"));
    assert_eq!(cfg.department["host_worker"].timeout, "45s");
    assert_eq!(
        cfg.department["host_worker"].lua,
        host.path()
            .canonicalize()
            .unwrap()
            .join("departments/host_worker/main.lua")
    );
    validate(&cfg, host.path()).unwrap();
    assert!(cfg.queue.contains_key("standard_tick"));
    assert!(cfg.queue.contains_key("host_done"));
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
    let host = write_repo(&[("host_worker", &dept(r#""standard_tick""#, ""))], &[]);
    let _env = EnvGuard::set(PACKAGE_ROOT_ENV, package.path());

    let roots = PackageRoots::resolve(host.path(), None).unwrap();
    assert_eq!(roots.package_root(), package.path().canonicalize().unwrap());
    assert_eq!(roots.host_root(), host.path().canonicalize().unwrap());
    let cfg = graph_scan::load_roots(&roots).unwrap();

    assert!(cfg.raiser.contains_key("standard_tick"));
    assert!(cfg.department.contains_key("host_worker"));
}

#[test]
fn missing_package_root_fails_closed_even_when_share_tree_exists() {
    let _env_lock = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let host = tempfile::tempdir().unwrap();
    fs::create_dir_all(host.path().join("share/fkst/departments")).unwrap();
    let _package_root = EnvGuard::unset(PACKAGE_ROOT_ENV);
    let _stdlib = EnvGuard::unset("FKST_STDLIB_ROOT");
    let _runtime_package = EnvGuard::unset("FKST_RUNTIME_PACKAGE_ROOT");
    let _graph_roots = EnvGuard::unset("FKST_GRAPH_ROOTS");

    let err = PackageRoots::resolve(host.path(), None).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(
        msg.contains("FKST_PACKAGE_ROOT or --package-root is required"),
        "got: {msg}"
    );
}

#[test]
fn removed_package_root_envs_fail_closed() {
    let host = write_repo(&[("host_worker", &dept(r#""tick""#, ""))], &[]);
    let _env = EnvGuard::set("FKST_STDLIB_ROOT", host.path());

    let err = PackageRoots::resolve(host.path(), None).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("FKST_STDLIB_ROOT"), "got: {}", msg);
    assert!(msg.contains("removed package root surface"), "got: {}", msg);
}

#[test]
fn duplicate_package_and_host_department_name_fails_closed() {
    let _root = EnvGuard::set("FKST_RUNTIME_ROOT", ".fkst/runtime");
    let package = write_repo(&[("same", &dept(r#""tick""#, ""))], &[]);
    let host = write_repo(&[("same", &dept(r#""tick""#, ""))], &[]);
    let roots = PackageRoots::resolve(host.path(), Some(package.path().to_path_buf())).unwrap();

    let err = graph_scan::load_roots(&roots).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("duplicate department `same`"), "got: {}", msg);
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
timeout = "30m",
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
fn resolves_runtime_file_watch_glob() {
    let _env = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let _root = EnvGuard::set(RUNTIME_ROOT_ENV, ".fkst/runtime");
    let dir = write_repo(
        &[],
        &[(
            "inbox_watch",
            r#"return { type = "file_watch", glob = "runtime://pipeline/inbox/*.md", produces = "pipeline_request" }"#,
        )],
    );
    let cfg = load(dir.path()).unwrap();
    match cfg.raiser.get("inbox_watch").unwrap() {
        RaiserDecl::FileWatch { glob, produces } => {
            assert_eq!(
                glob,
                &dir.path()
                    .canonicalize()
                    .unwrap()
                    .join(".fkst/runtime/pipeline/inbox/*.md")
                    .to_string_lossy()
                    .into_owned()
            );
            assert_eq!(produces, "pipeline_request");
        }
        _ => panic!("expected FileWatch"),
    }
}

#[test]
fn graph_scan_rejects_fkst_paths_global() {
    let _env = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let _root = EnvGuard::set(RUNTIME_ROOT_ENV, ".fkst/runtime");
    let dir = write_repo(
        &[(
            "runtime_paths",
            r#"
local M = {}
assert(fkst_paths == nil, "fkst_paths graph-scan global must not be injected")
local _ = fkst_paths.runtime_root()
M.spec = { consumes = {"tick"}, timeout = "30s" }
function pipeline(_) end
return M
"#,
        )],
        &[(
            "runtime_watch",
            r#"return { type = "file_watch", glob = "runtime://pipeline/*/meta.md", produces = "tick" }"#,
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
fn runtime_logs_file_watch_fails_closed() {
    let _env = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let _root = EnvGuard::set(RUNTIME_ROOT_ENV, ".fkst/runtime");
    let dir = write_repo(
        &[],
        &[(
            "logs_watch",
            r#"return { type = "file_watch", glob = "runtime://logs/github-publisher/outbox/*.md", produces = "tick" }"#,
        )],
    );

    let err = load(dir.path()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("runtime://logs"), "got: {msg}");
    assert!(msg.contains("local-only"), "got: {msg}");
}

#[test]
fn unknown_runtime_file_watch_kind_fails_closed() {
    let _env = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let _root = EnvGuard::set(RUNTIME_ROOT_ENV, ".fkst/runtime");
    let dir = write_repo(
        &[],
        &[(
            "unknown_watch",
            r#"return { type = "file_watch", glob = "runtime://unknown/inbox/*.md", produces = "tick" }"#,
        )],
    );

    let err = load(dir.path()).unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("unknown runtime kind"), "got: {msg}");
}

#[test]
fn resolves_runtime_file_watch_glob_with_out_of_tree_root() {
    let _env = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let runtime = TempDir::new().unwrap();
    let _root = EnvGuard::set(RUNTIME_ROOT_ENV, runtime.path());
    let dir = write_repo(
        &[],
        &[
            (
                "pipeline_watch",
                r#"return { type = "file_watch", glob = "runtime://pipeline/inbox/*.md", produces = "pipeline_request" }"#,
            ),
            (
                "mailbox_watch",
                r#"return { type = "file_watch", glob = "runtime://mailbox/threads/*/comments/*-human-issue.md", produces = "triage_request" }"#,
            ),
        ],
    );

    let cfg = load(dir.path()).unwrap();
    match cfg.raiser.get("pipeline_watch").unwrap() {
        RaiserDecl::FileWatch { glob, produces } => {
            assert_eq!(
                glob,
                &runtime
                    .path()
                    .join("pipeline/inbox/*.md")
                    .to_string_lossy()
                    .into_owned()
            );
            assert_eq!(produces, "pipeline_request");
        }
        _ => panic!("expected FileWatch"),
    }
    match cfg.raiser.get("mailbox_watch").unwrap() {
        RaiserDecl::FileWatch { glob, produces } => {
            assert_eq!(
                glob,
                &runtime
                    .path()
                    .join("mailbox/threads/*/comments/*-human-issue.md")
                    .to_string_lossy()
                    .into_owned()
            );
            assert_eq!(produces, "triage_request");
        }
        _ => panic!("expected FileWatch"),
    }
}

#[test]
fn skips_dept_without_main_lua() {
    let dir = TempDir::new().unwrap();
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
            consumes: vec!["tick".into()],
            produces: Vec::new(),
            timeout: "30s".into(),
        },
        &path,
    )
    .unwrap();
    let err = graph_scan::insert_department_decl(
        &mut departments,
        "alpha",
        DepartmentDecl {
            lua: path.clone(),
            consumes: vec!["tick".into()],
            produces: Vec::new(),
            timeout: "30s".into(),
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
    let err = graph_scan::insert_raiser_decl(
        &mut raisers,
        "cron_a",
        RaiserDecl::Cron {
            interval: "20s".into(),
            produces: "tick".into(),
        },
        &path,
    )
    .unwrap_err();
    let msg = format!("{:#}", err);

    assert!(msg.contains("duplicate raiser `cron_a`"), "got: {}", msg);
}

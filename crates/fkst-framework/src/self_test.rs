//! Framework-local startup self-test.

use anyhow::{Context, Result};
use fkst_common::config::{Config, DepartmentDecl, LimitsDecl, QueueDecl, RaiserDecl};
use fkst_common::validation::validate;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::path_resolver::NameResolver;
use crate::raise::RaiseBuffer;

pub struct Failure {
    class: &'static str,
    source: anyhow::Error,
}

impl Failure {
    pub fn class(&self) -> &'static str {
        self.class
    }

    pub fn source(&self) -> &anyhow::Error {
        &self.source
    }
}

pub fn run() -> std::result::Result<(), Failure> {
    check_validation().map_err(|source| Failure {
        class: "schema-validation",
        source,
    })?;
    let host_root = self_test_host_root().map_err(|source| Failure {
        class: "self-test-host-root",
        source,
    })?;
    check_sdk_registration(&host_root).map_err(|source| Failure {
        class: "sdk-registration",
        source,
    })?;
    let cwd = std::env::current_dir().map_err(|source| Failure {
        class: "self-test-host-root",
        source: anyhow::Error::new(source),
    })?;
    crate::sdk_codex::self_test_permit_pool(&cwd).map_err(|source| Failure {
        class: "permit-pool",
        source: anyhow::Error::msg(source.to_string()),
    })?;
    Ok(())
}

fn self_test_host_root() -> Result<PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "fkst-framework-self-test-host-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create temporary self-test host root {}", root.display()))?;
    Ok(root)
}

fn check_validation() -> Result<()> {
    validate_minimal_config()
}

fn validate_minimal_config() -> Result<()> {
    let root = std::env::temp_dir().join(format!(
        "fkst-framework-self-test-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create temporary validation root {}", root.display()))?;
    let lua_rel = PathBuf::from("self_test_department.lua");
    std::fs::write(root.join(&lua_rel), "-- self-test department\n")
        .with_context(|| format!("write temporary lua file {}", lua_rel.display()))?;

    let cfg = minimal_config(lua_rel, &root);
    let result = validate(&cfg, &root).context("validate minimal in-memory config");
    let cleanup = std::fs::remove_dir_all(&root)
        .with_context(|| format!("remove temporary validation root {}", root.display()));
    result.and(cleanup)
}

fn minimal_config(lua_rel: PathBuf, owner_root: &std::path::Path) -> Config {
    let mut queue = BTreeMap::new();
    queue.insert(
        "self_test".into(),
        QueueDecl {
            capacity: 1,
            fanout: false,
        },
    );

    let mut raiser = BTreeMap::new();
    raiser.insert(
        "self_test_cron".into(),
        RaiserDecl::Cron {
            interval: "60s".into(),
            produces: "self_test".into(),
        },
    );

    let mut department = BTreeMap::new();
    department.insert(
        "self_test_department".into(),
        DepartmentDecl {
            lua: lua_rel,
            owner_root: owner_root.to_path_buf(),
            owner_namespace: "pkg".to_string(),
            consumes: vec!["self_test".into()],
            produces: vec![],
            ephemeral: vec![],
            stall_window: "30s".into(),
            graph_json: false,
            retry: None,
        },
    );

    Config {
        queue,
        raiser,
        department,
        limits: LimitsDecl {
            global_codex_processes: 1,
        },
    }
}

// the self-test pins the fixed `file.read`, `file.write`, and `file.exists` table shape.
fn check_sdk_registration(host_root: &std::path::Path) -> Result<()> {
    let lua = crate::mlua_init::new_lua();
    crate::mlua_init::register_framework_sdk(
        &lua,
        RaiseBuffer::new(),
        host_root,
        host_root,
        Some("pkg.self_test".to_string()),
        NameResolver::new(["pkg".to_string()]),
        "pkg".to_string(),
        None,
        false,
    )
    .context("register framework SDK")?;
    lua.load(
        r#"
        local function expect_type(name, value, expected)
            local actual = type(value)
            if actual ~= expected then
                error(name .. " expected " .. expected .. ", got " .. actual)
            end
        end

        local function expect_nil(name, value)
            if value ~= nil then
                error(name .. " expected nil, got " .. type(value))
            end
        end

        expect_type("log", log, "table")
        expect_type("log.info", log.info, "function")
        expect_type("log.warn", log.warn, "function")
        expect_type("log.error", log.error, "function")
        expect_type("now", now, "function")
        expect_type("truncate_utf8", truncate_utf8, "function")
        expect_type("graph_json", graph_json, "function")
        expect_type("t", t, "function")
        expect_type("exec_sync", exec_sync, "function")
        expect_type("file", file, "table")
        expect_type("file.read", file.read, "function")
        expect_type("file.write", file.write, "function")
        expect_type("file.exists", file.exists, "function")
        expect_type("with_lock", with_lock, "function")
        expect_type("once", once, "function")
        expect_type("setup_worktree", setup_worktree, "function")
        expect_type("git_log_count", git_log_count, "function")
        expect_type("git_log_grep", git_log_grep, "function")
        expect_type("count_worktrees", count_worktrees, "function")
        expect_type("list_orphan_worktrees", list_orphan_worktrees, "function")
        expect_type("spawn_codex", spawn_codex, "function")
        expect_type("spawn_codex_sync", spawn_codex_sync, "function")
        expect_type("fkst", fkst, "table")
        expect_type("fkst.codex_runs", fkst.codex_runs, "function")
        expect_type("await_all", await_all, "function")
        expect_type("raise", raise, "function")
        expect_nil("codex_status", codex_status)
        expect_nil("await_any", await_any)
        expect_nil("await", await)
        expect_nil("sleep", sleep)
        expect_nil("read_file", read_file)
        expect_nil("write_file", write_file)
        expect_nil("path_exists", path_exists)
        expect_type("json", json, "table")
        expect_type("json.decode", json.decode, "function")
        expect_nil("json.encode", json.encode)
        for json_key in pairs(json) do
            if json_key ~= "decode" then
                error("json table has unexpected key: " .. tostring(json_key))
            end
        end
        expect_nil("notify_human", notify_human)
        "#,
    )
    .exec()
    .context("verify registered SDK globals")?;
    Ok(())
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

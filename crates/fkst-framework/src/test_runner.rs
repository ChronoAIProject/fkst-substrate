use anyhow::{Context, Result};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::external_command::{
    CommandCassetteMode, CommandCassetteOptions, CommandCassetteRedaction, MockCommandResult,
    MockCommandState,
};
use crate::lua_coverage::LuaCoverage;
use crate::manifest::UnitCatalog;
use crate::path_resolver::PackageRoots;
use crate::raise::RaiseBuffer;
use crate::sdk_observe::MockObserveState;
use crate::test_department_env::{DeptRunEnvGuard, DeptRunOptions};

pub(crate) fn run_tests(
    roots: PackageRoots,
    report_json: Option<PathBuf>,
    coverage_dir: Option<PathBuf>,
) -> Result<i32> {
    // NOTE: do NOT scrub the engine binary path env (BIN/...) here. run_tests is the
    // TEST HARNESS process, not a package runtime: integration tests legitimately read
    // BIN to spawn the engine as a subprocess (e.g. github-devloop-integration's
    // branch-scan serialization test). #191's intent ("package runtime must not receive
    // BIN") is enforced for real department runtimes by the spawner scrub
    // (cmd.env_remove, covered by spawn_framework_removes_engine_binary_path_env) and by
    // run_pipeline's scrub for `run` mode — scrubbing the whole test process here was
    // over-broad and broke harness-level engine spawning.
    let files = discover_test_files(&roots)?;
    let _supervisor_pid_guard = TestModeSupervisorPidGuard::remove();
    let cache = TestRunCache::new(roots.clone())?;
    let coverage = coverage_dir
        .as_ref()
        .map(|_| {
            LuaCoverage::new(
                roots
                    .graph_roots()
                    .into_iter()
                    .map(|root| root.root)
                    .collect::<Vec<_>>(),
            )
        })
        .transpose()
        .context("initialize Lua coverage")?;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut report = TestReport::new();

    for file in files {
        let relpath = display_path(&file.path, &file.owner_root);
        let mock_commands = MockCommandState::new();
        let mock_observe = MockObserveState::new();
        let lua = crate::mlua_init::new_lua();
        crate::mlua_init::register_framework_sdk_with_runner(
            &lua,
            RaiseBuffer::new(),
            roots.host_root(),
            &file.owner_root,
            None,
            roots.name_resolver(),
            file.owner_namespace.clone(),
            crate::raise::RaiseAuthority::new(Default::default()),
            Some(mock_commands.clone()),
            Some(roots.clone()),
            false,
            None,
            Some(mock_observe.clone()),
            crate::capabilities::CapabilityMode::Full,
        )
        .with_context(|| format!("register SDK for {}", relpath))?;
        register_test_sdk(
            &lua,
            cache.clone(),
            roots.clone(),
            file.owner_root.clone(),
            file.owner_namespace.clone(),
            mock_commands.clone(),
            mock_observe.clone(),
            coverage.clone(),
        )
        .with_context(|| format!("register fkst.test for {}", relpath))?;
        if let Some(coverage) = &coverage {
            coverage
                .install(&lua)
                .with_context(|| format!("install coverage hook for {}", relpath))?;
        }
        let owner_unit = cache
            .owner_unit_for_root(&file.owner_root)
            .with_context(|| format!("resolve owner unit for {}", relpath))?;

        match load_test_table(
            &lua,
            &file.path,
            &file.owner_root,
            cache.catalog_for_root(&file.owner_root)?,
            &owner_unit,
        ) {
            Ok(tests) => {
                for (name, func) in tests {
                    mock_commands
                        .reset()
                        .with_context(|| format!("reset mock commands for {relpath}::{name}"))?;
                    mock_observe
                        .reset()
                        .with_context(|| format!("reset mock observe for {relpath}::{name}"))?;
                    match func.call::<()>(()) {
                        Ok(()) => {
                            println!("PASS {relpath}::{name}");
                            passed += 1;
                            report.push_pass(&file.owner_namespace, &relpath, &name);
                        }
                        Err(err) => {
                            println!("FAIL {relpath}::{name}: {err}");
                            failed += 1;
                            report.push_fail(
                                &file.owner_namespace,
                                &relpath,
                                &name,
                                err.to_string(),
                            );
                        }
                    }
                }
            }
            Err(err) => {
                println!("FAIL {relpath}::<load>: {err:#}");
                failed += 1;
                report.push_fail(
                    &file.owner_namespace,
                    &relpath,
                    "<load>",
                    format!("{err:#}"),
                );
            }
        }
    }

    println!("{passed} passed, {failed} failed");
    report.summary = TestReportSummary { passed, failed };
    if let Some(path) = report_json {
        write_report_json(&path, &report)
            .with_context(|| format!("write test report {}", path.display()))?;
    }
    if let (Some(path), Some(coverage)) = (coverage_dir, coverage) {
        coverage
            .write_outputs(&path)
            .with_context(|| format!("write coverage outputs {}", path.display()))?;
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

#[derive(Debug, Serialize)]
struct TestReport {
    schema: &'static str,
    summary: TestReportSummary,
    tests: Vec<TestReportEntry>,
}

impl TestReport {
    fn new() -> Self {
        Self {
            schema: "fkst.test.report.v1",
            summary: TestReportSummary {
                passed: 0,
                failed: 0,
            },
            tests: Vec::new(),
        }
    }

    fn push_pass(&mut self, owner_namespace: &str, file: &str, name: &str) {
        self.tests.push(TestReportEntry {
            owner_namespace: owner_namespace.to_string(),
            file: file.to_string(),
            name: name.to_string(),
            status: TestReportStatus::Pass,
            error: None,
        });
    }

    fn push_fail(&mut self, owner_namespace: &str, file: &str, name: &str, error: String) {
        self.tests.push(TestReportEntry {
            owner_namespace: owner_namespace.to_string(),
            file: file.to_string(),
            name: name.to_string(),
            status: TestReportStatus::Fail,
            error: Some(error),
        });
    }
}

#[derive(Debug, Serialize)]
struct TestReportSummary {
    passed: usize,
    failed: usize,
}

#[derive(Debug, Serialize)]
struct TestReportEntry {
    owner_namespace: String,
    file: String,
    name: String,
    status: TestReportStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum TestReportStatus {
    Pass,
    Fail,
}

fn write_report_json(path: &Path, report: &TestReport) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("report path has no file name"))?;
    let tmp_path = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let data = serde_json::to_vec_pretty(report)?;
    std::fs::write(&tmp_path, data)
        .with_context(|| format!("write temporary report {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[derive(Clone, Debug)]
struct TestFile {
    path: PathBuf,
    owner_root: PathBuf,
    owner_namespace: String,
}

fn discover_test_files(roots: &PackageRoots) -> Result<Vec<TestFile>> {
    let mut files = BTreeMap::<PathBuf, TestFile>::new();
    for root in roots.graph_roots() {
        collect_department_tests(&root.root, &root.namespace, &mut files)
            .with_context(|| format!("scan department tests in {}", root.root.display()))?;
        collect_top_tests(&root.root, &root.namespace, &mut files)
            .with_context(|| format!("scan top-level tests in {}", root.root.display()))?;
    }
    Ok(files.into_values().collect())
}

fn collect_department_tests(
    root: &Path,
    namespace: &str,
    files: &mut BTreeMap<PathBuf, TestFile>,
) -> Result<()> {
    let departments = root.join("departments");
    if !departments.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&departments)
        .with_context(|| format!("read {}", departments.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
            let file = file?;
            let path = file.path();
            if path.is_file() && is_test_file(&path) {
                insert_test_file(files, root, namespace, &path)?;
            }
        }
    }
    Ok(())
}

fn collect_top_tests(
    root: &Path,
    namespace: &str,
    files: &mut BTreeMap<PathBuf, TestFile>,
) -> Result<()> {
    let tests = root.join("tests");
    if !tests.exists() {
        return Ok(());
    }
    for file in std::fs::read_dir(&tests).with_context(|| format!("read {}", tests.display()))? {
        let file = file?;
        let path = file.path();
        if path.is_file() && is_test_file(&path) {
            insert_test_file(files, root, namespace, &path)?;
        }
    }
    Ok(())
}

fn insert_test_file(
    files: &mut BTreeMap<PathBuf, TestFile>,
    owner_root: &Path,
    owner_namespace: &str,
    path: &Path,
) -> Result<()> {
    let canonical_path = path.canonicalize()?;
    files.insert(
        canonical_path.clone(),
        TestFile {
            path: canonical_path,
            owner_root: owner_root.to_path_buf(),
            owner_namespace: owner_namespace.to_string(),
        },
    );
    Ok(())
}

fn is_test_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with("_test.lua"))
}

fn load_test_table(
    lua: &Lua,
    file: &Path,
    owner_root: &Path,
    catalog: Arc<UnitCatalog>,
    owner_unit: &str,
) -> Result<Vec<(String, Function)>> {
    let value = crate::lua_require::load_unit_chunk(
        lua,
        catalog,
        owner_unit,
        file,
        crate::lua_coverage::chunk_name(file, owner_root),
        None,
    )
    .with_context(|| format!("eval {}", file.display()))?;
    let Value::Table(table) = value else {
        anyhow::bail!("{} did not return a table", file.display());
    };
    let mut tests = BTreeMap::<String, Function>::new();
    for pair in table.pairs::<String, Value>() {
        let (name, value) = pair?;
        if !name.starts_with("test_") {
            continue;
        }
        let Value::Function(func) = value else {
            anyhow::bail!("{} is not a function", name);
        };
        tests.insert(name, func);
    }
    Ok(tests.into_iter().collect())
}

fn register_test_sdk(
    lua: &Lua,
    cache: TestRunCache,
    roots: PackageRoots,
    owner_root: PathBuf,
    owner_namespace: String,
    mock_commands: MockCommandState,
    mock_observe: MockObserveState,
    coverage: Option<LuaCoverage>,
) -> mlua::Result<()> {
    let globals = lua.globals();
    let fkst = match globals.get::<Value>("fkst")? {
        Value::Table(table) => table,
        Value::Nil => lua.create_table()?,
        _ => {
            return Err(mlua::Error::runtime(
                "global fkst exists and is not a table",
            ))
        }
    };
    let test = lua.create_table()?;
    crate::test_assertions::register(lua, &test)?;
    test.set("mock_command", {
        let mock_commands = mock_commands.clone();
        lua.create_function(move |_, (pattern, result): (String, Table)| {
            let stdout = result.get::<Option<String>>("stdout")?.unwrap_or_default();
            let stderr = result.get::<Option<String>>("stderr")?.unwrap_or_default();
            let exit_code = result.get::<Option<i32>>("exit_code")?.unwrap_or(0);
            mock_commands.push_mock(
                pattern,
                MockCommandResult {
                    stdout,
                    stderr,
                    exit_code,
                },
            )
        })?
    })?;
    test.set("command_calls", {
        let mock_commands = mock_commands.clone();
        lua.create_function(move |lua, ()| {
            let calls = lua.create_table()?;
            for (idx, call) in mock_commands.calls()?.into_iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("rendered", call.rendered)?;
                entry.set("program", call.program)?;
                let args = lua.create_table()?;
                for (arg_idx, arg) in call.args.into_iter().enumerate() {
                    args.set(arg_idx + 1, arg)?;
                }
                entry.set("args", args)?;
                entry.set("stdin", call.stdin)?;
                if let Some(cwd) = call.cwd {
                    entry.set("cwd", cwd)?;
                }
                let env = lua.create_table()?;
                for (env_idx, (key, value)) in call.env.into_iter().enumerate() {
                    let pair = lua.create_table()?;
                    pair.set("key", key)?;
                    pair.set("value", value)?;
                    env.set(env_idx + 1, pair)?;
                }
                entry.set("env", env)?;
                entry.set("stdout", call.stdout)?;
                entry.set("stderr", call.stderr)?;
                entry.set("exit_code", call.exit_code)?;
                calls.set(idx + 1, entry)?;
            }
            Ok(calls)
        })?
    })?;
    test.set("with_command_cassette", {
        let mock_commands = mock_commands.clone();
        let owner_root = owner_root.clone();
        lua.create_function(move |_, (opts, func): (Table, Function)| {
            let cassette = CommandCassetteOptions::from_lua(&owner_root, opts)?;
            mock_commands.start_cassette(cassette)?;
            let result = func.call::<Value>(());
            match result {
                Ok(value) => {
                    mock_commands.finish_cassette()?;
                    Ok(value)
                }
                Err(err) => {
                    let _ = mock_commands.abort_cassette();
                    Err(err)
                }
            }
        })?
    })?;
    test.set("run_department", {
        let mock_commands = mock_commands.clone();
        let mock_observe = mock_observe.clone();
        let cache = cache.clone();
        let roots = roots.clone();
        let owner_namespace = owner_namespace.clone();
        let coverage = coverage.clone();
        lua.create_function(
            move |lua, (path, event, opts): (String, Value, Option<Table>)| {
                run_department(
                    lua,
                    &cache,
                    &roots,
                    &owner_root,
                    &owner_namespace,
                    mock_commands.clone(),
                    mock_observe.clone(),
                    path,
                    event,
                    opts,
                    coverage.clone(),
                )
            },
        )?
    })?;
    crate::test_fire_raiser::register(
        lua,
        cache.clone(),
        roots.clone(),
        owner_namespace.clone(),
        mock_commands.clone(),
        mock_observe.clone(),
        coverage.clone(),
        &test,
    )?;
    crate::test_runtime::register(
        lua,
        cache,
        roots,
        owner_namespace,
        mock_commands,
        mock_observe.clone(),
        coverage,
        &test,
    )?;
    crate::sdk_observe::register_test(lua, &test, mock_observe)?;
    fkst.set("test", test)?;
    globals.set("fkst", fkst)?;
    Ok(())
}

impl CommandCassetteOptions {
    fn from_lua(owner_root: &Path, opts: Table) -> mlua::Result<Self> {
        let path: String = opts.get("path")?;
        let mode: String = opts.get("mode")?;
        let mode = match mode.as_str() {
            "replay" => CommandCassetteMode::Replay,
            "record" => CommandCassetteMode::Record,
            other => {
                return Err(mlua::Error::external(format!(
                    "unsupported VCR command cassette mode: {other}"
                )))
            }
        };
        let redactions = parse_command_cassette_redactions(opts.get::<Option<Table>>("redact")?)?;
        Ok(Self {
            path: resolve_department_path(owner_root, &path),
            mode,
            redactions,
        })
    }
}

fn parse_command_cassette_redactions(
    table: Option<Table>,
) -> mlua::Result<Vec<CommandCassetteRedaction>> {
    let Some(table) = table else {
        return Ok(Vec::new());
    };
    let mut redactions = Vec::new();
    for value in table.sequence_values::<Table>() {
        let entry = value?;
        let value: String = entry.get("value")?;
        if value.is_empty() {
            return Err(mlua::Error::external(
                "VCR command cassette redaction value must not be empty",
            ));
        }
        let replacement = entry
            .get::<Option<String>>("replacement")?
            .unwrap_or_else(|| "<REDACTED>".to_string());
        redactions.push(CommandCassetteRedaction { value, replacement });
    }
    Ok(redactions)
}

pub(crate) fn run_department(
    lua: &Lua,
    cache: &TestRunCache,
    roots: &PackageRoots,
    owner_root: &Path,
    owner_namespace: &str,
    mock_commands: MockCommandState,
    mock_observe: MockObserveState,
    path: String,
    event: Value,
    opts: Option<Table>,
    coverage: Option<LuaCoverage>,
) -> mlua::Result<Table> {
    let opts = DeptRunOptions::from_lua(opts)?;
    let lua_path = resolve_department_path(owner_root, &path);
    let event_json: serde_json::Value = lua.from_value(event)?;
    let outcome = run_department_core(
        cache,
        roots,
        owner_root,
        owner_namespace,
        mock_commands,
        mock_observe,
        &lua_path,
        event_json,
        opts,
        coverage,
    )?;
    department_run_table(lua, outcome)
}

pub(crate) struct DepartmentRunOutcome {
    pub(crate) exit_code: i32,
    pub(crate) error: Option<String>,
    pub(crate) raises: Vec<(String, JsonValue)>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_department_core(
    cache: &TestRunCache,
    roots: &PackageRoots,
    owner_root: &Path,
    owner_namespace: &str,
    mock_commands: MockCommandState,
    mock_observe: MockObserveState,
    lua_path: &Path,
    event_json: JsonValue,
    opts: DeptRunOptions,
    coverage: Option<LuaCoverage>,
) -> mlua::Result<DepartmentRunOutcome> {
    let _guard = DeptRunEnvGuard::apply(opts)?;
    let owner_unit = cache.owner_unit_for_root(owner_root)?;
    let graph_json_authorized =
        crate::sdk_graph::department_authorized(roots, owner_root, &lua_path).unwrap_or(false);
    let qualified_consumes = cache.declared_qualified_consumes(owner_root, &lua_path)?;
    let event_json = normalize_run_department_event_queue(
        roots,
        owner_namespace,
        event_json,
        qualified_consumes,
    )
    .map_err(mlua::Error::external)?;
    let declared_produces =
        cache.declared_resolved_produces(owner_root, owner_namespace, &lua_path)?;

    let dept_lua = crate::mlua_init::new_lua();
    let raise_buf = RaiseBuffer::new();
    crate::mlua_init::register_framework_sdk_with_runner(
        &dept_lua,
        raise_buf.clone(),
        roots.host_root(),
        owner_root,
        department_name_for_lua(&lua_path, owner_root, owner_namespace),
        roots
            .name_resolver()
            .with_recorded_only_queues(declared_produces.clone()),
        owner_namespace.to_string(),
        crate::raise::RaiseAuthority::new(declared_produces),
        Some(mock_commands),
        Some(roots.clone()),
        graph_json_authorized,
        None,
        Some(mock_observe),
        crate::capabilities::CapabilityMode::Full,
    )?;
    if let Some(coverage) = &coverage {
        coverage.install(&dept_lua)?;
    }

    let chunk_cache = if coverage.is_some() {
        None
    } else {
        Some(cache.lua_chunk_cache())
    };
    let (exit_code, error) =
        match crate::mlua_init::run_dept_with_package_path_chunk_cache_and_name_root(
            &dept_lua,
            &lua_path,
            &event_json,
            chunk_cache,
            owner_root,
            cache.catalog_for_root(owner_root)?,
            &owner_unit,
        ) {
            Ok(()) => (0, None),
            Err(err) => {
                let message = format!("{err:#}");
                eprintln!(
                    "[framework:test] department failed {}: {message}",
                    lua_path.display()
                );
                (1, Some(message))
            }
        };

    Ok(DepartmentRunOutcome {
        exit_code,
        error,
        raises: raise_buf.snapshot(),
    })
}

pub(crate) fn department_run_table(
    lua: &Lua,
    outcome: DepartmentRunOutcome,
) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    result.set("exit_code", outcome.exit_code)?;
    if let Some(error) = outcome.error {
        result.set("error", error)?;
    }
    let raises = lua.create_table()?;
    for (idx, (queue, payload)) in outcome.raises.into_iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set("queue", queue)?;
        entry.set("payload", lua.to_value(&payload)?)?;
        raises.set(idx + 1, entry)?;
    }
    result.set("raises", raises)?;
    Ok(result)
}

#[derive(Clone)]
pub(crate) struct TestRunCache {
    roots: PackageRoots,
    inner: Arc<Mutex<TestRunCacheInner>>,
    lua_chunks: crate::mlua_init::LuaChunkCache,
}

#[derive(Default)]
struct TestRunCacheInner {
    owner_units: BTreeMap<PathBuf, String>,
    declared_produces: BTreeMap<PathBuf, BTreeSet<String>>,
    declared_consumes: BTreeMap<PathBuf, BTreeSet<String>>,
    #[cfg(test)]
    stats: TestRunCacheStats,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TestRunCacheStats {
    pub(crate) owner_unit_misses: usize,
    pub(crate) declared_produces_misses: usize,
    pub(crate) declared_consumes_misses: usize,
}

impl TestRunCache {
    pub(crate) fn new(roots: PackageRoots) -> Result<Self> {
        Ok(Self {
            roots,
            inner: Arc::new(Mutex::new(TestRunCacheInner::default())),
            lua_chunks: crate::mlua_init::LuaChunkCache::default(),
        })
    }

    fn catalog_for_root(&self, owner_root: &Path) -> mlua::Result<Arc<UnitCatalog>> {
        Ok(Arc::new(
            self.roots
                .catalog_for_owner_root(owner_root)
                .map_err(mlua::Error::external)?
                .clone(),
        ))
    }

    fn owner_unit_for_root(&self, owner_root: &Path) -> mlua::Result<String> {
        let owner_root = owner_root.canonicalize().map_err(mlua::Error::external)?;
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| mlua::Error::runtime("test run cache lock poisoned"))?;
        if let Some(unit) = inner.owner_units.get(&owner_root) {
            return Ok(unit.clone());
        }
        #[cfg(test)]
        {
            inner.stats.owner_unit_misses += 1;
        }
        drop(inner);
        let unit = self
            .roots
            .catalog_for_owner_root(&owner_root)
            .map_err(mlua::Error::external)?
            .unit_name_for_root(&owner_root)
            .map_err(mlua::Error::external)?
            .ok_or_else(|| {
                mlua::Error::external(format!("no manifest unit owns {}", owner_root.display()))
            })?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| mlua::Error::runtime("test run cache lock poisoned"))?;
        inner.owner_units.insert(owner_root, unit.clone());
        Ok(unit)
    }

    fn declared_resolved_produces(
        &self,
        owner_root: &Path,
        owner_namespace: &str,
        lua_path: &Path,
    ) -> mlua::Result<BTreeSet<String>> {
        let lua_path = lua_path.canonicalize().map_err(mlua::Error::external)?;
        {
            let inner = self
                .inner
                .lock()
                .map_err(|_| mlua::Error::runtime("test run cache lock poisoned"))?;
            if let Some(produces) = inner.declared_produces.get(&lua_path) {
                return Ok(produces.clone());
            }
        }
        let owner_unit = self.owner_unit_for_root(owner_root)?;
        let produces = crate::spec_queues::declared_resolved_produces(
            &self.roots,
            owner_namespace,
            owner_root,
            &lua_path,
            self.catalog_for_root(owner_root)?,
            &owner_unit,
            self.lua_chunk_cache(),
        )?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| mlua::Error::runtime("test run cache lock poisoned"))?;
        #[cfg(test)]
        {
            inner.stats.declared_produces_misses += 1;
        }
        inner.declared_produces.insert(lua_path, produces.clone());
        Ok(produces)
    }

    fn declared_qualified_consumes(
        &self,
        owner_root: &Path,
        lua_path: &Path,
    ) -> mlua::Result<BTreeSet<String>> {
        let lua_path = lua_path.canonicalize().map_err(mlua::Error::external)?;
        {
            let inner = self
                .inner
                .lock()
                .map_err(|_| mlua::Error::runtime("test run cache lock poisoned"))?;
            if let Some(consumes) = inner.declared_consumes.get(&lua_path) {
                return Ok(consumes.clone());
            }
        }
        let owner_unit = self.owner_unit_for_root(owner_root)?;
        let consumes = declared_qualified_consumes(
            owner_root,
            &lua_path,
            self.catalog_for_root(owner_root)?,
            &owner_unit,
            self.lua_chunk_cache(),
        )?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| mlua::Error::runtime("test run cache lock poisoned"))?;
        #[cfg(test)]
        {
            inner.stats.declared_consumes_misses += 1;
        }
        inner.declared_consumes.insert(lua_path, consumes.clone());
        Ok(consumes)
    }

    pub(crate) fn lua_chunk_cache(&self) -> &crate::mlua_init::LuaChunkCache {
        &self.lua_chunks
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> TestRunCacheStats {
        self.inner.lock().unwrap().stats
    }
}

fn declared_qualified_consumes(
    owner_root: &Path,
    lua_path: &Path,
    catalog: Arc<UnitCatalog>,
    owner_unit: &str,
    chunk_cache: &crate::mlua_init::LuaChunkCache,
) -> mlua::Result<BTreeSet<String>> {
    crate::spec_queues::declared_qualified_spec_queues(
        owner_root,
        lua_path,
        catalog,
        owner_unit,
        chunk_cache,
        "consumes",
    )
}

fn normalize_run_department_event_queue(
    roots: &PackageRoots,
    owner_namespace: &str,
    mut event: JsonValue,
    qualified_consumes: BTreeSet<String>,
) -> Result<JsonValue> {
    let Some(raw_queue) = event.get("queue").and_then(JsonValue::as_str) else {
        return Ok(event);
    };
    let dead_letter_queue = roots
        .name_resolver()
        .resolve(owner_namespace, "dead_letter")
        .context("resolve production dead_letter queue")?;
    let resolver = roots
        .name_resolver()
        .with_recorded_only_queues(qualified_consumes)
        .add_recorded_only_queue(crate::supervise::failure_fact::FAILURE_FACT_QUEUE)
        .add_recorded_only_queue(dead_letter_queue);
    let resolved = resolver
        .resolve(owner_namespace, raw_queue)
        .with_context(|| format!("resolve test event.queue `{raw_queue}`"))?;
    if let Some(object) = event.as_object_mut() {
        object.insert("queue".to_string(), JsonValue::String(resolved));
    }
    Ok(event)
}

fn department_name_for_lua(
    lua_path: &Path,
    owner_root: &Path,
    owner_namespace: &str,
) -> Option<String> {
    let rel = lua_path.strip_prefix(owner_root).ok()?;
    let mut components = rel.components();
    let first = components.next()?.as_os_str();
    let name = components.next()?.as_os_str().to_str()?;
    let last = components.next()?.as_os_str();
    if components.next().is_some()
        || first != std::ffi::OsStr::new("departments")
        || last != std::ffi::OsStr::new("main.lua")
    {
        return None;
    }
    if owner_namespace == crate::path_resolver::HOST_NAMESPACE {
        Some(name.to_string())
    } else {
        Some(format!("{owner_namespace}.{name}"))
    }
}

fn resolve_department_path(package_root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        package_root.join(path)
    }
}

struct TestModeSupervisorPidGuard {
    previous: Option<std::ffi::OsString>,
}

impl TestModeSupervisorPidGuard {
    fn remove() -> Self {
        let previous = std::env::var_os("FKST_SUPERVISOR_PID");
        std::env::remove_var("FKST_SUPERVISOR_PID");
        Self { previous }
    }
}

impl Drop for TestModeSupervisorPidGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var("FKST_SUPERVISOR_PID", value);
        }
    }
}

fn display_path(path: &Path, owner_root: &Path) -> String {
    path.strip_prefix(owner_root)
        .unwrap_or(path)
        .to_string_lossy()
        .trim_start_matches(std::path::MAIN_SEPARATOR)
        .to_string()
}

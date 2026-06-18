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
use crate::path_resolver::PackageRoots;
use crate::raise::RaiseBuffer;

pub(crate) fn run_tests(
    roots: PackageRoots,
    report_json: Option<PathBuf>,
    coverage_dir: Option<PathBuf>,
) -> Result<i32> {
    let files = discover_test_files(&roots)?;
    let _supervisor_pid_guard = TestModeSupervisorPidGuard::remove();
    let cache = TestRunCache::new(roots.clone());
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
        let lua = crate::mlua_init::new_lua();
        crate::mlua_init::set_package_roots_path(&lua, [file.owner_root.as_path()])
            .with_context(|| format!("set package.path for tests in {}", relpath))?;
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
        )
        .with_context(|| format!("register SDK for {}", relpath))?;
        register_test_sdk(
            &lua,
            cache.clone(),
            roots.clone(),
            file.owner_root.clone(),
            file.owner_namespace.clone(),
            mock_commands.clone(),
            coverage.clone(),
        )
        .with_context(|| format!("register fkst.test for {}", relpath))?;
        if let Some(coverage) = &coverage {
            coverage
                .install(&lua)
                .with_context(|| format!("install coverage hook for {}", relpath))?;
        }

        match load_test_table(&lua, &file.path, &file.owner_root) {
            Ok(tests) => {
                for (name, func) in tests {
                    mock_commands
                        .reset()
                        .with_context(|| format!("reset mock commands for {relpath}::{name}"))?;
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

fn load_test_table(lua: &Lua, file: &Path, owner_root: &Path) -> Result<Vec<(String, Function)>> {
    let src = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let chunk = lua
        .load(&src)
        .set_name(crate::lua_coverage::chunk_name(file, owner_root));
    let table: Table = chunk
        .eval()
        .with_context(|| format!("eval {}", file.display()))?;
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
    test.set(
        "eq",
        lua.create_function(
            |lua, (actual, expected, msg): (Value, Value, Option<String>)| {
                if lua_values_equal(lua, actual.clone(), expected.clone())? {
                    return Ok(());
                }
                Err(assertion_error(
                    "eq",
                    msg,
                    format!(
                        "expected {}, got {}",
                        display_value(expected),
                        display_value(actual)
                    ),
                ))
            },
        )?,
    )?;
    test.set(
        "is_true",
        lua.create_function(|_, (value, msg): (Value, Option<String>)| {
            if !matches!(value, Value::Nil | Value::Boolean(false)) {
                return Ok(());
            }
            Err(assertion_error(
                "is_true",
                msg,
                format!("got {}", display_value(value)),
            ))
        })?,
    )?;
    test.set(
        "raises",
        lua.create_function(|_, (func, msg): (Function, Option<String>)| {
            match func.call::<()>(()) {
                Ok(()) => Err(assertion_error(
                    "raises",
                    msg,
                    "function did not raise".to_string(),
                )),
                Err(_) => Ok(()),
            }
        })?,
    )?;
    test.set(
        "is_nil",
        lua.create_function(|_, (value, msg): (Value, Option<String>)| {
            if matches!(value, Value::Nil) {
                return Ok(());
            }
            Err(assertion_error(
                "is_nil",
                msg,
                format!("got {}", display_value(value)),
            ))
        })?,
    )?;
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
        let cache = cache.clone();
        lua.create_function(
            move |lua, (path, event, opts): (String, Value, Option<Table>)| {
                run_department(
                    lua,
                    &cache,
                    &roots,
                    &owner_root,
                    &owner_namespace,
                    mock_commands.clone(),
                    path,
                    event,
                    opts,
                    coverage.clone(),
                )
            },
        )?
    })?;
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

fn run_department(
    lua: &Lua,
    cache: &TestRunCache,
    roots: &PackageRoots,
    owner_root: &Path,
    owner_namespace: &str,
    mock_commands: MockCommandState,
    path: String,
    event: Value,
    opts: Option<Table>,
    coverage: Option<LuaCoverage>,
) -> mlua::Result<Table> {
    let opts = DeptRunOptions::from_lua(opts)?;
    let lua_path = resolve_department_path(owner_root, &path);
    let event_json: serde_json::Value = lua.from_value(event)?;
    let _guard = DeptRunEnvGuard::apply(opts)?;
    let require_roots = cache.require_roots_for_owner(owner_root)?;
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
    let package_path = cache.package_path_string(&require_roots)?;

    let dept_lua = crate::mlua_init::new_lua();
    let raise_buf = RaiseBuffer::new();
    crate::mlua_init::set_package_path_string(&dept_lua, &package_path)?;
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
    )?;
    if let Some(coverage) = &coverage {
        coverage.install(&dept_lua)?;
    }

    let chunk_cache = if coverage.is_some() {
        None
    } else {
        Some(cache.lua_chunk_cache())
    };
    let exit_code = match crate::mlua_init::run_dept_with_package_path_chunk_cache_and_name_root(
        &dept_lua,
        &lua_path,
        &event_json,
        chunk_cache,
        owner_root,
    ) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!(
                "[framework:test] department failed {}: {err:#}",
                lua_path.display()
            );
            1
        }
    };

    let result = lua.create_table()?;
    result.set("exit_code", exit_code)?;
    let raises = lua.create_table()?;
    for (idx, (queue, payload)) in raise_buf.snapshot().into_iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set("queue", queue)?;
        entry.set("payload", lua.to_value(&payload)?)?;
        raises.set(idx + 1, entry)?;
    }
    result.set("raises", raises)?;
    Ok(result)
}

#[derive(Clone)]
struct TestRunCache {
    roots: PackageRoots,
    inner: Arc<Mutex<TestRunCacheInner>>,
    lua_chunks: crate::mlua_init::LuaChunkCache,
}

#[derive(Default)]
struct TestRunCacheInner {
    require_roots: BTreeMap<PathBuf, Vec<PathBuf>>,
    package_paths: BTreeMap<Vec<PathBuf>, String>,
    declared_produces: BTreeMap<PathBuf, BTreeSet<String>>,
    declared_consumes: BTreeMap<PathBuf, BTreeSet<String>>,
    #[cfg(test)]
    stats: TestRunCacheStats,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestRunCacheStats {
    require_roots_misses: usize,
    package_path_misses: usize,
    declared_produces_misses: usize,
    declared_consumes_misses: usize,
}

impl TestRunCache {
    fn new(roots: PackageRoots) -> Self {
        Self {
            roots,
            inner: Arc::new(Mutex::new(TestRunCacheInner::default())),
            lua_chunks: crate::mlua_init::LuaChunkCache::default(),
        }
    }

    fn require_roots_for_owner(&self, owner_root: &Path) -> mlua::Result<Vec<PathBuf>> {
        let owner_root = owner_root.canonicalize().map_err(mlua::Error::external)?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| mlua::Error::runtime("test run cache lock poisoned"))?;
        if let Some(roots) = inner.require_roots.get(&owner_root) {
            return Ok(roots.clone());
        }
        #[cfg(test)]
        {
            inner.stats.require_roots_misses += 1;
        }
        let roots = self.roots.require_roots_for_owner(&owner_root);
        inner.require_roots.insert(owner_root, roots.clone());
        Ok(roots)
    }

    fn package_path_string(&self, require_roots: &[PathBuf]) -> mlua::Result<String> {
        let key = require_roots.to_vec();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| mlua::Error::runtime("test run cache lock poisoned"))?;
        if let Some(path) = inner.package_paths.get(&key) {
            return Ok(path.clone());
        }
        #[cfg(test)]
        {
            inner.stats.package_path_misses += 1;
        }
        let path = crate::mlua_init::package_roots_path(require_roots.iter().map(PathBuf::as_path));
        inner.package_paths.insert(key, path.clone());
        Ok(path)
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
        let require_roots = self.require_roots_for_owner(owner_root)?;
        let package_path = self.package_path_string(&require_roots)?;
        let produces = crate::spec_queues::declared_resolved_produces(
            &self.roots,
            owner_namespace,
            owner_root,
            &lua_path,
            &package_path,
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
        let require_roots = self.require_roots_for_owner(owner_root)?;
        let package_path = self.package_path_string(&require_roots)?;
        let consumes = declared_qualified_consumes(
            owner_root,
            &lua_path,
            &package_path,
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

    fn lua_chunk_cache(&self) -> &crate::mlua_init::LuaChunkCache {
        &self.lua_chunks
    }

    #[cfg(test)]
    fn stats(&self) -> TestRunCacheStats {
        self.inner.lock().unwrap().stats
    }
}

fn declared_qualified_consumes(
    owner_root: &Path,
    lua_path: &Path,
    package_path: &str,
    chunk_cache: &crate::mlua_init::LuaChunkCache,
) -> mlua::Result<BTreeSet<String>> {
    crate::spec_queues::declared_qualified_spec_queues(
        owner_root,
        lua_path,
        package_path,
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

#[derive(Debug)]
struct DeptRunOptions {
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    path_prepend: Option<String>,
}

impl DeptRunOptions {
    fn from_lua(opts: Option<Table>) -> mlua::Result<Self> {
        let Some(opts) = opts else {
            return Ok(Self {
                cwd: None,
                env: Vec::new(),
                path_prepend: None,
            });
        };
        let cwd = opts.get::<Option<String>>("cwd")?.map(PathBuf::from);
        let env = match opts.get::<Option<Table>>("env")? {
            Some(env_table) => env_table
                .pairs::<String, String>()
                .collect::<mlua::Result<Vec<(String, String)>>>()?,
            None => Vec::new(),
        };
        let path_prepend = opts.get::<Option<String>>("path_prepend")?;
        Ok(Self {
            cwd,
            env,
            path_prepend,
        })
    }
}

struct DeptRunEnvGuard {
    cwd: Option<PathBuf>,
    env: Vec<(String, Option<std::ffi::OsString>)>,
}

impl DeptRunEnvGuard {
    fn apply(opts: DeptRunOptions) -> mlua::Result<Self> {
        let mut guard = Self {
            cwd: if opts.cwd.is_some() {
                Some(std::env::current_dir().map_err(mlua::Error::external)?)
            } else {
                None
            },
            env: Vec::new(),
        };

        for (key, value) in opts.env {
            guard.env.push((key.clone(), std::env::var_os(&key)));
            std::env::set_var(key, value);
        }

        if let Some(prepend) = opts.path_prepend {
            let key = "PATH".to_string();
            let previous = std::env::var_os(&key);
            let mut paths = vec![PathBuf::from(prepend)];
            if let Some(previous) = &previous {
                paths.extend(std::env::split_paths(previous));
            }
            let next = std::env::join_paths(paths).map_err(mlua::Error::external)?;
            guard.env.push((key.clone(), previous));
            std::env::set_var(key, next);
        }

        if let Some(next_cwd) = opts.cwd {
            if let Err(err) = std::env::set_current_dir(&next_cwd) {
                drop(guard);
                return Err(mlua::Error::external(err));
            }
        }

        Ok(guard)
    }
}

impl Drop for DeptRunEnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.env.iter().rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        if let Some(cwd) = &self.cwd {
            let _ = std::env::set_current_dir(cwd);
        }
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

fn lua_values_equal(lua: &Lua, actual: Value, expected: Value) -> mlua::Result<bool> {
    lua.globals()
        .get::<Function>("rawequal")?
        .call::<bool>((actual, expected))
}

fn assertion_error(name: &str, msg: Option<String>, detail: String) -> mlua::Error {
    let prefix = match msg {
        Some(msg) if !msg.is_empty() => format!("{name}: {msg}: "),
        _ => format!("{name}: "),
    };
    mlua::Error::runtime(format!("{prefix}{detail}"))
}

fn display_value(value: Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(value) => value.to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => match value.to_str() {
            Ok(value) => format!("{value:?}"),
            Err(_) => "<non-utf8-string>".to_string(),
        },
        Value::Table(_) => "<table>".to_string(),
        Value::Function(_) => "<function>".to_string(),
        Value::Thread(_) => "<thread>".to_string(),
        Value::UserData(_) => "<userdata>".to_string(),
        Value::LightUserData(_) => "<lightuserdata>".to_string(),
        Value::Error(err) => format!("<error:{err}>"),
        Value::Other(_) => "<other>".to_string(),
    }
}

fn display_path(path: &Path, owner_root: &Path) -> String {
    path.strip_prefix(owner_root)
        .unwrap_or(path)
        .to_string_lossy()
        .trim_start_matches(std::path::MAIN_SEPARATOR)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn table_len(table: Table) -> usize {
        table.pairs::<Value, Value>().count()
    }

    #[test]
    fn run_department_cache_reuses_derivations_and_preserves_isolation() {
        let temp = TempDir::new().unwrap();
        let dept_dir = temp.path().join("departments/worker");
        std::fs::create_dir_all(&dept_dir).unwrap();
        std::fs::write(
            temp.path().join("helper.lua"),
            r#"
            local M = {}
            local counter = 0
            function M.next()
                counter = counter + 1
                return counter
            end
            return M
            "#,
        )
        .unwrap();
        let main = dept_dir.join("main.lua");
        std::fs::write(
            &main,
            r#"
            function pipeline(event)
                local helper = require("helper")
                local result = exec_sync({ cmd = "echo " .. tostring(event.n) })
                raise("pkg.done", { stdout = result.stdout, n = event.n, counter = helper.next() })
            end
            return {
                spec = { produces = { "pkg.done" } },
                pipeline = pipeline,
            }
            "#,
        )
        .unwrap();
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let owner_root = temp.path().canonicalize().unwrap();
        let owner_namespace = roots.sole_package_namespace().unwrap().to_string();
        let cache = TestRunCache::new(roots.clone());
        let mock_commands = MockCommandState::new();
        let outer_lua = crate::mlua_init::new_lua();

        let first_event = outer_lua
            .to_value(&serde_json::json!({"queue": "jobs", "payload": {}, "ts": 1, "n": 1}))
            .unwrap();
        mock_commands
            .push_mock(
                "echo 1".to_string(),
                MockCommandResult {
                    stdout: "one\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .unwrap();
        let first = run_department(
            &outer_lua,
            &cache,
            &roots,
            &owner_root,
            &owner_namespace,
            mock_commands.clone(),
            "departments/worker/main.lua".to_string(),
            first_event,
            None,
            None,
        )
        .unwrap();
        assert_eq!(first.get::<i64>("exit_code").unwrap(), 0);
        let first_raises: Table = first.get("raises").unwrap();
        let first_raise: Table = first_raises.get(1).unwrap();
        let first_payload: Table = first_raise.get("payload").unwrap();
        assert_eq!(first_payload.get::<i64>("counter").unwrap(), 1);
        assert_eq!(table_len(first_raises), 1);
        assert_eq!(mock_commands.calls().unwrap().len(), 1);

        mock_commands.reset().unwrap();
        let second_event = outer_lua
            .to_value(&serde_json::json!({"queue": "jobs", "payload": {}, "ts": 2, "n": 2}))
            .unwrap();
        mock_commands
            .push_mock(
                "echo 2".to_string(),
                MockCommandResult {
                    stdout: "two\n".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .unwrap();
        let second = run_department(
            &outer_lua,
            &cache,
            &roots,
            &owner_root,
            &owner_namespace,
            mock_commands.clone(),
            "departments/worker/main.lua".to_string(),
            second_event,
            None,
            None,
        )
        .unwrap();

        assert_eq!(second.get::<i64>("exit_code").unwrap(), 0);
        let second_raises: Table = second.get("raises").unwrap();
        let second_raise: Table = second_raises.get(1).unwrap();
        let second_payload: Table = second_raise.get("payload").unwrap();
        assert_eq!(second_payload.get::<i64>("counter").unwrap(), 1);
        assert_eq!(table_len(second_raises), 1);
        assert_eq!(mock_commands.calls().unwrap().len(), 1);
        assert_eq!(
            cache.stats(),
            TestRunCacheStats {
                require_roots_misses: 1,
                package_path_misses: 1,
                declared_produces_misses: 1,
                declared_consumes_misses: 1,
            }
        );
        assert_eq!(cache.lua_chunk_cache().chunk_count(), 1);
    }
}

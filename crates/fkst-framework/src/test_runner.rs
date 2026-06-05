use anyhow::{Context, Result};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::external_command::{MockCommandResult, MockCommandState};
use crate::path_resolver::PackageRoots;
use crate::raise::RaiseBuffer;

pub(crate) fn run_tests(roots: PackageRoots) -> Result<i32> {
    let files = discover_test_files(&roots)?;
    let mut passed = 0usize;
    let mut failed = 0usize;

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
            roots.name_resolver(),
            file.owner_namespace.clone(),
            Some(mock_commands.clone()),
        )
        .with_context(|| format!("register SDK for {}", relpath))?;
        register_test_sdk(
            &lua,
            roots.clone(),
            file.owner_root.clone(),
            file.owner_namespace.clone(),
            mock_commands.clone(),
        )
        .with_context(|| format!("register fkst.test for {}", relpath))?;

        match load_test_table(&lua, &file.path) {
            Ok(tests) => {
                for (name, func) in tests {
                    mock_commands
                        .reset()
                        .with_context(|| format!("reset mock commands for {relpath}::{name}"))?;
                    match func.call::<()>(()) {
                        Ok(()) => {
                            println!("PASS {relpath}::{name}");
                            passed += 1;
                        }
                        Err(err) => {
                            println!("FAIL {relpath}::{name}: {err}");
                            failed += 1;
                        }
                    }
                }
            }
            Err(err) => {
                println!("FAIL {relpath}::<load>: {err:#}");
                failed += 1;
            }
        }
    }

    println!("{passed} passed, {failed} failed");
    Ok(if failed == 0 { 0 } else { 1 })
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

fn load_test_table(lua: &Lua, file: &Path) -> Result<Vec<(String, Function)>> {
    let src = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let chunk = lua.load(&src).set_name(file.to_string_lossy());
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
    roots: PackageRoots,
    owner_root: PathBuf,
    owner_namespace: String,
    mock_commands: MockCommandState,
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
                entry.set("stdout", call.stdout)?;
                entry.set("stderr", call.stderr)?;
                entry.set("exit_code", call.exit_code)?;
                calls.set(idx + 1, entry)?;
            }
            Ok(calls)
        })?
    })?;
    test.set("run_department", {
        let mock_commands = mock_commands.clone();
        lua.create_function(
            move |lua, (path, event, opts): (String, Value, Option<Table>)| {
                run_department(
                    lua,
                    &roots,
                    &owner_root,
                    &owner_namespace,
                    mock_commands.clone(),
                    path,
                    event,
                    opts,
                )
            },
        )?
    })?;
    fkst.set("test", test)?;
    globals.set("fkst", fkst)?;
    Ok(())
}

fn run_department(
    lua: &Lua,
    roots: &PackageRoots,
    owner_root: &Path,
    owner_namespace: &str,
    mock_commands: MockCommandState,
    path: String,
    event: Value,
    opts: Option<Table>,
) -> mlua::Result<Table> {
    let opts = DeptRunOptions::from_lua(opts)?;
    let lua_path = resolve_department_path(owner_root, &path);
    let event_json: serde_json::Value = lua.from_value(event)?;
    let _guard = DeptRunEnvGuard::apply(opts)?;
    let require_roots = roots.require_roots_for_owner(owner_root);
    let qualified_produces = declared_qualified_produces(
        owner_root,
        &lua_path,
        require_roots.iter().map(PathBuf::as_path),
    )?;

    let dept_lua = crate::mlua_init::new_lua();
    let raise_buf = RaiseBuffer::new();
    crate::mlua_init::register_framework_sdk_with_runner(
        &dept_lua,
        raise_buf.clone(),
        roots.host_root(),
        roots
            .name_resolver()
            .with_recorded_only_queues(qualified_produces),
        owner_namespace.to_string(),
        Some(mock_commands),
    )?;

    let exit_code = match crate::mlua_init::run_dept_with_require_roots(
        &dept_lua,
        &lua_path,
        &event_json,
        require_roots.iter().map(PathBuf::as_path),
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

fn declared_qualified_produces<'a>(
    owner_root: &Path,
    lua_path: &Path,
    require_roots: impl IntoIterator<Item = &'a Path>,
) -> mlua::Result<BTreeSet<String>> {
    if !is_department_entrypoint(owner_root, lua_path) {
        return Ok(BTreeSet::new());
    }

    // Test-mode intentionally performs this bounded second eval before execution;
    // department entrypoints have no top-level side effects by contract.
    let lua = crate::mlua_init::new_lua();
    crate::mlua_init::set_package_roots_path(&lua, require_roots)?;
    let src = match std::fs::read_to_string(lua_path) {
        Ok(src) => src,
        Err(_) => return Ok(BTreeSet::new()),
    };
    let value = match lua
        .load(&src)
        .set_name(lua_path.to_string_lossy())
        .eval::<Value>()
    {
        Ok(value) => value,
        Err(_) => return Ok(BTreeSet::new()),
    };
    let Value::Table(module) = value else {
        return Ok(BTreeSet::new());
    };
    let Some(spec) = module.get::<Option<Table>>("spec")? else {
        return Ok(BTreeSet::new());
    };
    let Some(produces) = spec.get::<Option<Table>>("produces")? else {
        return Ok(BTreeSet::new());
    };
    let mut qualified = BTreeSet::new();
    for value in produces.sequence_values::<String>() {
        let value = value?;
        if value.contains('.') {
            qualified.insert(value);
        }
    }
    Ok(qualified)
}

fn is_department_entrypoint(owner_root: &Path, lua_path: &Path) -> bool {
    let Ok(relative) = lua_path.strip_prefix(owner_root) else {
        return false;
    };
    let components = relative.components().collect::<Vec<_>>();
    match components.as_slice() {
        [std::path::Component::Normal(departments), std::path::Component::Normal(name), std::path::Component::Normal(main)] => {
            *departments == std::ffi::OsStr::new("departments")
                && !name.is_empty()
                && *main == std::ffi::OsStr::new("main.lua")
        }
        _ => false,
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

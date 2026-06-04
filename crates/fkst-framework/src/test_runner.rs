use anyhow::{Context, Result};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::path_resolver::PackageRoots;
use crate::raise::RaiseBuffer;

pub(crate) fn run_tests(roots: PackageRoots) -> Result<i32> {
    let files = discover_test_files(&roots)?;
    let mut passed = 0usize;
    let mut failed = 0usize;

    for file in files {
        let relpath = display_path(&file, &roots);
        let lua = crate::mlua_init::new_lua();
        crate::mlua_init::set_package_roots_path(&lua, package_paths(&roots))
            .with_context(|| format!("set package.path for tests in {}", relpath))?;
        crate::mlua_init::register_framework_sdk(&lua, RaiseBuffer::new(), roots.host_root())
            .with_context(|| format!("register SDK for {}", relpath))?;
        register_test_sdk(&lua, roots.clone())
            .with_context(|| format!("register fkst.test for {}", relpath))?;

        match load_test_table(&lua, &file) {
            Ok(tests) => {
                for (name, func) in tests {
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

fn discover_test_files(roots: &PackageRoots) -> Result<Vec<PathBuf>> {
    let mut files = BTreeMap::<PathBuf, ()>::new();
    for root in roots.graph_roots() {
        collect_department_tests(&root.root, &mut files)
            .with_context(|| format!("scan department tests in {}", root.root.display()))?;
        collect_top_tests(&root.root, &mut files)
            .with_context(|| format!("scan top-level tests in {}", root.root.display()))?;
    }
    Ok(files.into_keys().collect())
}

fn collect_department_tests(root: &Path, files: &mut BTreeMap<PathBuf, ()>) -> Result<()> {
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
                files.insert(path.canonicalize()?, ());
            }
        }
    }
    Ok(())
}

fn collect_top_tests(root: &Path, files: &mut BTreeMap<PathBuf, ()>) -> Result<()> {
    let tests = root.join("tests");
    if !tests.exists() {
        return Ok(());
    }
    for file in std::fs::read_dir(&tests).with_context(|| format!("read {}", tests.display()))? {
        let file = file?;
        let path = file.path();
        if path.is_file() && is_test_file(&path) {
            files.insert(path.canonicalize()?, ());
        }
    }
    Ok(())
}

fn is_test_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with("_test.lua"))
}

fn package_paths(roots: &PackageRoots) -> Vec<&Path> {
    let mut paths = vec![roots.package_root()];
    if roots.package_root() != roots.host_root() {
        paths.push(roots.host_root());
    }
    paths
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

fn register_test_sdk(lua: &Lua, roots: PackageRoots) -> mlua::Result<()> {
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
    test.set(
        "run_department",
        lua.create_function(
            move |lua, (path, event, opts): (String, Value, Option<Table>)| {
                run_department(lua, &roots, path, event, opts)
            },
        )?,
    )?;
    fkst.set("test", test)?;
    globals.set("fkst", fkst)?;
    Ok(())
}

fn run_department(
    lua: &Lua,
    roots: &PackageRoots,
    path: String,
    event: Value,
    opts: Option<Table>,
) -> mlua::Result<Table> {
    let opts = DeptRunOptions::from_lua(opts)?;
    let lua_path = resolve_department_path(roots.package_root(), &path);
    let event_json: serde_json::Value = lua.from_value(event)?;
    let _guard = DeptRunEnvGuard::apply(opts)?;

    let dept_lua = crate::mlua_init::new_lua();
    let raise_buf = RaiseBuffer::new();
    crate::mlua_init::register_framework_sdk(&dept_lua, raise_buf.clone(), roots.host_root())?;

    let exit_code =
        match crate::mlua_init::run_dept_with_roots(&dept_lua, &lua_path, &event_json, roots) {
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

fn display_path(path: &Path, roots: &PackageRoots) -> String {
    let root = if path.starts_with(roots.host_root()) {
        roots.host_root()
    } else {
        roots.package_root()
    };
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .trim_start_matches(std::path::MAIN_SEPARATOR)
        .to_string()
}

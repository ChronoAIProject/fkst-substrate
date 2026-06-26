//! Initialize a Lua 5.4 state, expose SDK globals, load + run a lua file.

use anyhow::{Context, Result};
use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, Value as LuaValue};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::capabilities::CapabilityMode;
use crate::config_registry::ConfigContext;
use crate::external_command::MockCommandState;
use crate::manifest::UnitCatalog;
use crate::path_resolver::{NameResolver, PackageRoots};
use crate::raise::{RaiseAuthority, RaiseBuffer};

/// Create a Lua state with stdlib enabled.
pub fn new_lua() -> Lua {
    Lua::new()
}

/// Create a Lua state for stateless generator execution.
pub fn new_lua_restricted() -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    remove_restricted_generator_globals(&lua)?;
    Ok(lua)
}

fn remove_restricted_generator_globals(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in [
        "os",
        "io",
        "package",
        "debug",
        "require",
        "dofile",
        "loadfile",
        "load",
        "loadstring",
    ] {
        globals.set(name, LuaValue::Nil)?;
    }
    Ok(())
}

/// Register the framework SDK globals in the same order for every entry point.
pub fn register_framework_sdk(
    lua: &Lua,
    capability_mode: CapabilityMode,
    raise_buf: RaiseBuffer,
    host_root: &Path,
    owner_root: &Path,
    dept: Option<String>,
    resolver: NameResolver,
    owner_namespace: String,
    raise_authority: RaiseAuthority,
    graph_roots: Option<PackageRoots>,
    graph_json_authorized: bool,
    raised_auth_token: Option<String>,
) -> mlua::Result<()> {
    let config = ConfigContext::from_host_root(host_root).map_err(mlua::Error::external)?;
    crate::rate_pool::RatePoolRegistry::from_config(&config).map_err(mlua::Error::external)?;
    crate::sdk_log::register(lua)?;
    crate::sdk_i18n::register(lua, owner_root)?;
    crate::sdk_strings::register(lua)?;
    crate::sdk_json::register(lua)?;
    match capability_mode {
        CapabilityMode::Full => {
            crate::sdk_basic::register_with_runner(lua, config.clone(), None)?;
            crate::sdk_restricted_lua::register(lua)?;
            crate::sdk_graph::register(lua, graph_roots, graph_json_authorized)?;
            crate::sdk_fs::register(lua)?;
            crate::sdk_git::register(lua, host_root, config.clone())?;
            crate::sdk_mark::register(lua, host_root)?;
            crate::sdk_cache::register(lua, host_root)?;
            crate::sdk_observe::register(lua, None)?;
            crate::sdk_codex::register(
                lua,
                host_root,
                config,
                dept,
                raise_buf.clone(),
                raised_auth_token,
            )?;
            crate::raise::register(lua, raise_buf, resolver, owner_namespace, raise_authority)?;
        }
        CapabilityMode::StatelessGenerator(policy) => {
            let policy = policy
                .canonicalize_for_run(owner_root, host_root)
                .map_err(mlua::Error::external)?;
            crate::sdk_fs::register_confined(lua, owner_root, host_root, policy)?;
        }
    }
    Ok(())
}

pub(crate) fn register_framework_sdk_with_runner(
    lua: &Lua,
    capability_mode: CapabilityMode,
    raise_buf: RaiseBuffer,
    host_root: &Path,
    owner_root: &Path,
    dept: Option<String>,
    resolver: NameResolver,
    owner_namespace: String,
    raise_authority: RaiseAuthority,
    runner: Option<MockCommandState>,
    graph_roots: Option<PackageRoots>,
    graph_json_authorized: bool,
    raised_auth_token: Option<String>,
    mock_observe: Option<crate::sdk_observe::MockObserveState>,
) -> mlua::Result<()> {
    let config = ConfigContext::from_host_root(host_root).map_err(mlua::Error::external)?;
    crate::rate_pool::RatePoolRegistry::from_config(&config).map_err(mlua::Error::external)?;
    crate::sdk_log::register(lua)?;
    crate::sdk_i18n::register(lua, owner_root)?;
    crate::sdk_strings::register(lua)?;
    crate::sdk_json::register(lua)?;
    match capability_mode {
        CapabilityMode::Full => {
            crate::sdk_basic::register_with_runner(lua, config.clone(), runner.clone())?;
            crate::sdk_restricted_lua::register(lua)?;
            crate::sdk_graph::register(lua, graph_roots, graph_json_authorized)?;
            crate::sdk_fs::register(lua)?;
            crate::sdk_git::register_with_runner(lua, host_root, config.clone(), runner.clone())?;
            crate::sdk_mark::register(lua, host_root)?;
            crate::sdk_cache::register(lua, host_root)?;
            crate::sdk_observe::register(lua, mock_observe)?;
            crate::sdk_codex::register_with_runner(
                lua,
                host_root,
                config,
                dept,
                runner,
                raise_buf.clone(),
                raised_auth_token,
            )?;
            crate::raise::register(lua, raise_buf, resolver, owner_namespace, raise_authority)?;
        }
        CapabilityMode::StatelessGenerator(policy) => {
            let policy = policy
                .canonicalize_for_run(owner_root, host_root)
                .map_err(mlua::Error::external)?;
            crate::sdk_fs::register_confined(lua, owner_root, host_root, policy)?;
        }
    }
    Ok(())
}

/// Convert serde_json::Value to mlua::Value via LuaSerdeExt.
pub fn json_to_lua(lua: &Lua, v: &JsonValue) -> mlua::Result<LuaValue> {
    lua.to_value(v)
}

#[derive(Clone, Default)]
pub(crate) struct LuaChunkCache {
    chunks: Arc<Mutex<BTreeMap<CachedChunkKey, Vec<u8>>>>,
}

impl LuaChunkCache {
    pub(crate) fn load_cached_chunk_with_name(
        &self,
        lua: &Lua,
        path: &Path,
        owner_root: &Path,
    ) -> Result<()> {
        let bytecode = self.bytecode_for(path, owner_root)?;
        lua.load(bytecode.as_slice())
            .set_name(crate::lua_coverage::chunk_name(path, owner_root))
            .exec()
            .with_context(|| format!("exec {}", path.display()))
    }

    pub(crate) fn eval_cached_chunk(&self, lua: &Lua, path: &Path) -> Result<LuaValue> {
        let bytecode = self.bytecode_for(path, path)?;
        lua.load(bytecode.as_slice())
            .set_name(path.to_string_lossy())
            .eval()
            .with_context(|| format!("eval {}", path.display()))
    }

    pub(crate) fn eval_cached_chunk_with_env(
        &self,
        lua: &Lua,
        path: &Path,
        owner_root: &Path,
        env: mlua::Table,
    ) -> Result<LuaValue> {
        let bytecode = self.bytecode_for(path, owner_root)?;
        lua.load(bytecode.as_slice())
            .set_name(crate::lua_coverage::chunk_name(path, owner_root))
            .set_environment(env)
            .call(())
            .with_context(|| format!("eval {}", path.display()))
    }

    fn bytecode_for(&self, path: &Path, owner_root: &Path) -> Result<Vec<u8>> {
        let src =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let key = CachedChunkKey::for_path(path, src.as_bytes())?;
        if let Some(bytecode) = self
            .chunks
            .lock()
            .map_err(|_| anyhow::anyhow!("lua chunk cache lock poisoned"))?
            .get(&key)
            .cloned()
        {
            return Ok(bytecode);
        }
        let lua = new_lua();
        let function = lua
            .load(&src)
            .set_name(crate::lua_coverage::chunk_name(path, owner_root))
            .into_function()
            .with_context(|| format!("compile {}", path.display()))?;
        let bytecode = function.dump(true);
        self.chunks
            .lock()
            .map_err(|_| anyhow::anyhow!("lua chunk cache lock poisoned"))?
            .insert(key, bytecode.clone());
        Ok(bytecode)
    }

    #[cfg(test)]
    pub(crate) fn chunk_count(&self) -> usize {
        self.chunks.lock().map(|chunks| chunks.len()).unwrap_or(0)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CachedChunkKey {
    path: PathBuf,
    len: u64,
    mtime: Option<SystemTime>,
    content_hash: String,
}

impl CachedChunkKey {
    fn for_path(path: &Path, source: &[u8]) -> Result<Self> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize {}", path.display()))?;
        let metadata = std::fs::metadata(&canonical)
            .with_context(|| format!("stat {}", canonical.display()))?;
        Ok(Self {
            path: canonical,
            len: metadata.len(),
            mtime: metadata.modified().ok(),
            content_hash: content_hash(source),
        })
    }
}

fn content_hash(source: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in source {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Load the lua file at `path`, execute its top-level chunk, then call `pipeline(event)`.
/// `event` is a serde_json::Value that we expose as a Lua table under the global `event`
/// (and also pass as the argument to `pipeline`).
///
/// Returns Ok on success; errors propagate (script errors, missing pipeline fn, etc.).
#[cfg(test)]
fn run_dept_with_package_root(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    package_root: &Path,
) -> Result<()> {
    let catalog = UnitCatalog::discover(package_root)?
        .ok_or_else(|| anyhow::anyhow!("manifest catalog is required for department runner"))?;
    let owner_unit = catalog
        .unit_name_for_root(package_root)?
        .ok_or_else(|| anyhow::anyhow!("no manifest unit owns {}", package_root.display()))?;
    run_dept_with_require_roots(
        lua,
        lua_path,
        event,
        Arc::new(catalog),
        &owner_unit,
        package_root,
        None,
    )
}

pub fn run_dept_with_require_roots(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    catalog: Arc<UnitCatalog>,
    owner_unit: &str,
    owner_root: &Path,
    cache: Option<&LuaChunkCache>,
) -> Result<()> {
    run_dept_with_package_path_chunk_cache_and_name_root(
        lua, lua_path, event, cache, owner_root, catalog, owner_unit,
    )
}

pub(crate) fn run_dept_with_package_path_and_chunk_cache(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    catalog: Arc<UnitCatalog>,
    owner_unit: &str,
    owner_root: &Path,
    cache: Option<&LuaChunkCache>,
) -> Result<()> {
    run_dept_with_package_path_chunk_cache_and_name_root(
        lua, lua_path, event, cache, owner_root, catalog, owner_unit,
    )
}

pub(crate) fn run_dept_with_package_path_chunk_cache_and_name_root(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    cache: Option<&LuaChunkCache>,
    owner_root: &Path,
    catalog: Arc<UnitCatalog>,
    owner_unit: &str,
) -> Result<()> {
    let env = crate::lua_require::install_scoped_require(lua, catalog.clone(), owner_unit)
        .context("install scoped require")?;
    let module = if let Some(cache) = cache {
        cache.eval_cached_chunk_with_env(lua, lua_path, owner_root, env)?
    } else {
        crate::lua_require::load_unit_chunk(
            lua,
            catalog,
            owner_unit,
            lua_path,
            crate::lua_coverage::chunk_name(lua_path, owner_root),
            None,
        )
        .with_context(|| format!("exec {}", lua_path.display()))?
    };

    let pipeline: mlua::Function = match module {
        LuaValue::Table(table) => table
            .get::<Option<mlua::Function>>("pipeline")?
            .map(Ok)
            .unwrap_or_else(|| lua.globals().get("pipeline")),
        _ => lua.globals().get("pipeline"),
    }
    .context("lua file did not define global `pipeline` function")?;
    let event_lua = json_to_lua(lua, event).context("json -> lua event conversion")?;
    lua.globals()
        .set("event", event_lua.clone())
        .context("set global `event`")?;
    pipeline
        .call::<()>(event_lua)
        .context("pipeline(event) call")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{CapabilityMode, StatelessGeneratorPolicy};
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn write_package_manifest(root: &Path) {
        write(
            &root.join("fkst.workspace.toml"),
            r#"
[workspace]
units = ["."]
"#,
        );
        write(
            &root.join("fkst.toml"),
            r#"
kind = "package"
name = "pkg"

[code]
root = "."

[generated]
root = "generated"
"#,
        );
    }

    fn force_mtime(path: &Path, millis_since_epoch: u64) {
        let time = UNIX_EPOCH + Duration::from_millis(millis_since_epoch);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time)).unwrap();
    }

    fn register_test_sdk(lua: &Lua, root: &Path, capability_mode: CapabilityMode) {
        register_framework_sdk(
            lua,
            capability_mode,
            RaiseBuffer::new(),
            root,
            root,
            Some("pkg.test".to_string()),
            NameResolver::new(["pkg".to_string()]),
            "pkg".to_string(),
            RaiseAuthority::new(Default::default()),
            None,
            false,
            None,
        )
        .unwrap();
    }

    #[test]
    fn stateless_generator_omits_effect_primitives() {
        let lua = new_lua_restricted().unwrap();
        let dir = TempDir::new().unwrap();
        write_package_manifest(dir.path());
        std::fs::create_dir_all(dir.path().join("generated")).unwrap();
        register_test_sdk(
            &lua,
            dir.path(),
            CapabilityMode::StatelessGenerator(StatelessGeneratorPolicy {
                output_roots: vec![PathBuf::from("generated")],
                input_roots: Vec::new(),
            }),
        );

        let absent: bool = lua
            .load(
                r#"
                return raise == nil
                    and exec_argv == nil
                    and exec_sync == nil
                    and cache_set == nil
                    and spawn_codex_sync == nil
                    and with_lock == nil
                    and now == nil
                "#,
            )
            .eval()
            .unwrap();

        assert!(absent);
    }

    #[test]
    fn stateless_generator_restricted_lua_removes_ambient_stdlib_escape_hatches() {
        let lua = new_lua_restricted().unwrap();

        let absent: bool = lua
            .load(
                r#"
                return os == nil
                    and io == nil
                    and package == nil
                    and debug == nil
                    and require == nil
                    and load == nil
                    and loadfile == nil
                    and dofile == nil
                    and loadstring == nil
                "#,
            )
            .eval()
            .unwrap();

        assert!(absent);
        for expr in [
            "os.execute('true')",
            "io.open('/tmp/fkst-generator-bypass', 'w')",
            "io.popen('true')",
            "loadfile('/tmp/nope.lua')",
            "dofile('/tmp/nope.lua')",
            "load('return 1')",
        ] {
            let err = lua.load(expr).exec().unwrap_err().to_string();
            assert!(err.contains("nil value"), "{expr}: {err}");
        }
    }

    #[test]
    fn stateless_generator_pipeline_rejects_ambient_stdlib_escape_hatches() {
        for (name, expr) in [
            ("os_execute", "os.execute('true')"),
            ("io_open", "io.open('/tmp/fkst-generator-bypass', 'w')"),
            ("io_popen", "io.popen('true')"),
            ("loadfile", "loadfile('/tmp/nope.lua')"),
            ("dofile", "dofile('/tmp/nope.lua')"),
            ("load", "load('return 1')"),
        ] {
            let lua = new_lua_restricted().unwrap();
            let dir = TempDir::new().unwrap();
            write_package_manifest(dir.path());
            std::fs::create_dir_all(dir.path().join("generated")).unwrap();
            let main = dir.path().join(format!("departments/{name}/main.lua"));
            write(
                &main,
                &format!(
                    r#"
                    local M = {{}}
                    function M.pipeline(_)
                        {expr}
                    end
                    return M
                    "#
                ),
            );
            register_test_sdk(
                &lua,
                dir.path(),
                CapabilityMode::StatelessGenerator(StatelessGeneratorPolicy {
                    output_roots: vec![PathBuf::from("generated")],
                    input_roots: Vec::new(),
                }),
            );

            let err = run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path())
                .unwrap_err();
            let msg = format!("{err:#}");

            assert!(msg.contains("nil value"), "{name}: {msg}");
        }
    }

    #[test]
    fn stateless_generator_confined_fs_allows_only_policy_roots() {
        let lua = new_lua_restricted().unwrap();
        let dir = TempDir::new().unwrap();
        write_package_manifest(dir.path());
        let out = dir.path().join("generated");
        let input = dir.path().join("fixtures");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&input).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(input.join("seed.txt"), "seed").unwrap();
        register_test_sdk(
            &lua,
            dir.path(),
            CapabilityMode::StatelessGenerator(StatelessGeneratorPolicy {
                output_roots: vec![PathBuf::from("generated")],
                input_roots: vec![PathBuf::from("fixtures")],
            }),
        );

        lua.load(r#"file.write("generated/out.txt", "ok")"#)
            .exec()
            .unwrap();
        assert_eq!(std::fs::read_to_string(out.join("out.txt")).unwrap(), "ok");
        lua.load(r#"file.mkdir("generated/assets/css")"#)
            .exec()
            .unwrap();
        assert!(out.join("assets/css").is_dir());
        lua.load(r#"file.write("generated/deep/page.txt", "deep")"#)
            .exec()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("deep/page.txt")).unwrap(),
            "deep"
        );

        let read_input: String = lua
            .load(r#"return file.read("fixtures/seed.txt")"#)
            .eval()
            .unwrap();
        assert_eq!(read_input, "seed");

        let outside_write = lua
            .load(r#"file.write("outside/out.txt", "no")"#)
            .exec()
            .unwrap_err()
            .to_string();
        assert!(outside_write.contains("stateless_generator_fs_write_denied"));

        let outside_read = lua
            .load(r#"return file.exists("outside/out.txt")"#)
            .eval::<bool>()
            .unwrap_err()
            .to_string();
        assert!(outside_read.contains("stateless_generator_fs_read_denied"));
    }

    #[test]
    fn stateless_generator_can_require_module_and_write_output() {
        let lua = new_lua_restricted().unwrap();
        let dir = TempDir::new().unwrap();
        write_package_manifest(dir.path());
        std::fs::create_dir_all(dir.path().join("generated")).unwrap();
        write(
            &dir.path().join("core.lua"),
            r#"
            return {
                render = function(value)
                    local words = { string.upper(value), tostring(math.floor(2.8)) }
                    table.insert(words, utf8.len("ok"))
                    return table.concat(words, ":")
                end
            }
            "#,
        );
        let main = dir.path().join("departments/generate/main.lua");
        write(
            &main,
            r#"
            local core = require("core")
            local M = {}
            function M.pipeline(event)
                file.mkdir("generated/site")
                file.write("generated/site/index.txt", core.render(event.name))
            end
            return M
            "#,
        );
        register_test_sdk(
            &lua,
            dir.path(),
            CapabilityMode::StatelessGenerator(StatelessGeneratorPolicy {
                output_roots: vec![PathBuf::from("generated")],
                input_roots: Vec::new(),
            }),
        );

        run_dept_with_package_root(
            &lua,
            &main,
            &serde_json::json!({"name": "site"}),
            dir.path(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("generated/site/index.txt")).unwrap(),
            "SITE:2:2"
        );
    }

    #[test]
    fn full_capability_mode_keeps_current_primitives() {
        let lua = new_lua();
        let dir = TempDir::new().unwrap();
        register_test_sdk(&lua, dir.path(), CapabilityMode::Full);

        let present: bool = lua
            .load(
                r#"
                return type(raise) == "function"
                    and type(exec_argv) == "function"
                    and type(exec_sync) == "function"
                    and type(cache_set) == "function"
                    and type(spawn_codex_sync) == "function"
                    and type(with_lock) == "function"
                    and type(now) == "function"
                    and type(os.execute) == "function"
                    and type(io.open) == "function"
                "#,
            )
            .eval()
            .unwrap();

        assert!(present);
    }

    #[test]
    fn full_capability_pipeline_keeps_os_and_io_stdlib() {
        let lua = new_lua();
        let dir = TempDir::new().unwrap();
        write_package_manifest(dir.path());
        let main = dir.path().join("departments/full/main.lua");
        write(
            &main,
            r#"
            local M = {}
            function M.pipeline(_)
                assert(type(os.execute) == "function")
                assert(type(io.open) == "function")
                called = true
            end
            return M
            "#,
        );
        register_test_sdk(&lua, dir.path(), CapabilityMode::Full);

        run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path()).unwrap();

        let called: bool = lua.globals().get("called").unwrap();
        assert!(called);
    }

    #[test]
    fn run_dept_calls_pipeline() {
        let lua = new_lua();
        let dir = TempDir::new().unwrap();
        // Use a Lua global side channel to verify pipeline was called.
        let main = dir.path().join("main.lua");
        write_package_manifest(dir.path());
        write(
            &main,
            r#"
            called = 0
            function pipeline(event)
                called = called + 1
                assert(event.foo == "bar", "expected foo=bar, got " .. tostring(event.foo))
            end
        "#,
        );
        run_dept_with_package_root(&lua, &main, &serde_json::json!({"foo": "bar"}), dir.path())
            .unwrap();
        let called: i64 = lua.globals().get("called").unwrap();
        assert_eq!(called, 1);
    }

    #[test]
    fn missing_pipeline_returns_err() {
        let lua = new_lua();
        let dir = TempDir::new().unwrap();
        write_package_manifest(dir.path());
        let main = dir.path().join("main.lua");
        write(&main, "x = 1\n");
        let err = run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path())
            .unwrap_err();
        assert!(format!("{}", err).contains("pipeline"));
    }

    #[test]
    fn lua_syntax_error_returns_err() {
        let lua = new_lua();
        let dir = TempDir::new().unwrap();
        write_package_manifest(dir.path());
        let main = dir.path().join("main.lua");
        write(&main, "this is = not valid {{ lua");
        let err = run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path())
            .unwrap_err();
        assert!(format!("{}", err).contains("exec"));
    }

    #[test]
    fn cached_chunk_key_uses_content_hash_when_len_and_mtime_match() {
        let dir = TempDir::new().unwrap();
        let main = dir.path().join("main.lua");
        let first = "value = 'first'\n";
        let second = "value = 'fresh'\n";
        assert_eq!(first.len(), second.len());
        std::fs::write(&main, first).unwrap();
        force_mtime(&main, 1_000);
        let cache = LuaChunkCache::default();

        let lua = new_lua();
        cache
            .load_cached_chunk_with_name(&lua, &main, dir.path())
            .unwrap();
        let value: String = lua.globals().get("value").unwrap();
        assert_eq!(value, "first");

        std::fs::write(&main, second).unwrap();
        force_mtime(&main, 1_000);
        cache
            .load_cached_chunk_with_name(&lua, &main, dir.path())
            .unwrap();
        let value: String = lua.globals().get("value").unwrap();

        assert_eq!(value, "fresh");
        assert_eq!(cache.chunk_count(), 2);
    }

    #[test]
    fn run_dept_loads_package_root_modules() {
        let dir = TempDir::new().unwrap();
        write_package_manifest(dir.path());
        write(
            &dir.path().join("fkst/example.lua"),
            r#"return { value = function() return "ok" end }"#,
        );
        let main = dir.path().join("departments/demo/main.lua");
        write(
            &main,
            r#"
            local example = require("fkst.example")
            function pipeline(event)
                called = example.value()
            end
        "#,
        );

        let lua = new_lua();
        run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path()).unwrap();
        let called: String = lua.globals().get("called").unwrap();
        assert_eq!(called, "ok");
    }

    #[test]
    fn run_dept_names_loaded_chunk_relative_to_package_root() {
        let dir = TempDir::new().unwrap();
        write_package_manifest(dir.path());
        let main = dir.path().join("departments/demo/main.lua");
        write(
            &main,
            r#"
            function pipeline(event)
                called = true
            end
        "#,
        );

        let lua = new_lua();
        let sources = Arc::new(Mutex::new(Vec::<String>::new()));
        let hook_sources = sources.clone();
        lua.set_hook(mlua::HookTriggers::new().on_calls(), move |_, debug| {
            if let Some(source) = debug.source().source {
                if let Ok(mut sources) = hook_sources.lock() {
                    sources.push(source.into_owned());
                }
            }
            Ok(mlua::VmState::Continue)
        });
        run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path()).unwrap();
        let called: bool = lua.globals().get("called").unwrap();
        let sources = sources.lock().unwrap();

        assert!(called);
        assert!(
            sources
                .iter()
                .any(|source| source == "@departments/demo/main.lua"),
            "sources: {sources:?}"
        );
        assert!(
            sources.iter().all(|source| source != "@"),
            "sources: {sources:?}"
        );
    }

    #[test]
    fn scoped_require_resolves_module_main_lua() {
        let dir = TempDir::new().unwrap();
        write_package_manifest(dir.path());
        write(
            &dir.path().join("core/init.lua"),
            r#"return { marker = "from-main" }"#,
        );
        let main = dir.path().join("departments/demo/main.lua");
        write(
            &main,
            r#"
            local core = require("core")
            function pipeline(event)
                called = core.marker
            end
        "#,
        );

        let lua = new_lua();
        run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path()).unwrap();
        let called: String = lua.globals().get("called").unwrap();
        assert_eq!(called, "from-main");
    }

    #[test]
    fn run_dept_does_not_fall_back_to_cwd_or_existing_search_path() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let owner = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        write_package_manifest(owner.path());
        write(&cwd.path().join("core.lua"), r#"return { value = "cwd" }"#);
        let main = owner.path().join("departments/demo/main.lua");
        write(
            &main,
            r#"
            local core = require("core")
            function pipeline(event)
                called = core.value
            end
        "#,
        );

        let prior_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd.path()).unwrap();
        let lua = new_lua();
        lua.load(format!(
            "package.path = {:?}",
            cwd.path().join("?.lua").display().to_string()
        ))
        .exec()
        .unwrap();
        let err = run_dept_with_package_root(&lua, &main, &serde_json::json!({}), owner.path())
            .unwrap_err();
        std::env::set_current_dir(prior_cwd).unwrap();

        let msg = format!("{err:#}");
        assert!(msg.contains("require.denied"), "{msg}");
    }
}

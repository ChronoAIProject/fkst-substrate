//! Initialize a Lua 5.4 state, expose SDK globals, load + run a lua file.

use anyhow::{Context, Result};
use mlua::{Lua, LuaSerdeExt, Value as LuaValue};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::config_registry::ConfigContext;
use crate::external_command::MockCommandState;
use crate::path_resolver::{package_root_path, NameResolver, PackageRoots};
use crate::raise::{RaiseAuthority, RaiseBuffer};

/// Create a Lua state with stdlib enabled.
pub fn new_lua() -> Lua {
    Lua::new()
}

/// Register the framework SDK globals in the same order for every entry point.
pub fn register_framework_sdk(
    lua: &Lua,
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
    crate::sdk_basic::register_with_runner(lua, config.clone(), None)?;
    crate::sdk_strings::register(lua)?;
    crate::sdk_graph::register(lua, graph_roots, graph_json_authorized)?;
    crate::sdk_fs::register(lua)?;
    crate::sdk_json::register(lua)?;
    crate::sdk_git::register(lua, host_root, config.clone())?;
    crate::sdk_mark::register(lua, host_root)?;
    crate::sdk_cache::register(lua, host_root)?;
    crate::sdk_codex::register(
        lua,
        host_root,
        config,
        dept,
        raise_buf.clone(),
        raised_auth_token,
    )?;
    crate::raise::register(lua, raise_buf, resolver, owner_namespace, raise_authority)?;
    Ok(())
}

pub(crate) fn register_framework_sdk_with_runner(
    lua: &Lua,
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
) -> mlua::Result<()> {
    let config = ConfigContext::from_host_root(host_root).map_err(mlua::Error::external)?;
    crate::rate_pool::RatePoolRegistry::from_config(&config).map_err(mlua::Error::external)?;
    crate::sdk_log::register(lua)?;
    crate::sdk_i18n::register(lua, owner_root)?;
    crate::sdk_basic::register_with_runner(lua, config.clone(), runner.clone())?;
    crate::sdk_strings::register(lua)?;
    crate::sdk_graph::register(lua, graph_roots, graph_json_authorized)?;
    crate::sdk_fs::register(lua)?;
    crate::sdk_json::register(lua)?;
    crate::sdk_git::register_with_runner(lua, host_root, config.clone(), runner.clone())?;
    crate::sdk_mark::register(lua, host_root)?;
    crate::sdk_cache::register(lua, host_root)?;
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
    Ok(())
}

/// Convert serde_json::Value to mlua::Value via LuaSerdeExt.
pub fn json_to_lua(lua: &Lua, v: &JsonValue) -> mlua::Result<LuaValue> {
    lua.to_value(v)
}

/// Build the Lua search path for fixed graph roots in lookup order.
pub(crate) fn package_roots_path<'a>(roots: impl IntoIterator<Item = &'a Path>) -> String {
    roots
        .into_iter()
        .map(package_root_path)
        .collect::<Vec<_>>()
        .join(";")
}

pub(crate) fn set_package_roots_path<'a>(
    lua: &Lua,
    package_roots: impl IntoIterator<Item = &'a Path>,
) -> mlua::Result<()> {
    let roots_path = package_roots_path(package_roots);
    set_package_path_string(lua, &roots_path)
}

pub(crate) fn set_package_path_string(lua: &Lua, roots_path: &str) -> mlua::Result<()> {
    lua.load(format!(
        "package.path = {:?}; package.cpath = \"\"",
        roots_path
    ))
    .exec()
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
    run_dept_with_require_roots(lua, lua_path, event, [package_root])
}

pub fn run_dept_with_require_roots<'a>(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    package_roots: impl IntoIterator<Item = &'a Path>,
) -> Result<()> {
    let package_roots = package_roots.into_iter().collect::<Vec<_>>();
    let owner_root = package_roots
        .first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("department runner requires at least one package root"))?;
    let roots_label = package_roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(";");
    set_package_roots_path(lua, package_roots.iter().copied())
        .with_context(|| format!("set package.path for {}", roots_label))?;
    run_dept_with_package_path_chunk_cache_and_name_root(lua, lua_path, event, None, owner_root)
}

pub(crate) fn run_dept_with_package_path_and_chunk_cache(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    cache: Option<&LuaChunkCache>,
) -> Result<()> {
    run_dept_with_package_path_chunk_cache_and_name_root(lua, lua_path, event, cache, lua_path)
}

pub(crate) fn run_dept_with_package_path_chunk_cache_and_name_root(
    lua: &Lua,
    lua_path: &Path,
    event: &JsonValue,
    cache: Option<&LuaChunkCache>,
    owner_root: &Path,
) -> Result<()> {
    if let Some(cache) = cache {
        cache.load_cached_chunk_with_name(lua, lua_path, owner_root)?;
    } else {
        let src = std::fs::read_to_string(lua_path)
            .with_context(|| format!("read {}", lua_path.display()))?;
        let chunk = lua
            .load(&src)
            .set_name(crate::lua_coverage::chunk_name(lua_path, owner_root));
        chunk
            .exec()
            .with_context(|| format!("exec {}", lua_path.display()))?;
    }

    let pipeline: mlua::Function = lua
        .globals()
        .get("pipeline")
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
    use std::io::Write;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::{NamedTempFile, TempDir};

    fn write_lua(s: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(s.as_bytes()).unwrap();
        f
    }

    fn force_mtime(path: &Path, millis_since_epoch: u64) {
        let time = UNIX_EPOCH + Duration::from_millis(millis_since_epoch);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time)).unwrap();
    }

    #[test]
    fn run_dept_calls_pipeline() {
        let lua = new_lua();
        // Use a Lua global side channel to verify pipeline was called.
        let f = write_lua(
            r#"
            called = 0
            function pipeline(event)
                called = called + 1
                assert(event.foo == "bar", "expected foo=bar, got " .. tostring(event.foo))
            end
        "#,
        );
        let package_root = f.path().parent().unwrap();
        run_dept_with_package_root(
            &lua,
            f.path(),
            &serde_json::json!({"foo": "bar"}),
            package_root,
        )
        .unwrap();
        let called: i64 = lua.globals().get("called").unwrap();
        assert_eq!(called, 1);
    }

    #[test]
    fn missing_pipeline_returns_err() {
        let lua = new_lua();
        let f = write_lua("x = 1\n");
        let package_root = f.path().parent().unwrap();
        let err = run_dept_with_package_root(&lua, f.path(), &serde_json::json!({}), package_root)
            .unwrap_err();
        assert!(format!("{}", err).contains("pipeline"));
    }

    #[test]
    fn lua_syntax_error_returns_err() {
        let lua = new_lua();
        let f = write_lua("this is = not valid {{ lua");
        let package_root = f.path().parent().unwrap();
        let err = run_dept_with_package_root(&lua, f.path(), &serde_json::json!({}), package_root)
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
        std::fs::create_dir_all(dir.path().join("departments/demo")).unwrap();
        std::fs::create_dir_all(dir.path().join("fkst")).unwrap();
        std::fs::write(
            dir.path().join("fkst/example.lua"),
            r#"return { value = function() return "ok" end }"#,
        )
        .unwrap();
        let main = dir.path().join("departments/demo/main.lua");
        std::fs::write(
            &main,
            r#"
            local example = require("fkst.example")
            function pipeline(event)
                called = example.value()
            end
        "#,
        )
        .unwrap();

        let lua = new_lua();
        run_dept_with_package_root(&lua, &main, &serde_json::json!({}), dir.path()).unwrap();
        let called: String = lua.globals().get("called").unwrap();
        assert_eq!(called, "ok");
    }

    #[test]
    fn run_dept_names_loaded_chunk_relative_to_package_root() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("departments/demo")).unwrap();
        let main = dir.path().join("departments/demo/main.lua");
        std::fs::write(
            &main,
            r#"
            function pipeline(event)
                called = true
            end
        "#,
        )
        .unwrap();

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
        run_dept_with_require_roots(&lua, &main, &serde_json::json!({}), [dir.path()]).unwrap();
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
    fn set_package_root_path_replaces_existing_search_path() {
        let dir = TempDir::new().unwrap();
        let lua = new_lua();
        lua.load(r#"package.path = "prior/?.lua""#).exec().unwrap();

        set_package_roots_path(&lua, [dir.path()]).unwrap();

        let package: mlua::Table = lua.globals().get("package").unwrap();
        let path: String = package.get("path").unwrap();
        let cpath: String = package.get("cpath").unwrap();
        assert_eq!(path, package_root_path(dir.path()));
        assert_eq!(cpath, "");
    }

    #[test]
    fn package_root_path_resolves_module_main_lua() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("departments/demo")).unwrap();
        std::fs::create_dir_all(dir.path().join("core")).unwrap();
        std::fs::write(
            dir.path().join("core/main.lua"),
            r#"return { marker = "from-main" }"#,
        )
        .unwrap();
        let main = dir.path().join("departments/demo/main.lua");
        std::fs::write(
            &main,
            r#"
            local core = require("core")
            function pipeline(event)
                called = core.marker
            end
        "#,
        )
        .unwrap();

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
        std::fs::create_dir_all(owner.path().join("departments/demo")).unwrap();
        std::fs::write(cwd.path().join("core.lua"), r#"return { value = "cwd" }"#).unwrap();
        let main = owner.path().join("departments/demo/main.lua");
        std::fs::write(
            &main,
            r#"
            local core = require("core")
            function pipeline(event)
                called = core.value
            end
        "#,
        )
        .unwrap();

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
        assert!(msg.contains("module 'core' not found"), "{msg}");
    }
}

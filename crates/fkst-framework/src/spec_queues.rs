use crate::path_resolver::PackageRoots;
use mlua::{Table, Value};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

pub(crate) fn declared_resolved_produces(
    roots: &PackageRoots,
    owner_namespace: &str,
    owner_root: &Path,
    lua_path: &Path,
    catalog: Arc<crate::manifest::UnitCatalog>,
    owner_unit: &str,
    chunk_cache: &crate::mlua_init::LuaChunkCache,
) -> mlua::Result<BTreeSet<String>> {
    let raw = declared_spec_queues(
        owner_root,
        lua_path,
        catalog,
        owner_unit,
        chunk_cache,
        "produces",
    )?;
    let resolver = roots.name_resolver().with_recorded_only_queues(
        raw.iter()
            .filter(|queue| queue.contains('.'))
            .cloned()
            .collect(),
    );
    raw.into_iter()
        .map(|queue| {
            resolver
                .resolve(owner_namespace, &queue)
                .map_err(mlua::Error::external)
        })
        .collect()
}

pub(crate) fn declared_qualified_spec_queues(
    owner_root: &Path,
    lua_path: &Path,
    catalog: Arc<crate::manifest::UnitCatalog>,
    owner_unit: &str,
    chunk_cache: &crate::mlua_init::LuaChunkCache,
    field: &str,
) -> mlua::Result<BTreeSet<String>> {
    let raw = declared_spec_queues(
        owner_root,
        lua_path,
        catalog,
        owner_unit,
        chunk_cache,
        field,
    )?;
    Ok(raw
        .into_iter()
        .filter(|queue| queue.contains('.'))
        .collect())
}

pub(crate) fn declared_raw_spec_queues(
    owner_root: &Path,
    lua_path: &Path,
    catalog: Arc<crate::manifest::UnitCatalog>,
    owner_unit: &str,
    chunk_cache: &crate::mlua_init::LuaChunkCache,
    field: &str,
) -> mlua::Result<BTreeSet<String>> {
    declared_spec_queues(
        owner_root,
        lua_path,
        catalog,
        owner_unit,
        chunk_cache,
        field,
    )
}

fn declared_spec_queues(
    owner_root: &Path,
    lua_path: &Path,
    catalog: Arc<crate::manifest::UnitCatalog>,
    owner_unit: &str,
    chunk_cache: &crate::mlua_init::LuaChunkCache,
    field: &str,
) -> mlua::Result<BTreeSet<String>> {
    if !is_department_entrypoint(owner_root, lua_path) {
        return Ok(BTreeSet::new());
    }

    let lua = crate::mlua_init::new_lua();
    let env = crate::lua_require::unit_environment(&lua, catalog, owner_unit)?;
    let value = match chunk_cache.eval_cached_chunk_with_env(&lua, lua_path, owner_root, env) {
        Ok(value) => value,
        Err(_) => return Ok(BTreeSet::new()),
    };
    let Value::Table(module) = value else {
        return Ok(BTreeSet::new());
    };
    let Some(spec) = module.get::<Option<Table>>("spec")? else {
        return Ok(BTreeSet::new());
    };
    let Some(queues) = spec.get::<Option<Table>>(field)? else {
        return Ok(BTreeSet::new());
    };
    let mut declared = BTreeSet::new();
    for value in queues.sequence_values::<String>() {
        declared.insert(value?);
    }
    Ok(declared)
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

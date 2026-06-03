//! Graph scan derives the startup Config from the fixed package plus host layout.
//!
//! The package root owns standard assets and the host root owns host
//! departments and raisers. `package.lua` is not a supported graph input.
//!
//! Each department exposes `M.spec = { consumes, produces, fanout, timeout }`
//! at module top level. Queues are auto-derived from the union of department
//! and raiser consumes+produces, with fanout coming only from Department
//! `M.spec.fanout`. Host graph defaults are read before graph materialization.

use anyhow::{anyhow, bail, Context, Result};
use fkst_common::config::{Config, DepartmentDecl, LimitsDecl, QueueDecl, RaiserDecl};
use fkst_common::RuntimeKind;
use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config_registry::{ConfigContext, ConfigKey, ConfigValueType};
use crate::path_resolver::{GraphRoot, GraphRootKind, PackageRoots};
use crate::runtime_context;

/// Deserialization helper for a department's `M.spec` table.
#[derive(Deserialize)]
struct DeptSpec {
    #[serde(default)]
    consumes: Vec<String>,
    #[serde(default)]
    produces: Vec<String>,
    #[serde(default)]
    fanout: Vec<String>,
    #[serde(default)]
    timeout: String,
}

#[derive(Clone, Debug, PartialEq)]
struct HostGraphDefaults {
    queue_capacity: usize,
    department_default_timeout: String,
    codex_permit_slots: usize,
}

impl HostGraphDefaults {
    fn load(roots: &PackageRoots) -> Result<Self> {
        let config = ConfigContext::from_host_root(roots.host_root())?;
        Ok(Self {
            queue_capacity: resolve_usize(&config, ConfigKey::QueueCapacity)?,
            department_default_timeout: resolve_timeout(
                &config,
                ConfigKey::DepartmentDefaultTimeout,
            )?,
            codex_permit_slots: resolve_usize(&config, ConfigKey::CodexPermitSlots)?,
        })
    }
}

fn resolve_usize(config: &ConfigContext, key: ConfigKey) -> Result<usize> {
    let entry = crate::config_registry::entry(key);
    assert_eq!(entry.value_type, ConfigValueType::Usize);
    config.resolved_positive_usize(key)
}

fn resolve_timeout(config: &ConfigContext, key: ConfigKey) -> Result<String> {
    let entry = crate::config_registry::entry(key);
    assert_eq!(entry.value_type, ConfigValueType::DurationString);
    config.resolved_duration_string(key)
}

#[cfg(test)]
pub fn load(repo_root: &Path) -> Result<Config> {
    let roots = PackageRoots::resolve(repo_root, Some(repo_root.to_path_buf()))?;
    load_roots(&roots)
}

pub fn load_roots(roots: &PackageRoots) -> Result<Config> {
    let graph_roots = roots.graph_roots();
    let lua_roots: Vec<&Path> = graph_roots.iter().map(|root| root.root.as_path()).collect();
    for graph_root in &graph_roots {
        reject_removed_surfaces(&graph_root)?;
    }

    let defaults = HostGraphDefaults::load(roots)?;
    let lua = Lua::new();
    let mut departments: BTreeMap<String, DepartmentDecl> = BTreeMap::new();
    let mut raisers: BTreeMap<String, RaiserDecl> = BTreeMap::new();
    let mut department_fanout: HashMap<String, Vec<String>> = HashMap::new();

    for graph_root in &graph_roots {
        scan_departments(
            &lua,
            graph_root,
            &lua_roots,
            &defaults,
            &mut departments,
            &mut department_fanout,
        )?;
        scan_raisers(
            &lua,
            graph_root,
            &lua_roots,
            roots.host_root(),
            &mut raisers,
        )?;
    }

    let queues = derive_queues(&departments, &raisers, &department_fanout, &defaults)?;

    Ok(Config {
        queue: queues,
        raiser: raisers,
        department: departments,
        limits: LimitsDecl {
            global_codex_processes: defaults.codex_permit_slots,
        },
    })
}

fn reject_removed_surfaces(graph_root: &GraphRoot) -> Result<()> {
    if graph_root.root.join("package.lua").is_file() {
        bail!(
            "{} is a removed graph surface; use departments/<dept>/main.lua M.spec.fanout",
            graph_root.root.join("package.lua").display()
        );
    }
    Ok(())
}

fn scan_departments(
    lua: &Lua,
    graph_root: &GraphRoot,
    lua_roots: &[&Path],
    defaults: &HostGraphDefaults,
    departments: &mut BTreeMap<String, DepartmentDecl>,
    department_fanout: &mut HashMap<String, Vec<String>>,
) -> Result<()> {
    let repo_root = &graph_root.root;
    let dept_dir = repo_root.join("departments");
    if !dept_dir.is_dir() {
        return Ok(());
    }

    for path in sorted_dirs(&dept_dir).with_context(|| format!("read {}", dept_dir.display()))? {
        let name = file_name(&path)?;
        let main_lua = path.join("main.lua");
        if !main_lua.is_file() {
            continue;
        }

        let module = eval_lua_file(lua, lua_roots, &main_lua)
            .with_context(|| format!("eval department `{}` from {}", name, main_lua.display()))?;
        let spec_tbl: Table = module
            .get("spec")
            .with_context(|| format!("department `{}` missing `M.spec`", name))?;
        let spec: DeptSpec = lua
            .from_value(LuaValue::Table(spec_tbl))
            .with_context(|| format!("parse `{}.spec`", name))?;

        let config_path = config_path(repo_root, graph_root.kind, &main_lua);
        insert_department_decl_with_root(
            departments,
            &name,
            DepartmentDecl {
                lua: config_path.clone(),
                consumes: spec.consumes,
                produces: spec.produces,
                timeout: if spec.timeout.trim().is_empty() {
                    defaults.department_default_timeout.clone()
                } else {
                    spec.timeout
                },
            },
            &config_path,
            graph_root.kind,
        )?;
        department_fanout.insert(name, spec.fanout);
    }

    Ok(())
}

fn scan_raisers(
    lua: &Lua,
    graph_root: &GraphRoot,
    lua_roots: &[&Path],
    host_root: &Path,
    raisers: &mut BTreeMap<String, RaiserDecl>,
) -> Result<()> {
    let repo_root = &graph_root.root;
    let raisers_dir = repo_root.join("raisers");
    if !raisers_dir.is_dir() {
        return Ok(());
    }

    for path in
        sorted_lua_files(&raisers_dir).with_context(|| format!("read {}", raisers_dir.display()))?
    {
        let stem = path
            .file_stem()
            .ok_or_else(|| anyhow!("raiser file no stem"))?
            .to_string_lossy()
            .into_owned();
        let val = eval_lua_value(lua, lua_roots, &path)
            .with_context(|| format!("eval raiser `{}` from {}", stem, path.display()))?;
        let mut r: RaiserDecl = lua
            .from_value(val)
            .with_context(|| format!("parse raisers/{}.lua", stem))?;
        resolve_runtime_file_watch_glob(&mut r, host_root)?;

        let config_path = config_path(repo_root, graph_root.kind, &path);
        insert_raiser_decl_with_root(raisers, &stem, r, &config_path, graph_root.kind)?;
    }

    Ok(())
}

#[cfg(test)]
pub(crate) fn insert_department_decl(
    departments: &mut BTreeMap<String, DepartmentDecl>,
    name: &str,
    decl: DepartmentDecl,
    config_path: &Path,
) -> Result<()> {
    insert_department_decl_with_root(
        departments,
        name,
        decl,
        config_path,
        GraphRootKind::PackageAndHost,
    )
}

fn insert_department_decl_with_root(
    departments: &mut BTreeMap<String, DepartmentDecl>,
    name: &str,
    decl: DepartmentDecl,
    config_path: &Path,
    root_kind: GraphRootKind,
) -> Result<()> {
    if departments.insert(name.to_string(), decl).is_some() {
        bail!(
            "duplicate department `{}` in {} at {}",
            name,
            root_kind.label(),
            config_path.display(),
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn insert_raiser_decl(
    raisers: &mut BTreeMap<String, RaiserDecl>,
    name: &str,
    decl: RaiserDecl,
    config_path: &Path,
) -> Result<()> {
    insert_raiser_decl_with_root(
        raisers,
        name,
        decl,
        config_path,
        GraphRootKind::PackageAndHost,
    )
}

fn insert_raiser_decl_with_root(
    raisers: &mut BTreeMap<String, RaiserDecl>,
    name: &str,
    decl: RaiserDecl,
    config_path: &Path,
    root_kind: GraphRootKind,
) -> Result<()> {
    if raisers.insert(name.to_string(), decl).is_some() {
        bail!(
            "duplicate raiser `{}` in {} at {}",
            name,
            root_kind.label(),
            config_path.display()
        );
    }
    Ok(())
}

fn derive_queues(
    departments: &BTreeMap<String, DepartmentDecl>,
    raisers: &BTreeMap<String, RaiserDecl>,
    department_fanout: &HashMap<String, Vec<String>>,
    defaults: &HostGraphDefaults,
) -> Result<BTreeMap<String, QueueDecl>> {
    let mut referenced = BTreeSet::new();
    for dept in departments.values() {
        for q in &dept.consumes {
            referenced.insert(q.clone());
        }
        for q in &dept.produces {
            referenced.insert(q.clone());
        }
    }
    for raiser in raisers.values() {
        referenced.insert(raiser_produces(raiser).to_string());
    }

    let fanout = resolve_department_fanout(departments, department_fanout)?;
    let mut queues = BTreeMap::new();
    for q in referenced {
        let explicit_fanout = fanout.contains(&q);
        queues.insert(
            q,
            QueueDecl {
                capacity: defaults.queue_capacity,
                fanout: explicit_fanout,
            },
        );
    }
    Ok(queues)
}

fn resolve_department_fanout(
    departments: &BTreeMap<String, DepartmentDecl>,
    department_fanout: &HashMap<String, Vec<String>>,
) -> Result<HashSet<String>> {
    let mut fanout = BTreeSet::new();
    for (name, queues) in sorted_department_fanout(department_fanout) {
        let dept = departments
            .get(name)
            .ok_or_else(|| anyhow!("department `{}` fanout has no department", name))?;
        for queue in queues {
            if !dept.consumes.iter().any(|q| q == queue)
                && !dept.produces.iter().any(|q| q == queue)
            {
                bail!(
                    "department `{}` declares fanout queue `{}` which it does not consume or produce",
                    name,
                    queue
                );
            }
            fanout.insert(queue.clone());
        }
    }
    Ok(fanout.into_iter().collect())
}

fn sorted_department_fanout<'a>(
    department_fanout: &'a HashMap<String, Vec<String>>,
) -> Vec<(&'a String, &'a Vec<String>)> {
    let mut entries = department_fanout.iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    entries
}

fn resolve_runtime_file_watch_glob(raiser: &mut RaiserDecl, host_root: &Path) -> Result<()> {
    if let RaiserDecl::FileWatch { glob, .. } = raiser {
        if let Some(relative) = glob.strip_prefix("runtime://") {
            let layout = runtime_context::layout_from_host_root(host_root)?;
            let (kind, relative) = split_runtime_glob_kind(relative)?;
            if kind == RuntimeKind::Logs {
                bail!("runtime://logs is local-only and cannot be used as file_watch input");
            }
            *glob = layout
                .runtime_path(kind, relative)?
                .to_string_lossy()
                .into_owned();
        }
    }
    Ok(())
}

fn split_runtime_glob_kind(relative: &str) -> Result<(RuntimeKind, &str)> {
    let Some((head, rest)) = relative.split_once('/') else {
        bail!("runtime:// glob must include an explicit runtime kind");
    };
    let kind = RuntimeKind::parse(head)?;
    Ok((kind, rest))
}

fn eval_lua_file(lua: &Lua, lua_roots: &[&Path], path: &Path) -> Result<Table> {
    match eval_lua_value(lua, lua_roots, path)? {
        LuaValue::Table(t) => Ok(t),
        _ => Err(anyhow!("{} did not return a table", path.display())),
    }
}

fn eval_lua_value(lua: &Lua, lua_roots: &[&Path], path: &Path) -> Result<LuaValue> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    set_package_path(lua, lua_roots)?;
    lua.load(&source)
        .set_name(path.display().to_string())
        .eval()
        .with_context(|| format!("eval {}", path.display()))
}

fn set_package_path(lua: &Lua, lua_roots: &[&Path]) -> Result<()> {
    lua.load(format!(
        "package.path = {:?}",
        lua_package_root_path(lua_roots.iter().copied())
    ))
    .exec()
    .context("set package.path")
}

#[cfg(not(test))]
fn lua_package_root_path<'a>(roots: impl IntoIterator<Item = &'a Path>) -> String {
    crate::mlua_init::package_roots_path(roots)
}

#[cfg(test)]
fn lua_package_root_path<'a>(roots: impl IntoIterator<Item = &'a Path>) -> String {
    roots
        .into_iter()
        .map(|repo_root| {
            let root = repo_root.display();
            format!("{root}/?.lua;{root}/?/init.lua;{root}/?/main.lua")
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn sorted_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn sorted_lua_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("lua") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn file_name(path: &Path) -> Result<String> {
    Ok(path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?
        .to_string_lossy()
        .into_owned())
}

fn config_path(repo_root: &Path, root_kind: GraphRootKind, path: &Path) -> PathBuf {
    match root_kind {
        GraphRootKind::Package => path.to_path_buf(),
        GraphRootKind::PackageAndHost => path
            .strip_prefix(repo_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf()),
        GraphRootKind::Host => path.to_path_buf(),
    }
}

fn raiser_produces(r: &RaiserDecl) -> &str {
    match r {
        RaiserDecl::Cron { produces, .. } => produces,
        RaiserDecl::FileWatch { produces, .. } => produces,
    }
}

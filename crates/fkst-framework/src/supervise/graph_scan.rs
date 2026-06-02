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
use fkst_common::{RuntimeKind, RuntimeLayout};
use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::path_resolver::{GraphRoot, GraphRootKind, PackageRoots};

const QUEUE_CAPACITY_KEY: &str = "FKST_QUEUE_CAPACITY";
const DEPARTMENT_DEFAULT_TIMEOUT_KEY: &str = "FKST_DEPARTMENT_DEFAULT_TIMEOUT";
const CODEX_PERMIT_SLOTS_KEY: &str = "FKST_CODEX_PERMIT_SLOTS";

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
        let sources = HostGraphDefaultSources::load(roots)?;
        Ok(Self {
            queue_capacity: sources
                .positive_usize(QUEUE_CAPACITY_KEY, "tunables/queue_capacity.txt")?,
            department_default_timeout: sources.timeout(
                DEPARTMENT_DEFAULT_TIMEOUT_KEY,
                "tunables/department_default_timeout.txt",
            )?,
            codex_permit_slots: sources
                .positive_usize(CODEX_PERMIT_SLOTS_KEY, "tunables/codex_permit_slots.txt")?,
        })
    }
}

struct HostGraphDefaultSources {
    package_root: PathBuf,
    fkst_env: HashMap<String, String>,
}

impl HostGraphDefaultSources {
    fn load(roots: &PackageRoots) -> Result<Self> {
        Ok(Self {
            package_root: roots.package_root().to_path_buf(),
            fkst_env: read_fkst_env(roots.host_root())?,
        })
    }

    fn raw_value(&self, key: &str, tunable_rel: &str) -> Result<String> {
        if let Some(value) = std::env::var_os(key) {
            let value = value
                .into_string()
                .map_err(|_| anyhow!("{key} must be valid UTF-8"))?;
            return Ok(value);
        }

        if let Some(value) = self.fkst_env.get(key) {
            return Ok(value.clone());
        }

        let tunable_path = self.package_root.join(tunable_rel);
        if tunable_path.is_file() {
            return std::fs::read_to_string(&tunable_path)
                .with_context(|| format!("read {}", tunable_path.display()))
                .map(|value| value.trim().to_string());
        }

        bail!("{key} missing; set process env, fkst.env, or {tunable_rel}")
    }

    fn positive_usize(&self, key: &str, tunable_rel: &str) -> Result<usize> {
        let raw = self.raw_value(key, tunable_rel)?;
        let value = raw
            .trim()
            .parse::<usize>()
            .with_context(|| format!("{key} must be a positive integer, got {raw:?}"))?;
        if value == 0 {
            bail!("{key} must be > 0");
        }
        Ok(value)
    }

    fn timeout(&self, key: &str, tunable_rel: &str) -> Result<String> {
        let raw = self.raw_value(key, tunable_rel)?;
        let value = raw.trim().to_string();
        if value.is_empty()
            || !(value.ends_with('s') || value.ends_with('m') || value.ends_with('h'))
        {
            bail!("{key} must be a timeout ending with s/m/h, got {raw:?}");
        }
        Ok(value)
    }
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
    let mut departments: HashMap<String, DepartmentDecl> = HashMap::new();
    let mut raisers: HashMap<String, RaiserDecl> = HashMap::new();
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
        scan_raisers(&lua, graph_root, &lua_roots, &mut raisers)?;
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
    departments: &mut HashMap<String, DepartmentDecl>,
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
    raisers: &mut HashMap<String, RaiserDecl>,
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
        resolve_runtime_file_watch_glob(&mut r)?;

        let config_path = config_path(repo_root, graph_root.kind, &path);
        insert_raiser_decl_with_root(raisers, &stem, r, &config_path, graph_root.kind)?;
    }

    Ok(())
}

#[cfg(test)]
pub(crate) fn insert_department_decl(
    departments: &mut HashMap<String, DepartmentDecl>,
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
    departments: &mut HashMap<String, DepartmentDecl>,
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
    raisers: &mut HashMap<String, RaiserDecl>,
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
    raisers: &mut HashMap<String, RaiserDecl>,
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
    departments: &HashMap<String, DepartmentDecl>,
    raisers: &HashMap<String, RaiserDecl>,
    department_fanout: &HashMap<String, Vec<String>>,
    defaults: &HostGraphDefaults,
) -> Result<HashMap<String, QueueDecl>> {
    let mut referenced = HashSet::new();
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
    let mut queues = HashMap::new();
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

fn read_fkst_env(repo_root: &Path) -> Result<HashMap<String, String>> {
    let path = repo_root.join("fkst.env");
    if !path.is_file() {
        return Ok(HashMap::new());
    }

    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut values = HashMap::new();
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            bail!("{}:{} must use KEY=value", path.display(), idx + 1);
        };
        if key.trim() != key || key.is_empty() {
            bail!("{}:{} has invalid key", path.display(), idx + 1);
        }
        values.insert(key.to_string(), value.to_string());
    }
    Ok(values)
}

fn resolve_department_fanout(
    departments: &HashMap<String, DepartmentDecl>,
    department_fanout: &HashMap<String, Vec<String>>,
) -> Result<HashSet<String>> {
    let mut fanout = HashSet::new();
    for (name, queues) in department_fanout {
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
    Ok(fanout)
}

fn resolve_runtime_file_watch_glob(raiser: &mut RaiserDecl) -> Result<()> {
    if let RaiserDecl::FileWatch { glob, .. } = raiser {
        if let Some(relative) = glob.strip_prefix("runtime://") {
            let layout = RuntimeLayout::from_env()?;
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
        return Ok((RuntimeKind::EvolveRequests, relative));
    };
    match RuntimeKind::parse(head) {
        Ok(kind) => Ok((kind, rest)),
        Err(_) => Ok((RuntimeKind::EvolveRequests, relative)),
    }
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

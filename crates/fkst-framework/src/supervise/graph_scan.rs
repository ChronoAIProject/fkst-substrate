//! Graph scan derives the startup Config from the fixed packages plus host layout.
//!
//! Package roots own standard assets and the host root owns host
//! departments and raisers. `package.lua` is not a supported graph input.
//!
//! Each department exposes `M.spec = { consumes, produces, fanout, stall_window }`
//! at module top level. Queues are auto-derived from the union of department
//! and raiser consumes+produces, with fanout coming only from Department
//! `M.spec.fanout`. Host graph defaults are read before graph materialization.

use anyhow::{anyhow, bail, Context, Result};
use fkst_common::config::{Config, DepartmentDecl, LimitsDecl, QueueDecl, RaiserDecl, RetryDecl};
use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config_registry::{ConfigContext, ConfigKey, ConfigValueType};
use crate::path_resolver::{package_root_path, GraphRoot, GraphRootKind, PackageRoots};

/// Deserialization helper for a department's `M.spec` table.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeptSpec {
    #[serde(default)]
    consumes: Vec<String>,
    #[serde(default)]
    produces: Vec<String>,
    #[serde(default)]
    fanout: Vec<String>,
    #[serde(default)]
    ephemeral: Vec<String>,
    #[serde(default)]
    stall_window: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeptRetrySpec {
    #[serde(default)]
    max_attempts: Option<u64>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    cap: Option<String>,
}

enum DeptRetrySetting {
    Default,
    Custom(DeptRetrySpec),
    Disabled,
}

#[derive(Clone, Debug, PartialEq)]
struct HostGraphDefaults {
    queue_capacity: usize,
    department_default_stall_window: String,
    codex_permit_slots: usize,
    retry: RetryDecl,
}

impl HostGraphDefaults {
    fn load(roots: &PackageRoots) -> Result<Self> {
        let config = ConfigContext::from_host_root(roots.host_root())?;
        Ok(Self {
            queue_capacity: resolve_usize(&config, ConfigKey::QueueCapacity)?,
            department_default_stall_window: resolve_stall_window(
                &config,
                ConfigKey::DepartmentDefaultStallWindow,
            )?,
            codex_permit_slots: resolve_usize(&config, ConfigKey::CodexPermitSlots)?,
            retry: resolve_retry_defaults(&config)?,
        })
    }
}

fn resolve_usize(config: &ConfigContext, key: ConfigKey) -> Result<usize> {
    let entry = crate::config_registry::entry(key);
    assert_eq!(entry.value_type, ConfigValueType::Usize);
    config.resolved_positive_usize(key)
}

fn resolve_stall_window(config: &ConfigContext, key: ConfigKey) -> Result<String> {
    let entry = crate::config_registry::entry(key);
    assert_eq!(entry.value_type, ConfigValueType::DurationString);
    config.resolved_duration_string(key)
}

fn resolve_retry_defaults(config: &ConfigContext) -> Result<RetryDecl> {
    let retry = RetryDecl {
        max_attempts: resolve_usize(config, ConfigKey::RetryDefaultMaxAttempts)? as u64,
        base: resolve_stall_window(config, ConfigKey::RetryDefaultBase)?,
        cap: resolve_stall_window(config, ConfigKey::RetryDefaultCap)?,
    };
    validate_retry_default(&retry)?;
    Ok(retry)
}

fn validate_retry_default(retry: &RetryDecl) -> Result<()> {
    let base = parse_duration_millis(&retry.base)
        .ok_or_else(|| anyhow!("FKST_RETRY_DEFAULT_BASE must be a positive s/m/h duration"))?;
    let cap = parse_duration_millis(&retry.cap)
        .ok_or_else(|| anyhow!("FKST_RETRY_DEFAULT_CAP must be a positive s/m/h duration"))?;
    if cap < base {
        bail!("FKST_RETRY_DEFAULT_CAP must be >= FKST_RETRY_DEFAULT_BASE");
    }
    Ok(())
}

#[cfg(test)]
pub fn load(repo_root: &Path) -> Result<Config> {
    let roots = PackageRoots::resolve(repo_root, vec![repo_root.to_path_buf()])?;
    load_roots(&roots)
}

pub fn load_roots(roots: &PackageRoots) -> Result<Config> {
    let graph_roots = roots.graph_roots();
    for graph_root in &graph_roots {
        reject_removed_surfaces(&graph_root)?;
    }

    let defaults = HostGraphDefaults::load(roots)?;
    let mut departments: BTreeMap<String, DepartmentDecl> = BTreeMap::new();
    let mut raisers: BTreeMap<String, RaiserDecl> = BTreeMap::new();
    let mut department_owners: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut raiser_owners: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut department_fanout: HashMap<String, Vec<String>> = HashMap::new();

    for graph_root in &graph_roots {
        let lua = Lua::new();
        let require_roots = roots.require_roots_for_owner(&graph_root.root);
        scan_departments(
            &lua,
            graph_root,
            &require_roots,
            &defaults,
            &mut departments,
            &mut department_owners,
            &mut department_fanout,
        )?;
        scan_raisers(
            &lua,
            graph_root,
            &require_roots,
            &mut raisers,
            &mut raiser_owners,
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
    require_roots: &[PathBuf],
    defaults: &HostGraphDefaults,
    departments: &mut BTreeMap<String, DepartmentDecl>,
    department_owners: &mut BTreeMap<String, PathBuf>,
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

        let module = eval_lua_file(lua, require_roots, &main_lua)
            .with_context(|| format!("eval department `{}` from {}", name, main_lua.display()))?;
        let spec_tbl: Table = module
            .get("spec")
            .with_context(|| format!("department `{}` missing `M.spec`", name))?;
        let retry = parse_retry_setting(lua, &spec_tbl)
            .with_context(|| format!("parse `{}.spec.retry`", name))?;
        spec_tbl
            .raw_remove("retry")
            .with_context(|| format!("remove `{}.spec.retry` before spec parse", name))?;
        let spec: DeptSpec = lua
            .from_value(LuaValue::Table(spec_tbl))
            .with_context(|| format!("parse `{}.spec`", name))?;
        let retry = materialize_retry(&retry, defaults, &spec.consumes)
            .with_context(|| format!("materialize `{}.spec.retry`", name))?;

        let config_path = config_path(repo_root, graph_root.kind, &main_lua);
        insert_department_decl_with_root(
            departments,
            &name,
            DepartmentDecl {
                lua: config_path.clone(),
                owner_root: repo_root.clone(),
                consumes: spec.consumes,
                produces: spec.produces,
                ephemeral: spec.ephemeral,
                stall_window: if spec.stall_window.trim().is_empty() {
                    defaults.department_default_stall_window.clone()
                } else {
                    spec.stall_window
                },
                retry,
            },
            &config_path,
            repo_root,
            department_owners,
        )?;
        department_fanout.insert(name, spec.fanout);
    }

    Ok(())
}

fn parse_retry_setting(lua: &Lua, spec_tbl: &Table) -> Result<DeptRetrySetting> {
    match spec_tbl.raw_get::<LuaValue>("retry")? {
        LuaValue::Nil => Ok(DeptRetrySetting::Default),
        LuaValue::Boolean(false) => Ok(DeptRetrySetting::Disabled),
        LuaValue::Boolean(true) => bail!("retry=true is unsupported; use retry = {{}}"),
        LuaValue::Table(table) => {
            let spec: DeptRetrySpec = lua.from_value(LuaValue::Table(table))?;
            Ok(DeptRetrySetting::Custom(spec))
        }
        other => bail!(
            "retry must be nil, false, or table; got {}",
            other.type_name()
        ),
    }
}

fn materialize_retry(
    retry: &DeptRetrySetting,
    defaults: &HostGraphDefaults,
    consumes: &[String],
) -> Result<Option<RetryDecl>> {
    match retry {
        DeptRetrySetting::Disabled => Ok(None),
        DeptRetrySetting::Default if consumes.iter().any(|queue| queue == "dead_letter") => {
            if consumes.iter().any(|queue| queue != "dead_letter") {
                bail!(
                    "department consumes `dead_letter` and another queue with retry unset; set M.spec.retry to a table or false"
                );
            }
            Ok(None)
        }
        DeptRetrySetting::Default => Ok(Some(defaults.retry.clone())),
        DeptRetrySetting::Custom(custom) => Ok(Some(RetryDecl {
            max_attempts: custom.max_attempts.unwrap_or(defaults.retry.max_attempts),
            base: custom
                .base
                .clone()
                .unwrap_or_else(|| defaults.retry.base.clone()),
            cap: custom
                .cap
                .clone()
                .unwrap_or_else(|| defaults.retry.cap.clone()),
        })),
    }
}

fn scan_raisers(
    lua: &Lua,
    graph_root: &GraphRoot,
    require_roots: &[PathBuf],
    raisers: &mut BTreeMap<String, RaiserDecl>,
    raiser_owners: &mut BTreeMap<String, PathBuf>,
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
        let val = eval_lua_value(lua, require_roots, &path)
            .with_context(|| format!("eval raiser `{}` from {}", stem, path.display()))?;
        let r: RaiserDecl = lua
            .from_value(val)
            .with_context(|| format!("parse raisers/{}.lua", stem))?;
        reject_runtime_file_watch_glob(&r)?;

        let config_path = config_path(repo_root, graph_root.kind, &path);
        insert_raiser_decl_with_root(raisers, &stem, r, &config_path, repo_root, raiser_owners)?;
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
    let mut owners = departments
        .iter()
        .map(|(existing_name, existing_decl)| {
            (existing_name.clone(), existing_decl.owner_root.clone())
        })
        .collect();
    let owner_root = decl.owner_root.clone();
    insert_department_decl_with_root(
        departments,
        name,
        decl,
        config_path,
        &owner_root,
        &mut owners,
    )
}

fn insert_department_decl_with_root(
    departments: &mut BTreeMap<String, DepartmentDecl>,
    name: &str,
    decl: DepartmentDecl,
    config_path: &Path,
    owner_root: &Path,
    department_owners: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    if let Some(previous_root) = department_owners.get(name) {
        bail!(
            "duplicate department `{}`: previous root {} new root {} at {}",
            name,
            previous_root.display(),
            owner_root.display(),
            config_path.display(),
        );
    }
    department_owners.insert(name.to_string(), owner_root.to_path_buf());
    departments.insert(name.to_string(), decl);
    Ok(())
}

#[cfg(test)]
pub(crate) fn insert_raiser_decl(
    raisers: &mut BTreeMap<String, RaiserDecl>,
    name: &str,
    decl: RaiserDecl,
    config_path: &Path,
) -> Result<()> {
    let mut owners = raisers
        .keys()
        .map(|existing_name| {
            (
                existing_name.clone(),
                PathBuf::from("<existing raiser test root>"),
            )
        })
        .collect();
    let owner_root = config_path
        .parent()
        .and_then(Path::parent)
        .unwrap_or(config_path)
        .to_path_buf();
    insert_raiser_decl_with_root(raisers, name, decl, config_path, &owner_root, &mut owners)
}

fn insert_raiser_decl_with_root(
    raisers: &mut BTreeMap<String, RaiserDecl>,
    name: &str,
    decl: RaiserDecl,
    config_path: &Path,
    owner_root: &Path,
    raiser_owners: &mut BTreeMap<String, PathBuf>,
) -> Result<()> {
    if let Some(previous_root) = raiser_owners.get(name) {
        bail!(
            "duplicate raiser `{}`: previous root {} new root {} at {}",
            name,
            previous_root.display(),
            owner_root.display(),
            config_path.display()
        );
    }
    raiser_owners.insert(name.to_string(), owner_root.to_path_buf());
    raisers.insert(name.to_string(), decl);
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

const REMOVED_RUNTIME_SCHEME: &str = concat!("runtime", "://");

fn reject_runtime_file_watch_glob(raiser: &RaiserDecl) -> Result<()> {
    if let RaiserDecl::FileWatch { glob, .. } = raiser {
        if glob.starts_with(REMOVED_RUNTIME_SCHEME) {
            bail!(concat!(
                "runtime",
                ":// file_watch glob is a removed surface; use a host-root relative or absolute glob"
            ));
        }
    }
    Ok(())
}

fn eval_lua_file(lua: &Lua, require_roots: &[PathBuf], path: &Path) -> Result<Table> {
    match eval_lua_value(lua, require_roots, path)? {
        LuaValue::Table(t) => Ok(t),
        _ => Err(anyhow!("{} did not return a table", path.display())),
    }
}

fn eval_lua_value(lua: &Lua, require_roots: &[PathBuf], path: &Path) -> Result<LuaValue> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    set_package_roots_path(lua, require_roots.iter().map(PathBuf::as_path))?;
    lua.load(&source)
        .set_name(path.display().to_string())
        .eval()
        .with_context(|| format!("eval {}", path.display()))
}

fn set_package_roots_path<'a>(
    lua: &Lua,
    package_roots: impl IntoIterator<Item = &'a Path>,
) -> Result<()> {
    let roots_path = package_roots
        .into_iter()
        .map(package_root_path)
        .collect::<Vec<_>>()
        .join(";");
    lua.load(format!(
        "package.path = {:?}; package.cpath = \"\"",
        roots_path
    ))
    .exec()
    .context("set package.path")
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

fn parse_duration_millis(raw: &str) -> Option<u128> {
    let unit = raw.chars().last()?;
    let value = raw[..raw.len() - unit.len_utf8()].parse::<u128>().ok()?;
    if value == 0 {
        return None;
    }
    match unit {
        's' => Some(value.saturating_mul(1_000)),
        'm' => Some(value.saturating_mul(60_000)),
        'h' => Some(value.saturating_mul(3_600_000)),
        _ => None,
    }
}

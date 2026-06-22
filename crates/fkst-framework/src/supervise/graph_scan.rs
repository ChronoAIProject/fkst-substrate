//! Graph scan derives the startup Config from the fixed packages plus host layout.
//!
//! Package roots own standard assets and the host root owns host
//! departments and raisers. `package.lua` is not a supported graph input.
//!
//! Each department exposes `M.spec = { consumes, produces, fanout, stall_window }`
//! at module top level. Queues are auto-derived from the union of department
//! and raiser consumes+produces, with fanout coming only from Department
//! `M.spec.fanout`. `M.spec.published_seam` marks consumed queues as public
//! sibling entry points. `M.spec.graph_json` authorizes composed graph
//! introspection. Host graph defaults are read before graph materialization.

use anyhow::{anyhow, bail, Context, Result};
use fkst_common::built_in_provider::{built_in_provider_for_queue, BUILT_IN_DEAD_LETTER_PROVIDER};
use fkst_common::config::{Config, DepartmentDecl, LimitsDecl, QueueDecl, RaiserDecl, RetryDecl};
use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config_registry::{ConfigContext, ConfigKey, ConfigValueType};
use crate::path_resolver::{
    validate_name_segment, GraphRoot, GraphRootKind, NameResolver, PackageRoots,
};
const FAILURE_FACT_QUEUE: &str = "fkst.failure_fact";

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
    published_seam: Vec<String>,
    #[serde(default)]
    ephemeral: Vec<String>,
    #[serde(default)]
    graph_json: bool,
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
        crate::rate_pool::RatePoolRegistry::from_config(&config)?;
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
    let catalog = Arc::new(roots.unit_catalog().clone());
    let resolver = roots.name_resolver();
    let subscribe_resolver = resolver.clone().add_recorded_only_queue(FAILURE_FACT_QUEUE);
    let mut departments: BTreeMap<String, DepartmentDecl> = BTreeMap::new();
    let mut raisers: BTreeMap<String, RaiserDecl> = BTreeMap::new();
    let mut department_fanout: HashMap<String, Vec<String>> = HashMap::new();
    let mut department_published_seam: HashMap<String, Vec<String>> = HashMap::new();

    for graph_root in &graph_roots {
        let lua = Lua::new();
        register_spec_eval_pure_primitives(&lua, &graph_root.root)
            .context("register graph-scan pure primitives")?;
        let owner_unit = catalog
            .unit_name_for_root(&graph_root.root)?
            .ok_or_else(|| anyhow!("no manifest unit owns {}", graph_root.root.display()))?;
        scan_departments(
            &lua,
            graph_root,
            catalog.clone(),
            &owner_unit,
            &resolver,
            &subscribe_resolver,
            &defaults,
            &mut departments,
            &mut department_fanout,
            &mut department_published_seam,
        )?;
        scan_raisers(
            &lua,
            graph_root,
            catalog.clone(),
            &owner_unit,
            &resolver,
            &mut raisers,
        )?;
    }

    validate_cross_package_produces(&departments, &raisers, &department_published_seam)?;
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

fn register_spec_eval_pure_primitives(lua: &Lua, owner_root: &Path) -> mlua::Result<()> {
    crate::sdk_strings::register_truncate_utf8(lua)?;
    crate::sdk_i18n::register(lua, owner_root)?;
    Ok(())
}

fn scan_departments(
    lua: &Lua,
    graph_root: &GraphRoot,
    catalog: Arc<crate::manifest::UnitCatalog>,
    owner_unit: &str,
    resolver: &NameResolver,
    subscribe_resolver: &NameResolver,
    defaults: &HostGraphDefaults,
    departments: &mut BTreeMap<String, DepartmentDecl>,
    department_fanout: &mut HashMap<String, Vec<String>>,
    department_published_seam: &mut HashMap<String, Vec<String>>,
) -> Result<()> {
    let repo_root = &graph_root.root;
    let dept_dir = repo_root.join("departments");
    if !dept_dir.is_dir() {
        return Ok(());
    }

    for path in sorted_dirs(&dept_dir).with_context(|| format!("read {}", dept_dir.display()))? {
        let name = file_name(&path)?;
        validate_name_segment("department name", &name)
            .with_context(|| format!("validate department `{}`", name))?;
        let main_lua = path.join("main.lua");
        if !main_lua.is_file() {
            continue;
        }

        let module = eval_lua_file(lua, catalog.clone(), owner_unit, &main_lua)
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
        let consumes = resolve_queues(subscribe_resolver, &graph_root.namespace, spec.consumes)
            .with_context(|| format!("resolve `{}.spec.consumes`", name))?;
        let produces = resolve_queues(resolver, &graph_root.namespace, spec.produces)
            .with_context(|| format!("resolve `{}.spec.produces`", name))?;
        reject_engine_dead_letter_produces(&produces, &name)
            .with_context(|| format!("validate `{}.spec.produces`", name))?;
        let fanout = resolve_queues(resolver, &graph_root.namespace, spec.fanout)
            .with_context(|| format!("resolve `{}.spec.fanout`", name))?;
        let published_seam = resolve_queues(resolver, &graph_root.namespace, spec.published_seam)
            .with_context(|| format!("resolve `{}.spec.published_seam`", name))?;
        validate_published_seam(&published_seam, &consumes, &graph_root.namespace, &name)
            .with_context(|| format!("validate `{}.spec.published_seam`", name))?;
        let ephemeral = resolve_queues(subscribe_resolver, &graph_root.namespace, spec.ephemeral)
            .with_context(|| format!("resolve `{}.spec.ephemeral`", name))?;
        let retry = materialize_retry(&retry, defaults, &consumes)
            .with_context(|| format!("materialize `{}.spec.retry`", name))?;

        let config_path = config_path(repo_root, graph_root.kind, &main_lua);
        let canonical_name = resolver
            .canonical_decl_name(&graph_root.namespace, &name)
            .with_context(|| format!("resolve department `{}`", name))?;
        insert_department_decl(
            departments,
            &canonical_name,
            DepartmentDecl {
                lua: config_path.clone(),
                owner_root: repo_root.clone(),
                owner_namespace: graph_root.namespace.clone(),
                consumes,
                produces,
                ephemeral,
                stall_window: if spec.stall_window.trim().is_empty() {
                    defaults.department_default_stall_window.clone()
                } else {
                    spec.stall_window
                },
                graph_json: spec.graph_json,
                retry,
            },
            &config_path,
        )?;
        department_fanout.insert(canonical_name.clone(), fanout);
        department_published_seam.insert(canonical_name, published_seam);
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
        DeptRetrySetting::Default if consumes.iter().any(|queue| is_dead_letter_queue(queue)) => {
            if consumes.iter().any(|queue| !is_dead_letter_queue(queue)) {
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
    catalog: Arc<crate::manifest::UnitCatalog>,
    owner_unit: &str,
    resolver: &NameResolver,
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
        validate_name_segment("raiser name", &stem)
            .with_context(|| format!("validate raiser `{}`", stem))?;
        let val = eval_lua_value(lua, catalog.clone(), owner_unit, &path)
            .with_context(|| format!("eval raiser `{}` from {}", stem, path.display()))?;
        let r: RaiserDecl = lua
            .from_value(val)
            .with_context(|| format!("parse raisers/{}.lua", stem))?;
        reject_runtime_file_watch_glob(&r)?;
        let r = resolve_raiser(resolver, &graph_root.namespace, r)
            .with_context(|| format!("resolve raiser `{}`", stem))?;

        let config_path = config_path(repo_root, graph_root.kind, &path);
        let canonical_name = resolver
            .canonical_decl_name(&graph_root.namespace, &stem)
            .with_context(|| format!("resolve raiser `{}`", stem))?;
        insert_raiser_decl(raisers, &canonical_name, r, &config_path)?;
    }

    Ok(())
}

pub(crate) fn insert_department_decl(
    departments: &mut BTreeMap<String, DepartmentDecl>,
    name: &str,
    decl: DepartmentDecl,
    config_path: &Path,
) -> Result<()> {
    if departments.contains_key(name) {
        bail!(
            "duplicate department `{}` at {}",
            name,
            config_path.display()
        );
    }
    departments.insert(name.to_string(), decl);
    Ok(())
}

pub(crate) fn insert_raiser_decl(
    raisers: &mut BTreeMap<String, RaiserDecl>,
    name: &str,
    decl: RaiserDecl,
    config_path: &Path,
) -> Result<()> {
    if raisers.contains_key(name) {
        bail!("duplicate raiser `{}` at {}", name, config_path.display());
    }
    raisers.insert(name.to_string(), decl);
    Ok(())
}

fn is_dead_letter_queue(queue: &str) -> bool {
    built_in_provider_for_queue(queue) == Some(BUILT_IN_DEAD_LETTER_PROVIDER)
}

fn reject_engine_dead_letter_produces(produces: &[String], department_name: &str) -> Result<()> {
    if let Some((queue, contract)) = produces
        .iter()
        .find_map(|queue| built_in_provider_for_queue(queue).map(|contract| (queue, contract)))
    {
        bail!(
            "department `{}` must not produce runtime-owned queue `{}` from built-in provider `{}`",
            department_name,
            queue,
            contract.provider
        );
    }
    Ok(())
}

fn validate_published_seam(
    published_seam: &[String],
    consumes: &[String],
    owner_namespace: &str,
    department_name: &str,
) -> Result<()> {
    for queue in published_seam {
        if !consumes.iter().any(|consumed| consumed == queue) {
            bail!(
                "department `{}` publishes queue `{}` in M.spec.published_seam but does not consume it",
                department_name,
                queue
            );
        }
        if !queue_owned_by_namespace(queue, owner_namespace) {
            bail!(
                "department `{}` publishes queue `{}` in M.spec.published_seam but the queue is not owned by namespace `{}`",
                department_name,
                queue,
                owner_namespace
            );
        }
    }
    Ok(())
}

fn validate_cross_package_produces(
    departments: &BTreeMap<String, DepartmentDecl>,
    raisers: &BTreeMap<String, RaiserDecl>,
    department_published_seam: &HashMap<String, Vec<String>>,
) -> Result<()> {
    let mut published_seam = HashSet::new();
    for queues in department_published_seam.values() {
        published_seam.extend(queues.iter().cloned());
    }

    for (name, dept) in departments {
        for queue in &dept.produces {
            if queue_owned_by_namespace(queue, &dept.owner_namespace)
                || published_seam.contains(queue)
            {
                continue;
            }
            bail!(
                "department `{}` produces sibling queue `{}` which is not published by that package in M.spec.published_seam",
                name,
                queue
            );
        }
    }
    for (name, raiser) in raisers {
        let queue = raiser_produces(raiser);
        let owner_namespace = declaration_namespace(name).unwrap_or("");
        if queue_owned_by_namespace(queue, owner_namespace) || published_seam.contains(queue) {
            continue;
        }
        bail!(
            "raiser `{}` produces sibling queue `{}` which is not published by that package in M.spec.published_seam",
            name,
            queue
        );
    }
    Ok(())
}

fn queue_owned_by_namespace(queue: &str, owner_namespace: &str) -> bool {
    match queue.split_once('.') {
        Some((namespace, _)) => namespace == owner_namespace,
        None => true,
    }
}

fn declaration_namespace(name: &str) -> Option<&str> {
    name.split_once('.').map(|(namespace, _)| namespace)
}

fn resolve_queues(
    resolver: &NameResolver,
    owner_namespace: &str,
    queues: Vec<String>,
) -> Result<Vec<String>> {
    queues
        .into_iter()
        .map(|queue| resolver.resolve(owner_namespace, &queue))
        .collect()
}

fn resolve_raiser(
    resolver: &NameResolver,
    owner_namespace: &str,
    raiser: RaiserDecl,
) -> Result<RaiserDecl> {
    match raiser {
        RaiserDecl::Cron { interval, produces } => Ok(RaiserDecl::Cron {
            interval,
            produces: resolver.resolve(owner_namespace, &produces)?,
        }),
        RaiserDecl::FileWatch { glob, produces } => Ok(RaiserDecl::FileWatch {
            glob,
            produces: resolver.resolve(owner_namespace, &produces)?,
        }),
    }
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

fn eval_lua_file(
    lua: &Lua,
    catalog: Arc<crate::manifest::UnitCatalog>,
    owner_unit: &str,
    path: &Path,
) -> Result<Table> {
    match eval_lua_value(lua, catalog, owner_unit, path)? {
        LuaValue::Table(t) => Ok(t),
        _ => Err(anyhow!("{} did not return a table", path.display())),
    }
}

fn eval_lua_value(
    lua: &Lua,
    catalog: Arc<crate::manifest::UnitCatalog>,
    owner_unit: &str,
    path: &Path,
) -> Result<LuaValue> {
    crate::lua_require::load_unit_chunk(
        lua,
        catalog,
        owner_unit,
        path,
        format!("@{}", path.display()),
        None,
    )
    .with_context(|| format!("eval {}", path.display()))
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

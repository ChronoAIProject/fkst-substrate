//! Resolve fixed package roots, host graph inputs, and package-local names.

use crate::manifest::UnitCatalog;
use crate::manifest_external::{
    local_source_roots_for_package_roots, validate_locked_local_sources, Lockfile,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const PACKAGE_ROOT_ENV: &str = "FKST_PACKAGE_ROOT";
pub(crate) const PACKAGE_ROOTS_ENV: &str = "FKST_PACKAGE_ROOTS";
pub(crate) const HOST_NAMESPACE: &str = "host";

const CATALOG_SNAPSHOT_SCHEMA: &str = "fkst.package-roots-catalog.v1";
const MAX_CATALOG_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;

const REJECTED_PACKAGE_ROOT_ENVS: &[&str] = &[
    "FKST_STDLIB_ROOT",
    "FKST_RUNTIME_PACKAGE_ROOT",
    "FKST_GRAPH_ROOTS",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PackageRoots {
    package_roots: Vec<PathBuf>,
    host_root: PathBuf,
    package_namespaces: Vec<String>,
    run_host_owner: bool,
    catalog: Option<UnitCatalog>,
    external_catalogs: BTreeMap<PathBuf, UnitCatalog>,
    external_package_catalog_roots: BTreeMap<PathBuf, PathBuf>,
    #[serde(skip)]
    catalog_snapshot: Option<Arc<[u8]>>,
}

impl PackageRoots {
    pub(crate) fn resolve(
        host_root: impl AsRef<Path>,
        explicit_package_roots: Vec<PathBuf>,
    ) -> Result<Self> {
        reject_removed_package_root_envs()?;
        let host_root = canonical_dir(host_root.as_ref(), "--project-root")?;
        let package_roots = if explicit_package_roots.is_empty() {
            PackageRootInput {
                roots: roots_from_env()?,
                explicit: false,
            }
        } else {
            PackageRootInput {
                roots: canonical_dirs(explicit_package_roots, "--package-root")?,
                explicit: true,
            }
        };
        Self::from_canonical(host_root, package_roots, false)
    }

    pub(crate) fn resolve_run(
        host_root: impl AsRef<Path>,
        explicit_package_roots: Vec<PathBuf>,
    ) -> Result<Self> {
        reject_removed_package_root_envs()?;
        if std::env::var_os(PACKAGE_ROOTS_ENV).is_some() {
            bail!("{PACKAGE_ROOTS_ENV} is not valid for `run`; pass one --package-root");
        }
        let host_root = canonical_dir(host_root.as_ref(), "--project-root")?;
        let package_roots = if explicit_package_roots.is_empty() {
            match std::env::var_os(PACKAGE_ROOT_ENV) {
                Some(root) if !root.is_empty() => PackageRootInput {
                    roots: vec![canonical_dir(Path::new(&root), PACKAGE_ROOT_ENV)?],
                    explicit: false,
                },
                Some(_) => bail!("{PACKAGE_ROOT_ENV} must not be empty"),
                None => bail!("{PACKAGE_ROOT_ENV} or --package-root is required"),
            }
        } else {
            PackageRootInput {
                roots: canonical_dirs(explicit_package_roots, "--package-root")?,
                explicit: true,
            }
        };
        let run_host_owner = package_roots.roots.len() > 1
            && package_roots.roots.iter().any(|root| root == &host_root);
        Self::from_canonical(host_root, package_roots, run_host_owner)
    }

    pub(crate) fn prepare_catalog_snapshot(&mut self) -> Result<()> {
        if self.catalog_snapshot.is_none() {
            self.catalog_snapshot = Some(self.serialize_catalog_snapshot()?.into());
        }
        Ok(())
    }

    pub(crate) fn catalog_snapshot(&self) -> Result<Arc<[u8]>> {
        match self.catalog_snapshot.as_ref() {
            Some(snapshot) => Ok(Arc::clone(snapshot)),
            None => Ok(self.serialize_catalog_snapshot()?.into()),
        }
    }

    fn serialize_catalog_snapshot(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(&CatalogSnapshot {
            schema: CATALOG_SNAPSHOT_SCHEMA,
            roots: self,
        })
        .context("serialize package catalog snapshot")
    }

    pub(crate) fn resolve_run_snapshot(
        host_root: impl AsRef<Path>,
        explicit_package_roots: Vec<PathBuf>,
        reader: impl Read,
    ) -> Result<Self> {
        reject_removed_package_root_envs()?;
        if std::env::var_os(PACKAGE_ROOTS_ENV).is_some() {
            bail!("{PACKAGE_ROOTS_ENV} is not valid for `run`; pass one --package-root");
        }
        if explicit_package_roots.is_empty() {
            bail!("--catalog-stdin requires at least one --package-root");
        }
        let host_root = canonical_dir(host_root.as_ref(), "--project-root")?;
        let package_roots = canonical_dirs(explicit_package_roots, "--package-root")?;
        let mut bytes = Vec::new();
        reader
            .take(MAX_CATALOG_SNAPSHOT_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("read package catalog snapshot from stdin")?;
        if bytes.len() as u64 > MAX_CATALOG_SNAPSHOT_BYTES {
            bail!(
                "package catalog snapshot exceeds {} bytes",
                MAX_CATALOG_SNAPSHOT_BYTES
            );
        }
        let snapshot: OwnedCatalogSnapshot =
            serde_json::from_slice(&bytes).context("decode package catalog snapshot from stdin")?;
        if snapshot.schema != CATALOG_SNAPSHOT_SCHEMA {
            bail!(
                "unsupported package catalog snapshot schema `{}`",
                snapshot.schema
            );
        }
        snapshot
            .roots
            .validate_snapshot_binding(&host_root, &package_roots)?;
        Ok(snapshot.roots)
    }

    fn validate_snapshot_binding(&self, host_root: &Path, package_roots: &[PathBuf]) -> Result<()> {
        if self.host_root != host_root || self.package_roots != package_roots {
            bail!("package catalog snapshot roots do not match run arguments");
        }
        let run_host_owner =
            package_roots.len() > 1 && package_roots.iter().any(|root| root == host_root);
        let package_namespaces = package_namespaces(host_root, package_roots, run_host_owner)?;
        if self.run_host_owner != run_host_owner || self.package_namespaces != package_namespaces {
            bail!("package catalog snapshot namespace binding is invalid");
        }
        for package_root in package_roots {
            let catalog = self.catalog_for_owner_root(package_root)?;
            if catalog.unit_name_for_root(package_root)?.is_none() {
                bail!(
                    "package catalog snapshot has no manifest unit for owner root {}",
                    package_root.display()
                );
            }
        }
        Ok(())
    }

    pub(crate) fn package_roots(&self) -> &[PathBuf] {
        &self.package_roots
    }

    pub(crate) fn host_root(&self) -> &Path {
        &self.host_root
    }

    /// The owner namespace for a direct `run` when `--owner-namespace` is omitted:
    /// a single package root unambiguously owns the spawned department. Returns
    /// None when several package roots are present (the supervisor then passes the
    /// owner namespace explicitly, e.g. `host` for host departments).
    pub(crate) fn sole_package_namespace(&self) -> Option<&str> {
        match self.package_namespaces.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    fn from_canonical(
        host_root: PathBuf,
        package_roots: PackageRootInput,
        run_host_owner: bool,
    ) -> Result<Self> {
        let package_roots_are_explicit = package_roots.explicit;
        let package_roots = package_roots.roots;
        if !package_roots_are_explicit {
            reject_env_external_package_roots(&host_root, &package_roots)?;
        }
        let package_namespaces = package_namespaces(&host_root, &package_roots, run_host_owner)?;
        let host_is_folded = package_roots.iter().any(|root| root == &host_root);
        // Package basenames become queue/dept qualifiers only when more than one
        // graph root is composed. A single folded package==host root stays in
        // LegacyFlat and never uses its basename as a name, so its directory name
        // is unconstrained (e.g. tempdir names beginning with a dot).
        let namespaced = package_roots.len() + usize::from(!host_is_folded) > 1;
        if namespaced {
            for namespace in &package_namespaces {
                validate_name_segment("package root basename", namespace)?;
            }
            if !host_is_folded
                && package_namespaces
                    .iter()
                    .any(|namespace| namespace == HOST_NAMESPACE)
            {
                bail!(
                    "package root basename `{HOST_NAMESPACE}` conflicts with independent host namespace `{HOST_NAMESPACE}`"
                );
            }
        }
        let excluded_roots = if package_roots_are_explicit {
            explicit_external_roots(&host_root, &package_roots)?
        } else {
            BTreeSet::new()
        };
        let host_catalog_may_be_absent = package_roots_are_explicit
            && package_roots
                .iter()
                .all(|package_root| !package_root.starts_with(&host_root));
        let mut catalog = UnitCatalog::discover_excluding_roots(&host_root, &excluded_roots)?;
        if catalog.is_none() && !host_catalog_may_be_absent {
            bail!("manifest catalog is required: missing fkst.workspace.toml");
        }
        let local_source_roots = if package_roots_are_explicit {
            match catalog.as_ref() {
                Some(catalog) => local_source_roots_for_package_roots(
                    &host_root,
                    catalog.workspace().external_sources(),
                    &package_roots,
                )?,
                None => BTreeMap::new(),
            }
        } else {
            BTreeMap::new()
        };
        if !local_source_roots.is_empty() {
            catalog = Some(
                UnitCatalog::discover_excluding_roots_with_local_sources(
                    &host_root,
                    &excluded_roots,
                    local_source_roots.clone(),
                )?
                .ok_or_else(|| {
                    anyhow::anyhow!("manifest catalog is required: missing fkst.workspace.toml")
                })?,
            );
        }
        if let Some(catalog) = catalog.as_ref() {
            validate_locked_local_package_sources(catalog, &local_source_roots)?;
        }
        let (external_catalogs, external_package_catalog_roots) = if package_roots_are_explicit {
            if let Some(catalog) = catalog.as_ref() {
                validate_external_package_declarations(&host_root, catalog, &package_roots)?;
            }
            external_package_catalogs(&host_root, catalog.as_ref(), &package_roots)?
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
        Ok(Self {
            package_roots,
            host_root,
            package_namespaces,
            run_host_owner,
            catalog,
            external_catalogs,
            external_package_catalog_roots,
            catalog_snapshot: None,
        })
    }

    pub(crate) fn unit_catalog(&self) -> Option<&UnitCatalog> {
        self.catalog.as_ref()
    }

    pub(crate) fn catalog_for_owner_root(&self, owner_root: &Path) -> Result<&UnitCatalog> {
        let canonical = owner_root
            .canonicalize()
            .with_context(|| format!("canonicalize {}", owner_root.display()))?;
        self.catalog_for_canonical_owner_root(&canonical)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "manifest catalog is required for owner root {}",
                    canonical.display()
                )
            })
    }

    fn catalog_for_canonical_owner_root(&self, canonical: &Path) -> Result<Option<&UnitCatalog>> {
        if let Some(workspace_root) = self.external_package_catalog_roots.get(canonical) {
            return self
                .external_catalogs
                .get(workspace_root)
                .map(Some)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "external package root {} maps to missing catalog {}",
                        canonical.display(),
                        workspace_root.display()
                    )
                });
        }
        if let Some(catalog) = self.catalog.as_ref() {
            if catalog.unit_name_for_root(canonical)?.is_some() {
                return Ok(Some(catalog));
            }
        }
        for catalog in self.external_catalogs.values() {
            if catalog.unit_name_for_root(canonical)?.is_some() {
                return Ok(Some(catalog));
            }
        }
        Ok(self.catalog.as_ref())
    }

    pub(crate) fn unit_name_for_owner_root(&self, owner_root: &Path) -> Result<Option<String>> {
        let canonical = owner_root
            .canonicalize()
            .with_context(|| format!("canonicalize {}", owner_root.display()))?;
        match self.catalog_for_canonical_owner_root(&canonical)? {
            Some(catalog) => catalog.unit_name_for_root(&canonical),
            None => Ok(None),
        }
    }

    pub(crate) fn graph_roots(&self) -> Vec<GraphRoot> {
        let mut roots = Vec::new();
        let mut host_folded = false;
        for (package_root, namespace) in self.package_roots.iter().zip(&self.package_namespaces) {
            if package_root == &self.host_root {
                roots.push(GraphRoot {
                    root: package_root.clone(),
                    kind: if self.run_host_owner {
                        GraphRootKind::Host
                    } else {
                        GraphRootKind::PackageAndHost
                    },
                    namespace: namespace.clone(),
                });
                host_folded = true;
            } else {
                roots.push(GraphRoot {
                    root: package_root.clone(),
                    kind: GraphRootKind::Package,
                    namespace: namespace.clone(),
                });
            }
        }
        if !host_folded {
            roots.push(GraphRoot {
                root: self.host_root.clone(),
                kind: GraphRootKind::Host,
                namespace: HOST_NAMESPACE.to_string(),
            });
        }
        roots
    }

    pub(crate) fn is_single_folded_graph_root(&self) -> bool {
        matches!(
            self.graph_roots().as_slice(),
            [GraphRoot {
                kind: GraphRootKind::PackageAndHost,
                ..
            }]
        )
    }

    pub(crate) fn name_resolver(&self) -> NameResolver {
        let graph_roots = self.graph_roots();
        NameResolver::new(graph_roots.iter().map(|root| root.namespace.clone()))
    }

    pub(crate) fn owner_root_for_namespace(&self, namespace: &str) -> Option<&Path> {
        self.graph_roots()
            .into_iter()
            .find(|root| root.namespace == namespace)
            .map(|root| {
                if root.root == self.host_root {
                    self.host_root.as_path()
                } else {
                    self.package_roots
                        .iter()
                        .find(|package_root| *package_root == &root.root)
                        .map(PathBuf::as_path)
                        .unwrap_or(self.host_root.as_path())
                }
            })
    }

    pub(crate) fn conformance_library_roots(&self) -> Vec<LibraryConformanceRoot> {
        let mut roots = BTreeMap::new();
        if let Some(catalog) = self.catalog.as_ref() {
            collect_conformance_library_roots(catalog.units(), &mut roots);
        }
        for catalog in self.external_catalogs.values() {
            collect_conformance_library_roots(catalog.units(), &mut roots);
        }
        roots.into_values().collect()
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogSnapshot<'a> {
    schema: &'static str,
    roots: &'a PackageRoots,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedCatalogSnapshot {
    schema: String,
    roots: PackageRoots,
}

struct PackageRootInput {
    roots: Vec<PathBuf>,
    explicit: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct GraphRoot {
    pub(crate) root: PathBuf,
    pub(crate) kind: GraphRootKind,
    pub(crate) namespace: String,
}

#[derive(Clone, Debug)]
pub(crate) struct LibraryConformanceRoot {
    pub(crate) root: PathBuf,
    pub(crate) namespace: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GraphRootKind {
    Package,
    Host,
    PackageAndHost,
}

fn reject_removed_package_root_envs() -> Result<()> {
    for key in REJECTED_PACKAGE_ROOT_ENVS {
        if std::env::var_os(key).is_some() {
            bail!(
                "{key} is a removed package root surface; use {PACKAGE_ROOTS_ENV}, {PACKAGE_ROOT_ENV}, or --package-root"
            );
        }
    }
    Ok(())
}

fn roots_from_env() -> Result<Vec<PathBuf>> {
    let plural = std::env::var_os(PACKAGE_ROOTS_ENV);
    let singular = std::env::var_os(PACKAGE_ROOT_ENV);
    match (plural, singular) {
        (Some(_), Some(_)) => bail!(
            "{PACKAGE_ROOTS_ENV} and {PACKAGE_ROOT_ENV} are mutually exclusive without --package-root"
        ),
        (Some(raw), None) if raw.is_empty() => bail!("{PACKAGE_ROOTS_ENV} must not be empty"),
        (Some(raw), None) => {
            let roots = std::env::split_paths(&raw).collect::<Vec<_>>();
            if roots.is_empty() {
                bail!("{PACKAGE_ROOTS_ENV} must contain at least one path");
            }
            canonical_dirs(roots, PACKAGE_ROOTS_ENV)
        }
        (None, Some(root)) if root.is_empty() => bail!("{PACKAGE_ROOT_ENV} must not be empty"),
        (None, Some(root)) => Ok(vec![canonical_dir(Path::new(&root), PACKAGE_ROOT_ENV)?]),
        (None, None) => bail!(
            "{PACKAGE_ROOTS_ENV}, {PACKAGE_ROOT_ENV}, or --package-root is required"
        ),
    }
}

fn canonical_dirs(roots: Vec<PathBuf>, label: &str) -> Result<Vec<PathBuf>> {
    if roots.is_empty() {
        bail!("{label} must contain at least one path");
    }
    let roots = roots
        .into_iter()
        .map(|root| canonical_dir(&root, label))
        .collect::<Result<Vec<_>>>()?;
    reject_duplicate_roots(&roots)?;
    Ok(roots)
}

fn package_namespaces(
    host_root: &Path,
    package_roots: &[PathBuf],
    run_host_owner: bool,
) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut namespaces = Vec::new();
    for root in package_roots {
        let namespace = if run_host_owner && root == host_root {
            HOST_NAMESPACE.to_string()
        } else {
            let namespace = root
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow::anyhow!("package root has no basename: {}", root.display()))?
                .to_string();
            namespace
        };
        if !seen.insert(namespace.clone()) {
            bail!("duplicate package root basename `{namespace}`");
        }
        namespaces.push(namespace);
    }
    Ok(namespaces)
}

fn reject_duplicate_roots(roots: &[PathBuf]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for root in roots {
        if !seen.insert(root.clone()) {
            bail!("duplicate package root: {}", root.display());
        }
    }
    Ok(())
}

fn reject_env_external_package_roots(host_root: &Path, package_roots: &[PathBuf]) -> Result<()> {
    for package_root in package_roots {
        if package_root != host_root && !is_under(package_root, host_root) {
            bail!(
                "external package root {} requires explicit --package-root",
                package_root.display()
            );
        }
    }
    Ok(())
}

fn explicit_external_roots(
    host_root: &Path,
    package_roots: &[PathBuf],
) -> Result<BTreeSet<PathBuf>> {
    let mut roots = BTreeSet::new();
    for package_root in package_roots {
        let Some(workspace_root) = UnitCatalog::discover_workspace_root(package_root)? else {
            continue;
        };
        if workspace_root != host_root && workspace_root.starts_with(host_root) {
            roots.insert(workspace_root);
            roots.insert(package_root.clone());
        }
    }
    Ok(roots)
}

fn external_package_catalogs(
    host_root: &Path,
    host_catalog: Option<&UnitCatalog>,
    package_roots: &[PathBuf],
) -> Result<(BTreeMap<PathBuf, UnitCatalog>, BTreeMap<PathBuf, PathBuf>)> {
    let mut catalogs = BTreeMap::new();
    let mut package_catalog_roots = BTreeMap::new();
    for package_root in package_roots {
        let owned_by_host = match host_catalog {
            Some(catalog) => catalog.unit_name_for_root(package_root)?.is_some(),
            None => false,
        };
        if package_root == host_root || owned_by_host {
            continue;
        }
        let catalog = UnitCatalog::discover(package_root)?.ok_or_else(|| {
            anyhow::anyhow!(
                "manifest catalog is required for package root {}",
                package_root.display()
            )
        })?;
        if host_catalog.is_some_and(|host| catalog.workspace_root() == host.workspace_root()) {
            bail!("no manifest unit owns {}", package_root.display());
        }
        if catalog.unit_name_for_root(package_root)?.is_none() {
            bail!("no manifest unit owns {}", package_root.display());
        }
        let workspace_root = catalog.workspace_root().to_path_buf();
        package_catalog_roots.insert(package_root.clone(), workspace_root.clone());
        catalogs.entry(workspace_root).or_insert(catalog);
    }
    Ok((catalogs, package_catalog_roots))
}

fn validate_external_package_declarations(
    host_root: &Path,
    host_catalog: &UnitCatalog,
    package_roots: &[PathBuf],
) -> Result<()> {
    if host_catalog.workspace().external_sources().is_empty() {
        return Ok(());
    }
    let declared_packages = external_package_declarations(host_catalog)?;
    let mut missing = Vec::new();
    for package_root in package_roots {
        if package_root == host_root || host_catalog.unit_name_for_root(package_root)?.is_some() {
            continue;
        }
        let catalog = UnitCatalog::discover(package_root)?.ok_or_else(|| {
            anyhow::anyhow!(
                "manifest catalog is required for package root {}",
                package_root.display()
            )
        })?;
        if catalog.unit_for_root(package_root)?.is_none() {
            continue;
        };
        let package = package_root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                anyhow::anyhow!("package root has no basename: {}", package_root.display())
            })?
            .to_string();
        validate_name_segment("package root basename", &package)?;
        if !declared_packages.contains_key(&package) {
            missing.push(package);
        }
    }
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        bail!(
            "target fkst.workspace.toml does not declare platform packages in [[external_sources]].packages: {}; add them to the external source block that owns these package roots",
            missing.join(", ")
        );
    }
    Ok(())
}

fn external_package_declarations(host_catalog: &UnitCatalog) -> Result<BTreeMap<String, String>> {
    let mut declared_packages = BTreeMap::new();
    for source in host_catalog.workspace().external_sources() {
        validate_name_segment("external source id", &source.id)?;
        let mut source_seen = BTreeSet::new();
        for package in &source.packages {
            validate_name_segment("external source package", package)?;
            if !source_seen.insert(package.clone()) {
                bail!(
                    "external source `{}` declares duplicate platform package `{package}`",
                    source.id
                );
            }
            if let Some(previous_source) =
                declared_packages.insert(package.clone(), source.id.clone())
            {
                bail!(
                    "ambiguous target fkst.workspace.toml platform package `{package}` declared by external sources `{previous_source}` and `{}`",
                    source.id
                );
            }
        }
    }
    Ok(declared_packages)
}

fn validate_locked_local_package_sources(
    host_catalog: &UnitCatalog,
    local_source_roots: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    if local_source_roots.is_empty() {
        return Ok(());
    }
    let lockfile_path = host_catalog.workspace_root().join("fkst.lock");
    if !lockfile_path.exists() {
        bail!(
            "fkst.lock is required for explicit external --package-root sources; run `fkst-framework host lock --project-root <root> --package-root <root> ...`"
        );
    }
    let lockfile = Lockfile::parse_file(&lockfile_path)?;
    validate_locked_local_sources(
        host_catalog.workspace().external_sources(),
        &lockfile,
        local_source_roots,
    )
}

fn collect_conformance_library_roots<'a>(
    units: impl Iterator<Item = &'a crate::manifest::CatalogUnit>,
    roots: &mut BTreeMap<(String, PathBuf), LibraryConformanceRoot>,
) {
    for unit in units {
        if !unit.is_library() {
            continue;
        }
        let namespace = unit.library_name().to_string();
        let root = unit.unit_root().to_path_buf();
        roots.insert(
            (namespace.clone(), root.clone()),
            LibraryConformanceRoot { root, namespace },
        );
    }
}

fn is_under(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NameResolutionMode {
    LegacyFlat,
    Namespaced,
}

#[derive(Clone, Debug)]
pub(crate) struct NameResolver {
    mode: NameResolutionMode,
    namespaces: BTreeSet<String>,
    recorded_only_queues: BTreeSet<String>,
}

impl NameResolver {
    pub(crate) fn new(namespaces: impl IntoIterator<Item = String>) -> Self {
        let namespaces = namespaces.into_iter().collect::<BTreeSet<_>>();
        let mode = if namespaces.len() == 1 {
            NameResolutionMode::LegacyFlat
        } else {
            NameResolutionMode::Namespaced
        };
        Self {
            mode,
            namespaces,
            recorded_only_queues: BTreeSet::new(),
        }
    }

    pub(crate) fn with_recorded_only_queues(mut self, queues: BTreeSet<String>) -> Self {
        self.recorded_only_queues = queues;
        self
    }

    pub(crate) fn add_recorded_only_queue(mut self, queue: impl Into<String>) -> Self {
        self.recorded_only_queues.insert(queue.into());
        self
    }

    #[cfg(test)]
    pub(crate) fn mode(&self) -> &NameResolutionMode {
        &self.mode
    }

    pub(crate) fn validate_owner(&self, owner_namespace: &str) -> Result<()> {
        // The owner namespace is only used as a qualifier in Namespaced mode; in
        // LegacyFlat it is an inert label (the folded root basename), so it is not
        // charset-constrained.
        if self.mode == NameResolutionMode::Namespaced {
            validate_name_segment("owner namespace", owner_namespace)?;
        }
        if !self.namespaces.contains(owner_namespace) {
            bail!("unknown owner namespace `{owner_namespace}`");
        }
        Ok(())
    }

    pub(crate) fn resolve(&self, owner_namespace: &str, raw: &str) -> Result<String> {
        self.validate_owner(owner_namespace)?;
        let parsed = parse_name(raw)?;
        match (&self.mode, parsed) {
            (NameResolutionMode::LegacyFlat, ParsedName::Bare(name)) => Ok(name),
            (NameResolutionMode::LegacyFlat, ParsedName::Qualified { namespace, name }) => {
                let qualified = format!("{namespace}.{name}");
                if self.recorded_only_queues.contains(&qualified) {
                    return Ok(qualified);
                }
                if namespace != owner_namespace {
                    bail!(
                        "qualified name `{raw}` uses namespace `{namespace}` but legacy owner namespace is `{owner_namespace}`"
                    );
                }
                Ok(name)
            }
            (NameResolutionMode::Namespaced, ParsedName::Bare(name)) => {
                Ok(format!("{owner_namespace}.{name}"))
            }
            (NameResolutionMode::Namespaced, ParsedName::Qualified { namespace, name }) => {
                let qualified = format!("{namespace}.{name}");
                if !self.namespaces.contains(&namespace) {
                    if self.recorded_only_queues.contains(&qualified) {
                        return Ok(qualified);
                    }
                    bail!("qualified name `{raw}` uses unknown namespace `{namespace}`");
                }
                Ok(qualified)
            }
        }
    }

    pub(crate) fn canonical_decl_name(&self, owner_namespace: &str, raw: &str) -> Result<String> {
        if self.mode == NameResolutionMode::Namespaced {
            validate_name_segment("owner namespace", owner_namespace)?;
        }
        validate_name_segment("declaration name", raw)?;
        match self.mode {
            NameResolutionMode::LegacyFlat => Ok(raw.to_string()),
            NameResolutionMode::Namespaced => Ok(format!("{owner_namespace}.{raw}")),
        }
    }
}

enum ParsedName {
    Bare(String),
    Qualified { namespace: String, name: String },
}

fn parse_name(raw: &str) -> Result<ParsedName> {
    let parts = raw.split('.').collect::<Vec<_>>();
    match parts.as_slice() {
        [name] => {
            validate_name_segment("name", name)?;
            Ok(ParsedName::Bare((*name).to_string()))
        }
        [namespace, name] => {
            validate_name_segment("namespace", namespace)?;
            validate_name_segment("name", name)?;
            Ok(ParsedName::Qualified {
                namespace: (*namespace).to_string(),
                name: (*name).to_string(),
            })
        }
        _ => bail!("name `{raw}` must be bare `name` or qualified `namespace.name`"),
    }
}

pub(crate) fn validate_name_segment(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} must not be empty");
    }
    if !value
        .bytes()
        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
    {
        bail!("{label} `{value}` must match [A-Za-z0-9_-]+");
    }
    Ok(())
}

fn canonical_dir(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {label} {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{label} is not a directory: {}", canonical.display());
    }
    std::fs::read_dir(&canonical)
        .with_context(|| format!("read {label} {}", canonical.display()))?;
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(package_roots: &[&str], host_root: &str) -> PackageRoots {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join("fkst.workspace.toml"),
            r#"
[workspace]
units = ["."]
"#,
        );
        write(
            &temp.path().join("fkst.toml"),
            r#"
kind = "package"
name = "dummy"

[code]
root = "."
"#,
        );
        let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();
        PackageRoots {
            package_roots: package_roots.iter().map(PathBuf::from).collect(),
            host_root: PathBuf::from(host_root),
            package_namespaces: package_roots
                .iter()
                .map(|root| {
                    Path::new(root)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect(),
            run_host_owner: false,
            catalog: Some(catalog),
            external_catalogs: BTreeMap::new(),
            external_package_catalog_roots: BTreeMap::new(),
            catalog_snapshot: None,
        }
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_git_repo(root: &Path) -> String {
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("init")
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&init.stdout),
            String::from_utf8_lossy(&init.stderr)
        );
        git(root, &["config", "user.email", "fkst-test@example.invalid"]);
        git(root, &["config", "user.name", "fkst test"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "Add platform packages"]);
        git(root, &["rev-parse", "HEAD"])
    }

    fn write_lock_for_local_platform(host: &Path, external: &Path, resolved_rev: &str) {
        let tree_hash =
            crate::manifest_external::tree_sha256(external).expect("hash local platform tree");
        write(
            &host.join("fkst.lock"),
            &format!(
                r#"
[[external_source]]
id = "fkst-platform"
git = "/tmp/fkst-platform"

[external_source.intent]
rev = "0123456789abcdef0123456789abcdef01234567"

[external_source.resolved]
rev = "{resolved_rev}"
tree_sha256 = "sha256-{tree_hash}"
"#
            ),
        );
    }

    #[test]
    fn catalog_snapshot_reuses_indexes_and_loads_current_package_bytes() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_with_units(temp.path(), &["."]);
        write_package_unit_manifest(temp.path(), "package");
        let helper = temp.path().join("helper.lua");
        let entry = temp.path().join("entry.lua");
        write(&helper, "return { value = 'before' }\n");
        write(&entry, "return require('helper').value\n");
        let mut roots =
            PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();

        roots.prepare_catalog_snapshot().unwrap();
        let first = roots.catalog_snapshot().unwrap();
        let second = roots.catalog_snapshot().unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        write(&helper, "return { value = 'after' }\n");
        std::fs::remove_file(temp.path().join("fkst.workspace.toml")).unwrap();
        std::fs::remove_file(temp.path().join("fkst.toml")).unwrap();
        let restored = PackageRoots::resolve_run_snapshot(
            temp.path(),
            vec![temp.path().to_path_buf()],
            first.as_ref(),
        )
        .unwrap();
        let owner_root = temp.path().canonicalize().unwrap();
        let catalog = restored
            .catalog_for_owner_root(&owner_root)
            .unwrap()
            .clone();
        let owner_unit = catalog.unit_name_for_root(&owner_root).unwrap().unwrap();
        let lua = crate::mlua_init::new_lua();

        let value = crate::lua_require::load_unit_chunk(
            &lua,
            Arc::new(catalog),
            &owner_unit,
            &entry,
            "@entry.lua",
            None,
        )
        .unwrap();

        let mlua::Value::String(value) = value else {
            panic!("entry chunk did not return a string");
        };
        assert_eq!(value.to_str().unwrap(), "after");
    }

    #[test]
    fn catalog_snapshot_rejects_different_run_roots() {
        let temp = tempfile::tempdir().unwrap();
        let other = temp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        write_workspace_with_units(temp.path(), &["."]);
        write_package_unit_manifest(temp.path(), "package");
        let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let snapshot = roots.catalog_snapshot().unwrap();

        let err = PackageRoots::resolve_run_snapshot(temp.path(), vec![other], snapshot.as_ref())
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("package catalog snapshot roots do not match run arguments"),
            "{err:#}"
        );
    }

    #[test]
    fn missing_workspace_manifest_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let err = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap_err();

        assert!(
            err.to_string().contains("manifest catalog is required"),
            "{err:#}"
        );
    }

    #[test]
    fn manifestless_host_accepts_explicit_external_package_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("host");
        let package = temp.path().join("external/package");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        write_workspace_with_units(&package, &["."]);
        write_package_unit_manifest(&package, "package");

        let roots = PackageRoots::resolve(&host, vec![package.clone()]).unwrap();

        assert!(roots.unit_catalog().is_none());
        assert_eq!(
            roots.unit_name_for_owner_root(&package).unwrap().as_deref(),
            Some("package")
        );

        let snapshot = roots.catalog_snapshot().unwrap();
        let restored =
            PackageRoots::resolve_run_snapshot(&host, vec![package.clone()], snapshot.as_ref())
                .unwrap();
        assert_eq!(
            restored
                .unit_name_for_owner_root(&package)
                .unwrap()
                .as_deref(),
            Some("package")
        );
    }

    #[test]
    fn manifestless_host_rejects_unowned_external_package_root() {
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("host");
        let package = temp.path().join("external/package");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::create_dir_all(&package).unwrap();

        let err = PackageRoots::resolve(&host, vec![package.clone()]).unwrap_err();
        let msg = format!("{err:#}");

        assert!(
            msg.contains("manifest catalog is required for package root"),
            "{msg}"
        );
        assert!(msg.contains("external/package"), "{msg}");
    }

    #[test]
    fn manifestless_host_rejects_explicit_internal_package_root() {
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("host");
        let package = host.join("packages/package");
        std::fs::create_dir_all(&package).unwrap();
        write_workspace_with_units(&package, &["."]);
        write_package_unit_manifest(&package, "package");

        let err = PackageRoots::resolve(&host, vec![package]).unwrap_err();

        assert!(
            err.to_string()
                .contains("manifest catalog is required: missing fkst.workspace.toml"),
            "{err:#}"
        );
    }

    #[test]
    fn workspace_manifest_yields_catalog() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join("fkst.workspace.toml"),
            r#"
[workspace]
units = ["packages/*", "libraries/*"]
"#,
        );
        write(
            &temp.path().join("packages/app/fkst.toml"),
            r#"
kind = "package"
name = "app"

[code]
root = "."

[lib_deps]
libraries = ["std"]
"#,
        );
        write(&temp.path().join("packages/app/main.lua"), "");
        write(
            &temp.path().join("libraries/std/fkst.toml"),
            r#"
kind = "library"
name = "std"

[code]
root = "."
"#,
        );
        write(&temp.path().join("libraries/std/public/fkst/json.lua"), "");

        let app_root = temp.path().join("packages/app");
        let r = PackageRoots::resolve(temp.path(), vec![app_root.clone()]).unwrap();

        let scope = r
            .unit_catalog()
            .unwrap()
            .require_scope_for_root(&app_root)
            .unwrap();
        assert_eq!(scope.owner_unit(), "app");
        assert!(scope.resolve("main").is_some());
        assert!(scope.resolve("std.fkst.json").is_some());
        assert!(scope.resolve("fkst.json").is_none());
    }

    #[test]
    fn folded_single_package_with_dotted_basename_stays_legacy_flat() {
        let temp = tempfile::Builder::new()
            .prefix("repo.with.dot")
            .tempdir()
            .unwrap();
        write(
            &temp.path().join("fkst.workspace.toml"),
            r#"
[workspace]
units = ["."]
"#,
        );
        write(
            &temp.path().join("fkst.toml"),
            r#"
kind = "package"
name = "app"

[code]
root = "."
"#,
        );

        let r = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
        let resolver = r.name_resolver();

        assert_eq!(resolver.mode(), &NameResolutionMode::LegacyFlat);
        assert_eq!(
            resolver
                .resolve(&r.graph_roots()[0].namespace, "tick")
                .unwrap(),
            "tick"
        );
    }

    #[test]
    fn resolver_legacy_flat_keeps_bare_names_and_same_package_alias() {
        let resolver = NameResolver::new(["pkg".to_string()]);

        assert_eq!(resolver.mode(), &NameResolutionMode::LegacyFlat);
        assert_eq!(resolver.resolve("pkg", "q").unwrap(), "q");
        assert_eq!(resolver.resolve("pkg", "pkg.q").unwrap(), "q");
        assert!(resolver.resolve("pkg", "other.q").is_err());
    }

    #[test]
    fn resolver_namespaced_qualifies_bare_names_and_accepts_known_qualified_names() {
        let resolver = NameResolver::new(["pkg".to_string(), HOST_NAMESPACE.to_string()]);

        assert_eq!(resolver.mode(), &NameResolutionMode::Namespaced);
        assert_eq!(resolver.resolve("pkg", "q").unwrap(), "pkg.q");
        assert_eq!(resolver.resolve(HOST_NAMESPACE, "q").unwrap(), "host.q");
        assert_eq!(resolver.resolve("pkg", "host.q").unwrap(), "host.q");
        assert!(resolver.resolve("pkg", "missing.q").is_err());
    }

    #[test]
    fn resolver_namespaced_allows_exact_recorded_only_qualified_raise() {
        let resolver = NameResolver::new(["autochrono".to_string(), HOST_NAMESPACE.to_string()])
            .with_recorded_only_queues(BTreeSet::from(["consensus.proposal".to_string()]));

        assert_eq!(resolver.mode(), &NameResolutionMode::Namespaced);
        assert_eq!(
            resolver
                .resolve("autochrono", "consensus.proposal")
                .unwrap(),
            "consensus.proposal"
        );
        assert!(resolver.resolve("autochrono", "consensus.typo").is_err());
        assert!(resolver.validate_owner("consensus").is_err());
        assert_eq!(
            resolver.resolve("autochrono", "proposal").unwrap(),
            "autochrono.proposal"
        );

        let strict_resolver =
            NameResolver::new(["autochrono".to_string(), HOST_NAMESPACE.to_string()]);
        assert!(strict_resolver
            .resolve("autochrono", "consensus.proposal")
            .is_err());
    }

    #[test]
    fn resolver_legacy_flat_allows_exact_recorded_only_qualified_raise() {
        let resolver = NameResolver::new(["autochrono".to_string()])
            .with_recorded_only_queues(BTreeSet::from(["consensus.proposal".to_string()]));

        assert_eq!(resolver.mode(), &NameResolutionMode::LegacyFlat);
        assert_eq!(
            resolver
                .resolve("autochrono", "consensus.proposal")
                .unwrap(),
            "consensus.proposal"
        );
        assert!(resolver.resolve("autochrono", "consensus.typo").is_err());

        let strict_resolver = NameResolver::new(["autochrono".to_string()]);
        assert!(strict_resolver
            .resolve("autochrono", "consensus.proposal")
            .is_err());
    }

    #[test]
    fn resolver_rejects_dot_inside_segments() {
        let resolver = NameResolver::new(["pkg".to_string(), HOST_NAMESPACE.to_string()]);

        assert!(resolver.resolve("pkg", "bad.name.extra").is_err());
        assert!(resolver.resolve("pkg", "bad name").is_err());
        assert!(resolver.resolve("pkg", ".q").is_err());
    }

    #[test]
    fn host_workspace_unit_pattern_must_not_escape_project_root() {
        let root = tempfile::Builder::new()
            .prefix("host-claim-external")
            .tempdir()
            .unwrap();
        let host = root.path().join("host");
        let external = root.path().join("external/pkg");
        std::fs::create_dir_all(&external).unwrap();
        write(
            &host.join("fkst.workspace.toml"),
            r#"
[workspace]
units = [".", "../external/pkg"]
"#,
        );
        write(
            &host.join("fkst.toml"),
            r#"
kind = "package"
name = "host"

[code]
root = "."
"#,
        );
        write(
            &external.join("fkst.toml"),
            r#"
kind = "package"
name = "external-pkg"

[code]
root = "."
"#,
        );

        let err = PackageRoots::resolve(&host, vec![external.clone()]).unwrap_err();
        let msg = format!("{err:#}");

        assert!(msg.contains("must not contain `..`"), "{msg}");
    }

    #[test]
    fn explicit_external_package_root_must_be_declared_by_host_external_source() {
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("host");
        let external = temp.path().join("platform");
        let platform_a = external.join("packages/platform-a");
        let platform_b = external.join("packages/platform-b");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::create_dir_all(&platform_a).unwrap();
        std::fs::create_dir_all(&platform_b).unwrap();
        write(
            &host.join("fkst.workspace.toml"),
            r#"
[workspace]
units = []

[[external_sources]]
id = "fkst-platform"
git = "/tmp/fkst-platform"
rev = "0123456789abcdef0123456789abcdef01234567"
packages = ["platform-a"]
"#,
        );
        write_workspace_with_units(&external, &["packages/platform-a", "packages/platform-b"]);
        write_package_unit_manifest(&platform_a, "platform-a");
        write_package_unit_manifest(&platform_b, "platform-b");

        let err = PackageRoots::resolve(&host, vec![platform_a, platform_b]).unwrap_err();
        let msg = format!("{err:#}");

        assert!(
            msg.contains("target fkst.workspace.toml does not declare platform packages"),
            "{msg}"
        );
        assert!(msg.contains("platform-b"), "{msg}");
        assert!(!msg.contains("platform-a,"), "{msg}");
    }

    #[test]
    fn declared_external_package_roots_use_their_own_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("host");
        let external = temp.path().join("platform");
        let platform_a = external.join("packages/platform-a");
        let platform_b = external.join("packages/platform-b");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::create_dir_all(&platform_a).unwrap();
        std::fs::create_dir_all(&platform_b).unwrap();
        write(
            &host.join("fkst.workspace.toml"),
            r#"
[workspace]
units = []

[[external_sources]]
id = "fkst-platform"
git = "/tmp/fkst-platform"
rev = "0123456789abcdef0123456789abcdef01234567"
packages = ["platform-a", "platform-b"]
"#,
        );
        write_workspace_with_units(&external, &["packages/platform-a", "packages/platform-b"]);
        write_package_unit_manifest(&platform_a, "platform-a");
        write_package_unit_manifest(&platform_b, "platform-b");
        let rev = init_git_repo(&external);
        write_lock_for_local_platform(&host, &external, &rev);

        let roots = PackageRoots::resolve(&host, vec![platform_a.clone(), platform_b]).unwrap();

        assert!(roots
            .catalog_for_owner_root(&platform_a)
            .unwrap()
            .contains_unit("platform-a"));
    }

    #[test]
    fn external_package_declaration_uses_root_basename_not_manifest_name() {
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("host");
        let external = temp.path().join("platform");
        let package_root = external.join("packages/platform-a");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::create_dir_all(&package_root).unwrap();
        write(
            &host.join("fkst.workspace.toml"),
            r#"
[workspace]
units = []

[[external_sources]]
id = "fkst-platform"
git = "/tmp/fkst-platform"
rev = "0123456789abcdef0123456789abcdef01234567"
packages = ["platform-a"]
"#,
        );
        write_workspace_with_units(&external, &["packages/platform-a"]);
        write_package_unit_manifest(&package_root, "logical-a");
        let rev = init_git_repo(&external);
        write_lock_for_local_platform(&host, &external, &rev);

        let roots = PackageRoots::resolve(&host, vec![package_root.clone()]).unwrap();

        assert!(roots
            .catalog_for_owner_root(&package_root)
            .unwrap()
            .contains_unit("logical-a"));
    }

    #[test]
    fn external_platform_package_declaration_is_aggregated() {
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("host");
        let external = temp.path().join("platform");
        let platform_a = external.join("packages/platform-a");
        let platform_b = external.join("packages/platform-b");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::create_dir_all(&platform_a).unwrap();
        std::fs::create_dir_all(&platform_b).unwrap();
        write(
            &host.join("fkst.workspace.toml"),
            r#"
[workspace]
units = []

[[external_sources]]
id = "fkst-platform"
git = "/tmp/fkst-platform"
rev = "0123456789abcdef0123456789abcdef01234567"
packages = ["declared"]
"#,
        );
        write_workspace_with_units(&external, &["packages/platform-a", "packages/platform-b"]);
        write_package_unit_manifest(&platform_a, "platform-a");
        write_package_unit_manifest(&platform_b, "platform-b");

        let err = PackageRoots::resolve(&host, vec![platform_b, platform_a]).unwrap_err();
        let msg = format!("{err:#}");

        assert!(msg.contains("platform-a, platform-b"), "{msg}");
    }

    fn write_workspace_with_units(root: &Path, units: &[&str]) {
        let units = units
            .iter()
            .map(|unit| format!(r#""{unit}""#))
            .collect::<Vec<_>>()
            .join(", ");
        write(
            &root.join("fkst.workspace.toml"),
            &format!(
                r#"
[workspace]
units = [{units}]
"#
            ),
        );
    }

    fn write_package_unit_manifest(root: &Path, name: &str) {
        write(
            &root.join("fkst.toml"),
            &format!(
                r#"
kind = "package"
name = "{name}"

[code]
root = "."
"#
            ),
        );
    }
}

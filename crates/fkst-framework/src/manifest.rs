//! Manifest parsing and library dependency catalog construction.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value;

#[path = "manifest_exports.rs"]
pub(crate) mod manifest_exports;
#[path = "manifest_modules.rs"]
mod manifest_modules;
#[path = "manifest_workspace.rs"]
mod manifest_workspace;

use crate::manifest_external::{fetch_locked_sources, ExternalSourceCheckout, Lockfile};
pub(crate) use manifest_exports::Exports;
use manifest_modules::{
    canonical_unit_code_root, insert_module_entry, scan_own_modules, unit_manifest_path,
};
pub(crate) use manifest_workspace::{ExternalSourceDecl, WorkspaceManifest};

const WORKSPACE_MANIFEST: &str = "fkst.workspace.toml";
pub(crate) const UNIT_MANIFEST: &str = "fkst.toml";
const LOCKFILE: &str = "fkst.lock";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UnitKind {
    Package(PackageKind),
    Library,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PackageKind {
    Flat,
    Composed,
}

impl<'de> Deserialize<'de> for UnitKind {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "package" | "package.flat" | "package_flat" | "flat-package" => {
                Ok(Self::Package(PackageKind::Flat))
            }
            "package.composed" | "package_composed" | "composed-package" => {
                Ok(Self::Package(PackageKind::Composed))
            }
            "library" => Ok(Self::Library),
            _ => Err(serde::de::Error::custom(format!(
                "unknown unit kind `{raw}`"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct LibDep {
    name: String,
}

impl LibDep {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.name
    }
}

impl<'de> Deserialize<'de> for LibDep {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::new(String::deserialize(deserializer)?))
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct EventDep {
    name: String,
}

impl EventDep {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.name
    }
}

impl<'de> Deserialize<'de> for EventDep {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::new(String::deserialize(deserializer)?))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Visibility {
    Public,
    Allow(Vec<String>),
}

impl Default for Visibility {
    fn default() -> Self {
        Self::Public
    }
}

impl<'de> Deserialize<'de> for Visibility {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = VisibilityToml::deserialize(deserializer)?;
        Ok(match raw.allow {
            Some(allow) => Self::Allow(allow),
            None => Self::Public,
        })
    }
}

#[derive(Deserialize)]
struct VisibilityToml {
    allow: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct LibraryMeta {
    pub(crate) name: String,
    pub(crate) stable_id: String,
    pub(crate) version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnitManifest {
    pub(crate) kind: UnitKind,
    pub(crate) name: String,
    pub(crate) code_root: PathBuf,
    pub(crate) lib_deps: Vec<LibDep>,
    pub(crate) event_deps: Vec<EventDep>,
    pub(crate) library: Option<LibraryMeta>,
    pub(crate) visibility: Visibility,
    pub(crate) exports: Exports,
    pub(crate) conformance: Option<ConformanceManifest>,
}

impl UnitManifest {
    pub(crate) fn parse_file(path: &Path) -> Result<Self> {
        Self::parse_file_inner(path, false)
    }

    pub(crate) fn parse_file_strict(path: &Path) -> Result<Self> {
        Self::parse_file_inner(path, true)
    }

    fn parse_file_inner(path: &Path, strict_conformance: bool) -> Result<Self> {
        let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str::<UnitManifestToml>(&raw)
            .and_then(|manifest| manifest.into_manifest(strict_conformance))
            .with_context(|| format!("parse {}", path.display()))
    }
}

#[derive(Deserialize)]
struct UnitManifestToml {
    kind: UnitKind,
    name: String,
    code: CodeToml,
    #[serde(default)]
    lib_deps: LibDepsToml,
    #[serde(default)]
    event_deps: EventDepsToml,
    library: Option<LibraryMeta>,
    #[serde(default)]
    visibility: Visibility,
    #[serde(default)]
    exports: Exports,
    conformance: Option<Value>,
}

impl UnitManifestToml {
    fn into_manifest(
        self,
        strict_conformance: bool,
    ) -> std::result::Result<UnitManifest, toml::de::Error> {
        let conformance = match self.conformance {
            Some(raw) if strict_conformance => {
                Some(raw.try_into::<ConformanceToml>()?.into_manifest())
            }
            Some(raw) => Some(ConformanceManifest::from_value(raw)),
            None => None,
        };
        Ok(UnitManifest {
            kind: self.kind,
            name: self.name,
            code_root: self.code.root,
            lib_deps: self.lib_deps.libraries,
            event_deps: self.event_deps.packages,
            library: self.library,
            visibility: self.visibility,
            exports: self.exports,
            conformance,
        })
    }
}

#[derive(Deserialize)]
struct CodeToml {
    root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConformanceManifest {
    pub(crate) pack: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConformanceToml {
    pack: PathBuf,
}

impl ConformanceToml {
    fn into_manifest(self) -> ConformanceManifest {
        ConformanceManifest { pack: self.pack }
    }
}

impl ConformanceManifest {
    fn from_value(value: Value) -> Self {
        let pack = value
            .get("pack")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_default();
        Self { pack }
    }
}

#[derive(Default, Deserialize)]
struct LibDepsToml {
    #[serde(default)]
    libraries: Vec<LibDep>,
}

#[derive(Default, Deserialize)]
struct EventDepsToml {
    #[serde(default)]
    packages: Vec<EventDep>,
}

#[derive(Clone, Debug)]
pub(crate) struct UnitCatalog {
    workspace_root: PathBuf,
    workspace: WorkspaceManifest,
    lockfile: Lockfile,
    units: BTreeMap<String, CatalogUnit>,
    library_units: BTreeMap<String, String>,
    denied_external_libraries: BTreeMap<String, String>,
    graph: UnitGraph,
}

impl UnitCatalog {
    pub(crate) fn discover(start: &Path) -> Result<Option<Self>> {
        let Some(workspace_manifest_path) = find_workspace_manifest(start)? else {
            return Ok(None);
        };
        let workspace_root = workspace_manifest_path
            .parent()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "workspace manifest has no parent: {}",
                    workspace_manifest_path.display()
                )
            })?
            .to_path_buf();
        let workspace = WorkspaceManifest::parse_file(&workspace_manifest_path)?;
        Self::from_workspace(workspace_root, workspace).map(Some)
    }

    pub(crate) fn discover_for_validation(start: &Path) -> Result<Option<Self>> {
        let Some(workspace_manifest_path) = find_workspace_manifest(start)? else {
            return Ok(None);
        };
        let workspace_root = workspace_manifest_path
            .parent()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "workspace manifest has no parent: {}",
                    workspace_manifest_path.display()
                )
            })?
            .to_path_buf();
        let workspace = WorkspaceManifest::parse_file(&workspace_manifest_path)?;
        Self::from_workspace_for_validation(workspace_root, workspace).map(Some)
    }

    pub(crate) fn discover_with_lock(start: &Path, lockfile: Lockfile) -> Result<Option<Self>> {
        let Some(workspace_manifest_path) = find_workspace_manifest(start)? else {
            return Ok(None);
        };
        let workspace_root = workspace_manifest_path
            .parent()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "workspace manifest has no parent: {}",
                    workspace_manifest_path.display()
                )
            })?
            .to_path_buf();
        let workspace = WorkspaceManifest::parse_file(&workspace_manifest_path)?;
        Self::from_workspace_inner(workspace_root, workspace, true, Some(lockfile)).map(Some)
    }

    fn from_workspace(workspace_root: PathBuf, workspace: WorkspaceManifest) -> Result<Self> {
        Self::from_workspace_inner(workspace_root, workspace, true, None)
    }

    fn from_workspace_for_validation(
        workspace_root: PathBuf,
        workspace: WorkspaceManifest,
    ) -> Result<Self> {
        Self::from_workspace_inner(workspace_root, workspace, false, None)
    }

    fn from_workspace_inner(
        workspace_root: PathBuf,
        workspace: WorkspaceManifest,
        fail_closed: bool,
        lockfile_override: Option<Lockfile>,
    ) -> Result<Self> {
        let lockfile_path = workspace_root.join(LOCKFILE);
        let lockfile = match lockfile_override {
            Some(lockfile) => lockfile,
            None if lockfile_path.exists() => Lockfile::parse_file(&lockfile_path)?,
            None => Lockfile::default(),
        };
        let unit_roots = discover_unit_roots(&workspace_root, workspace.discovered_units())?;
        let mut units = BTreeMap::new();
        let mut library_units = BTreeMap::new();

        for unit_root in unit_roots {
            ensure_catalog_path_under_workspace(&workspace_root, &unit_root, "unit root")?;
            let manifest_path = unit_manifest_path(&unit_root);
            let manifest = UnitManifest::parse_file(&manifest_path)?;
            validate_manifest_name(&manifest.name)
                .with_context(|| format!("validate unit `{}`", manifest.name))?;
            if units.contains_key(&manifest.name) {
                bail!("duplicate unit name `{}`", manifest.name);
            }
            let code_root = canonical_unit_code_root(&unit_root, &manifest)?;
            ensure_catalog_path_under_workspace(&workspace_root, &code_root, "code root")?;
            let unit = CatalogUnit::new(unit_root, code_root, manifest)?;
            if unit.is_library() {
                let library_name = unit.library_name().to_string();
                validate_manifest_name(&library_name)
                    .with_context(|| format!("validate library `{library_name}`"))?;
                if let Some(existing_unit) =
                    library_units.insert(library_name.clone(), unit.catalog_name().to_string())
                {
                    bail!(
                        "duplicate library name `{library_name}` in units `{existing_unit}` and `{}`",
                        unit.catalog_name()
                    );
                }
            }
            units.insert(unit.catalog_name().to_string(), unit);
        }

        let external_checkouts = if workspace.external_sources().is_empty() {
            Vec::new()
        } else {
            fetch_locked_sources(workspace.external_sources(), &lockfile)?
        };
        add_external_units(
            &workspace_root,
            &external_checkouts,
            &mut units,
            &mut library_units,
        )?;
        let denied_external_libraries =
            denied_external_libraries(&external_checkouts, &library_units);

        let graph = UnitGraph::from_units(&units);
        let mut catalog = Self {
            workspace_root,
            workspace,
            lockfile,
            units,
            library_units,
            denied_external_libraries,
            graph,
        };
        if fail_closed {
            catalog.build_indexes()?;
        } else {
            catalog.build_partial_indexes_for_validation()?;
        }
        Ok(catalog)
    }

    pub(crate) fn require_scope_for_unit(&self, unit_name: &str) -> Result<ManifestRequireScope> {
        let unit = self
            .units
            .get(unit_name)
            .ok_or_else(|| anyhow::anyhow!("unknown unit `{unit_name}`"))?;
        Ok(ManifestRequireScope {
            owner_unit: unit.catalog_name().to_string(),
            modules: unit.module_index.clone(),
        })
    }

    pub(crate) fn require_scope_for_root(&self, owner_root: &Path) -> Result<ManifestRequireScope> {
        let unit_name = self
            .unit_name_for_root(owner_root)?
            .ok_or_else(|| anyhow::anyhow!("no manifest unit owns {}", owner_root.display()))?;
        self.require_scope_for_unit(&unit_name)
    }

    pub(crate) fn unit_name_for_root(&self, owner_root: &Path) -> Result<Option<String>> {
        let canonical = owner_root
            .canonicalize()
            .with_context(|| format!("canonicalize {}", owner_root.display()))?;
        Ok(self.units.values().find_map(|unit| {
            if unit.unit_root == canonical || unit.code_root == canonical {
                Some(unit.catalog_name().to_string())
            } else {
                None
            }
        }))
    }

    pub(crate) fn module_index_for_unit(&self, unit_name: &str) -> Option<&ModuleIndex> {
        self.units.get(unit_name).map(|unit| &unit.module_index)
    }

    pub(crate) fn contains_unit(&self, unit_name: &str) -> bool {
        self.units.contains_key(unit_name)
    }

    pub(crate) fn graph(&self) -> &UnitGraph {
        &self.graph
    }

    pub(crate) fn workspace(&self) -> &WorkspaceManifest {
        &self.workspace
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn lockfile(&self) -> &Lockfile {
        &self.lockfile
    }

    pub(crate) fn units(&self) -> impl Iterator<Item = &CatalogUnit> {
        self.units.values()
    }

    pub(crate) fn library_unit_name(&self, library_name: &str) -> Option<&str> {
        self.library_units
            .get(library_name)
            .map(std::string::String::as_str)
    }

    pub(crate) fn build_partial_indexes_for_validation(&mut self) -> Result<()> {
        self.build_own_module_indexes()?;
        self.build_visible_module_indexes(false)
    }

    fn build_indexes(&mut self) -> Result<()> {
        self.build_own_module_indexes()?;
        self.build_visible_module_indexes(true)
    }

    fn build_own_module_indexes(&mut self) -> Result<()> {
        let unit_names = self.units.keys().cloned().collect::<Vec<_>>();
        for unit_name in &unit_names {
            let scan = scan_own_modules(self.units.get(unit_name).unwrap())?;
            let unit = self.units.get_mut(unit_name).unwrap();
            unit.own_modules = scan.own_modules;
            unit.public_modules = scan.public_modules;
            unit.private_modules = scan.private_modules;
        }
        Ok(())
    }

    fn build_visible_module_indexes(&mut self, fail_closed: bool) -> Result<()> {
        let unit_names = self.units.keys().cloned().collect::<Vec<_>>();
        for unit_name in unit_names {
            let mut module_index = BTreeMap::new();
            let own_modules = self.units[&unit_name].own_modules.clone();
            for (logical, path) in own_modules {
                insert_module_entry(
                    &mut module_index,
                    logical,
                    ModuleEntry::new(unit_name.clone(), path, ModuleVisibility::Owner),
                    &format!("unit `{unit_name}`"),
                )?;
            }

            let mut visible_library_modules: BTreeMap<String, String> = BTreeMap::new();
            for dep in self.units[&unit_name].manifest.lib_deps.clone() {
                let Some(library_unit_name) = self.library_units.get(dep.as_str()).cloned() else {
                    if fail_closed {
                        if let Some(source_id) = self.denied_external_libraries.get(dep.as_str()) {
                            bail!(
                                "external source `{source_id}` does not allow library `{}`",
                                dep.as_str()
                            );
                        }
                        anyhow::bail!(
                            "unit `{unit_name}` declares unknown library `{}`",
                            dep.as_str()
                        );
                    }
                    continue;
                };
                if let Err(err) = validate_library_visibility(
                    unit_name.as_str(),
                    dep.as_str(),
                    &self.units[&library_unit_name],
                ) {
                    if fail_closed {
                        return Err(err);
                    }
                    continue;
                }
                let public_modules = self.units[&library_unit_name].public_modules.clone();
                for (logical, path) in public_modules {
                    if let Some(previous_library) =
                        visible_library_modules.insert(logical.clone(), library_unit_name.clone())
                    {
                        if fail_closed {
                            bail!(
                                "ambiguous module `{logical}` visible to unit `{unit_name}` from libraries `{previous_library}` and `{library_unit_name}`"
                            );
                        }
                        continue;
                    }
                    insert_module_entry(
                        &mut module_index,
                        logical,
                        ModuleEntry::new(
                            library_unit_name.clone(),
                            path,
                            ModuleVisibility::PublicLibrary,
                        ),
                        &format!("unit `{unit_name}`"),
                    )?;
                }
            }

            self.units.get_mut(&unit_name).unwrap().module_index = module_index;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CatalogUnit {
    catalog_name: String,
    unit_root: PathBuf,
    code_root: PathBuf,
    manifest: UnitManifest,
    own_modules: ModulePaths,
    public_modules: ModulePaths,
    private_modules: ModulePaths,
    module_index: ModuleIndex,
}

impl CatalogUnit {
    fn new(unit_root: PathBuf, code_root: PathBuf, manifest: UnitManifest) -> Result<Self> {
        let catalog_name = manifest.name.clone();
        Ok(Self::new_with_catalog_name(
            catalog_name,
            unit_root,
            code_root,
            manifest,
        ))
    }

    fn new_with_catalog_name(
        catalog_name: String,
        unit_root: PathBuf,
        code_root: PathBuf,
        manifest: UnitManifest,
    ) -> Self {
        Self {
            catalog_name,
            unit_root,
            code_root,
            manifest,
            own_modules: BTreeMap::new(),
            public_modules: BTreeMap::new(),
            private_modules: BTreeMap::new(),
            module_index: BTreeMap::new(),
        }
    }

    pub(crate) fn is_library(&self) -> bool {
        self.manifest.kind == UnitKind::Library
    }

    pub(crate) fn library_name(&self) -> &str {
        self.manifest
            .library
            .as_ref()
            .map(|library| library.name.as_str())
            .unwrap_or(&self.manifest.name)
    }

    pub(crate) fn name(&self) -> &str {
        &self.manifest.name
    }

    pub(crate) fn catalog_name(&self) -> &str {
        &self.catalog_name
    }

    pub(crate) fn unit_root(&self) -> &Path {
        &self.unit_root
    }

    pub(crate) fn code_root(&self) -> &Path {
        &self.code_root
    }

    pub(crate) fn manifest(&self) -> &UnitManifest {
        &self.manifest
    }

    pub(crate) fn public_modules(&self) -> &ModulePaths {
        &self.public_modules
    }

    pub(crate) fn own_modules(&self) -> &ModulePaths {
        &self.own_modules
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnitGraph {
    lib_deps: BTreeMap<String, Vec<String>>,
}

impl UnitGraph {
    fn from_units(units: &BTreeMap<String, CatalogUnit>) -> Self {
        Self {
            lib_deps: units
                .iter()
                .map(|(name, unit)| {
                    (
                        name.clone(),
                        unit.manifest
                            .lib_deps
                            .iter()
                            .map(|dep| dep.as_str().to_string())
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    pub(crate) fn lib_deps_for(&self, unit_name: &str) -> Option<&[String]> {
        self.lib_deps.get(unit_name).map(Vec::as_slice)
    }
}

pub(crate) type ModuleIndex = BTreeMap<String, ModuleEntry>;
pub(crate) type ModulePaths = BTreeMap<String, PathBuf>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModuleEntry {
    pub(crate) provider_source: Option<String>,
    pub(crate) provider_unit: String,
    pub(crate) path: PathBuf,
    pub(crate) visibility: ModuleVisibility,
}

impl ModuleEntry {
    fn new(provider_unit: String, path: PathBuf, visibility: ModuleVisibility) -> Self {
        let provider_source = provider_unit
            .strip_prefix("external:")
            .and_then(|rest| rest.split_once(':'))
            .map(|(source, _)| source.to_string());
        Self {
            provider_source,
            provider_unit,
            path,
            visibility,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ModuleVisibility {
    Owner,
    PublicLibrary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManifestRequireScope {
    owner_unit: String,
    modules: ModuleIndex,
}

impl ManifestRequireScope {
    pub(crate) fn owner_unit(&self) -> &str {
        &self.owner_unit
    }

    pub(crate) fn modules(&self) -> &ModuleIndex {
        &self.modules
    }

    pub(crate) fn resolve(&self, logical: &str) -> Option<&Path> {
        self.modules.get(logical).map(ModuleEntry::path)
    }
}

fn ensure_catalog_path_under_workspace(
    workspace_root: &Path,
    path: &Path,
    label: &str,
) -> Result<()> {
    if path != workspace_root && !path.starts_with(workspace_root) {
        bail!(
            "{label} {} must stay under workspace root {}",
            path.display(),
            workspace_root.display()
        );
    }
    Ok(())
}

fn add_external_units(
    consumer_workspace_root: &Path,
    checkouts: &[ExternalSourceCheckout],
    units: &mut BTreeMap<String, CatalogUnit>,
    library_units: &mut BTreeMap<String, String>,
) -> Result<()> {
    for checkout in checkouts {
        let workspace = WorkspaceManifest::parse_file(&checkout.root.join(WORKSPACE_MANIFEST))?;
        let unit_roots = discover_unit_roots(&checkout.root, workspace.discovered_units())?;
        let unit_roots = unit_roots
            .into_iter()
            .map(|unit_root| {
                let manifest = UnitManifest::parse_file(&unit_root.join(UNIT_MANIFEST))?;
                Ok((manifest.name.clone(), unit_root, manifest))
            })
            .collect::<Result<Vec<_>>>()?;
        for library in &checkout.libraries {
            let Some((_, unit_root, manifest)) =
                unit_roots.iter().find(|(_, unit_root, manifest)| {
                    manifest.kind == UnitKind::Library
                        && unit_root
                            .strip_prefix(&checkout.root)
                            .map(|relative| {
                                relative.to_string_lossy().replace('\\', "/") == library.unit
                            })
                            .unwrap_or(false)
                })
            else {
                bail!(
                    "external source `{}` locked library `{}` is missing unit `{}`",
                    checkout.source_id,
                    library.name,
                    library.unit
                );
            };
            let library_name = manifest
                .library
                .as_ref()
                .map(|meta| meta.name.as_str())
                .unwrap_or(&manifest.name);
            if library_name != library.name {
                bail!(
                    "external source `{}` locked library `{}` resolved to manifest library `{library_name}`",
                    checkout.source_id,
                    library.name
                );
            }
            let code_root = canonical_unit_code_root(unit_root, manifest)?;
            ensure_catalog_path_under_workspace(&checkout.root, unit_root, "external unit root")?;
            ensure_catalog_path_under_workspace(&checkout.root, &code_root, "external code root")?;
            let catalog_name = external_provider_unit_id(&checkout.source_id, &library.name);
            if units.contains_key(&catalog_name) {
                bail!("duplicate external library provider `{catalog_name}`");
            }
            if let Some(existing) = library_units.insert(library.name.clone(), catalog_name.clone())
            {
                bail!(
                    "duplicate library name `{}` from units `{existing}` and `{catalog_name}`",
                    library.name
                );
            }
            let unit = CatalogUnit::new_with_catalog_name(
                catalog_name.clone(),
                unit_root.clone(),
                code_root,
                manifest.clone(),
            );
            units.insert(catalog_name, unit);
        }
    }
    let _ = consumer_workspace_root;
    Ok(())
}

fn denied_external_libraries(
    checkouts: &[ExternalSourceCheckout],
    library_units: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut denied = BTreeMap::new();
    for checkout in checkouts {
        for library in &checkout.available_libraries {
            if !library_units.contains_key(library) {
                denied.insert(library.clone(), checkout.source_id.clone());
            }
        }
    }
    denied
}

fn external_provider_unit_id(source_id: &str, library_name: &str) -> String {
    format!("external:{source_id}:{library_name}")
}

fn validate_library_visibility(
    consumer_unit: &str,
    declared_library: &str,
    library_unit: &CatalogUnit,
) -> Result<()> {
    match &library_unit.manifest.visibility {
        Visibility::Public => Ok(()),
        Visibility::Allow(allow) if allow.iter().any(|unit| unit == consumer_unit) => Ok(()),
        Visibility::Allow(_) => {
            bail!("unit `{consumer_unit}` is not allowed to declare library `{declared_library}`")
        }
    }
}

pub(crate) fn discover_unit_roots(
    workspace_root: &Path,
    patterns: &[String],
) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeSet::new();
    for pattern in patterns {
        collect_glob(workspace_root, pattern, &mut roots)
            .with_context(|| format!("discover workspace units from `{pattern}`"))?;
    }
    Ok(roots.into_iter().collect())
}

fn collect_glob(workspace_root: &Path, pattern: &str, roots: &mut BTreeSet<PathBuf>) -> Result<()> {
    if pattern.is_empty() || pattern.starts_with('/') {
        bail!("workspace unit pattern `{pattern}` must be a relative path");
    }
    let segments = pattern.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| *segment == "..") {
        bail!("workspace unit pattern `{pattern}` must not contain `..`");
    }
    collect_glob_inner(workspace_root, &segments, roots)
}

fn collect_glob_inner(base: &Path, segments: &[&str], roots: &mut BTreeSet<PathBuf>) -> Result<()> {
    let Some((segment, rest)) = segments.split_first() else {
        let manifest = base.join(UNIT_MANIFEST);
        if !manifest.exists() {
            bail!(
                "workspace unit {} is missing {UNIT_MANIFEST}",
                base.display()
            );
        }
        roots.insert(
            base.canonicalize()
                .with_context(|| format!("canonicalize {}", base.display()))?,
        );
        return Ok(());
    };

    if *segment == "*" {
        let mut entries = fs::read_dir(base)
            .with_context(|| format!("read {}", base.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("read {}", base.display()))?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            collect_glob_inner(&path, rest, roots)?;
        }
        return Ok(());
    }
    if segment.contains('*') {
        bail!("workspace unit pattern segment `{segment}` only supports bare `*` globs");
    }

    let next = base.join(segment);
    let metadata =
        fs::symlink_metadata(&next).with_context(|| format!("stat {}", next.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "workspace unit path is not a real directory: {}",
            next.display()
        );
    }
    collect_glob_inner(&next, rest, roots)
}

fn find_workspace_manifest(start: &Path) -> Result<Option<PathBuf>> {
    let mut current = start
        .canonicalize()
        .with_context(|| format!("canonicalize {}", start.display()))?;
    if !current.is_dir() {
        current.pop();
    }
    loop {
        let candidate = current.join(WORKSPACE_MANIFEST);
        if candidate.exists() {
            return Ok(Some(candidate));
        }
        if !current.pop() {
            return Ok(None);
        }
    }
}

fn validate_manifest_name(value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("manifest name must not be empty");
    }
    if !value
        .bytes()
        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
    {
        bail!("manifest name `{value}` must match [A-Za-z0-9_-]+");
    }
    Ok(())
}

#[cfg(test)]
#[path = "manifest/tests.rs"]
mod tests;

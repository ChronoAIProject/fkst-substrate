//! Manifest parsing and library dependency catalog construction.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const WORKSPACE_MANIFEST: &str = "fkst.workspace.toml";
const UNIT_MANIFEST: &str = "fkst.toml";
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
}

impl UnitManifest {
    pub(crate) fn parse_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str::<UnitManifestToml>(&raw)
            .map(UnitManifestToml::into_manifest)
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
}

impl UnitManifestToml {
    fn into_manifest(self) -> UnitManifest {
        UnitManifest {
            kind: self.kind,
            name: self.name,
            code_root: self.code.root,
            lib_deps: self.lib_deps.libraries,
            event_deps: self.event_deps.packages,
            library: self.library,
            visibility: self.visibility,
        }
    }
}

#[derive(Deserialize)]
struct CodeToml {
    root: PathBuf,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceManifest {
    discovered_units: Vec<String>,
    registries: BTreeMap<String, String>,
}

impl WorkspaceManifest {
    pub(crate) fn parse_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str::<WorkspaceManifestToml>(&raw)
            .map(WorkspaceManifestToml::into_manifest)
            .with_context(|| format!("parse {}", path.display()))
    }

    pub(crate) fn discovered_units(&self) -> &[String] {
        &self.discovered_units
    }

    pub(crate) fn registries(&self) -> &BTreeMap<String, String> {
        &self.registries
    }
}

#[derive(Deserialize)]
struct WorkspaceManifestToml {
    workspace: WorkspaceToml,
    #[serde(default)]
    registries: BTreeMap<String, String>,
}

impl WorkspaceManifestToml {
    fn into_manifest(self) -> WorkspaceManifest {
        WorkspaceManifest {
            discovered_units: self.workspace.units,
            registries: self.registries,
        }
    }
}

#[derive(Deserialize)]
struct WorkspaceToml {
    #[serde(default)]
    units: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Lockfile {
    entries: Vec<LockEntry>,
}

impl Lockfile {
    pub(crate) fn parse_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str::<LockfileToml>(&raw)
            .map(LockfileToml::into_lockfile)
            .with_context(|| format!("parse {}", path.display()))
    }

    pub(crate) fn entries(&self) -> &[LockEntry] {
        &self.entries
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct LockEntry {
    pub(crate) name: String,
    pub(crate) source: String,
    pub(crate) version: Option<String>,
    pub(crate) checksum: Option<String>,
}

#[derive(Deserialize)]
struct LockfileToml {
    #[serde(default, rename = "package")]
    packages: Vec<LockEntry>,
    #[serde(default, rename = "library")]
    libraries: Vec<LockEntry>,
}

impl LockfileToml {
    fn into_lockfile(mut self) -> Lockfile {
        self.packages.append(&mut self.libraries);
        Lockfile {
            entries: self.packages,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UnitCatalog {
    workspace_root: PathBuf,
    workspace: WorkspaceManifest,
    lockfile: Lockfile,
    units: BTreeMap<String, CatalogUnit>,
    library_units: BTreeMap<String, String>,
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

    fn from_workspace(workspace_root: PathBuf, workspace: WorkspaceManifest) -> Result<Self> {
        let lockfile_path = workspace_root.join(LOCKFILE);
        let lockfile = if lockfile_path.exists() {
            Lockfile::parse_file(&lockfile_path)?
        } else {
            Lockfile::default()
        };
        let unit_roots = discover_unit_roots(&workspace_root, workspace.discovered_units())?;
        let mut units = BTreeMap::new();
        let mut library_units = BTreeMap::new();

        for unit_root in unit_roots {
            let manifest_path = unit_root.join(UNIT_MANIFEST);
            let manifest = UnitManifest::parse_file(&manifest_path)?;
            validate_manifest_name(&manifest.name)
                .with_context(|| format!("validate unit `{}`", manifest.name))?;
            if units.contains_key(&manifest.name) {
                bail!("duplicate unit name `{}`", manifest.name);
            }
            let code_root = canonical_unit_code_root(&unit_root, &manifest)?;
            let unit = CatalogUnit::new(unit_root, code_root, manifest)?;
            if unit.is_library() {
                let library_name = unit.library_name().to_string();
                validate_manifest_name(&library_name)
                    .with_context(|| format!("validate library `{library_name}`"))?;
                if let Some(existing_unit) =
                    library_units.insert(library_name.clone(), unit.name().to_string())
                {
                    bail!(
                        "duplicate library name `{library_name}` in units `{existing_unit}` and `{}`",
                        unit.name()
                    );
                }
            }
            units.insert(unit.name().to_string(), unit);
        }

        let graph = UnitGraph::from_units(&units);
        let mut catalog = Self {
            workspace_root,
            workspace,
            lockfile,
            units,
            library_units,
            graph,
        };
        catalog.build_indexes()?;
        Ok(catalog)
    }

    pub(crate) fn require_scope_for_unit(&self, unit_name: &str) -> Result<ManifestRequireScope> {
        let unit = self
            .units
            .get(unit_name)
            .ok_or_else(|| anyhow::anyhow!("unknown unit `{unit_name}`"))?;
        Ok(ManifestRequireScope {
            owner_unit: unit.name().to_string(),
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
                Some(unit.name().to_string())
            } else {
                None
            }
        }))
    }

    pub(crate) fn module_index_for_unit(&self, unit_name: &str) -> Option<&ModuleIndex> {
        self.units.get(unit_name).map(|unit| &unit.module_index)
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

    fn build_indexes(&mut self) -> Result<()> {
        let unit_names = self.units.keys().cloned().collect::<Vec<_>>();
        for unit_name in &unit_names {
            let scan = scan_own_modules(self.units.get(unit_name).unwrap())?;
            let unit = self.units.get_mut(unit_name).unwrap();
            unit.own_modules = scan.own_modules;
            unit.public_modules = scan.public_modules;
            unit.private_modules = scan.private_modules;
        }

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
                let library_unit_name = self
                    .library_units
                    .get(dep.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "unit `{unit_name}` declares unknown library `{}`",
                            dep.as_str()
                        )
                    })?
                    .clone();
                let public_modules = self.units[&library_unit_name].public_modules.clone();
                for (logical, path) in public_modules {
                    if let Some(previous_library) =
                        visible_library_modules.insert(logical.clone(), library_unit_name.clone())
                    {
                        bail!(
                            "ambiguous module `{logical}` visible to unit `{unit_name}` from libraries `{previous_library}` and `{library_unit_name}`"
                        );
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
struct CatalogUnit {
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
        Ok(Self {
            unit_root,
            code_root,
            manifest,
            own_modules: BTreeMap::new(),
            public_modules: BTreeMap::new(),
            private_modules: BTreeMap::new(),
            module_index: BTreeMap::new(),
        })
    }

    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn is_library(&self) -> bool {
        self.manifest.kind == UnitKind::Library
    }

    fn library_name(&self) -> &str {
        self.manifest
            .library
            .as_ref()
            .map(|library| library.name.as_str())
            .unwrap_or(&self.manifest.name)
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
type ModulePaths = BTreeMap<String, PathBuf>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModuleEntry {
    pub(crate) provider_unit: String,
    pub(crate) path: PathBuf,
    pub(crate) visibility: ModuleVisibility,
}

impl ModuleEntry {
    fn new(provider_unit: String, path: PathBuf, visibility: ModuleVisibility) -> Self {
        Self {
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

struct OwnModuleScan {
    own_modules: ModulePaths,
    public_modules: ModulePaths,
    private_modules: ModulePaths,
}

fn scan_own_modules(unit: &CatalogUnit) -> Result<OwnModuleScan> {
    if !unit.is_library() {
        let own_modules = scan_lua_modules(&unit.code_root)?;
        return Ok(OwnModuleScan {
            public_modules: BTreeMap::new(),
            private_modules: BTreeMap::new(),
            own_modules,
        });
    }

    let public_root = unit.code_root.join("public");
    let private_root = unit.code_root.join("private");
    let has_public = is_real_dir(&public_root)?;
    let has_private = is_real_dir(&private_root)?;
    let public_modules = if has_public {
        scan_lua_modules(&public_root)?
    } else if !has_private {
        scan_lua_modules(&unit.code_root)?
    } else {
        BTreeMap::new()
    };
    let private_modules = if has_private {
        scan_lua_modules(&private_root)?
    } else {
        BTreeMap::new()
    };
    for logical in public_modules.keys() {
        if private_modules.contains_key(logical) {
            bail!(
                "library `{}` exports duplicate public/private module `{logical}`",
                unit.name()
            );
        }
    }

    let mut own_modules = BTreeMap::new();
    for (logical, path) in public_modules.iter().chain(private_modules.iter()) {
        insert_path_entry(
            &mut own_modules,
            logical.clone(),
            path.clone(),
            &format!("library `{}`", unit.name()),
        )?;
    }

    Ok(OwnModuleScan {
        own_modules,
        public_modules,
        private_modules,
    })
}

fn scan_lua_modules(root: &Path) -> Result<ModulePaths> {
    let mut modules = BTreeMap::new();
    scan_lua_modules_inner(root, root, &mut modules)?;
    Ok(modules)
}

fn scan_lua_modules_inner(root: &Path, dir: &Path, modules: &mut ModulePaths) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            scan_lua_modules_inner(root, &path, modules)?;
            continue;
        }
        if !metadata.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
            continue;
        }
        let logical = logical_module_name(root, &path)?;
        insert_path_entry(
            modules,
            logical,
            path.canonicalize()
                .with_context(|| format!("canonicalize {}", path.display()))?,
            &format!("module root {}", root.display()),
        )?;
    }
    Ok(())
}

fn logical_module_name(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("strip {} from {}", root.display(), path.display()))?;
    let mut segments = relative
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-utf8 module path {}", path.display()))
                .map(str::to_string)
        })
        .collect::<Result<Vec<_>>>()?;
    let Some(last) = segments.pop() else {
        bail!("empty module path {}", path.display());
    };
    if last == "init.lua" {
        if segments.is_empty() {
            bail!(
                "root init.lua has no logical module name: {}",
                path.display()
            );
        }
    } else {
        let stem = Path::new(&last)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid lua filename {}", path.display()))?;
        segments.push(stem.to_string());
    }
    if segments.iter().any(|segment| segment.is_empty()) {
        bail!("empty module segment in {}", path.display());
    }
    Ok(segments.join("."))
}

fn insert_path_entry(
    modules: &mut ModulePaths,
    logical: String,
    path: PathBuf,
    context: &str,
) -> Result<()> {
    if let Some(previous) = modules.insert(logical.clone(), path.clone()) {
        bail!(
            "{context} has duplicate logical module `{logical}` at {} and {}",
            previous.display(),
            path.display()
        );
    }
    Ok(())
}

fn insert_module_entry(
    modules: &mut ModuleIndex,
    logical: String,
    entry: ModuleEntry,
    context: &str,
) -> Result<()> {
    if let Some(previous) = modules.insert(logical.clone(), entry.clone()) {
        bail!(
            "{context} has ambiguous module `{logical}` at {} and {}",
            previous.path.display(),
            entry.path.display()
        );
    }
    Ok(())
}

fn is_real_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(!metadata.file_type().is_symlink() && metadata.is_dir()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
    }
}

fn canonical_unit_code_root(unit_root: &Path, manifest: &UnitManifest) -> Result<PathBuf> {
    let code_root = unit_root.join(&manifest.code_root);
    let canonical = code_root
        .canonicalize()
        .with_context(|| format!("canonicalize code root {}", code_root.display()))?;
    if !canonical.is_dir() {
        bail!("code root is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn discover_unit_roots(workspace_root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
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

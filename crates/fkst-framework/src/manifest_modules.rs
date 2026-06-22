use crate::manifest::{
    manifest_exports, CatalogUnit, ModuleIndex, ModulePaths, UnitManifest, UNIT_MANIFEST,
};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct OwnModuleScan {
    pub(crate) own_modules: ModulePaths,
    pub(crate) public_modules: ModulePaths,
    pub(crate) private_modules: ModulePaths,
}

pub(crate) fn scan_own_modules(unit: &CatalogUnit) -> Result<OwnModuleScan> {
    if !unit.is_library() {
        let own_modules = scan_lua_modules(unit.code_root())?;
        return Ok(OwnModuleScan {
            public_modules: BTreeMap::new(),
            private_modules: BTreeMap::new(),
            own_modules,
        });
    }

    let public_root = unit.code_root().join("public");
    let private_root = unit.code_root().join("private");
    let has_public = is_real_dir(&public_root)?;
    let has_private = is_real_dir(&private_root)?;
    let raw_public_modules = if has_public {
        scan_lua_modules_allow_root_init(&public_root)?
    } else if !has_private {
        scan_lua_modules_allow_root_init(unit.code_root())?
    } else {
        BTreeMap::new()
    };
    let private_modules = if has_private {
        scan_lua_modules(&private_root)?
    } else {
        BTreeMap::new()
    };
    for logical in raw_public_modules.keys() {
        if private_modules.contains_key(logical) {
            bail!(
                "library `{}` exports duplicate public/private module `{logical}`",
                unit.name()
            );
        }
    }
    let public_modules = prefix_library_public_modules(unit, raw_public_modules)?;
    manifest_exports::validate_public_export_patterns(
        unit.library_name(),
        &unit.manifest().exports.public,
        &public_modules,
    )?;

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

fn prefix_library_public_modules(unit: &CatalogUnit, modules: ModulePaths) -> Result<ModulePaths> {
    let mut prefixed = BTreeMap::new();
    let library_name = unit.library_name();
    for (logical, path) in modules {
        let export = if logical.is_empty() {
            library_name.to_string()
        } else {
            format!("{library_name}.{logical}")
        };
        insert_path_entry(
            &mut prefixed,
            export,
            path,
            &format!("library `{}`", unit.name()),
        )?;
    }
    Ok(prefixed)
}

fn scan_lua_modules(root: &Path) -> Result<ModulePaths> {
    let mut modules = BTreeMap::new();
    scan_lua_modules_inner(root, root, &mut modules, false)?;
    Ok(modules)
}

fn scan_lua_modules_allow_root_init(root: &Path) -> Result<ModulePaths> {
    let mut modules = BTreeMap::new();
    scan_lua_modules_inner(root, root, &mut modules, true)?;
    Ok(modules)
}

fn scan_lua_modules_inner(
    root: &Path,
    dir: &Path,
    modules: &mut ModulePaths,
    allow_root_init: bool,
) -> Result<()> {
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
            scan_lua_modules_inner(root, &path, modules, allow_root_init)?;
            continue;
        }
        if !metadata.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
            continue;
        }
        let logical = logical_module_name(root, &path, allow_root_init)?;
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

fn logical_module_name(root: &Path, path: &Path, allow_root_init: bool) -> Result<String> {
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
            if allow_root_init {
                return Ok(String::new());
            }
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

pub(crate) fn insert_path_entry(
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

pub(crate) fn insert_module_entry(
    modules: &mut ModuleIndex,
    logical: String,
    entry: crate::manifest::ModuleEntry,
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

pub(crate) fn is_real_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(!metadata.file_type().is_symlink() && metadata.is_dir()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
    }
}

pub(crate) fn canonical_unit_code_root(
    unit_root: &Path,
    manifest: &UnitManifest,
) -> Result<PathBuf> {
    let code_root = unit_root.join(&manifest.code_root);
    let canonical = code_root
        .canonicalize()
        .with_context(|| format!("canonicalize code root {}", code_root.display()))?;
    if !canonical.is_dir() {
        bail!("code root is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

pub(crate) fn unit_manifest_path(unit_root: &Path) -> PathBuf {
    unit_root.join(UNIT_MANIFEST)
}

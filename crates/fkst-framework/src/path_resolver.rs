//! Resolve the fixed package roots plus host root graph inputs.

use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(crate) const PACKAGE_ROOT_ENV: &str = "FKST_PACKAGE_ROOT";
pub(crate) const PACKAGE_ROOTS_ENV: &str = "FKST_PACKAGE_ROOTS";

const REJECTED_PACKAGE_ROOT_ENVS: &[&str] = &[
    "FKST_STDLIB_ROOT",
    "FKST_RUNTIME_PACKAGE_ROOT",
    "FKST_GRAPH_ROOTS",
];

/// Build the Lua search path for one graph root.
pub(crate) fn package_root_path(package_root: &Path) -> String {
    let root = package_root.display();
    format!("{root}/?.lua;{root}/?/init.lua;{root}/?/main.lua")
}

#[derive(Clone, Debug)]
pub(crate) struct PackageRoots {
    package_roots: Vec<PathBuf>,
    host_root: PathBuf,
}

impl PackageRoots {
    pub(crate) fn resolve(
        host_root: impl AsRef<Path>,
        explicit_package_roots: Vec<PathBuf>,
    ) -> Result<Self> {
        reject_removed_package_root_envs()?;
        let host_root = canonical_dir(host_root.as_ref(), "--project-root")?;
        let package_roots = if explicit_package_roots.is_empty() {
            roots_from_env()?
        } else {
            canonical_dirs(explicit_package_roots, "--package-root")?
        };
        Ok(Self {
            package_roots,
            host_root,
        })
    }

    pub(crate) fn resolve_run(
        host_root: impl AsRef<Path>,
        explicit_package_root: Option<PathBuf>,
    ) -> Result<Self> {
        reject_removed_package_root_envs()?;
        if std::env::var_os(PACKAGE_ROOTS_ENV).is_some() {
            bail!("{PACKAGE_ROOTS_ENV} is not valid for `run`; pass one --package-root");
        }
        let host_root = canonical_dir(host_root.as_ref(), "--project-root")?;
        let package_root = match explicit_package_root {
            Some(root) => canonical_dir(&root, "--package-root")?,
            None => match std::env::var_os(PACKAGE_ROOT_ENV) {
                Some(root) if !root.is_empty() => {
                    canonical_dir(Path::new(&root), PACKAGE_ROOT_ENV)?
                }
                Some(_) => bail!("{PACKAGE_ROOT_ENV} must not be empty"),
                None => bail!("{PACKAGE_ROOT_ENV} or --package-root is required"),
            },
        };
        Ok(Self {
            package_roots: vec![package_root],
            host_root,
        })
    }

    pub(crate) fn package_roots(&self) -> &[PathBuf] {
        &self.package_roots
    }

    pub(crate) fn single_package_root(&self) -> Result<&Path> {
        match self.package_roots.as_slice() {
            [root] => Ok(root.as_path()),
            _ => bail!(
                "single package root required, got {} package roots",
                self.package_roots.len()
            ),
        }
    }

    pub(crate) fn host_root(&self) -> &Path {
        &self.host_root
    }

    pub(crate) fn graph_roots(&self) -> Vec<GraphRoot> {
        let mut roots = Vec::new();
        let mut host_folded = false;
        for package_root in &self.package_roots {
            if package_root == &self.host_root {
                roots.push(GraphRoot {
                    root: package_root.clone(),
                    kind: GraphRootKind::PackageAndHost,
                });
                host_folded = true;
            } else {
                roots.push(GraphRoot {
                    root: package_root.clone(),
                    kind: GraphRootKind::Package,
                });
            }
        }
        if !host_folded {
            roots.push(GraphRoot {
                root: self.host_root.clone(),
                kind: GraphRootKind::Host,
            });
        }
        roots
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GraphRoot {
    pub(crate) root: PathBuf,
    pub(crate) kind: GraphRootKind,
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

fn reject_duplicate_roots(roots: &[PathBuf]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for root in roots {
        if !seen.insert(root.clone()) {
            bail!("duplicate package root: {}", root.display());
        }
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

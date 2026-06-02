//! Resolve the fixed package root plus host root graph inputs.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub(crate) const PACKAGE_ROOT_ENV: &str = "FKST_PACKAGE_ROOT";

const REJECTED_PACKAGE_ROOT_ENVS: &[&str] = &[
    "FKST_STDLIB_ROOT",
    "FKST_RUNTIME_PACKAGE_ROOT",
    "FKST_GRAPH_ROOTS",
];

#[derive(Clone, Debug)]
pub(crate) struct PackageRoots {
    package_root: PathBuf,
    host_root: PathBuf,
}

impl PackageRoots {
    pub(crate) fn resolve(
        host_root: impl AsRef<Path>,
        explicit_package_root: Option<PathBuf>,
    ) -> Result<Self> {
        reject_removed_package_root_envs()?;
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
            package_root,
            host_root,
        })
    }

    pub(crate) fn package_root(&self) -> &Path {
        &self.package_root
    }

    pub(crate) fn host_root(&self) -> &Path {
        &self.host_root
    }

    pub(crate) fn graph_roots(&self) -> Vec<GraphRoot> {
        if self.package_root == self.host_root {
            vec![GraphRoot {
                root: self.package_root.clone(),
                kind: GraphRootKind::PackageAndHost,
            }]
        } else {
            vec![
                GraphRoot {
                    root: self.package_root.clone(),
                    kind: GraphRootKind::Package,
                },
                GraphRoot {
                    root: self.host_root.clone(),
                    kind: GraphRootKind::Host,
                },
            ]
        }
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

impl GraphRootKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Package => "package root",
            Self::Host => "host root",
            Self::PackageAndHost => "package/host root",
        }
    }
}

fn reject_removed_package_root_envs() -> Result<()> {
    for key in REJECTED_PACKAGE_ROOT_ENVS {
        if std::env::var_os(key).is_some() {
            bail!(
                "{key} is a removed package root surface; use {PACKAGE_ROOT_ENV} or --package-root"
            );
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

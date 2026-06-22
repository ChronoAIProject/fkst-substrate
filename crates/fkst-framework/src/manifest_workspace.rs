use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceManifest {
    discovered_units: Vec<String>,
    registries: BTreeMap<String, String>,
    external_sources: Vec<ExternalSourceDecl>,
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

    pub(crate) fn external_sources(&self) -> &[ExternalSourceDecl] {
        &self.external_sources
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct ExternalSourceDecl {
    pub(crate) id: String,
    pub(crate) git: String,
    pub(crate) rev: Option<String>,
    pub(crate) tag: Option<String>,
    #[serde(default)]
    pub(crate) libraries: Vec<String>,
}

#[derive(Deserialize)]
struct WorkspaceManifestToml {
    workspace: WorkspaceToml,
    #[serde(default)]
    registries: BTreeMap<String, String>,
    #[serde(default)]
    external_sources: Vec<ExternalSourceDecl>,
}

impl WorkspaceManifestToml {
    fn into_manifest(self) -> WorkspaceManifest {
        let mut discovered_units = self.workspace.units;
        discovered_units.extend(self.workspace.packages);
        discovered_units.extend(self.workspace.libraries);
        WorkspaceManifest {
            discovered_units,
            registries: self.registries,
            external_sources: self.external_sources,
        }
    }
}

#[derive(Deserialize)]
struct WorkspaceToml {
    #[serde(default)]
    units: Vec<String>,
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    libraries: Vec<String>,
}

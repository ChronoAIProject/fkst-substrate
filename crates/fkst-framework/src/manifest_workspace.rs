use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceManifest {
    discovered_units: Vec<String>,
    registries: BTreeMap<String, String>,
    external_sources: Vec<ExternalSourceDecl>,
    generator_grants: BTreeMap<String, GeneratorGrant>,
}

impl WorkspaceManifest {
    pub(crate) fn parse_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let manifest = toml::from_str::<WorkspaceManifestToml>(&raw)
            .with_context(|| format!("parse {}", path.display()))?;
        manifest
            .into_manifest()
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

    pub(crate) fn generator_grant(&self, unit: &str) -> Option<&GeneratorGrant> {
        self.generator_grants.get(unit)
    }

    pub(crate) fn generator_grants(&self) -> &BTreeMap<String, GeneratorGrant> {
        &self.generator_grants
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GeneratorGrant {
    pub(crate) output_roots: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) project_input_roots: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) allow_host_source_mutation: bool,
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
    #[serde(default)]
    generators: BTreeMap<String, GeneratorGrant>,
}

impl WorkspaceManifestToml {
    fn into_manifest(self) -> Result<WorkspaceManifest> {
        let mut discovered_units = self.workspace.units;
        discovered_units.extend(self.workspace.packages);
        discovered_units.extend(self.workspace.libraries);
        validate_generator_grants(&self.generators)?;
        Ok(WorkspaceManifest {
            discovered_units,
            registries: self.registries,
            external_sources: self.external_sources,
            generator_grants: self.generators,
        })
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

fn validate_generator_grants(grants: &BTreeMap<String, GeneratorGrant>) -> Result<()> {
    for (unit, grant) in grants {
        crate::path_resolver::validate_name_segment("generator grant unit", unit)?;
        if grant.output_roots.is_empty() {
            bail!("`[generators.{unit}].output_roots` must contain at least one path");
        }
        for root in grant
            .output_roots
            .iter()
            .chain(grant.project_input_roots.iter())
        {
            validate_generator_grant_root(root)?;
        }
        if grant.output_roots.iter().any(|root| root == Path::new("."))
            && !grant.allow_host_source_mutation
        {
            bail!(
                "`[generators.{unit}].output_roots = [\".\"]` requires `allow_host_source_mutation = true`"
            );
        }
    }
    Ok(())
}

fn validate_generator_grant_root(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("generator grant root path must not be empty");
    }
    if path.is_absolute() {
        bail!("generator grant root path must be relative to the host root");
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("generator grant root path must not contain `..`");
    }
    Ok(())
}

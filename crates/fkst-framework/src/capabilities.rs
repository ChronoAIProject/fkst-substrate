use crate::manifest::{PersistenceClass, UnitManifest};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct UnitCapabilities {
    /// Follow-up: gate saga-recovery marker primitives on capabilities.saga_recovery.
    pub(crate) saga_recovery: bool,
    pub(crate) mode: CapabilityMode,
}

impl UnitCapabilities {
    #[allow(dead_code)]
    pub(crate) fn for_manifest(manifest: &UnitManifest) -> Self {
        Self {
            saga_recovery: manifest.persistence_class() == Some(PersistenceClass::Saga),
            mode: CapabilityMode::from_manifest(manifest),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CapabilityMode {
    Full,
    StatelessGenerator(StatelessGeneratorPolicy),
}

impl CapabilityMode {
    pub(crate) fn from_manifest(manifest: &UnitManifest) -> Self {
        match manifest.persistence_class() {
            Some(PersistenceClass::StatelessGenerator) => CapabilityMode::StatelessGenerator(
                StatelessGeneratorPolicy::from_manifest(manifest),
            ),
            _ => CapabilityMode::Full,
        }
    }

    pub(crate) fn is_stateless_generator(&self) -> bool {
        matches!(self, CapabilityMode::StatelessGenerator(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatelessGeneratorPolicy {
    pub(crate) input_roots: Vec<PathBuf>,
    pub(crate) output_roots: Vec<PathBuf>,
}

impl StatelessGeneratorPolicy {
    fn from_manifest(manifest: &UnitManifest) -> Self {
        let generator = manifest
            .generator
            .as_ref()
            .expect("manifest validation requires [generator]");
        Self {
            input_roots: generator.input_roots.clone(),
            output_roots: generator.output_roots.clone(),
        }
    }

    pub(crate) fn canonicalize_under(&self, owner_root: &Path) -> Result<Self> {
        let output_roots = canonicalize_roots(
            owner_root,
            &self.output_roots,
            "output",
            MissingRoot::Create,
        )?;
        let input_roots =
            canonicalize_roots(owner_root, &self.input_roots, "input", MissingRoot::Fail)?;
        Ok(Self {
            input_roots,
            output_roots,
        })
    }
}

#[derive(Clone, Copy)]
enum MissingRoot {
    Create,
    Fail,
}

fn canonicalize_roots(
    owner_root: &Path,
    roots: &[PathBuf],
    label: &str,
    missing: MissingRoot,
) -> Result<Vec<PathBuf>> {
    let owner_root = owner_root
        .canonicalize()
        .with_context(|| format!("canonicalize owner root {}", owner_root.display()))?;
    roots
        .iter()
        .map(|root| {
            let joined = owner_root.join(root);
            if matches!(missing, MissingRoot::Create) {
                std::fs::create_dir_all(&joined).with_context(|| {
                    format!(
                        "create stateless_generator {label}_root {}",
                        joined.display()
                    )
                })?;
            }
            let canonical = joined.canonicalize().with_context(|| {
                format!(
                    "canonicalize stateless_generator {label}_root {}",
                    joined.display()
                )
            })?;
            if !canonical.starts_with(&owner_root) {
                bail!(
                    "stateless_generator {label}_root {} escapes owner root {}",
                    canonical.display(),
                    owner_root.display()
                );
            }
            Ok(canonical)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::UnitManifest;
    use std::fs;

    fn manifest_with_persistence_class(persistence_class: &str) -> UnitManifest {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("fkst.toml"),
            format!(
                r#"
kind = "package"
name = "unit"
persistence_class = "{persistence_class}"

[code]
root = "."
"#
            ),
        )
        .unwrap();

        UnitManifest::parse_file_strict(&temp.path().join("fkst.toml")).unwrap()
    }

    fn manifest_without_persistence_class() -> UnitManifest {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("fkst.toml"),
            r#"
kind = "package"
name = "unit"

[code]
root = "."
"#,
        )
        .unwrap();

        UnitManifest::parse_file(&temp.path().join("fkst.toml")).unwrap()
    }

    #[test]
    fn saga_manifest_enables_saga_recovery() {
        let manifest = manifest_with_persistence_class("saga");

        assert_eq!(
            UnitCapabilities::for_manifest(&manifest),
            UnitCapabilities {
                saga_recovery: true,
                mode: CapabilityMode::Full
            }
        );
    }

    #[test]
    fn non_saga_manifest_disables_saga_recovery() {
        for persistence_class in [
            "stateless_adapter",
            "stateless_generator",
            "judgment_pipeline",
            "composed_judgment_pipeline",
        ] {
            let manifest = if persistence_class == "stateless_generator" {
                manifest_with_stateless_generator()
            } else {
                manifest_with_persistence_class(persistence_class)
            };

            assert_eq!(
                UnitCapabilities::for_manifest(&manifest),
                UnitCapabilities {
                    saga_recovery: false,
                    mode: if persistence_class == "stateless_generator" {
                        CapabilityMode::StatelessGenerator(StatelessGeneratorPolicy {
                            input_roots: Vec::new(),
                            output_roots: vec![PathBuf::from("dist")],
                        })
                    } else {
                        CapabilityMode::Full
                    }
                }
            );
        }
    }

    #[test]
    fn missing_persistence_class_disables_saga_recovery() {
        let manifest = manifest_without_persistence_class();

        assert_eq!(
            UnitCapabilities::for_manifest(&manifest),
            UnitCapabilities {
                saga_recovery: false,
                mode: CapabilityMode::Full
            }
        );
    }

    fn manifest_with_stateless_generator() -> UnitManifest {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("fkst.toml"),
            r#"
kind = "package"
name = "unit"
persistence_class = "stateless_generator"

[code]
root = "."

[generator]
output_roots = ["dist"]
"#,
        )
        .unwrap();

        UnitManifest::parse_file_strict(&temp.path().join("fkst.toml")).unwrap()
    }
}

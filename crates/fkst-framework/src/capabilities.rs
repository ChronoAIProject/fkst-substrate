use crate::manifest::{GeneratorGrant, PersistenceClass, UnitManifest, UNIT_MANIFEST};
use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct UnitCapabilities {
    /// Follow-up: gate saga-recovery marker primitives on capabilities.saga_recovery.
    pub(crate) saga_recovery: bool,
    pub(crate) mode: CapabilityMode,
}

impl UnitCapabilities {
    #[allow(dead_code)]
    pub(crate) fn for_manifest_with_generator_grant(
        manifest: &UnitManifest,
        grant: Option<&GeneratorGrant>,
        grant_label: &str,
    ) -> Result<Self> {
        Ok(Self {
            saga_recovery: manifest.persistence_class() == Some(PersistenceClass::Saga),
            mode: CapabilityMode::for_manifest_with_generator_grant(manifest, grant, grant_label)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CapabilityMode {
    Full,
    StatelessGenerator(StatelessGeneratorPolicy),
}

impl CapabilityMode {
    pub(crate) fn for_manifest_with_generator_grant(
        manifest: &UnitManifest,
        grant: Option<&GeneratorGrant>,
        grant_label: &str,
    ) -> Result<Self> {
        match manifest.persistence_class() {
            Some(PersistenceClass::StatelessGenerator) => {
                let Some(grant) = grant else {
                    bail!(
                        "stateless_generator_host_grant_missing: `{grant_label}` must declare `output_roots`"
                    );
                };
                Ok(CapabilityMode::StatelessGenerator(
                    StatelessGeneratorPolicy::from_manifest_and_grant(manifest, grant),
                ))
            }
            _ => Ok(CapabilityMode::Full),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatelessGeneratorPolicy {
    pub(crate) package_input_roots: Vec<PathBuf>,
    pub(crate) project_input_roots: Vec<PathBuf>,
    pub(crate) output_roots: Vec<PathBuf>,
}

impl StatelessGeneratorPolicy {
    fn from_manifest_and_grant(manifest: &UnitManifest, grant: &GeneratorGrant) -> Self {
        let generator = manifest
            .generator
            .as_ref()
            .expect("manifest validation requires [generator]");
        Self {
            package_input_roots: generator.package_input_roots.clone(),
            project_input_roots: grant.project_input_roots.clone(),
            output_roots: grant.output_roots.clone(),
        }
    }

    pub(crate) fn canonicalize_for_run(&self, owner_root: &Path, host_root: &Path) -> Result<Self> {
        let generated_root = generated_namespace_root(host_root)?;
        let output_roots = canonicalize_roots(
            host_root,
            &self.output_roots,
            "output",
            "host",
            MissingRoot::Create,
            Some(&generated_root),
        )?;
        let package_input_roots = canonicalize_roots(
            owner_root,
            &self.package_input_roots,
            "package_input",
            "owner",
            MissingRoot::Fail,
            None,
        )?;
        let project_input_roots = canonicalize_roots(
            host_root,
            &self.project_input_roots,
            "project_input",
            "host",
            MissingRoot::Fail,
            None,
        )?;
        Ok(Self {
            package_input_roots,
            project_input_roots,
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
    authority_root: &Path,
    roots: &[PathBuf],
    label: &str,
    authority_label: &str,
    missing: MissingRoot,
    namespace_root: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let authority_root = authority_root.canonicalize().with_context(|| {
        format!(
            "canonicalize {authority_label} root {}",
            authority_root.display()
        )
    })?;
    roots
        .iter()
        .map(|root| {
            ensure_relative_root_path(root, label)?;
            if let Some(namespace_root) = namespace_root {
                ensure_relative_root_under_namespace(&authority_root, root, namespace_root, label)?;
            }
            let joined = authority_root.join(root);
            if matches!(missing, MissingRoot::Create) {
                create_dir_all_relative_no_symlink(&authority_root, root, label)?;
            }
            let canonical = joined.canonicalize().with_context(|| {
                format!(
                    "canonicalize stateless_generator {label}_root {}",
                    joined.display()
                )
            })?;
            if !canonical.starts_with(&authority_root) {
                bail!(
                    "stateless_generator {label}_root {} escapes {authority_label} root {}",
                    canonical.display(),
                    authority_root.display()
                );
            }
            if let Some(namespace_root) = namespace_root {
                if !canonical.starts_with(namespace_root) {
                    bail!(
                        "stateless_generator {label}_root {} escapes generated namespace {}",
                        canonical.display(),
                        namespace_root.display()
                    );
                }
            }
            Ok(canonical)
        })
        .collect()
}

fn generated_namespace_root(host_root: &Path) -> Result<PathBuf> {
    let host_root = host_root
        .canonicalize()
        .with_context(|| format!("canonicalize host root {}", host_root.display()))?;
    let manifest_path = host_root.join(UNIT_MANIFEST);
    let manifest = UnitManifest::parse_file(&manifest_path).with_context(|| {
        format!(
            "stateless_generator requires host `[generated].root`: parse {}",
            manifest_path.display()
        )
    })?;
    let generated = manifest
        .generated
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("stateless_generator requires host `[generated].root`"))?;
    create_dir_all_relative_no_symlink(&host_root, &generated.root, "generated namespace")?;
    let canonical = host_root
        .join(&generated.root)
        .canonicalize()
        .with_context(|| {
            format!(
                "canonicalize host generated namespace {}",
                host_root.join(&generated.root).display()
            )
        })?;
    if !canonical.starts_with(&host_root) {
        bail!(
            "host `[generated].root` {} escapes host root {}",
            canonical.display(),
            host_root.display()
        );
    }
    Ok(canonical)
}

fn ensure_relative_root_under_namespace(
    base_root: &Path,
    root: &Path,
    namespace_root: &Path,
    label: &str,
) -> Result<()> {
    let lexical = base_root.join(root);
    if lexical.starts_with(namespace_root) {
        return Ok(());
    }
    bail!(
        "stateless_generator {label}_root {} escapes generated namespace {}",
        root.display(),
        namespace_root.display()
    );
}

fn ensure_relative_root_path(root: &Path, label: &str) -> Result<()> {
    if root.as_os_str().is_empty()
        || root.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!(
            "stateless_generator {label}_root {} must stay relative",
            root.display()
        );
    }
    Ok(())
}

fn create_dir_all_relative_no_symlink(base_root: &Path, root: &Path, label: &str) -> Result<()> {
    let mut current = base_root.to_path_buf();
    for component in root.components() {
        match component {
            Component::Normal(name) => {
                current.push(name);
                create_dir_component_no_symlink(&current, label)?;
            }
            Component::CurDir => {}
            _ => bail!(
                "stateless_generator {label}_root {} must stay relative",
                root.display()
            ),
        }
    }
    Ok(())
}

fn create_dir_component_no_symlink(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "stateless_generator {label}_root {} contains symlink component",
                path.display()
            )
        }
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => bail!(
            "stateless_generator {label}_root {} has non-directory component",
            path.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => match std::fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                create_dir_component_no_symlink(path, label)
            }
            Err(err) => Err(err).with_context(|| {
                format!("create stateless_generator {label}_root {}", path.display())
            }),
        },
        Err(err) => Err(err).with_context(|| {
            format!(
                "inspect stateless_generator {label}_root {}",
                path.display()
            )
        }),
    }
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

        let capabilities = UnitCapabilities::for_manifest_with_generator_grant(
            &manifest,
            None,
            "[generators.unit]",
        )
        .unwrap();

        assert_eq!(
            capabilities,
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

            let grant = if persistence_class == "stateless_generator" {
                Some(GeneratorGrant {
                    output_roots: vec![PathBuf::from("dist")],
                    project_input_roots: Vec::new(),
                    allow_host_source_mutation: false,
                })
            } else {
                None
            };
            let capabilities = UnitCapabilities::for_manifest_with_generator_grant(
                &manifest,
                grant.as_ref(),
                "[generators.unit]",
            )
            .unwrap();

            assert_eq!(
                capabilities,
                UnitCapabilities {
                    saga_recovery: false,
                    mode: if persistence_class == "stateless_generator" {
                        CapabilityMode::StatelessGenerator(StatelessGeneratorPolicy {
                            package_input_roots: Vec::new(),
                            project_input_roots: Vec::new(),
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

        let capabilities = UnitCapabilities::for_manifest_with_generator_grant(
            &manifest,
            None,
            "[generators.unit]",
        )
        .unwrap();

        assert_eq!(
            capabilities,
            UnitCapabilities {
                saga_recovery: false,
                mode: CapabilityMode::Full
            }
        );
    }

    #[test]
    fn stateless_generator_capability_mode_requires_host_grant() {
        let manifest = manifest_with_stateless_generator();

        let err =
            CapabilityMode::for_manifest_with_generator_grant(&manifest, None, "[generators.unit]")
                .unwrap_err();
        let msg = format!("{err:#}");

        assert!(
            msg.contains("stateless_generator_host_grant_missing"),
            "{msg}"
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
suggested_output_roots = ["dist"]
"#,
        )
        .unwrap();

        UnitManifest::parse_file_strict(&temp.path().join("fkst.toml")).unwrap()
    }
}

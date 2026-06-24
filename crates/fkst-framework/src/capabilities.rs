use crate::manifest::{PersistenceClass, UnitManifest};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct UnitCapabilities {
    /// Follow-up: gate saga-recovery marker primitives on capabilities.saga_recovery.
    pub(crate) saga_recovery: bool,
}

#[allow(dead_code)]
impl UnitCapabilities {
    pub(crate) fn for_manifest(manifest: &UnitManifest) -> Self {
        Self {
            saga_recovery: manifest.persistence_class() == Some(PersistenceClass::Saga),
        }
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

        assert_eq!(
            UnitCapabilities::for_manifest(&manifest),
            UnitCapabilities {
                saga_recovery: true
            }
        );
    }

    #[test]
    fn non_saga_manifest_disables_saga_recovery() {
        for persistence_class in [
            "stateless_adapter",
            "judgment_pipeline",
            "composed_judgment_pipeline",
        ] {
            let manifest = manifest_with_persistence_class(persistence_class);

            assert_eq!(
                UnitCapabilities::for_manifest(&manifest),
                UnitCapabilities {
                    saga_recovery: false
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
                saga_recovery: false
            }
        );
    }
}

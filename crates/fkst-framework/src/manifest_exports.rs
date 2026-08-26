use crate::manifest::ModulePaths;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Exports {
    #[serde(default)]
    pub(crate) public: Vec<String>,
    #[serde(default)]
    pub(crate) exact: bool,
}

pub(crate) fn validate_public_exports(
    library_name: &str,
    exports: &Exports,
    public_modules: &ModulePaths,
) -> Result<()> {
    if exports.exact {
        validate_exact_public_exports(library_name, &exports.public, public_modules)
    } else {
        validate_public_export_patterns(library_name, &exports.public, public_modules)
    }
}

fn validate_public_export_patterns(
    library_name: &str,
    patterns: &[String],
    public_modules: &ModulePaths,
) -> Result<()> {
    for pattern in patterns {
        if !public_modules
            .keys()
            .any(|module| export_pattern_matches(pattern, module))
        {
            bail!("library `{library_name}` export pattern `{pattern}` matches no public modules");
        }
    }
    Ok(())
}

fn validate_exact_public_exports(
    library_name: &str,
    declared_modules: &[String],
    public_modules: &ModulePaths,
) -> Result<()> {
    let mut declared = BTreeSet::new();
    for module in declared_modules {
        if !declared.insert(module.as_str()) {
            bail!(
                "library `{library_name}` exact exports declare duplicate public module `{module}`"
            );
        }
    }
    let actual = public_modules
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();

    if let Some(module) = declared.difference(&actual).next() {
        bail!("library `{library_name}` exact exports declare missing public module `{module}`");
    }
    if let Some(module) = actual.difference(&declared).next() {
        bail!("library `{library_name}` exact exports omit public module `{module}`");
    }
    Ok(())
}

fn export_pattern_matches(pattern: &str, module: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(".*") {
        module == prefix || module.starts_with(&format!("{prefix}."))
    } else {
        pattern == module
    }
}

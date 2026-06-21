use crate::manifest::ModulePaths;
use anyhow::{bail, Result};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub(crate) struct Exports {
    #[serde(default)]
    pub(crate) public: Vec<String>,
}

pub(crate) fn validate_public_export_patterns(
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

fn export_pattern_matches(pattern: &str, module: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(".*") {
        module == prefix || module.starts_with(&format!("{prefix}."))
    } else {
        pattern == module
    }
}

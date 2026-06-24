use crate::manifest::{PackageKind, UnitKind, UnitManifest};
use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(crate) struct Options {
    pub(crate) manifest: PathBuf,
}

pub(crate) fn run(options: Options) -> Result<i32> {
    match UnitManifest::parse_file(&options.manifest)
        .with_context(|| format!("load manifest {}", options.manifest.display()))
    {
        Ok(manifest) => {
            if manifest.kind != UnitKind::Package(PackageKind::Composed) {
                return Ok(10);
            }
            for dep in manifest.event_deps {
                println!("{}", dep.as_str());
            }
            Ok(0)
        }
        Err(err) => {
            eprintln!("manifest composed-deps error: {err:#}");
            Ok(1)
        }
    }
}

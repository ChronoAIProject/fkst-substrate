use anyhow::Result;
use fkst_common::RuntimeLayout;
use std::path::Path;

pub(crate) fn layout_from_host_root(host_root: &Path) -> Result<RuntimeLayout> {
    let layout = RuntimeLayout::from_env()?;
    if layout.runtime_root().is_absolute() {
        return Ok(layout);
    }
    RuntimeLayout::new(host_root.join(layout.runtime_root()))
}

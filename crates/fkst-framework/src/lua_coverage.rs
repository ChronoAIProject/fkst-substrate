use anyhow::{Context, Result};
use mlua::{HookTriggers, Lua, Thread, Value, VmState};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub(crate) struct LuaCoverage {
    inner: Arc<Mutex<BTreeMap<String, BTreeSet<u32>>>>,
    roots: Arc<Vec<PathBuf>>,
}

impl LuaCoverage {
    pub(crate) fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
            roots: Arc::new(roots.into_iter().collect()),
        }
    }

    pub(crate) fn install(&self, lua: &Lua) -> mlua::Result<()> {
        let coverage = self.clone();
        lua.set_hook(HookTriggers::EVERY_LINE, move |_, debug| {
            let line = debug.curr_line();
            if line <= 0 {
                return Ok(VmState::Continue);
            }
            let source = debug.source().source.map(|value| value.into_owned());
            coverage.record(source.as_deref(), line as u32);
            Ok(VmState::Continue)
        });
        self.install_coroutine_create_hook(lua)
    }

    pub(crate) fn write_outputs(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let snapshot = self.snapshot()?;
        write_json(&dir.join("coverage.json"), &snapshot)?;
        write_lcov(&dir.join("lcov.info"), &snapshot)?;
        Ok(())
    }

    fn install_coroutine_create_hook(&self, lua: &Lua) -> mlua::Result<()> {
        let globals = lua.globals();
        let Value::Table(coroutine) = globals.get::<Value>("coroutine")? else {
            return Ok(());
        };
        let create: mlua::Function = coroutine.get("create")?;
        let coverage = self.clone();
        coroutine.set(
            "create",
            lua.create_function(move |_, func: mlua::Function| {
                let thread = create.call::<Thread>(func)?;
                coverage.install_thread(&thread);
                Ok(thread)
            })?,
        )?;
        Ok(())
    }

    fn install_thread(&self, thread: &Thread) {
        let coverage = self.clone();
        thread.set_hook(HookTriggers::EVERY_LINE, move |_, debug| {
            let line = debug.curr_line();
            if line <= 0 {
                return Ok(VmState::Continue);
            }
            let source = debug.source().source.map(|value| value.into_owned());
            coverage.record(source.as_deref(), line as u32);
            Ok(VmState::Continue)
        });
    }

    fn record(&self, source: Option<&str>, line: u32) {
        let Some(file) = normalize_source(source, &self.roots) else {
            return;
        };
        if let Ok(mut covered) = self.inner.lock() {
            covered.entry(file).or_default().insert(line);
        }
    }

    fn snapshot(&self) -> Result<BTreeMap<String, CoverageFile>> {
        let covered = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("lua coverage lock poisoned"))?;
        Ok(covered
            .iter()
            .map(|(file, lines)| {
                (
                    file.clone(),
                    CoverageFile {
                        covered_lines: lines.iter().copied().collect(),
                    },
                )
            })
            .collect())
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CoverageFile {
    covered_lines: Vec<u32>,
}

pub(crate) fn chunk_name(path: &Path, owner_root: &Path) -> String {
    let rel = path.strip_prefix(owner_root).unwrap_or(path);
    format!("@{}", normalize_path(rel))
}

fn normalize_source(source: Option<&str>, roots: &[PathBuf]) -> Option<String> {
    let source = source?;
    let file = source.strip_prefix('@')?;
    if file.is_empty() || file.starts_with("fkst:") || file.ends_with("_test.lua") {
        return None;
    }
    let path = Path::new(file);
    let path = roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .unwrap_or(path);
    let normalized = normalize_path(path);
    if normalized.is_empty() || normalized.ends_with("_test.lua") {
        return None;
    }
    Some(normalized)
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_string()),
            Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn write_json(path: &Path, snapshot: &BTreeMap<String, CoverageFile>) -> Result<()> {
    let data = serde_json::to_vec_pretty(snapshot)?;
    write_atomic(path, &data)
}

fn write_lcov(path: &Path, snapshot: &BTreeMap<String, CoverageFile>) -> Result<()> {
    let mut data = String::new();
    for (file, entry) in snapshot {
        data.push_str("TN:\n");
        data.push_str("SF:");
        data.push_str(file);
        data.push('\n');
        for line in &entry.covered_lines {
            data.push_str("DA:");
            data.push_str(&line.to_string());
            data.push_str(",1\n");
        }
        data.push_str("end_of_record\n");
    }
    write_atomic(path, data.as_bytes())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("coverage path has no file name"))?;
    let tmp_path = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp_path, data)
        .with_context(|| format!("write temporary coverage {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_name_uses_at_prefixed_relative_paths() {
        let root = Path::new("/tmp/pkg");
        let path = root.join("departments/worker/main.lua");
        assert_eq!(chunk_name(&path, root), "@departments/worker/main.lua");
    }
}

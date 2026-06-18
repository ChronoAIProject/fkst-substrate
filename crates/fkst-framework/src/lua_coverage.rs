use anyhow::{Context, Result};
use mlua::{DebugEvent, HookTriggers, Lua, Thread, Value, VmState};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const NO_SOURCE_ID: usize = usize::MAX;
const COVERAGE_TRIGGERS: HookTriggers = HookTriggers::new().on_calls().on_returns().every_line();

#[derive(Clone, Debug)]
pub(crate) struct LuaCoverage {
    files: Arc<Vec<CoverageHits>>,
    source_ids: Arc<BTreeMap<String, usize>>,
}

impl LuaCoverage {
    pub(crate) fn new(roots: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let roots = roots.into_iter().collect::<Vec<_>>();
        let files = collect_coverage_files(&roots)?;
        let mut source_ids = BTreeMap::new();
        for (idx, file) in files.iter().enumerate() {
            for source in &file.sources {
                source_ids.insert(source.clone(), idx);
            }
        }
        Ok(Self {
            files: Arc::new(files),
            source_ids: Arc::new(source_ids),
        })
    }

    pub(crate) fn install(&self, lua: &Lua) -> mlua::Result<()> {
        let coverage = self.clone();
        let state = Arc::new(HookState::default());
        lua.set_hook(COVERAGE_TRIGGERS, move |_, debug| {
            coverage.record_hook_event(&state, debug);
            Ok(VmState::Continue)
        });
        self.install_coroutine_create_hook(lua)
    }

    pub(crate) fn write_outputs(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let snapshot = self.snapshot();
        write_json(&dir.join("coverage.json"), &snapshot)?;
        write_lcov(&dir.join("lcov.info"), &snapshot)?;
        Ok(())
    }

    fn record_hook_event(&self, state: &HookState, debug: mlua::Debug<'_>) {
        match debug.event() {
            DebugEvent::Call => {
                let id = self.source_id(debug.source().source.as_deref());
                state.enter(id);
            }
            DebugEvent::TailCall => {
                let id = self.source_id(debug.source().source.as_deref());
                state.replace(id);
            }
            DebugEvent::Ret => state.leave(),
            DebugEvent::Line => {
                let line = debug.curr_line();
                if line > 0 {
                    self.mark(state.current(), line as u32);
                }
            }
            DebugEvent::Count | DebugEvent::Unknown(_) => {}
        }
    }

    fn source_id(&self, source: Option<&str>) -> usize {
        source
            .and_then(|source| source.strip_prefix('@'))
            .and_then(|source| self.source_ids.get(source).copied())
            .unwrap_or(NO_SOURCE_ID)
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
        let state = Arc::new(HookState::default());
        thread.set_hook(COVERAGE_TRIGGERS, move |_, debug| {
            coverage.record_hook_event(&state, debug);
            Ok(VmState::Continue)
        });
    }

    fn mark(&self, source_id: usize, line: u32) {
        let Some(file) = self.files.get(source_id) else {
            return;
        };
        let line_idx = line.saturating_sub(1) as usize;
        let word_idx = line_idx / 64;
        let bit = 1u64 << (line_idx % 64);
        if let Some(word) = file.words.get(word_idx) {
            word.fetch_or(bit, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> BTreeMap<String, CoverageFile> {
        self.files
            .iter()
            .filter_map(|file| {
                let covered_lines = file.covered_lines();
                if covered_lines.is_empty() {
                    None
                } else {
                    Some((file.file.clone(), CoverageFile { covered_lines }))
                }
            })
            .collect()
    }
}

#[derive(Debug)]
struct CoverageHits {
    file: String,
    sources: Vec<String>,
    words: Vec<AtomicU64>,
}

impl CoverageHits {
    fn new(file: String, sources: Vec<String>, line_count: usize) -> Self {
        let word_count = line_count.div_ceil(64).max(1);
        Self {
            file,
            sources,
            words: (0..word_count).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn covered_lines(&self) -> Vec<u32> {
        let mut lines = Vec::new();
        for (word_idx, word) in self.words.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            while bits != 0 {
                let bit_idx = bits.trailing_zeros() as usize;
                lines.push((word_idx * 64 + bit_idx + 1) as u32);
                bits &= !(1u64 << bit_idx);
            }
        }
        lines
    }
}

#[derive(Debug)]
struct HookState {
    current: AtomicUsize,
    stack: Mutex<Vec<usize>>,
}

impl Default for HookState {
    fn default() -> Self {
        Self {
            current: AtomicUsize::new(NO_SOURCE_ID),
            stack: Mutex::new(Vec::new()),
        }
    }
}

impl HookState {
    fn current(&self) -> usize {
        self.current.load(Ordering::Relaxed)
    }

    fn enter(&self, id: usize) {
        let previous = self.current.swap(id, Ordering::Relaxed);
        if let Ok(mut stack) = self.stack.lock() {
            stack.push(previous);
        }
    }

    fn replace(&self, id: usize) {
        self.current.store(id, Ordering::Relaxed);
    }

    fn leave(&self) {
        let previous = self
            .stack
            .lock()
            .ok()
            .and_then(|mut stack| stack.pop())
            .unwrap_or(NO_SOURCE_ID);
        self.current.store(previous, Ordering::Relaxed);
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

fn collect_coverage_files(roots: &[PathBuf]) -> Result<Vec<CoverageHits>> {
    let mut files = BTreeMap::<String, CoverageSource>::new();
    for root in roots {
        collect_coverage_files_from_root(root, root, &mut files)
            .with_context(|| format!("scan coverage files in {}", root.display()))?;
    }
    Ok(files
        .into_iter()
        .map(|(file, source)| CoverageHits::new(file, source.sources, source.line_count))
        .collect())
}

fn collect_coverage_files_from_root(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, CoverageSource>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_coverage_files_from_root(root, &path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("lua") {
            let file = chunk_name(&path, root);
            let Some(file) = normalize_source(Some(&file), &[]) else {
                continue;
            };
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            files.entry(file.clone()).or_insert_with(|| CoverageSource {
                sources: coverage_source_names(&path, &file),
                line_count: line_count(&source),
            });
        }
    }
    Ok(())
}

#[derive(Debug)]
struct CoverageSource {
    sources: Vec<String>,
    line_count: usize,
}

fn coverage_source_names(path: &Path, normalized_file: &str) -> Vec<String> {
    let mut sources = vec![normalized_file.to_string()];
    let raw_path = path.to_string_lossy().into_owned();
    if raw_path != normalized_file {
        sources.push(raw_path);
    }
    sources
}

fn line_count(source: &str) -> usize {
    source
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
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

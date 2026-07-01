use crate::manifest::ExternalSourceDecl;
use crate::manifest_hash::sha256_hex;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const CACHE_ROOT_ENV: &str = "FKST_CACHE_ROOT";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Lockfile {
    entries: Vec<LockEntry>,
    external_sources: Vec<ExternalSourceLock>,
}

impl Lockfile {
    pub(crate) fn parse_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str::<LockfileToml>(&raw)
            .map(LockfileToml::into_lockfile)
            .with_context(|| format!("parse {}", path.display()))
    }

    pub(crate) fn write_file(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let body = toml::to_string_pretty(&LockfileWriteToml::from_lockfile(self))
            .context("serialize fkst.lock")?;
        let tmp = parent.join(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("fkst.lock"),
            std::process::id()
        ));
        fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| {
            format!(
                "rename temporary lockfile {} to {}",
                tmp.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[LockEntry] {
        &self.entries
    }

    pub(crate) fn external_source(&self, id: &str) -> Option<&ExternalSourceLock> {
        self.external_sources.iter().find(|source| source.id == id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LockEntry {
    pub(crate) name: String,
    pub(crate) source: String,
    pub(crate) version: Option<String>,
    pub(crate) checksum: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ExternalSourceLock {
    pub(crate) id: String,
    pub(crate) git: String,
    pub(crate) intent: ExternalSourceIntent,
    pub(crate) resolved: ExternalSourceResolved,
    #[serde(default)]
    pub(crate) libraries: Vec<ExternalLibraryLock>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ExternalSourceIntent {
    pub(crate) rev: Option<String>,
    pub(crate) tag: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ExternalSourceResolved {
    pub(crate) rev: String,
    pub(crate) tree_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ExternalLibraryLock {
    pub(crate) name: String,
    pub(crate) unit: String,
    pub(crate) exports_sha256: String,
}

#[derive(Deserialize)]
struct LockfileToml {
    #[serde(default, rename = "package")]
    packages: Vec<LockEntry>,
    #[serde(default, rename = "library")]
    libraries: Vec<LockEntry>,
    #[serde(default, rename = "external_source")]
    external_sources: Vec<ExternalSourceLock>,
}

impl LockfileToml {
    fn into_lockfile(mut self) -> Lockfile {
        self.packages.append(&mut self.libraries);
        Lockfile {
            entries: self.packages,
            external_sources: self.external_sources,
        }
    }
}

#[derive(Serialize)]
struct LockfileWriteToml<'a> {
    #[serde(rename = "package", skip_serializing_if = "Vec::is_empty")]
    packages: Vec<&'a LockEntry>,
    #[serde(rename = "external_source", skip_serializing_if = "Vec::is_empty")]
    external_sources: Vec<&'a ExternalSourceLock>,
}

impl<'a> LockfileWriteToml<'a> {
    fn from_lockfile(lockfile: &'a Lockfile) -> Self {
        Self {
            packages: lockfile.entries.iter().collect(),
            external_sources: lockfile.external_sources.iter().collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ExternalSourceCheckout {
    pub(crate) source_id: String,
    pub(crate) root: PathBuf,
    pub(crate) libraries: Vec<ExternalLibraryCheckout>,
    pub(crate) available_libraries: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ExternalLibraryCheckout {
    pub(crate) name: String,
    pub(crate) unit: String,
    pub(crate) publishable: bool,
}

pub(crate) fn lock_external_sources(
    workspace_root: &Path,
    sources: &[ExternalSourceDecl],
) -> Result<Lockfile> {
    let cache = cache_root();
    let mut locked_sources = Vec::new();
    for source in sources {
        validate_source_decl(source)?;
        let resolved_rev = resolve_source_rev(source)?;
        let checkout_root = export_source_tree(&cache, source, &resolved_rev)?;
        let tree_sha256 = prefixed_tree_sha256(&checkout_root)?;
        let store_root = cache.store_checkout(&tree_sha256);
        fs::create_dir_all(store_root.parent().unwrap())
            .with_context(|| format!("create {}", store_root.parent().unwrap().display()))?;
        if store_root.exists() {
            remove_dir_all(&store_root)?;
        }
        fs::rename(&checkout_root, &store_root).with_context(|| {
            format!(
                "move source checkout {} to {}",
                checkout_root.display(),
                store_root.display()
            )
        })?;
        let libraries = catalog_allowed_libraries(source, &store_root)?;
        locked_sources.push(ExternalSourceLock {
            id: source.id.clone(),
            git: source.git.clone(),
            intent: ExternalSourceIntent {
                rev: source.rev.clone(),
                tag: source.tag.clone(),
            },
            resolved: ExternalSourceResolved {
                rev: resolved_rev,
                tree_sha256,
            },
            libraries,
        });
    }
    let existing_path = workspace_root.join("fkst.lock");
    let mut lockfile = if existing_path.exists() {
        Lockfile::parse_file(&existing_path)?
    } else {
        Lockfile::default()
    };
    lockfile.external_sources = locked_sources;
    Ok(lockfile)
}

pub(crate) fn fetch_locked_sources(
    sources: &[ExternalSourceDecl],
    lockfile: &Lockfile,
) -> Result<Vec<ExternalSourceCheckout>> {
    sources
        .iter()
        .map(|source| locked_source_checkout(source, lockfile))
        .collect()
}

fn locked_source_checkout(
    source: &ExternalSourceDecl,
    lockfile: &Lockfile,
) -> Result<ExternalSourceCheckout> {
    validate_source_decl(source)?;
    let Some(lock) = lockfile.external_source(&source.id) else {
        bail!(
            "external source `{}` is missing from fkst.lock; run `fkst-framework host lock --project-root <root>`",
            source.id
        );
    };
    validate_lock_matches_manifest(source, lock)?;
    let cache = cache_root();
    let store_root = cache.store_checkout(&lock.resolved.tree_sha256);
    if !store_root.exists() {
        let temp = export_source_tree(&cache, source, &lock.resolved.rev)?;
        let actual = prefixed_tree_sha256(&temp)?;
        if actual != lock.resolved.tree_sha256 {
            remove_dir_all(&temp)?;
            bail!(
                "external source `{}` tree hash mismatch: lock has {}, checkout has {}",
                source.id,
                lock.resolved.tree_sha256,
                actual
            );
        }
        fs::create_dir_all(store_root.parent().unwrap())
            .with_context(|| format!("create {}", store_root.parent().unwrap().display()))?;
        if store_root.exists() {
            remove_dir_all(&temp)?;
        } else {
            fs::rename(&temp, &store_root)
                .with_context(|| format!("move {} to {}", temp.display(), store_root.display()))?;
        }
    }
    let store_root = store_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", store_root.display()))?;
    let actual_tree = prefixed_tree_sha256(&store_root)?;
    if actual_tree != lock.resolved.tree_sha256 {
        bail!(
            "external source `{}` cached tree hash mismatch: lock has {}, cache has {}",
            source.id,
            lock.resolved.tree_sha256,
            actual_tree
        );
    }
    let libraries = lock
        .libraries
        .iter()
        .map(|library| {
            if !source.libraries.iter().any(|allowed| allowed == &library.name) {
                bail!(
                    "external source `{}` lock admits library `{}` not allowed by manifest",
                    source.id,
                    library.name
                );
            }
            let unit_root = store_root.join(&library.unit);
            let actual_exports = prefixed_exports_sha256(&unit_root)?;
            if actual_exports != library.exports_sha256 {
                bail!(
                    "external source `{}` library `{}` exports hash mismatch: lock has {}, checkout has {}",
                    source.id,
                    library.name,
                    library.exports_sha256,
                    actual_exports
                );
            }
            Ok(ExternalLibraryCheckout {
                name: library.name.clone(),
                unit: library.unit.clone(),
                publishable: publishable_library_at(&unit_root)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let available_libraries = available_library_names(&store_root)?;
    Ok(ExternalSourceCheckout {
        source_id: source.id.clone(),
        root: store_root,
        libraries,
        available_libraries,
    })
}

fn validate_source_decl(source: &ExternalSourceDecl) -> Result<()> {
    validate_name("external source id", &source.id)?;
    if source.git.trim().is_empty() {
        bail!("external source `{}` git must not be empty", source.id);
    }
    match (&source.rev, &source.tag) {
        (Some(rev), None) => validate_full_sha(rev)
            .with_context(|| format!("validate external source `{}` rev", source.id)),
        (None, Some(tag)) if tag.trim().is_empty() => {
            bail!("external source `{}` tag must not be empty", source.id)
        }
        (None, Some(_)) => Ok(()),
        (Some(_), Some(_)) => bail!(
            "external source `{}` must set only one of rev or tag",
            source.id
        ),
        (None, None) => bail!("external source `{}` must set rev or tag", source.id),
    }?;
    if source.packages.is_empty() && source.libraries.is_empty() {
        bail!(
            "external source `{}` must declare at least one package or library",
            source.id
        );
    }
    let mut seen = BTreeSet::new();
    for package in &source.packages {
        validate_name("external source package", package)?;
        if !seen.insert(package.clone()) {
            bail!(
                "external source `{}` declares duplicate package `{package}`",
                source.id
            );
        }
    }
    let mut seen = BTreeSet::new();
    for library in &source.libraries {
        validate_name("external source library", library)?;
        if !seen.insert(library.clone()) {
            bail!(
                "external source `{}` declares duplicate library `{library}`",
                source.id
            );
        }
    }
    Ok(())
}

fn validate_lock_matches_manifest(
    source: &ExternalSourceDecl,
    lock: &ExternalSourceLock,
) -> Result<()> {
    if lock.git != source.git {
        bail!(
            "external source `{}` git mismatch between workspace manifest and fkst.lock",
            source.id
        );
    }
    if lock.intent.rev != source.rev || lock.intent.tag != source.tag {
        bail!(
            "external source `{}` intent mismatch between workspace manifest and fkst.lock",
            source.id
        );
    }
    validate_full_sha(&lock.resolved.rev)
        .with_context(|| format!("validate external source `{}` locked rev", source.id))?;
    validate_prefixed_hash(&lock.resolved.tree_sha256)
        .with_context(|| format!("validate external source `{}` tree hash", source.id))?;
    // Lock allowlist completeness: every manifest-allowlisted library MUST be
    // present in the lock. Without this, a stale/tampered lock could omit an
    // allowlisted external library; the external provider would then never be
    // cataloged, the duplicate-name fail-closed check would not fire, and an
    // internal library of the same name would be silently selected — violating
    // "no workspace-internal-wins precedence". Combined with the locked-library
    // loop (which rejects locked libraries not in the manifest), this pins
    // lock.libraries == source.libraries exactly.
    for allowed in &source.libraries {
        if !lock
            .libraries
            .iter()
            .any(|library| &library.name == allowed)
        {
            bail!(
                "external source `{}` library `{}` is allowed by the workspace manifest but missing from fkst.lock; run `fkst-framework host lock --project-root <root>`",
                source.id,
                allowed
            );
        }
    }
    Ok(())
}

fn resolve_source_rev(source: &ExternalSourceDecl) -> Result<String> {
    if let Some(rev) = &source.rev {
        return Ok(rev.clone());
    }
    let tag = source
        .tag
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("external source `{}` must set rev or tag", source.id))?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&source.git)
        .args([
            "rev-parse",
            "--verify",
            &format!("refs/tags/{tag}^{{commit}}"),
        ])
        .output()
        .with_context(|| format!("resolve tag `{tag}` for external source `{}`", source.id))?;
    if !output.status.success() {
        bail!(
            "external source `{}` tag `{tag}` resolve failed: {}",
            source.id,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let rev = String::from_utf8_lossy(&output.stdout).trim().to_string();
    validate_full_sha(&rev)?;
    Ok(rev)
}

fn export_source_tree(
    cache: &ExternalCache,
    source: &ExternalSourceDecl,
    rev: &str,
) -> Result<PathBuf> {
    validate_full_sha(rev)?;
    let mirror = cache.mirror_path(&source.git);
    fs::create_dir_all(mirror.parent().unwrap())
        .with_context(|| format!("create {}", mirror.parent().unwrap().display()))?;
    if mirror.exists() {
        let output = Command::new("git")
            .arg("-C")
            .arg(&mirror)
            .args(["fetch", "--tags", "--force", "--prune", &source.git])
            .output()
            .with_context(|| format!("fetch external source `{}` mirror", source.id))?;
        if !output.status.success() {
            bail!(
                "external source `{}` fetch failed: {}",
                source.id,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    } else {
        let output = Command::new("git")
            .args(["clone", "--mirror", &source.git])
            .arg(&mirror)
            .output()
            .with_context(|| format!("clone external source `{}` mirror", source.id))?;
        if !output.status.success() {
            bail!(
                "external source `{}` mirror clone failed: {}",
                source.id,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    let work = cache.temp_checkout(&source.id, rev);
    if work.exists() {
        remove_dir_all(&work)?;
    }
    fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let archive = Command::new("git")
        .arg("-C")
        .arg(&mirror)
        .args(["archive", "--format=tar", rev])
        .output()
        .with_context(|| format!("archive external source `{}`", source.id))?;
    if !archive.status.success() {
        remove_dir_all(&work)?;
        bail!(
            "external source `{}` archive failed: {}",
            source.id,
            String::from_utf8_lossy(&archive.stderr).trim()
        );
    }
    let mut tar = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(&work)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("extract external source `{}` archive", source.id))?;
    use std::io::Write as _;
    tar.stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("tar stdin unavailable"))?
        .write_all(&archive.stdout)
        .context("write git archive to tar")?;
    drop(tar.stdin.take());
    let output = tar.wait_with_output().context("wait for tar extraction")?;
    if !output.status.success() {
        remove_dir_all(&work)?;
        bail!(
            "external source `{}` archive extraction failed: {}",
            source.id,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(work)
}

fn catalog_allowed_libraries(
    source: &ExternalSourceDecl,
    checkout_root: &Path,
) -> Result<Vec<ExternalLibraryLock>> {
    let workspace =
        crate::manifest::WorkspaceManifest::parse_file(&checkout_root.join("fkst.workspace.toml"))?;
    let checkout_root = checkout_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", checkout_root.display()))?;
    let unit_roots =
        crate::manifest::discover_unit_roots(&checkout_root, workspace.discovered_units())?;
    let mut libraries_by_name = BTreeMap::new();
    for unit_root in unit_roots {
        let manifest = crate::manifest::UnitManifest::parse_file(&unit_root.join("fkst.toml"))?;
        if manifest.kind != crate::manifest::UnitKind::Library {
            continue;
        }
        let library_name = manifest
            .library
            .as_ref()
            .map(|library| library.name.clone())
            .unwrap_or_else(|| manifest.name.clone());
        if let Some(previous) = libraries_by_name.insert(library_name.clone(), unit_root.clone()) {
            bail!(
                "external source `{}` declares duplicate library `{library_name}` at {} and {}",
                source.id,
                previous.display(),
                unit_root.display()
            );
        }
    }
    let mut admitted = Vec::new();
    for library in &source.libraries {
        let Some(unit_root) = libraries_by_name.get(library) else {
            bail!(
                "external source `{}` does not allow library `{library}` because it is absent",
                source.id
            );
        };
        admitted.push(ExternalLibraryLock {
            name: library.clone(),
            unit: unit_root
                .strip_prefix(&checkout_root)
                .with_context(|| {
                    format!(
                        "strip {} from {}",
                        checkout_root.display(),
                        unit_root.display()
                    )
                })?
                .to_string_lossy()
                .replace('\\', "/"),
            exports_sha256: prefixed_exports_sha256(unit_root)?,
        });
    }
    Ok(admitted)
}

fn available_library_names(checkout_root: &Path) -> Result<BTreeSet<String>> {
    let workspace =
        crate::manifest::WorkspaceManifest::parse_file(&checkout_root.join("fkst.workspace.toml"))?;
    let unit_roots =
        crate::manifest::discover_unit_roots(checkout_root, workspace.discovered_units())?;
    let mut names = BTreeSet::new();
    for unit_root in unit_roots {
        let manifest = crate::manifest::UnitManifest::parse_file(&unit_root.join("fkst.toml"))?;
        if manifest.kind == crate::manifest::UnitKind::Library {
            names.insert(
                manifest
                    .library
                    .as_ref()
                    .map(|library| library.name.clone())
                    .unwrap_or(manifest.name),
            );
        }
    }
    Ok(names)
}

fn publishable_library_at(unit_root: &Path) -> Result<bool> {
    let manifest = crate::manifest::UnitManifest::parse_file(&unit_root.join("fkst.toml"))?;
    Ok(manifest
        .library
        .as_ref()
        .map(|library| library.publishable)
        .unwrap_or(false))
}

// A tree-hash entry is either a regular file (hashed by content) or a symlink
// (hashed by its target path, git-style). Symlinks are recorded — not rejected
// nor dereferenced — so a real source repo with benign symlinks (e.g. a root
// `AGENTS.md` -> `CLAUDE.md`) can be locked, while a retargeted symlink still
// changes the hash. The module index is no-follow, so symlinks stay inert at
// load time; recording the target here is integrity, not a dereference.
enum HashEntry {
    File(PathBuf),
    Symlink(Vec<u8>),
}

pub(crate) fn tree_sha256(root: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect_hash_files(root, root, &mut files)?;
    let mut data = Vec::new();
    for (relative, entry) in files {
        data.extend_from_slice(relative.as_bytes());
        data.push(0);
        match entry {
            HashEntry::File(path) => {
                data.push(b'f');
                data.extend_from_slice(
                    &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
                );
            }
            HashEntry::Symlink(target) => {
                data.push(b'l');
                data.extend_from_slice(&target);
            }
        }
        data.push(0);
    }
    Ok(sha256_hex(&data))
}

pub(crate) fn exports_sha256(unit_root: &Path) -> Result<String> {
    let public_root = unit_root.join("public");
    if public_root.is_dir() {
        tree_sha256(&public_root)
    } else {
        tree_sha256(unit_root)
    }
}

fn prefixed_tree_sha256(root: &Path) -> Result<String> {
    Ok(format!("sha256-{}", tree_sha256(root)?))
}

fn prefixed_exports_sha256(unit_root: &Path) -> Result<String> {
    Ok(format!("sha256-{}", exports_sha256(unit_root)?))
}

fn collect_hash_files(root: &Path, dir: &Path, files: &mut Vec<(String, HashEntry)>) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let file_name = entry.file_name();
        if file_name.to_str() == Some(".git") {
            continue;
        }
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            collect_hash_files(root, &path, files)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .with_context(|| format!("strip {} from {}", root.display(), path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        if file_type.is_symlink() {
            // Record the link target (NOT the dereferenced content): integrity
            // without following the link out of the cache.
            let target =
                fs::read_link(&path).with_context(|| format!("readlink {}", path.display()))?;
            files.push((
                relative,
                HashEntry::Symlink(target.into_os_string().into_vec()),
            ));
        } else if file_type.is_file() {
            files.push((relative, HashEntry::File(path)));
        }
        // Other node types (fifo/socket/device) are ignored, as before.
    }
    Ok(())
}

fn cache_root() -> ExternalCache {
    let root = std::env::var_os(CACHE_ROOT_ENV)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".cache/fkst"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("fkst-cache"));
    ExternalCache { root }
}

struct ExternalCache {
    root: PathBuf,
}

impl ExternalCache {
    fn mirror_path(&self, git: &str) -> PathBuf {
        self.root
            .join("git-mirrors")
            .join(format!("{}.git", sha256_hex(git.as_bytes())))
    }

    fn store_checkout(&self, tree_sha256: &str) -> PathBuf {
        self.root.join("store").join(tree_sha256)
    }

    fn temp_checkout(&self, source_id: &str, rev: &str) -> PathBuf {
        self.root.join("tmp").join(format!(
            "{}-{}-{}",
            source_id,
            &rev[..12],
            std::process::id()
        ))
    }
}

fn remove_dir_all(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn validate_name(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} must not be empty");
    }
    if !value
        .bytes()
        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
    {
        bail!("{label} `{value}` must match [A-Za-z0-9_-]+");
    }
    Ok(())
}

fn validate_full_sha(value: &str) -> Result<()> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("git rev `{value}` must be a 40-hex sha");
    }
    Ok(())
}

fn validate_prefixed_hash(value: &str) -> Result<()> {
    let Some(hash) = value.strip_prefix("sha256-") else {
        bail!("hash `{value}` must start with sha256-");
    };
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("hash `{value}` must be sha256- plus 64 hex chars");
    }
    Ok(())
}

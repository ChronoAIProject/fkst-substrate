use crate::host_conformance::{ConformanceContext, HostCheck, RulePack};
use crate::manifest::{UnitManifest, UNIT_MANIFEST};
use crate::path_resolver::GraphRoot;
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

const PACK_SCHEMA: u32 = 1;
const RUNNER_PROTOCOL: &str = "fkst-declarative-rulepack@1";
const LOADER_CHECK_ID: &str = "conformance-pack-loader";
const MAX_GLOB_SEGMENTS: usize = 128;

#[derive(Debug)]
pub(crate) struct DeclarativeRulePack {
    name: String,
    owner_package: String,
    owner_root: PathBuf,
    rules: Result<Vec<Rule>, String>,
}

impl DeclarativeRulePack {
    pub(crate) fn from_graph_root(graph_root: &GraphRoot) -> Option<Self> {
        let owner_root = graph_root.root.clone();
        let owner_package = graph_root.namespace.clone();
        let name = format!("declarative:{owner_package}");
        let manifest_path = owner_root.join(UNIT_MANIFEST);
        let manifest = match UnitManifest::parse_file_strict(&manifest_path) {
            Ok(manifest) => manifest,
            Err(err) if manifest_path.exists() => {
                return Some(Self {
                    name,
                    owner_package,
                    owner_root,
                    rules: Err(format!(
                        "load declarative conformance manifest {}: {err:#}",
                        manifest_path.display()
                    )),
                });
            }
            Err(_) => return None,
        };
        let Some(conformance) = manifest.conformance else {
            return None;
        };

        let rules = load_pack(&owner_root, &owner_package, &conformance.pack).map_err(|err| {
            format!(
                "load declarative conformance pack for package `{owner_package}` at {}: {err:#}",
                conformance.pack.display()
            )
        });

        Some(Self {
            name,
            owner_package,
            owner_root,
            rules,
        })
    }
}

impl RulePack for DeclarativeRulePack {
    fn name(&self) -> &str {
        &self.name
    }

    fn run(&self, _context: &ConformanceContext<'_>) -> Vec<HostCheck> {
        let rules = match &self.rules {
            Ok(rules) => rules,
            Err(message) => {
                return vec![HostCheck::fail_for_package(
                    LOADER_CHECK_ID,
                    self.owner_package.clone(),
                    message.clone(),
                )];
            }
        };

        let mut checks = Vec::new();
        for rule in rules {
            match rule {
                Rule::MaxLineCount(rule) => {
                    checks.extend(rule.run(&self.owner_package, &self.owner_root))
                }
                Rule::TextForbidRegex(rule) => {
                    checks.extend(rule.run(&self.owner_package, &self.owner_root))
                }
                Rule::TextRequireRegex(rule) => {
                    checks.extend(rule.run(&self.owner_package, &self.owner_root))
                }
            }
        }
        checks
    }
}

fn load_pack(owner_root: &Path, owner_package: &str, pack_path: &Path) -> Result<Vec<Rule>> {
    let manifest_pack_path = validate_manifest_pack_path(pack_path)?;
    let pack_path = canonical_child_path(owner_root, &manifest_pack_path, "conformance pack")?;
    let raw = fs::read_to_string(&pack_path)
        .with_context(|| format!("read conformance pack {}", pack_path.display()))?;
    let pack: RulePackToml = toml::from_str(&raw)
        .with_context(|| format!("parse conformance pack {}", pack_path.display()))?;
    if pack.schema != PACK_SCHEMA {
        bail!(
            "unsupported declarative conformance schema {}; expected {PACK_SCHEMA}",
            pack.schema
        );
    }
    if pack.runner_protocol != RUNNER_PROTOCOL {
        bail!(
            "unsupported runner_protocol `{}`; expected `{RUNNER_PROTOCOL}`",
            pack.runner_protocol
        );
    }
    if pack.owner_package != owner_package {
        bail!(
            "owner_package `{}` does not match active package `{owner_package}`",
            pack.owner_package
        );
    }
    if pack.rules.is_empty() {
        bail!("declarative conformance pack must declare at least one rule");
    }

    pack.rules
        .into_iter()
        .map(Rule::from_toml)
        .collect::<Result<Vec<_>>>()
}

fn validate_manifest_pack_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("[conformance].pack must not be empty");
    }
    if path.is_absolute() {
        bail!("[conformance].pack must be package-relative, not absolute");
    }
    reject_parent_or_prefix_components(path, "[conformance].pack")?;
    Ok(path.to_path_buf())
}

fn canonical_child_path(root: &Path, relative: &Path, label: &str) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize owner package root {}", root.display()))?;
    let candidate = root.join(relative);
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("canonicalize {label} {}", candidate.display()))?;
    if !canonical.starts_with(&root) {
        bail!(
            "{label} {} escapes owner package root {}",
            canonical.display(),
            root.display()
        );
    }
    Ok(canonical)
}

fn reject_parent_or_prefix_components(path: &Path, label: &str) -> Result<()> {
    for component in path.components() {
        match component {
            Component::ParentDir => bail!("{label} must not contain `..`"),
            Component::Prefix(_) | Component::RootDir => {
                bail!("{label} must be package-relative")
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulePackToml {
    schema: u32,
    runner_protocol: String,
    owner_package: String,
    rules: Vec<RuleToml>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleToml {
    id: String,
    severity: SeverityToml,
    kind: String,
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    max: Option<usize>,
    pattern: Option<String>,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SeverityToml {
    Error,
}

#[derive(Debug)]
enum Rule {
    MaxLineCount(MaxLineCountRule),
    TextForbidRegex(TextForbidRegexRule),
    TextRequireRegex(TextRequireRegexRule),
}

impl Rule {
    fn from_toml(rule: RuleToml) -> Result<Self> {
        let RuleToml {
            id,
            severity,
            kind,
            include,
            exclude,
            max,
            pattern,
            message,
        } = rule;
        match kind.as_str() {
            "max_line_count" => {
                if pattern.is_some() {
                    bail!("rule `{id}` field `pattern` is not allowed for kind `max_line_count`");
                }
                let max = max.ok_or_else(|| anyhow!("rule `{id}` missing required field `max`"))?;
                Ok(Self::MaxLineCount(MaxLineCountRule::new(
                    id, severity, include, exclude, max, message,
                )?))
            }
            "text_forbid_regex" => {
                if max.is_some() {
                    bail!("rule `{id}` field `max` is not allowed for kind `text_forbid_regex`");
                }
                let pattern = pattern
                    .ok_or_else(|| anyhow!("rule `{id}` missing required field `pattern`"))?;
                Ok(Self::TextForbidRegex(TextForbidRegexRule::new(
                    id, severity, include, exclude, pattern, message,
                )?))
            }
            "text_require_regex" => {
                if max.is_some() {
                    bail!("rule `{id}` field `max` is not allowed for kind `text_require_regex`");
                }
                let pattern = pattern
                    .ok_or_else(|| anyhow!("rule `{id}` missing required field `pattern`"))?;
                Ok(Self::TextRequireRegex(TextRequireRegexRule::new(
                    id, severity, include, exclude, pattern, message,
                )?))
            }
            other => bail!("rule `{id}` uses unknown kind `{other}`"),
        }
    }
}

#[derive(Debug)]
struct TextForbidRegexRule {
    id: String,
    include: Vec<GlobPattern>,
    exclude: Vec<GlobPattern>,
    pattern: Regex,
    message: String,
}

impl TextForbidRegexRule {
    fn new(
        id: String,
        severity: SeverityToml,
        include: Vec<String>,
        exclude: Vec<String>,
        pattern: String,
        message: String,
    ) -> Result<Self> {
        match severity {
            SeverityToml::Error => {}
        }
        validate_rule_id(&id)?;
        if include.is_empty() {
            bail!("rule `{id}` include must not be empty");
        }
        Ok(Self {
            pattern: compile_text_forbid_regex(&id, &pattern)?,
            id,
            include: build_glob_patterns(&include, "include")?,
            exclude: build_glob_patterns(&exclude, "exclude")?,
            message,
        })
    }

    fn run(&self, owner_package: &str, owner_root: &Path) -> Vec<HostCheck> {
        match owner_package_files(owner_root) {
            Ok(scan) => self.run_with_files(owner_package, &scan.root, scan.files),
            Err(err) => vec![HostCheck::fail_for_package(
                LOADER_CHECK_ID,
                owner_package.to_string(),
                format!("scan owner package files failed: {err:#}"),
            )],
        }
    }

    fn run_with_files(
        &self,
        owner_package: &str,
        owner_root: &Path,
        files: Vec<PathBuf>,
    ) -> Vec<HostCheck> {
        let mut checks = Vec::new();
        for file in files {
            let relative = match file.strip_prefix(owner_root) {
                Ok(relative) => relative,
                Err(_) => {
                    checks.push(HostCheck::fail_for_package(
                        LOADER_CHECK_ID,
                        owner_package.to_string(),
                        format!(
                            "owner package file {} escaped root {}",
                            file.display(),
                            owner_root.display()
                        ),
                    ));
                    continue;
                }
            };
            let relative_segments = match relative_path_segments(relative) {
                Ok(segments) => segments,
                Err(err) => {
                    checks.push(HostCheck::fail_for_package(
                        LOADER_CHECK_ID,
                        owner_package.to_string(),
                        format!(
                            "owner package file {} has an invalid relative path: {err:#}",
                            file.display()
                        ),
                    ));
                    continue;
                }
            };
            let relative_display = display_relative_segments(&relative_segments);
            if !matches_any(&self.include, &relative_segments)
                || matches_any(&self.exclude, &relative_segments)
            {
                continue;
            }
            match first_forbidden_match_line(&file, &self.pattern) {
                Ok(Some(line)) => checks.push(HostCheck::fail_for_package(
                    format!("{}:{}", self.id, relative_display),
                    owner_package.to_string(),
                    format!(
                        "{}: {} matches forbidden text at line {line}",
                        self.message, relative_display
                    ),
                )),
                Ok(None) => {}
                Err(err) => checks.push(HostCheck::fail_for_package(
                    format!("{}:{}", self.id, relative_display),
                    owner_package.to_string(),
                    format!(
                        "{}: cannot read {}: {err:#}",
                        self.message, relative_display
                    ),
                )),
            }
        }
        if checks.is_empty() {
            checks.push(HostCheck::pass_for_package(
                self.id.clone(),
                owner_package.to_string(),
                format!("{}: no matching forbidden text found", self.message),
            ));
        }
        checks
    }
}

fn compile_text_forbid_regex(rule_id: &str, pattern: &str) -> Result<Regex> {
    if pattern.is_empty() {
        bail!("rule `{rule_id}` pattern must not be empty");
    }
    Regex::new(pattern)
        .with_context(|| format!("rule `{rule_id}` invalid text_forbid_regex pattern `{pattern}`"))
}

#[derive(Debug)]
struct TextRequireRegexRule {
    id: String,
    include: Vec<GlobPattern>,
    exclude: Vec<GlobPattern>,
    pattern: Regex,
    message: String,
}

impl TextRequireRegexRule {
    fn new(
        id: String,
        severity: SeverityToml,
        include: Vec<String>,
        exclude: Vec<String>,
        pattern: String,
        message: String,
    ) -> Result<Self> {
        match severity {
            SeverityToml::Error => {}
        }
        validate_rule_id(&id)?;
        if include.is_empty() {
            bail!("rule `{id}` include must not be empty");
        }
        Ok(Self {
            pattern: compile_text_require_regex(&id, &pattern)?,
            id,
            include: build_glob_patterns(&include, "include")?,
            exclude: build_glob_patterns(&exclude, "exclude")?,
            message,
        })
    }

    fn run(&self, owner_package: &str, owner_root: &Path) -> Vec<HostCheck> {
        match owner_package_files(owner_root) {
            Ok(scan) => self.run_with_files(owner_package, &scan.root, scan.files),
            Err(err) => vec![HostCheck::fail_for_package(
                LOADER_CHECK_ID,
                owner_package.to_string(),
                format!("scan owner package files failed: {err:#}"),
            )],
        }
    }

    fn run_with_files(
        &self,
        owner_package: &str,
        owner_root: &Path,
        files: Vec<PathBuf>,
    ) -> Vec<HostCheck> {
        let mut checks = Vec::new();
        let mut matched_files = 0usize;
        for file in files {
            let relative = match file.strip_prefix(owner_root) {
                Ok(relative) => relative,
                Err(_) => {
                    checks.push(HostCheck::fail_for_package(
                        LOADER_CHECK_ID,
                        owner_package.to_string(),
                        format!(
                            "owner package file {} escaped root {}",
                            file.display(),
                            owner_root.display()
                        ),
                    ));
                    continue;
                }
            };
            let relative_segments = match relative_path_segments(relative) {
                Ok(segments) => segments,
                Err(err) => {
                    checks.push(HostCheck::fail_for_package(
                        LOADER_CHECK_ID,
                        owner_package.to_string(),
                        format!(
                            "owner package file {} has an invalid relative path: {err:#}",
                            file.display()
                        ),
                    ));
                    continue;
                }
            };
            let relative_display = display_relative_segments(&relative_segments);
            if !matches_any(&self.include, &relative_segments)
                || matches_any(&self.exclude, &relative_segments)
            {
                continue;
            }
            matched_files += 1;
            match text_matches_required_pattern(&file, &self.pattern) {
                Ok(true) => {}
                Ok(false) => checks.push(HostCheck::fail_for_package(
                    format!("{}:{}", self.id, relative_display),
                    owner_package.to_string(),
                    format!(
                        "{}: {} does not match required text",
                        self.message, relative_display
                    ),
                )),
                Err(err) => checks.push(HostCheck::fail_for_package(
                    format!("{}:{}", self.id, relative_display),
                    owner_package.to_string(),
                    format!(
                        "{}: cannot read {}: {err:#}",
                        self.message, relative_display
                    ),
                )),
            }
        }
        if matched_files == 0 {
            checks.push(HostCheck::fail_for_package(
                self.id.clone(),
                owner_package.to_string(),
                format!("{}: no files matched required include globs", self.message),
            ));
        } else if checks.is_empty() {
            checks.push(HostCheck::pass_for_package(
                self.id.clone(),
                owner_package.to_string(),
                format!("{}: all matching files contain required text", self.message),
            ));
        }
        checks
    }
}

fn compile_text_require_regex(rule_id: &str, pattern: &str) -> Result<Regex> {
    if pattern.is_empty() {
        bail!("rule `{rule_id}` pattern must not be empty");
    }
    Regex::new(pattern)
        .with_context(|| format!("rule `{rule_id}` invalid text_require_regex pattern `{pattern}`"))
}

fn text_matches_required_pattern(path: &Path, pattern: &Regex) -> Result<bool> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read UTF-8 {}", path.display()))?;
    Ok(pattern.is_match(&text))
}

fn first_forbidden_match_line(path: &Path, pattern: &Regex) -> Result<Option<usize>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read UTF-8 {}", path.display()))?;
    Ok(pattern
        .find(&text)
        .map(|found| line_number_at_byte_offset(&text, found.start())))
}

fn line_number_at_byte_offset(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1
}

#[derive(Debug)]
struct MaxLineCountRule {
    id: String,
    include: Vec<GlobPattern>,
    exclude: Vec<GlobPattern>,
    max: usize,
    message: String,
}

impl MaxLineCountRule {
    fn new(
        id: String,
        severity: SeverityToml,
        include: Vec<String>,
        exclude: Vec<String>,
        max: usize,
        message: String,
    ) -> Result<Self> {
        match severity {
            SeverityToml::Error => {}
        }
        validate_rule_id(&id)?;
        if include.is_empty() {
            bail!("rule `{id}` include must not be empty");
        }
        if max == 0 {
            bail!("rule `{id}` max must be greater than zero");
        }
        Ok(Self {
            id,
            include: build_glob_patterns(&include, "include")?,
            exclude: build_glob_patterns(&exclude, "exclude")?,
            max,
            message,
        })
    }

    fn run(&self, owner_package: &str, owner_root: &Path) -> Vec<HostCheck> {
        match owner_package_files(owner_root) {
            Ok(scan) => self.run_with_files(owner_package, &scan.root, scan.files),
            Err(err) => vec![HostCheck::fail_for_package(
                LOADER_CHECK_ID,
                owner_package.to_string(),
                format!("scan owner package files failed: {err:#}"),
            )],
        }
    }

    fn run_with_files(
        &self,
        owner_package: &str,
        owner_root: &Path,
        files: Vec<PathBuf>,
    ) -> Vec<HostCheck> {
        let mut checks = Vec::new();
        for file in files {
            let relative = match file.strip_prefix(owner_root) {
                Ok(relative) => relative,
                Err(_) => {
                    checks.push(HostCheck::fail_for_package(
                        LOADER_CHECK_ID,
                        owner_package.to_string(),
                        format!(
                            "owner package file {} escaped root {}",
                            file.display(),
                            owner_root.display()
                        ),
                    ));
                    continue;
                }
            };
            let relative_segments = match relative_path_segments(relative) {
                Ok(segments) => segments,
                Err(err) => {
                    checks.push(HostCheck::fail_for_package(
                        LOADER_CHECK_ID,
                        owner_package.to_string(),
                        format!(
                            "owner package file {} has an invalid relative path: {err:#}",
                            file.display()
                        ),
                    ));
                    continue;
                }
            };
            let relative_display = display_relative_segments(&relative_segments);
            if !matches_any(&self.include, &relative_segments)
                || matches_any(&self.exclude, &relative_segments)
            {
                continue;
            }
            match count_text_lines(&file, self.max) {
                Ok(lines) if lines <= self.max => {}
                Ok(lines) => checks.push(HostCheck::fail_for_package(
                    format!("{}:{}", self.id, relative_display),
                    owner_package.to_string(),
                    format!(
                        "{}: {} has {lines} lines, max is {}",
                        self.message, relative_display, self.max
                    ),
                )),
                Err(err) => checks.push(HostCheck::fail_for_package(
                    format!("{}:{}", self.id, relative_display),
                    owner_package.to_string(),
                    format!(
                        "{}: cannot read {}: {err:#}",
                        self.message, relative_display
                    ),
                )),
            }
        }
        if checks.is_empty() {
            checks.push(HostCheck::pass_for_package(
                self.id.clone(),
                owner_package.to_string(),
                format!(
                    "{}: all matching files have at most {} lines",
                    self.message, self.max
                ),
            ));
        }
        checks
    }
}

fn validate_rule_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("rule id must not be empty");
    }
    if !id
        .bytes()
        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        bail!("rule id `{id}` must match [A-Za-z0-9._-]+");
    }
    Ok(())
}

fn build_glob_patterns(patterns: &[String], field: &str) -> Result<Vec<GlobPattern>> {
    patterns
        .iter()
        .map(|pattern| GlobPattern::new(pattern, field))
        .collect()
}

#[derive(Debug)]
struct GlobPattern {
    segments: Vec<GlobSegment>,
}

#[derive(Debug)]
enum GlobSegment {
    Recursive,
    Segment(String),
}

impl GlobPattern {
    fn new(pattern: &str, field: &str) -> Result<Self> {
        validate_glob_pattern(pattern, field)?;
        let segments = pattern
            .split('/')
            .map(|segment| {
                if segment == "**" {
                    GlobSegment::Recursive
                } else {
                    GlobSegment::Segment(segment.to_string())
                }
            })
            .collect::<Vec<_>>();
        Ok(Self { segments })
    }

    fn is_match(&self, path_segments: &[String]) -> bool {
        match_segments(&self.segments, path_segments)
    }
}

fn validate_glob_pattern(pattern: &str, field: &str) -> Result<()> {
    if pattern.trim().is_empty() {
        bail!("{field} glob must not be empty");
    }
    if pattern.contains('\\') {
        bail!("{field} glob `{pattern}` must use `/` separators");
    }
    let path = Path::new(pattern);
    if path.is_absolute() {
        bail!("{field} glob `{pattern}` must be package-relative");
    }
    let raw_segments = pattern.split('/').collect::<Vec<_>>();
    if raw_segments.len() > MAX_GLOB_SEGMENTS {
        bail!(
            "{field} glob `{pattern}` has {} segments; max is {MAX_GLOB_SEGMENTS}",
            raw_segments.len()
        );
    }
    for segment in &raw_segments {
        if segment.is_empty() {
            bail!("{field} glob `{pattern}` must not contain empty path segments");
        }
        if *segment == "." {
            bail!("{field} glob `{pattern}` must not contain `.` path segments");
        }
        if *segment == ".." {
            bail!("{field} glob `{pattern}` must not contain `..`");
        }
    }
    reject_parent_or_prefix_components(path, field)
        .with_context(|| format!("validate {field} glob `{pattern}`"))?;
    Ok(())
}

fn matches_any(patterns: &[GlobPattern], path_segments: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| pattern.is_match(path_segments))
}

fn match_segments(pattern: &[GlobSegment], path: &[String]) -> bool {
    let mut dp = vec![vec![false; path.len() + 1]; pattern.len() + 1];
    dp[pattern.len()][path.len()] = true;

    for pattern_index in (0..pattern.len()).rev() {
        for path_index in (0..=path.len()).rev() {
            dp[pattern_index][path_index] = match &pattern[pattern_index] {
                GlobSegment::Recursive => {
                    dp[pattern_index + 1][path_index]
                        || (path_index < path.len() && dp[pattern_index][path_index + 1])
                }
                GlobSegment::Segment(segment) => {
                    path_index < path.len()
                        && glob_segment_match(segment, &path[path_index])
                        && dp[pattern_index + 1][path_index + 1]
                }
            };
        }
    }
    dp[0][0]
}

fn glob_segment_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut p = 0usize;
    let mut t = 0usize;
    let mut star: Option<usize> = None;
    let mut match_after_star = 0usize;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            match_after_star = t;
            p += 1;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            match_after_star += 1;
            t = match_after_star;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

struct OwnerPackageFiles {
    root: PathBuf,
    files: Vec<PathBuf>,
}

fn owner_package_files(owner_root: &Path) -> Result<OwnerPackageFiles> {
    let root = owner_root
        .canonicalize()
        .with_context(|| format!("canonicalize owner package root {}", owner_root.display()))?;
    let mut files = Vec::new();
    let mut seen_dirs = BTreeSet::new();
    collect_files(&root, &root, &mut seen_dirs, &mut files)?;
    files.sort();
    Ok(OwnerPackageFiles { root, files })
}

fn collect_files(
    root: &Path,
    dir: &Path,
    seen_dirs: &mut BTreeSet<PathBuf>,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let canonical_dir = dir
        .canonicalize()
        .with_context(|| format!("canonicalize directory {}", dir.display()))?;
    if !canonical_dir.starts_with(root) {
        bail!(
            "directory {} escapes owner package root {}",
            canonical_dir.display(),
            root.display()
        );
    }
    if !seen_dirs.insert(canonical_dir) {
        return Ok(());
    }

    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("read directory {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read directory entry in {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "symlink entries are not allowed in declarative conformance scan: {}",
                path.display()
            );
        } else if metadata.is_dir() {
            collect_files(root, &path, seen_dirs, files)?;
        } else if metadata.is_file() {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("canonicalize file {}", path.display()))?;
            if !canonical.starts_with(root) {
                bail!(
                    "file {} escapes owner package root {}",
                    canonical.display(),
                    root.display()
                );
            }
            files.push(canonical);
        }
    }
    Ok(())
}

fn count_text_lines(path: &Path, max: usize) -> Result<usize> {
    let file = fs::File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut count = 0usize;
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .with_context(|| format!("decode UTF-8 {}", path.display()))?;
        if bytes == 0 {
            return Ok(count);
        }
        count += 1;
        if count > max {
            return Ok(count);
        }
    }
}

fn relative_path_segments(path: &Path) -> Result<Vec<String>> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .ok_or_else(|| anyhow!("non-utf8 relative path component in {}", path.display()))
                .map(str::to_string),
            Component::CurDir => bail!("relative path contains `.`: {}", path.display()),
            Component::ParentDir => bail!("relative path contains `..`: {}", path.display()),
            Component::Prefix(_) | Component::RootDir => {
                bail!("relative path is not package-relative: {}", path.display())
            }
        })
        .collect()
}

fn display_relative_segments(segments: &[String]) -> String {
    segments.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_segments(path: &str) -> Vec<String> {
        path.split('/').map(str::to_string).collect()
    }

    fn assert_glob(pattern: &str, path: &str, expected: bool) {
        let glob = GlobPattern::new(pattern, "include").unwrap();
        assert_eq!(
            glob.is_match(&path_segments(path)),
            expected,
            "pattern `{pattern}` path `{path}`"
        );
    }

    #[test]
    fn recursive_glob_matches_zero_or_more_segments() {
        assert_glob("src/**/*.lua", "src/main.lua", true);
        assert_glob("src/**/*.lua", "src/a/b/main.lua", true);
        assert_glob("src/**", "src/a/b/main.lua", true);
        assert_glob("src/**/main.lua", "src/main.lua", true);
    }

    #[test]
    fn star_does_not_cross_path_separator() {
        assert_glob("src/*.lua", "src/main.lua", true);
        assert_glob("src/*.lua", "src/a/main.lua", false);
    }

    #[test]
    fn leading_and_trailing_slash_globs_are_rejected() {
        assert!(GlobPattern::new("/src/*.lua", "include").is_err());
        assert!(GlobPattern::new("src/*.lua/", "include").is_err());
    }

    #[test]
    fn backslash_glob_is_rejected() {
        assert!(GlobPattern::new(r"src\*.lua", "include").is_err());
    }

    #[test]
    fn question_mark_matches_exactly_one_byte() {
        assert_glob("src/file?.lua", "src/file1.lua", true);
        assert_glob("src/file?.lua", "src/file10.lua", false);
    }

    #[test]
    fn recursive_glob_matching_is_bounded() {
        let pattern = (0..64)
            .map(|_| "**")
            .chain(std::iter::once("target.lua"))
            .collect::<Vec<_>>()
            .join("/");
        let path = (0..64)
            .map(|index| format!("dir{index}"))
            .chain(std::iter::once("target.lua".to_string()))
            .collect::<Vec<_>>();
        let glob = GlobPattern::new(&pattern, "include").unwrap();
        assert!(glob.is_match(&path));
    }
}

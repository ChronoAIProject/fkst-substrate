//! Dependency graph validator for manifest workspaces.

use crate::manifest::{CatalogUnit, UnitCatalog, UnitKind, Visibility};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub(crate) struct DepsOptions {
    pub(crate) project_root: PathBuf,
    pub(crate) package_roots: Vec<PathBuf>,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Serialize)]
struct DepsReport {
    ok: bool,
    workspace_root: String,
    units: Vec<UnitReport>,
    lib_edges: Vec<EdgeReport>,
    event_edges: Vec<EdgeReport>,
    failures: Vec<Diagnostic>,
    warnings: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Serialize)]
struct UnitReport {
    name: String,
    kind: String,
    root: String,
    code_root: String,
    library: Option<String>,
    lib_deps: Vec<String>,
    event_deps: Vec<String>,
    actual_lib_requires: Vec<String>,
    modules: Vec<String>,
    public_exports: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct EdgeReport {
    from: String,
    to: String,
}

#[derive(Clone, Debug, Serialize)]
struct Diagnostic {
    kind: &'static str,
    level: &'static str,
    unit: Option<String>,
    library: Option<String>,
    module: Option<String>,
    message: String,
}

impl Diagnostic {
    fn fail(
        kind: &'static str,
        unit: Option<&str>,
        library: Option<&str>,
        module: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            level: "fail",
            unit: unit.map(str::to_string),
            library: library.map(str::to_string),
            module: module.map(str::to_string),
            message: message.into(),
        }
    }

    fn warn(
        kind: &'static str,
        unit: Option<&str>,
        library: Option<&str>,
        module: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            level: "warn",
            unit: unit.map(str::to_string),
            library: library.map(str::to_string),
            module: module.map(str::to_string),
            message: message.into(),
        }
    }
}

pub(crate) fn run(options: DepsOptions) -> Result<i32> {
    let project_root = canonical_dir(&options.project_root, "--project-root")?;
    for package_root in &options.package_roots {
        let _ = canonical_dir(package_root, "--package-root")?;
    }
    let report = validate(&project_root)?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }
    Ok(if report.ok { 0 } else { 1 })
}

fn validate(project_root: &Path) -> Result<DepsReport> {
    let validation_catalog =
        UnitCatalog::discover_for_validation(project_root)?.ok_or_else(|| {
            anyhow::anyhow!("manifest catalog is required: missing fkst.workspace.toml")
        })?;
    let mut failures = Vec::new();
    let mut warnings = Vec::new();
    let library_names = library_names(&validation_catalog);
    let mut actual_requires: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    validate_declared_libs(
        &validation_catalog,
        &library_names,
        &mut failures,
        &mut warnings,
    );
    validate_cycles(&validation_catalog, &library_names, &mut failures);

    for unit in validation_catalog.units() {
        let required =
            scan_actual_library_requires(unit, &validation_catalog, &library_names, &mut failures)?;
        validate_actual_requires(
            unit,
            &required,
            &library_names,
            &mut failures,
            &mut warnings,
        );
        actual_requires.insert(unit.name().to_string(), required);
        validate_composed_deps(unit, &mut failures)?;
    }

    let units = unit_reports(&validation_catalog, &actual_requires);
    let lib_edges = lib_edges(&validation_catalog);
    let event_edges = event_edges(&validation_catalog);
    let report = DepsReport {
        ok: failures.is_empty(),
        workspace_root: validation_catalog.workspace_root().display().to_string(),
        units,
        lib_edges,
        event_edges,
        failures,
        warnings,
    };
    Ok(report)
}

fn validate_declared_libs(
    catalog: &UnitCatalog,
    library_names: &BTreeSet<String>,
    failures: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) {
    let mut declared = BTreeSet::new();
    for unit in catalog.units() {
        for dep in &unit.manifest().lib_deps {
            let lib = dep.as_str();
            declared.insert(lib.to_string());
            let Some(library_unit_name) = catalog.library_unit_name(lib) else {
                failures.push(Diagnostic::fail(
                    "missing-lib",
                    Some(unit.name()),
                    Some(lib),
                    None,
                    format!("{} declares unknown library `{lib}`", unit.name()),
                ));
                continue;
            };
            let Some(library_unit) = catalog.units().find(|candidate| {
                candidate.name() == library_unit_name && candidate.library_name() == lib
            }) else {
                failures.push(Diagnostic::fail(
                    "missing-lib",
                    Some(unit.name()),
                    Some(lib),
                    None,
                    format!("{} declares unknown library `{lib}`", unit.name()),
                ));
                continue;
            };
            if !unit_allowed_to_declare(unit.name(), library_unit) {
                failures.push(Diagnostic::fail(
                    "visibility",
                    Some(unit.name()),
                    Some(lib),
                    None,
                    format!("{} is not allowed to declare library `{lib}`", unit.name()),
                ));
            }
        }
    }

    for library in library_names {
        if !declared.contains(library) {
            warnings.push(Diagnostic::warn(
                "orphan-library",
                None,
                Some(library),
                None,
                format!("library `{library}` is declared by no unit"),
            ));
        }
    }
}

fn validate_cycles(
    catalog: &UnitCatalog,
    library_names: &BTreeSet<String>,
    failures: &mut Vec<Diagnostic>,
) {
    let graph = catalog
        .units()
        .filter(|unit| unit.is_library())
        .map(|unit| {
            (
                unit.library_name().to_string(),
                unit.manifest()
                    .lib_deps
                    .iter()
                    .filter_map(|dep| {
                        let lib = dep.as_str();
                        library_names.contains(lib).then(|| lib.to_string())
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut state = BTreeMap::<String, VisitState>::new();
    let mut stack = Vec::new();
    let mut seen_cycles = BTreeSet::new();
    for library in graph.keys() {
        visit_cycle(
            library,
            &graph,
            &mut state,
            &mut stack,
            &mut seen_cycles,
            failures,
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisitState {
    Visiting,
    Done,
}

fn visit_cycle(
    library: &str,
    graph: &BTreeMap<String, Vec<String>>,
    state: &mut BTreeMap<String, VisitState>,
    stack: &mut Vec<String>,
    seen_cycles: &mut BTreeSet<String>,
    failures: &mut Vec<Diagnostic>,
) {
    match state.get(library) {
        Some(VisitState::Done) => return,
        Some(VisitState::Visiting) => {
            if let Some(start) = stack.iter().position(|value| value == library) {
                let mut cycle = stack[start..].to_vec();
                cycle.push(library.to_string());
                let key = cycle.join(" -> ");
                if seen_cycles.insert(key.clone()) {
                    failures.push(Diagnostic::fail(
                        "cycle",
                        None,
                        Some(library),
                        None,
                        format!("lib_deps cycle: {key}"),
                    ));
                }
            }
            return;
        }
        None => {}
    }

    state.insert(library.to_string(), VisitState::Visiting);
    stack.push(library.to_string());
    if let Some(next) = graph.get(library) {
        for dep in next {
            visit_cycle(dep, graph, state, stack, seen_cycles, failures);
        }
    }
    stack.pop();
    state.insert(library.to_string(), VisitState::Done);
}

fn validate_actual_requires(
    unit: &CatalogUnit,
    actual: &BTreeSet<String>,
    library_names: &BTreeSet<String>,
    failures: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) {
    let declared = unit
        .manifest()
        .lib_deps
        .iter()
        .map(|dep| dep.as_str().to_string())
        .collect::<BTreeSet<_>>();

    for required in actual {
        if !declared.contains(required) {
            failures.push(Diagnostic::fail(
                "undeclared-require",
                Some(unit.name()),
                Some(required),
                None,
                format!(
                    "{} requires library `{required}` but does not declare it in lib_deps",
                    unit.name()
                ),
            ));
        }
    }

    for declared_lib in declared {
        if library_names.contains(&declared_lib) && !actual.contains(&declared_lib) {
            warnings.push(Diagnostic::warn(
                "unused-lib-dep",
                Some(unit.name()),
                Some(&declared_lib),
                None,
                format!(
                    "{} declares library `{declared_lib}` but no Lua file requires it",
                    unit.name()
                ),
            ));
        }
    }
}

fn validate_composed_deps(unit: &CatalogUnit, failures: &mut Vec<Diagnostic>) -> Result<()> {
    let path = unit.unit_root().join("composed.deps");
    if !path.exists() {
        return Ok(());
    }
    let actual = parse_composed_deps(&path)?;
    let declared = unit
        .manifest()
        .event_deps
        .iter()
        .map(|dep| dep.as_str().to_string())
        .collect::<BTreeSet<_>>();
    if actual != declared {
        failures.push(Diagnostic::fail(
            "event-deps",
            Some(unit.name()),
            None,
            None,
            format!(
                "{} event_deps {:?} do not match composed.deps {:?}",
                unit.name(),
                declared,
                actual
            ),
        ));
    }
    Ok(())
}

fn scan_actual_library_requires(
    unit: &CatalogUnit,
    catalog: &UnitCatalog,
    library_names: &BTreeSet<String>,
    failures: &mut Vec<Diagnostic>,
) -> Result<BTreeSet<String>> {
    let mut required = BTreeSet::new();
    for path in lua_files(unit.code_root())? {
        let body = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        for module in require_literals(&body) {
            if let Some((library, export)) = module.split_once('.') {
                if library_names.contains(library) {
                    let self_library_require = unit.is_library() && unit.library_name() == library;
                    if !self_library_require {
                        required.insert(library.to_string());
                    }
                    validate_public_export_reference(
                        unit.name(),
                        library,
                        export,
                        &module,
                        catalog,
                        failures,
                    );
                }
            }
        }
    }
    Ok(required)
}

fn lua_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_lua_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_lua_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_lua_files(&path, files)?;
        } else if metadata.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("lua")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn require_literals(source: &str) -> Vec<String> {
    let mut modules = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_lua_line_comment(bytes, i) {
            i = skip_line(bytes, i + 2);
            continue;
        }
        if bytes[i..].starts_with(b"require") {
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after = i + "require".len();
            let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
            if before_ok && after_ok {
                if let Some((module, next)) = parse_require_call(source, after) {
                    modules.push(module);
                    i = next;
                    continue;
                }
            }
        }
        i += 1;
    }
    modules
}

fn validate_public_export_reference(
    unit_name: &str,
    library: &str,
    export: &str,
    module: &str,
    catalog: &UnitCatalog,
    failures: &mut Vec<Diagnostic>,
) {
    let Some(library_unit_name) = catalog.library_unit_name(library) else {
        return;
    };
    let Some(library_unit) = catalog
        .units()
        .find(|candidate| candidate.name() == library_unit_name)
    else {
        return;
    };
    if !library_unit.public_modules().contains_key(module) {
        failures.push(Diagnostic::fail(
            "missing-export",
            Some(unit_name),
            Some(library),
            Some(export),
            format!("{unit_name} references missing public export `{library}.{export}`"),
        ));
    }
}

fn parse_require_call(source: &str, mut i: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    i = skip_space(bytes, i);
    if i < bytes.len() && bytes[i] == b'(' {
        i += 1;
        i = skip_space(bytes, i);
    }
    if i >= bytes.len() || (bytes[i] != b'"' && bytes[i] != b'\'') {
        return None;
    }
    let quote = bytes[i];
    i += 1;
    let mut value = String::new();
    while i < bytes.len() {
        match bytes[i] {
            byte if byte == quote => return Some((value, i + 1)),
            b'\\' if i + 1 < bytes.len() => {
                i += 1;
                value.push(bytes[i] as char);
            }
            byte => value.push(byte as char),
        }
        i += 1;
    }
    None
}

fn is_lua_line_comment(bytes: &[u8], i: usize) -> bool {
    i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-'
}

fn skip_line(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_space(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn parse_composed_deps(path: &Path) -> Result<BTreeSet<String>> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn library_names(catalog: &UnitCatalog) -> BTreeSet<String> {
    catalog
        .units()
        .filter(|unit| unit.is_library())
        .map(|unit| unit.library_name().to_string())
        .collect()
}

fn unit_allowed_to_declare(consumer: &str, library_unit: &CatalogUnit) -> bool {
    match &library_unit.manifest().visibility {
        Visibility::Public => true,
        Visibility::Allow(allow) => allow.iter().any(|unit| unit == consumer),
    }
}

fn unit_reports(
    catalog: &UnitCatalog,
    actual_requires: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<UnitReport> {
    catalog
        .units()
        .map(|unit| UnitReport {
            name: unit.name().to_string(),
            kind: match unit.manifest().kind {
                UnitKind::Package(_) => "package".to_string(),
                UnitKind::Library => "library".to_string(),
            },
            root: unit.unit_root().display().to_string(),
            code_root: unit.code_root().display().to_string(),
            library: unit.is_library().then(|| unit.library_name().to_string()),
            lib_deps: unit
                .manifest()
                .lib_deps
                .iter()
                .map(|dep| dep.as_str().to_string())
                .collect(),
            event_deps: unit
                .manifest()
                .event_deps
                .iter()
                .map(|dep| dep.as_str().to_string())
                .collect(),
            actual_lib_requires: actual_requires
                .get(unit.name())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            modules: unit.own_modules().keys().cloned().collect(),
            public_exports: unit.public_modules().keys().cloned().collect(),
        })
        .collect()
}

fn lib_edges(catalog: &UnitCatalog) -> Vec<EdgeReport> {
    catalog
        .units()
        .flat_map(|unit| {
            unit.manifest().lib_deps.iter().map(|dep| EdgeReport {
                from: unit.name().to_string(),
                to: dep.as_str().to_string(),
            })
        })
        .collect()
}

fn event_edges(catalog: &UnitCatalog) -> Vec<EdgeReport> {
    catalog
        .units()
        .flat_map(|unit| {
            unit.manifest().event_deps.iter().map(|dep| EdgeReport {
                from: unit.name().to_string(),
                to: dep.as_str().to_string(),
            })
        })
        .collect()
}

fn print_human(report: &DepsReport) {
    println!("fkst deps: {}", if report.ok { "PASS" } else { "FAIL" });
    println!("workspace: {}", report.workspace_root);
    println!("units:");
    for unit in &report.units {
        println!(
            "  {} ({}) root={} code_root={}",
            unit.name, unit.kind, unit.root, unit.code_root
        );
        if !unit.lib_deps.is_empty() {
            println!("    lib_deps: {}", unit.lib_deps.join(", "));
        }
        if !unit.event_deps.is_empty() {
            println!("    event_deps: {}", unit.event_deps.join(", "));
        }
        if !unit.public_exports.is_empty() {
            println!("    public_exports: {}", unit.public_exports.join(", "));
        }
    }
    println!("lib_deps edges:");
    if report.lib_edges.is_empty() {
        println!("  (none)");
    } else {
        for edge in &report.lib_edges {
            println!("  {} -> {}", edge.from, edge.to);
        }
    }
    println!("event_deps edges:");
    if report.event_edges.is_empty() {
        println!("  (none)");
    } else {
        for edge in &report.event_edges {
            println!("  {} -> {}", edge.from, edge.to);
        }
    }
    if !report.failures.is_empty() {
        println!("failures:");
        for diagnostic in &report.failures {
            println!("  [{}] {}", diagnostic.kind, diagnostic.message);
        }
    }
    if !report.warnings.is_empty() {
        println!("warnings:");
        for diagnostic in &report.warnings {
            println!("  [{}] {}", diagnostic.kind, diagnostic.message);
        }
    }
}

fn canonical_dir(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {label} {}", path.display()))?;
    if !canonical.is_dir() {
        anyhow::bail!("{label} is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

use crate::path_resolver::PackageRoots;
use crate::supervise;
use anyhow::{Context, Result};
use fkst_common::runtime_layout::RUNTIME_ROOT_ENV;
use fkst_common::validation::{validate_with_scope, ValidationScope};
use fkst_common::RuntimeKind;
use serde::Serialize;
use std::path::PathBuf;

use crate::declarative_conformance::DeclarativeRulePack;

pub(crate) struct HostConformanceOptions {
    pub(crate) roots: PackageRoots,
    pub(crate) config: Option<HostConformanceConfig>,
}

pub(crate) struct HostConformanceConfig {
    path: PathBuf,
}

impl HostConformanceConfig {
    pub(crate) fn load(path: PathBuf) -> Result<Self> {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read conformance config {}", path.display()))?;
        let _config: toml::Value = toml::from_str(&raw)
            .with_context(|| format!("parse conformance config {}", path.display()))?;
        Ok(Self { path })
    }
}

pub(crate) struct HostConformanceSuite {
    options: HostConformanceOptions,
}

pub(crate) struct HostCheck {
    pack: String,
    id: String,
    package: Option<String>,
    status: CheckStatus,
    message: String,
}

enum CheckStatus {
    Pass,
    Fail,
}

impl HostConformanceSuite {
    pub(crate) fn new(options: HostConformanceOptions) -> Self {
        Self { options }
    }

    pub(crate) fn run(&self) -> Result<i32> {
        let context = ConformanceContext {
            options: &self.options,
        };
        let registry = RulePackRegistry::from_options(&self.options);
        let checks = registry.run(&context);
        let report = ConformanceReport::from_checks(registry.pack_count(), &checks);

        for check in &checks {
            check.print();
        }
        println!("{}", serde_json::to_string(&report)?);

        Ok(if report.ok { 0 } else { 1 })
    }
}

pub(crate) struct ConformanceContext<'a> {
    options: &'a HostConformanceOptions,
}

pub(crate) trait RulePack {
    fn name(&self) -> &str;
    fn run(&self, context: &ConformanceContext<'_>) -> Vec<HostCheck>;
}

struct RulePackRegistry {
    packs: Vec<Box<dyn RulePack>>,
}

impl RulePackRegistry {
    fn from_options(options: &HostConformanceOptions) -> Self {
        let mut registry = Self { packs: Vec::new() };
        registry.register(Box::new(EngineRulePack));
        for graph_root in options.roots.graph_roots() {
            if let Some(pack) = DeclarativeRulePack::from_graph_root(&graph_root) {
                registry.register(Box::new(pack));
            }
        }
        if let Some(config) = &options.config {
            let _config_path = &config.path;
            // Future rule packs, including a source-ratchet pack that migrates
            // external repository ratchets, should be selected from this parsed
            // config seam. The keystone intentionally registers only engine.
        }
        registry
    }

    fn register(&mut self, pack: Box<dyn RulePack>) {
        self.packs.push(pack);
    }

    fn pack_count(&self) -> usize {
        self.packs.len()
    }

    fn run(&self, context: &ConformanceContext<'_>) -> Vec<HostCheck> {
        let mut checks = Vec::new();

        for pack in &self.packs {
            let mut pack_checks = pack.run(context);
            for check in &mut pack_checks {
                check.pack = pack.name().to_string();
            }
            checks.extend(pack_checks);
        }

        checks
    }
}

struct EngineRulePack;

impl RulePack for EngineRulePack {
    fn name(&self) -> &str {
        "engine"
    }

    fn run(&self, context: &ConformanceContext<'_>) -> Vec<HostCheck> {
        let mut checks = Vec::new();

        checks.push(self.check_runtime_layout(context));
        checks.push(self.check_project_layout(context));
        checks.push(self.check_locale_catalogs(context));

        let graph_result = supervise::load_host_graph_for_conformance(&context.options.roots);
        let graph_available = match &graph_result {
            Ok(cfg) => {
                checks.push(HostCheck::pass(
                    "graph-scan",
                    format!(
                        "loaded {} departments, {} raisers, {} queues",
                        cfg.department.len(),
                        cfg.raiser.len(),
                        cfg.queue.len()
                    ),
                ));
                checks.push(self.check_department_non_empty(cfg));
                checks.push(self.check_schema_validation(context, cfg));
                true
            }
            Err(err) => {
                checks.push(HostCheck::fail(
                    "graph-scan",
                    format!("graph scan failed: {err:#}"),
                ));
                false
            }
        };

        if !graph_available {
            checks.push(HostCheck::fail(
                "department-non-empty",
                "graph scan did not produce a department set".to_string(),
            ));
            checks.push(HostCheck::fail(
                "schema-validation",
                "graph scan did not produce a schema candidate".to_string(),
            ));
        }

        checks
    }
}

impl EngineRulePack {
    fn check_runtime_layout(&self, context: &ConformanceContext<'_>) -> HostCheck {
        if std::env::var_os(RUNTIME_ROOT_ENV)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            return HostCheck::pass(
                "runtime-layout",
                format!("{RUNTIME_ROOT_ENV} not set; runtime scratch unused by conformance"),
            );
        }

        match crate::runtime_context::layout_from_host_root(context.options.roots.host_root())
            .and_then(|layout| {
                layout.runtime_dir(RuntimeKind::Worktrees);
                layout.runtime_dir(RuntimeKind::CodexPermits);
                layout.runtime_dir(RuntimeKind::Locks);
                layout.runtime_dir(RuntimeKind::Logs);
                layout.runtime_dir(RuntimeKind::Marks);
                layout.runtime_dir(RuntimeKind::Cache);
                Ok(layout)
            }) {
            Ok(layout) => HostCheck::pass(
                "runtime-layout",
                format!("runtime root accepted: {}", layout.runtime_root().display()),
            ),
            Err(err) => {
                HostCheck::fail("runtime-layout", format!("runtime layout failed: {err:#}"))
            }
        }
    }

    fn check_project_layout(&self, context: &ConformanceContext<'_>) -> HostCheck {
        let departments = context.options.roots.host_root().join("departments");
        if !departments.exists() {
            return HostCheck::pass(
                "project-layout",
                format!(
                    "host departments directory absent: {}",
                    departments.display()
                ),
            );
        }
        match std::fs::read_dir(&departments)
            .with_context(|| format!("read {}", departments.display()))
        {
            Ok(_) => HostCheck::pass(
                "project-layout",
                format!(
                    "host departments directory readable: {}",
                    departments.display()
                ),
            ),
            Err(err) => HostCheck::fail("project-layout", format!("{err:#}")),
        }
    }

    fn check_locale_catalogs(&self, context: &ConformanceContext<'_>) -> HostCheck {
        let mut checked = 0usize;
        for graph_root in context.options.roots.graph_roots() {
            if let Err(err) = crate::sdk_i18n::validate_graph_root_catalogs(&graph_root.root)
                .with_context(|| {
                    format!(
                        "validate locale catalogs for namespace `{}` at {}",
                        graph_root.namespace,
                        graph_root.root.display()
                    )
                })
            {
                return HostCheck::fail_for_package(
                    "locale-catalogs",
                    graph_root.namespace,
                    format!("{err:#}"),
                );
            }
            if graph_root.root.join("locales").exists() {
                checked += 1;
            }
        }
        HostCheck::pass(
            "locale-catalogs",
            format!("validated locale catalogs for {checked} graph roots"),
        )
    }

    fn check_department_non_empty(&self, cfg: &fkst_common::config::Config) -> HostCheck {
        if cfg.department.is_empty() {
            HostCheck::fail(
                "department-non-empty",
                "host graph contains no departments".to_string(),
            )
        } else {
            HostCheck::pass(
                "department-non-empty",
                format!("host graph contains {} departments", cfg.department.len()),
            )
        }
    }

    fn check_schema_validation(
        &self,
        context: &ConformanceContext<'_>,
        cfg: &fkst_common::config::Config,
    ) -> HostCheck {
        let scope = if context.options.roots.is_single_folded_graph_root() {
            ValidationScope::PartialGraph
        } else {
            ValidationScope::ClosedWorld
        };
        match validate_with_scope(cfg, context.options.roots.host_root(), scope) {
            Ok(warnings) => {
                if warnings.is_empty() {
                    HostCheck::pass("schema-validation", "schema validation passed".to_string())
                } else {
                    HostCheck::pass(
                        "schema-validation",
                        format!("schema validation passed with {} warnings", warnings.len()),
                    )
                }
            }
            Err(err) => HostCheck::fail("schema-validation", format!("{err}")),
        }
    }
}

impl HostCheck {
    pub(crate) fn pass(id: impl Into<String>, message: String) -> Self {
        Self {
            pack: "unregistered".to_string(),
            id: id.into(),
            package: None,
            status: CheckStatus::Pass,
            message,
        }
    }

    pub(crate) fn fail(id: impl Into<String>, message: String) -> Self {
        Self {
            pack: "unregistered".to_string(),
            id: id.into(),
            package: None,
            status: CheckStatus::Fail,
            message,
        }
    }

    pub(crate) fn pass_for_package(
        id: impl Into<String>,
        package: impl Into<String>,
        message: String,
    ) -> Self {
        Self {
            pack: "unregistered".to_string(),
            id: id.into(),
            package: Some(package.into()),
            status: CheckStatus::Pass,
            message,
        }
    }

    pub(crate) fn fail_for_package(
        id: impl Into<String>,
        package: impl Into<String>,
        message: String,
    ) -> Self {
        Self {
            pack: "unregistered".to_string(),
            id: id.into(),
            package: Some(package.into()),
            status: CheckStatus::Fail,
            message,
        }
    }

    fn print(&self) {
        let status = match self.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Fail => "FAIL",
        };
        println!("{status} {} {}", self.id, self.message);
    }
}

#[derive(Debug, Serialize)]
struct ConformanceReport {
    ok: bool,
    violations: Vec<ConformanceViolation>,
    counts: ConformanceCounts,
}

#[derive(Debug, Serialize)]
struct ConformanceViolation {
    rule: String,
    package: Option<String>,
    detail: String,
}

#[derive(Debug, Serialize)]
struct ConformanceCounts {
    packs: usize,
    checks: usize,
    passed: usize,
    failed: usize,
}

impl ConformanceReport {
    fn from_checks(packs: usize, checks: &[HostCheck]) -> Self {
        let violations = checks
            .iter()
            .filter(|check| matches!(check.status, CheckStatus::Fail))
            .map(|check| ConformanceViolation {
                rule: format!("{}.{}", check.pack, check.id),
                package: check.package.clone(),
                detail: check.message.clone(),
            })
            .collect::<Vec<_>>();
        let failed = violations.len();
        let checks_count = checks.len();
        Self {
            ok: failed == 0,
            violations,
            counts: ConformanceCounts {
                packs,
                checks: checks_count,
                passed: checks_count - failed,
                failed,
            },
        }
    }
}

pub(crate) fn run(options: HostConformanceOptions) -> Result<i32> {
    HostConformanceSuite::new(options).run()
}

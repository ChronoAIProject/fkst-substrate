use crate::path_resolver::PackageRoots;
use crate::supervise;
use anyhow::{Context, Result};
use fkst_common::validation::validate;
use fkst_common::RuntimeKind;

pub(crate) struct HostConformanceOptions {
    pub(crate) roots: PackageRoots,
}

pub(crate) struct HostConformanceSuite {
    options: HostConformanceOptions,
}

struct HostCheck {
    id: &'static str,
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
        let mut checks = Vec::new();

        checks.push(self.check_runtime_layout());
        checks.push(self.check_project_layout());

        let graph_result = supervise::load_host_graph_for_conformance(&self.options.roots);
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
                checks.push(self.check_schema_validation(cfg));
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

        let mut failed = false;
        for check in checks {
            if matches!(check.status, CheckStatus::Fail) {
                failed = true;
            }
            check.print();
        }

        Ok(if failed { 1 } else { 0 })
    }

    fn check_runtime_layout(&self) -> HostCheck {
        match crate::runtime_context::layout_from_host_root(self.options.roots.host_root())
            .and_then(|layout| {
                layout.runtime_dir(RuntimeKind::Worktrees);
                layout.runtime_dir(RuntimeKind::CodexPermits);
                layout.runtime_dir(RuntimeKind::Locks);
                layout.runtime_dir(RuntimeKind::Logs);
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

    fn check_project_layout(&self) -> HostCheck {
        let departments = self.options.roots.host_root().join("departments");
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

    fn check_schema_validation(&self, cfg: &fkst_common::config::Config) -> HostCheck {
        match validate(cfg, self.options.roots.host_root()) {
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
    fn pass(id: &'static str, message: String) -> Self {
        Self {
            id,
            status: CheckStatus::Pass,
            message,
        }
    }

    fn fail(id: &'static str, message: String) -> Self {
        Self {
            id,
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

pub(crate) fn run(options: HostConformanceOptions) -> Result<i32> {
    HostConformanceSuite::new(options).run()
}

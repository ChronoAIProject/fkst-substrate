//! Schema validation on config load. Refuse-to-start on any violation.

use crate::config::{Config, RaiserDecl};
use crate::error::FkstError;

/// Validate cross-references and per-decl invariants.
///
/// Rules:
/// - every queue name referenced by raiser.produces or department.{consumes,produces}
///   must be declared in [queue.*]
/// - every queue must have at least one producer OR consumer (no isolated queues)
/// - queues default to one active consumer unless `fanout = true`
/// - queues with only producers or only consumers emit startup warnings
/// - every department's `lua` path must exist on disk
/// - queue capacity > 0
/// - department timeout is parseable (we just check non-empty + ends with s/m/h)
/// - duplicate raiser → same name impossible (HashMap), so skip
pub fn validate(cfg: &Config, project_root: &std::path::Path) -> Result<Vec<String>, FkstError> {
    if cfg.limits.global_codex_processes == 0 {
        return Err(FkstError::Schema(
            "limits.global_codex_processes must be > 0".to_string(),
        ));
    }

    // Build set of declared queue names.
    let queues: std::collections::HashSet<&String> = cfg.queue.keys().collect();

    // Check every queue capacity > 0.
    for (name, q) in &cfg.queue {
        if q.capacity == 0 {
            return Err(FkstError::Schema(format!(
                "queue '{}' has capacity 0 (must be > 0)",
                name
            )));
        }
    }

    // Check every raiser.produces references a declared queue.
    for (name, raiser) in &cfg.raiser {
        let produces = match raiser {
            RaiserDecl::Cron { produces, .. } => produces,
            RaiserDecl::FileWatch { produces, .. } => produces,
        };
        if !queues.contains(produces) {
            return Err(FkstError::Schema(format!(
                "raiser '{}' produces queue '{}' which is not declared in [queue.*]",
                name, produces
            )));
        }
    }

    // Check every department's consumes/produces references declared queues.
    for (name, dept) in &cfg.department {
        for q in &dept.consumes {
            if !queues.contains(q) {
                return Err(FkstError::Schema(format!(
                    "department '{}' consumes queue '{}' which is not declared",
                    name, q
                )));
            }
        }
        for q in &dept.produces {
            if !queues.contains(q) {
                return Err(FkstError::Schema(format!(
                    "department '{}' produces queue '{}' which is not declared",
                    name, q
                )));
            }
        }
        let lua_full = project_root.join(&dept.lua);
        if !lua_full.is_file() {
            return Err(FkstError::Schema(format!(
                "department '{}' lua file '{}' does not exist (resolved: {})",
                name,
                dept.lua.display(),
                lua_full.display()
            )));
        }
        if !(dept.timeout.ends_with('s')
            || dept.timeout.ends_with('m')
            || dept.timeout.ends_with('h'))
        {
            return Err(FkstError::Schema(format!(
                "department '{}' timeout '{}' must end with s/m/h",
                name, dept.timeout
            )));
        }
    }

    let mut warnings = Vec::new();
    let mut queue_names: Vec<&String> = queues.iter().copied().collect();
    queue_names.sort();

    // Check every queue has at least one producer or consumer.
    for qname in queue_names {
        let producers = queue_producers(cfg, qname);
        let consumers = queue_consumers(cfg, qname);
        if producers.is_empty() && consumers.is_empty() {
            return Err(FkstError::Schema(format!(
                "queue '{}' has no producer and no consumer (isolated)",
                qname
            )));
        }
        if !producers.is_empty() && consumers.is_empty() {
            warnings.push(format!(
                "queue '{}' is produced by {} but has no consumer",
                qname,
                producers.join(", ")
            ));
        }
        if producers.is_empty() && !consumers.is_empty() {
            warnings.push(format!(
                "queue '{}' is consumed by {} but has no producer",
                qname,
                consumers.join(", ")
            ));
        }
        validate_queue_contract(cfg, qname, &consumers)?;
    }

    Ok(warnings)
}

// Startup validation enforces each queue contract in one place; queues are
// single active consumer unless Department M.spec declares fanout.
fn validate_queue_contract(
    cfg: &Config,
    qname: &str,
    consumers: &[String],
) -> Result<(), FkstError> {
    if queue_is_fanout(cfg, qname) {
        return Ok(());
    }

    if consumers.len() > 1 {
        return Err(FkstError::Schema(format!(
            "queue '{}' has multiple consumers ({}) but is not declared fanout",
            qname,
            consumers.join(", ")
        )));
    }

    let feedback_depts = queue_feedback_departments(cfg, qname);
    if !feedback_depts.is_empty() {
        return Err(FkstError::Schema(format!(
            "queue '{}' is consumed and produced by {} but is not declared fanout",
            qname,
            feedback_depts.join(", ")
        )));
    }

    Ok(())
}

fn queue_is_fanout(cfg: &Config, qname: &str) -> bool {
    cfg.queue.get(qname).is_some_and(|queue| queue.fanout)
}

fn queue_producers(cfg: &Config, qname: &str) -> Vec<String> {
    let mut producers = Vec::new();
    for (name, raiser) in &cfg.raiser {
        let produces = match raiser {
            RaiserDecl::Cron { produces, .. } => produces,
            RaiserDecl::FileWatch { produces, .. } => produces,
        };
        if produces == qname {
            producers.push(format!("raiser '{}'", name));
        }
    }
    for (name, dept) in &cfg.department {
        if dept.produces.iter().any(|q| q == qname) {
            producers.push(format!("department '{}'", name));
        }
    }
    producers.sort();
    producers
}

fn queue_consumers(cfg: &Config, qname: &str) -> Vec<String> {
    let mut consumers = Vec::new();
    for (name, dept) in &cfg.department {
        if dept.consumes.iter().any(|q| q == qname) {
            consumers.push(format!("department '{}'", name));
        }
    }
    consumers.sort();
    consumers
}

// Same-queue feedback is accepted only when the queue contract is explicitly
// fanout.
fn queue_feedback_departments(cfg: &Config, qname: &str) -> Vec<String> {
    let mut departments = Vec::new();
    for (name, dept) in &cfg.department {
        let consumes = dept.consumes.iter().any(|q| q == qname);
        let produces = dept.produces.iter().any(|q| q == qname);
        if consumes && produces {
            departments.push(format!("department '{}'", name));
        }
    }
    departments.sort();
    departments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DepartmentDecl, LimitsDecl, QueueDecl};
    use tempfile::tempdir;

    fn touch(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = root.join(name);
        std::fs::write(&path, "-- test\n").unwrap();
        path
    }

    fn cfg_minimal(lua: &std::path::Path) -> Config {
        let mut cfg = Config {
            queue: Default::default(),
            raiser: Default::default(),
            department: Default::default(),
            limits: LimitsDecl {
                global_codex_processes: 1,
            },
        };
        cfg.queue.insert(
            "tick".into(),
            QueueDecl {
                capacity: 10,
                fanout: false,
            },
        );
        cfg.raiser.insert(
            "timer".into(),
            RaiserDecl::Cron {
                interval: "10s".into(),
                produces: "tick".into(),
            },
        );
        cfg.department.insert(
            "d".into(),
            DepartmentDecl {
                lua: lua.file_name().unwrap().into(),
                consumes: vec!["tick".into()],
                produces: Vec::new(),
                timeout: "30s".into(),
            },
        );
        cfg
    }

    #[test]
    fn valid_config_passes() {
        let tmp = tempdir().unwrap();
        let lua = touch(tmp.path(), "d.lua");
        let cfg = cfg_minimal(&lua);
        let warnings = validate(&cfg, tmp.path()).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn unknown_produces_queue_rejected() {
        let tmp = tempdir().unwrap();
        let lua = touch(tmp.path(), "d.lua");
        let mut cfg = cfg_minimal(&lua);
        cfg.raiser.insert(
            "bad".into(),
            RaiserDecl::Cron {
                interval: "10s".into(),
                produces: "nonexistent".into(),
            },
        );
        let e = validate(&cfg, tmp.path()).unwrap_err();
        assert!(e.to_string().contains("nonexistent"), "{}", e);
    }

    #[test]
    fn unknown_consumes_queue_rejected() {
        let tmp = tempdir().unwrap();
        let lua = touch(tmp.path(), "d.lua");
        let mut cfg = cfg_minimal(&lua);
        cfg.department
            .get_mut("d")
            .unwrap()
            .consumes
            .push("ghost".into());
        let e = validate(&cfg, tmp.path()).unwrap_err();
        assert!(e.to_string().contains("ghost"), "{}", e);
    }

    #[test]
    fn missing_lua_file_rejected() {
        let tmp = tempdir().unwrap();
        let lua = tmp.path().join("does_not_exist.lua");
        let cfg = cfg_minimal(&lua);
        let e = validate(&cfg, tmp.path()).unwrap_err();
        assert!(e.to_string().contains("does not exist"), "{}", e);
    }

    #[test]
    fn capacity_zero_rejected() {
        let tmp = tempdir().unwrap();
        let lua = touch(tmp.path(), "d.lua");
        let mut cfg = cfg_minimal(&lua);
        cfg.queue.get_mut("tick").unwrap().capacity = 0;
        let e = validate(&cfg, tmp.path()).unwrap_err();
        assert!(e.to_string().contains("capacity 0"), "{}", e);
    }

    #[test]
    fn bad_timeout_rejected() {
        let tmp = tempdir().unwrap();
        let lua = touch(tmp.path(), "d.lua");
        let mut cfg = cfg_minimal(&lua);
        cfg.department.get_mut("d").unwrap().timeout = "30x".into();
        let e = validate(&cfg, tmp.path()).unwrap_err();
        assert!(e.to_string().contains("timeout"), "{}", e);
    }

    #[test]
    fn isolated_queue_rejected() {
        let tmp = tempdir().unwrap();
        let lua = touch(tmp.path(), "d.lua");
        let mut cfg = cfg_minimal(&lua);
        cfg.queue.insert(
            "orphan".into(),
            QueueDecl {
                capacity: 10,
                fanout: false,
            },
        );
        let e = validate(&cfg, tmp.path()).unwrap_err();
        assert!(e.to_string().contains("orphan"), "{}", e);
    }

    #[test]
    fn producer_without_consumer_warns() {
        let tmp = tempdir().unwrap();
        let lua = touch(tmp.path(), "d.lua");
        let mut cfg = cfg_minimal(&lua);
        cfg.queue.insert(
            "produced_only".into(),
            QueueDecl {
                capacity: 10,
                fanout: false,
            },
        );
        cfg.department
            .get_mut("d")
            .unwrap()
            .produces
            .push("produced_only".into());

        let warnings = validate(&cfg, tmp.path()).unwrap();

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("produced_only"), "{warnings:?}");
        assert!(warnings[0].contains("department 'd'"), "{warnings:?}");
        assert!(warnings[0].contains("no consumer"), "{warnings:?}");
    }

    #[test]
    fn consumer_without_producer_warns() {
        let tmp = tempdir().unwrap();
        let lua = touch(tmp.path(), "d.lua");
        let mut cfg = cfg_minimal(&lua);
        cfg.queue.insert(
            "consumed_only".into(),
            QueueDecl {
                capacity: 10,
                fanout: false,
            },
        );
        cfg.department
            .get_mut("d")
            .unwrap()
            .consumes
            .push("consumed_only".into());

        let warnings = validate(&cfg, tmp.path()).unwrap();

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("consumed_only"), "{warnings:?}");
        assert!(warnings[0].contains("department 'd'"), "{warnings:?}");
        assert!(warnings[0].contains("no producer"), "{warnings:?}");
    }
}

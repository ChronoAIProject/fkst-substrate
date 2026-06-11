//! SDK: composed graph introspection.

use fkst_common::config::{Config, RaiserDecl, RetryDecl};
use mlua::{Lua, Result};
use serde::Serialize;
use std::sync::{Arc, Mutex};

use crate::path_resolver::PackageRoots;

#[derive(Clone)]
struct GraphJsonState {
    roots: PackageRoots,
    cached: Arc<Mutex<Option<String>>>,
}

pub(crate) fn register(lua: &Lua, roots: Option<PackageRoots>) -> Result<()> {
    let state = roots.map(|roots| GraphJsonState {
        roots,
        cached: Arc::new(Mutex::new(None)),
    });
    lua.globals().set(
        "graph_json",
        lua.create_function(move |_, ()| {
            let Some(state) = &state else {
                return Err(mlua::Error::external(
                    "graph_json unavailable without composed graph roots",
                ));
            };
            state.graph_json()
        })?,
    )?;
    Ok(())
}

impl GraphJsonState {
    fn graph_json(&self) -> Result<String> {
        let mut cached = self
            .cached
            .lock()
            .map_err(|_| mlua::Error::external("graph_json cache poisoned"))?;
        if let Some(json) = &*cached {
            return Ok(json.clone());
        }

        let config = crate::supervise::load_host_graph_for_conformance(&self.roots)
            .map_err(mlua::Error::external)?;
        fkst_common::validation::validate(&config, self.roots.host_root())
            .map_err(mlua::Error::external)?;
        let graph = graph_snapshot(&config);
        let json = serde_json::to_string(&graph).map_err(mlua::Error::external)?;
        *cached = Some(json.clone());
        Ok(json)
    }
}

#[derive(Debug, Serialize)]
struct GraphSnapshot {
    schema: &'static str,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GraphNode {
    Queue {
        id: String,
        name: String,
        package: String,
        fanout: bool,
    },
    Raiser {
        id: String,
        name: String,
        package: String,
        source: RaiserSource,
    },
    Department {
        id: String,
        name: String,
        package: String,
        consumes: Vec<String>,
        produces: Vec<String>,
        ephemeral: Vec<String>,
        stall_window: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        retry: Option<RetryDecl>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RaiserSource {
    Cron { interval: String },
    FileWatch { glob: String },
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
struct GraphEdge {
    from: String,
    to: String,
    relation: GraphEdgeRelation,
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum GraphEdgeRelation {
    Raises,
    Consumes,
    Produces,
}

fn graph_snapshot(config: &Config) -> GraphSnapshot {
    let mut nodes = Vec::new();
    for (id, queue) in &config.queue {
        let (package, name) = split_package_name(id);
        nodes.push(GraphNode::Queue {
            id: node_id("queue", id),
            name,
            package,
            fanout: queue.fanout,
        });
    }
    for (id, raiser) in &config.raiser {
        let (package, name) = split_package_name(id);
        nodes.push(GraphNode::Raiser {
            id: node_id("raiser", id),
            name,
            package,
            source: raiser_source(raiser),
        });
    }
    for (id, department) in &config.department {
        let (package, name) = split_package_name(id);
        nodes.push(GraphNode::Department {
            id: node_id("department", id),
            name,
            package,
            consumes: department.consumes.clone(),
            produces: department.produces.clone(),
            ephemeral: department.ephemeral.clone(),
            stall_window: department.stall_window.clone(),
            retry: department.retry.clone(),
        });
    }
    nodes.sort_by(|left, right| node_sort_key(left).cmp(&node_sort_key(right)));

    let mut edges = Vec::new();
    for (id, raiser) in &config.raiser {
        edges.push(GraphEdge {
            from: node_id("raiser", id),
            to: node_id("queue", raiser_produces(raiser)),
            relation: GraphEdgeRelation::Raises,
        });
    }
    for (id, department) in &config.department {
        for queue in &department.consumes {
            edges.push(GraphEdge {
                from: node_id("queue", queue),
                to: node_id("department", id),
                relation: GraphEdgeRelation::Consumes,
            });
        }
        for queue in &department.produces {
            edges.push(GraphEdge {
                from: node_id("department", id),
                to: node_id("queue", queue),
                relation: GraphEdgeRelation::Produces,
            });
        }
    }
    edges.sort();

    GraphSnapshot {
        schema: "fkst.graph.v1",
        nodes,
        edges,
    }
}

fn node_sort_key(node: &GraphNode) -> (u8, &str) {
    match node {
        GraphNode::Raiser { id, .. } => (0, id),
        GraphNode::Queue { id, .. } => (1, id),
        GraphNode::Department { id, .. } => (2, id),
    }
}

fn raiser_source(raiser: &RaiserDecl) -> RaiserSource {
    match raiser {
        RaiserDecl::Cron { interval, .. } => RaiserSource::Cron {
            interval: interval.clone(),
        },
        RaiserDecl::FileWatch { glob, .. } => RaiserSource::FileWatch { glob: glob.clone() },
    }
}

fn raiser_produces(raiser: &RaiserDecl) -> &str {
    match raiser {
        RaiserDecl::Cron { produces, .. } => produces,
        RaiserDecl::FileWatch { produces, .. } => produces,
    }
}

fn split_package_name(id: &str) -> (String, String) {
    match id.split_once('.') {
        Some((package, name)) => (package.to_string(), name.to_string()),
        None => ("".to_string(), id.to_string()),
    }
}

fn node_id(kind: &str, canonical_name: &str) -> String {
    format!("{kind}:{canonical_name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fkst_common::config::{DepartmentDecl, LimitsDecl, QueueDecl};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn graph_snapshot_serializes_exact_topology_shape() {
        let mut queue = BTreeMap::new();
        queue.insert(
            "host.done".to_string(),
            QueueDecl {
                capacity: 32,
                fanout: false,
            },
        );
        queue.insert(
            "pkg.tick".to_string(),
            QueueDecl {
                capacity: 32,
                fanout: true,
            },
        );

        let mut raiser = BTreeMap::new();
        raiser.insert(
            "pkg.clock".to_string(),
            RaiserDecl::Cron {
                interval: "60s".to_string(),
                produces: "pkg.tick".to_string(),
            },
        );

        let mut department = BTreeMap::new();
        department.insert(
            "host.sink".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/sink/main.lua"),
                owner_root: PathBuf::from("/host"),
                owner_namespace: "host".to_string(),
                consumes: vec!["host.done".to_string()],
                produces: vec![],
                ephemeral: vec![],
                stall_window: "30s".to_string(),
                retry: None,
            },
        );
        department.insert(
            "pkg.worker".to_string(),
            DepartmentDecl {
                lua: PathBuf::from("departments/worker/main.lua"),
                owner_root: PathBuf::from("/pkg"),
                owner_namespace: "pkg".to_string(),
                consumes: vec!["pkg.tick".to_string()],
                produces: vec!["host.done".to_string()],
                ephemeral: vec!["pkg.tick".to_string()],
                stall_window: "45s".to_string(),
                retry: Some(RetryDecl {
                    max_attempts: 3,
                    base: "10s".to_string(),
                    cap: "1m".to_string(),
                }),
            },
        );

        let config = Config {
            queue,
            raiser,
            department,
            limits: LimitsDecl {
                global_codex_processes: 4,
            },
        };

        let actual: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&graph_snapshot(&config)).unwrap())
                .unwrap();

        assert_eq!(
            actual,
            json!({
                "schema": "fkst.graph.v1",
                "nodes": [
                    {
                        "kind": "raiser",
                        "id": "raiser:pkg.clock",
                        "name": "clock",
                        "package": "pkg",
                        "source": {
                            "type": "cron",
                            "interval": "60s"
                        }
                    },
                    {
                        "kind": "queue",
                        "id": "queue:host.done",
                        "name": "done",
                        "package": "host",
                        "fanout": false
                    },
                    {
                        "kind": "queue",
                        "id": "queue:pkg.tick",
                        "name": "tick",
                        "package": "pkg",
                        "fanout": true
                    },
                    {
                        "kind": "department",
                        "id": "department:host.sink",
                        "name": "sink",
                        "package": "host",
                        "consumes": ["host.done"],
                        "produces": [],
                        "ephemeral": [],
                        "stall_window": "30s"
                    },
                    {
                        "kind": "department",
                        "id": "department:pkg.worker",
                        "name": "worker",
                        "package": "pkg",
                        "consumes": ["pkg.tick"],
                        "produces": ["host.done"],
                        "ephemeral": ["pkg.tick"],
                        "stall_window": "45s",
                        "retry": {
                            "max_attempts": 3,
                            "base": "10s",
                            "cap": "1m"
                        }
                    }
                ],
                "edges": [
                    {
                        "from": "department:pkg.worker",
                        "to": "queue:host.done",
                        "relation": "produces"
                    },
                    {
                        "from": "queue:host.done",
                        "to": "department:host.sink",
                        "relation": "consumes"
                    },
                    {
                        "from": "queue:pkg.tick",
                        "to": "department:pkg.worker",
                        "relation": "consumes"
                    },
                    {
                        "from": "raiser:pkg.clock",
                        "to": "queue:pkg.tick",
                        "relation": "raises"
                    }
                ]
            })
        );
    }

    #[test]
    fn graph_json_requires_composed_roots() {
        let lua = Lua::new();
        register(&lua, None).unwrap();

        let err = lua
            .load("return graph_json()")
            .eval::<String>()
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("graph_json unavailable without composed graph roots"),
            "{err}"
        );
    }
}

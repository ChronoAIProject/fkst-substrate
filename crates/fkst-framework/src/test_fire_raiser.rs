use crate::external_command::MockCommandState;
use crate::lua_coverage::LuaCoverage;
use crate::path_resolver::PackageRoots;
use crate::supervise::event_fanout::Fanout;
use crate::supervise::source_runner::{cron_emission, file_watch_fixture_emission, SourceEmission};
use crate::test_department_env::DeptRunOptions;
use crate::test_runner::{run_department_core, DepartmentRunOutcome, TestRunCache};
use fkst_common::config::{Config, DepartmentDecl, RaiserDecl};
use mlua::{Lua, LuaSerdeExt, Table};
use serde_json::Value as JsonValue;
use std::path::{Path, PathBuf};

const TEST_CRON_SLOT_MS: u64 = 0;
const TEST_OBSERVED_AT_MS: u64 = 0;

pub(crate) fn register(
    lua: &Lua,
    cache: TestRunCache,
    roots: PackageRoots,
    owner_namespace: String,
    mock_commands: MockCommandState,
    coverage: Option<LuaCoverage>,
    test: &Table,
) -> mlua::Result<()> {
    // #1362 conformance evidence checks and package fixture migration are follow-ups;
    // this test-mode primitive only fires declared sources and returns trace evidence.
    test.set("fire_raiser", {
        lua.create_function(move |lua, (raiser_name, opts): (String, Option<Table>)| {
            fire_raiser(
                lua,
                &cache,
                &roots,
                &owner_namespace,
                mock_commands.clone(),
                &raiser_name,
                opts,
                coverage.clone(),
            )
        })?
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fire_raiser(
    lua: &Lua,
    cache: &TestRunCache,
    roots: &PackageRoots,
    owner_namespace: &str,
    mock_commands: MockCommandState,
    raiser_name: &str,
    opts: Option<Table>,
    coverage: Option<LuaCoverage>,
) -> mlua::Result<Table> {
    let opts = FireRaiserOptions::from_lua(roots.host_root(), opts)?;
    let cfg = crate::supervise::graph_scan::load_roots(roots).map_err(mlua::Error::external)?;
    let resolved_name = roots
        .name_resolver()
        .resolve(owner_namespace, raiser_name)
        .map_err(mlua::Error::external)?;
    let raiser = cfg
        .raiser
        .get(&resolved_name)
        .ok_or_else(|| mlua::Error::external(format!("raiser `{raiser_name}` is not declared")))?;
    let emission = source_emission(&resolved_name, raiser, roots.host_root(), &opts)
        .map_err(mlua::Error::external)?;
    let routed = routed_events(&cfg, &emission).map_err(mlua::Error::external)?;
    if routed.is_empty() {
        return Err(mlua::Error::external(format!(
            "raiser `{resolved_name}` produced queue `{}` with no consumer",
            emission.event.queue
        )));
    }

    let mut consumer_results = Vec::new();
    let mut raised = Vec::new();
    for delivery in &routed {
        let outcome = run_consumer(
            cache,
            roots,
            mock_commands.clone(),
            &delivery.dept,
            &delivery.event,
            coverage.clone(),
        )?;
        for (queue, payload) in &outcome.raises {
            raised.push(TraceRaised {
                consumer: delivery.dept_name.clone(),
                queue: queue.clone(),
                payload: payload.clone(),
            });
        }
        consumer_results.push(TraceConsumerResult {
            consumer: delivery.dept_name.clone(),
            exit_code: outcome.exit_code,
            error: outcome.error,
        });
    }

    trace_table(lua, emission, routed, consumer_results, raised)
}

fn source_emission(
    resolved_name: &str,
    raiser: &RaiserDecl,
    host_root: &Path,
    opts: &FireRaiserOptions,
) -> anyhow::Result<SourceEmission> {
    match raiser {
        RaiserDecl::Cron { produces, .. } => {
            if opts.fixture.is_some() {
                anyhow::bail!("cron fire_raiser does not accept a fixture");
            }
            Ok(cron_emission(
                resolved_name,
                produces,
                TEST_CRON_SLOT_MS,
                TEST_OBSERVED_AT_MS,
            ))
        }
        RaiserDecl::FileWatch { glob, produces } => {
            let fixture = opts
                .fixture
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("file_watch fire_raiser requires opts.fixture"))?;
            file_watch_fixture_emission(
                resolved_name,
                glob,
                host_root,
                produces,
                fixture,
                TEST_OBSERVED_AT_MS,
            )
        }
    }
}

fn routed_events(cfg: &Config, emission: &SourceEmission) -> anyhow::Result<Vec<RoutedEvent>> {
    let subscriptions = crate::supervise::route_subscriptions_for_test(cfg, &emission.event.queue)
        .into_iter()
        .filter_map(|sub| {
            cfg.department
                .get(&sub.dept)
                .map(|dept| (sub.dept, dept.clone()))
        })
        .collect::<Vec<_>>();
    if subscriptions.is_empty() {
        return Ok(Vec::new());
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    rt.block_on(async {
        let fanout = Fanout::new();
        let mut receivers = Vec::new();
        for (dept_name, dept) in &subscriptions {
            let rx = fanout.subscribe(&emission.event.queue, 1).await;
            receivers.push((dept_name.clone(), dept.clone(), rx));
        }
        fanout.send(&emission.event.queue, emission.event.clone())?;

        let mut routed = Vec::new();
        for (dept_name, dept, mut rx) in receivers {
            let event = rx.recv().await.ok_or_else(|| {
                anyhow::anyhow!("fanout route to department `{dept_name}` closed before delivery")
            })?;
            routed.push(RoutedEvent {
                dept_name,
                dept,
                event,
            });
        }
        Ok(routed)
    })
}

fn run_consumer(
    cache: &TestRunCache,
    roots: &PackageRoots,
    mock_commands: MockCommandState,
    dept: &DepartmentDecl,
    event: &fkst_common::Event,
    coverage: Option<LuaCoverage>,
) -> mlua::Result<DepartmentRunOutcome> {
    let lua_path = if dept.lua.is_absolute() {
        dept.lua.clone()
    } else {
        roots.host_root().join(&dept.lua)
    };
    run_department_core(
        cache,
        roots,
        &dept.owner_root,
        &dept.owner_namespace,
        mock_commands,
        &lua_path,
        serde_json::to_value(event).map_err(mlua::Error::external)?,
        DeptRunOptions::default_env(),
        coverage,
    )
}

fn trace_table(
    lua: &Lua,
    emission: SourceEmission,
    routed: Vec<RoutedEvent>,
    consumer_results: Vec<TraceConsumerResult>,
    raised: Vec<TraceRaised>,
) -> mlua::Result<Table> {
    let trace = lua.create_table()?;
    trace.set("source_payload", lua.to_value(&emission.event.payload)?)?;
    trace.set(
        "routed_to",
        strings_table(lua, routed.into_iter().map(|delivery| delivery.dept_name))?,
    )?;
    trace.set(
        "consumer_result",
        consumer_result_table(lua, &consumer_results)?,
    )?;
    trace.set("raised", raised_table(lua, raised)?)?;
    let source = lua.create_table()?;
    source.set("kind", source_kind_name(&emission.source.kind))?;
    source.set("reference", emission.source.reference)?;
    trace.set("source_ref", source)?;
    if let Some(cron_payload) = emission.cron_payload {
        trace.set("cron_payload", lua.to_value(&cron_payload)?)?;
    }
    Ok(trace)
}

fn strings_table(lua: &Lua, values: impl IntoIterator<Item = String>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (idx, value) in values.into_iter().enumerate() {
        table.set(idx + 1, value)?;
    }
    Ok(table)
}

fn consumer_result_table(lua: &Lua, results: &[TraceConsumerResult]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let aggregate_status = if results.iter().all(|result| result.exit_code == 0) {
        "accepted"
    } else {
        "error"
    };
    table.set("status", aggregate_status)?;
    if let Some(first_error) = results.iter().find_map(|result| result.error.as_deref()) {
        table.set("message", first_error)?;
    }
    let consumers = lua.create_table()?;
    for (idx, result) in results.iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set("consumer", result.consumer.clone())?;
        entry.set(
            "status",
            if result.exit_code == 0 {
                "accepted"
            } else {
                "error"
            },
        )?;
        entry.set("exit_code", result.exit_code)?;
        if let Some(error) = &result.error {
            entry.set("message", error.clone())?;
        }
        consumers.set(idx + 1, entry)?;
    }
    table.set("consumers", consumers)?;
    Ok(table)
}

fn raised_table(lua: &Lua, raised: Vec<TraceRaised>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (idx, entry) in raised.into_iter().enumerate() {
        let row = lua.create_table()?;
        row.set("consumer", entry.consumer)?;
        row.set("queue", entry.queue)?;
        row.set("payload", lua.to_value(&entry.payload)?)?;
        table.set(idx + 1, row)?;
    }
    Ok(table)
}

fn source_kind_name(kind: &crate::supervise::delivery_types::SourceKind) -> &'static str {
    match kind {
        crate::supervise::delivery_types::SourceKind::Cron => "cron",
        crate::supervise::delivery_types::SourceKind::File => "file_watch",
        crate::supervise::delivery_types::SourceKind::Git => "git",
        crate::supervise::delivery_types::SourceKind::External => "external",
    }
}

#[derive(Debug)]
struct FireRaiserOptions {
    fixture: Option<PathBuf>,
}

impl FireRaiserOptions {
    fn from_lua(host_root: &Path, opts: Option<Table>) -> mlua::Result<Self> {
        let Some(opts) = opts else {
            return Ok(Self { fixture: None });
        };
        let fixture = opts
            .get::<Option<String>>("fixture")?
            .map(|path| resolve_fixture_path(host_root, &path));
        Ok(Self { fixture })
    }
}

fn resolve_fixture_path(host_root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        host_root.join(path)
    }
}

struct TraceConsumerResult {
    consumer: String,
    exit_code: i32,
    error: Option<String>,
}

struct RoutedEvent {
    dept_name: String,
    dept: DepartmentDecl,
    event: fkst_common::Event,
}

struct TraceRaised {
    consumer: String,
    queue: String,
    payload: JsonValue,
}

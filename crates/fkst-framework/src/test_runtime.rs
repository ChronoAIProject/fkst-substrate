//! Deterministic graph runner for `fkst-framework test`.
//!
//! Lua tests call `fkst.test.run_graph(event_or_source, opts)` to publish one
//! initial event or declared source into the real production delivery router,
//! then drive durable deliveries to quiescence. Dispatch order is stable:
//! `(not_before_ms, queue, dept, delivery_id)`. The driver owns only ordering,
//! `max_steps` / `max_deliveries` caps, and trace shape; routing, durable
//! enqueue/lease/ack, retry handling, and raised-event republish stay in the
//! production supervise modules.

use crate::external_command::MockCommandState;
use crate::lua_coverage::LuaCoverage;
use crate::path_resolver::PackageRoots;
use crate::sdk_codex::RUNTIME_LOG_DIR_ENV;
use crate::sdk_observe::MockObserveState;
use crate::supervise::consumer::{finish_test_durable_record, TestDurableCompletion};
use crate::supervise::delivery_observe::{observe_snapshot, DeliveryObserveOptions};
use crate::supervise::delivery_router::{DeliveryRouter, PublishEnvelope};
use crate::supervise::delivery_store::DeliveryStore;
use crate::supervise::delivery_types::{DeliveryRecord, SourceKind, SourceRef};
use crate::supervise::event_fanout::Fanout;
use crate::supervise::journal::SupervisorJournal;
use crate::supervise::source_runner::{cron_emission, file_watch_fixture_emission};
use crate::test_department_env::DeptRunOptions;
use crate::test_runner::{run_department_core, TestRunCache};
use fkst_common::config::{Config, DepartmentDecl, RaiserDecl, RetryDecl};
use fkst_common::validation::{validate_with_scope, ValidationScope};
use fkst_common::Event;
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const TEST_OBSERVED_AT_MS: u64 = 0;
const TEST_LEASE_MS: u64 = 30_000;
const DEFAULT_MAX_STEPS: usize = 128;
const MAX_TRACE_DELIVERIES: usize = 10_000;

pub(crate) fn register(
    lua: &Lua,
    cache: TestRunCache,
    roots: PackageRoots,
    owner_namespace: String,
    mock_commands: MockCommandState,
    mock_observe: MockObserveState,
    coverage: Option<LuaCoverage>,
    test: &Table,
) -> mlua::Result<()> {
    test.set("run_graph", {
        lua.create_function(move |lua, (input, opts): (Value, Option<Table>)| {
            run_graph(
                lua,
                &cache,
                &roots,
                &owner_namespace,
                mock_commands.clone(),
                mock_observe.clone(),
                input,
                opts,
                coverage.clone(),
            )
        })?
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_graph(
    lua: &Lua,
    cache: &TestRunCache,
    roots: &PackageRoots,
    owner_namespace: &str,
    mock_commands: MockCommandState,
    mock_observe: MockObserveState,
    input: Value,
    opts: Option<Table>,
    coverage: Option<LuaCoverage>,
) -> mlua::Result<Table> {
    let opts = RunGraphOptions::from_lua(roots.host_root(), opts)?;
    let cfg = crate::supervise::graph_scan::load_roots(roots).map_err(mlua::Error::external)?;
    validate_with_scope(&cfg, roots.host_root(), ValidationScope::PartialGraph)
        .map_err(mlua::Error::external)?;
    let temp = tempfile::Builder::new()
        .prefix("fkst-test-runtime")
        .tempdir()
        .map_err(mlua::Error::external)?;
    let database = temp.path().join("delivery.redb");
    let runtime_log_dir = temp.path().join("logs");
    let store = Arc::new(DeliveryStore::open(&database).map_err(mlua::Error::external)?);
    let router = DeliveryRouter::new_with_clock(
        &cfg,
        Fanout::new(),
        Some(store.clone()),
        None,
        Arc::new(|| TEST_OBSERVED_AT_MS),
    );
    let initial = initial_emission(lua, roots, owner_namespace, &cfg, input, &opts)?;
    router
        .publish(PublishEnvelope {
            event: initial.event,
            source: initial.source,
            cron_payload: initial.cron_payload,
            derived: None,
        })
        .map_err(mlua::Error::external)?;

    let mut trace = RunGraphTrace::default();
    let journal = SupervisorJournal::disabled();
    for step_index in 0..opts.max_steps {
        let Some(record) = lease_next_record(&store, TEST_OBSERVED_AT_MS)? else {
            return quiescent_trace_table(lua, &store, trace);
        };
        let dept = cfg
            .department
            .get(&record.dept)
            .ok_or_else(|| mlua::Error::external(format!("unknown department `{}`", record.dept)))?
            .clone();
        let retry_policy = retry_policy(&dept).map_err(mlua::Error::external)?.clone();
        let outcome = run_record(
            cache,
            roots,
            mock_commands.clone(),
            mock_observe.clone(),
            &dept,
            &record,
            &runtime_log_dir,
            coverage.clone(),
        )?;
        let step = TraceStep {
            step: step_index + 1,
            delivery_id: record.delivery_id.clone(),
            queue: record.queue.clone(),
            consumer: record.dept.clone(),
            status: if outcome.exit_code == 0 {
                "accepted".to_string()
            } else {
                "error".to_string()
            },
            exit_code: outcome.exit_code,
            error: outcome.error.clone(),
            raises: outcome.raises.clone(),
        };
        finish_test_durable_record(
            &store,
            &router,
            retry_policy.as_ref(),
            record,
            TestDurableCompletion {
                exit_code: outcome.exit_code,
                error: outcome.error,
                raises: outcome
                    .raises
                    .iter()
                    .map(|raised| Event {
                        queue: raised.queue.clone(),
                        payload: raised.payload.clone(),
                        ts: TEST_OBSERVED_AT_MS,
                    })
                    .collect(),
            },
            &journal,
        )
        .map_err(mlua::Error::external)?;
        trace.steps.push(step);
        if final_state(&store)?.pending == 0 {
            return quiescent_trace_table(lua, &store, trace);
        }
    }

    let pending = final_state(&store)?;
    Err(mlua::Error::external(format!(
        "run_graph exceeded max_steps={} with pending={}",
        opts.max_steps, pending.pending
    )))
}

fn quiescent_trace_table(
    lua: &Lua,
    store: &DeliveryStore,
    mut trace: RunGraphTrace,
) -> mlua::Result<Table> {
    trace.status = "quiescent".to_string();
    trace.final_state = final_state(store)?;
    trace_table(lua, trace)
}

fn initial_emission(
    lua: &Lua,
    roots: &PackageRoots,
    owner_namespace: &str,
    cfg: &Config,
    input: Value,
    opts: &RunGraphOptions,
) -> mlua::Result<InitialEmission> {
    match input {
        Value::String(name) => source_input(roots, owner_namespace, cfg, &name.to_str()?, opts),
        Value::Table(table) => {
            if let Some(raiser) = table.get::<Option<String>>("raiser")? {
                source_input(roots, owner_namespace, cfg, &raiser, opts)
            } else if let Some(source) = table.get::<Option<String>>("source")? {
                source_input(roots, owner_namespace, cfg, &source, opts)
            } else {
                event_input(lua, roots, owner_namespace, table)
            }
        }
        other => Err(mlua::Error::external(format!(
            "run_graph input must be event table or source name, got {}",
            other.type_name()
        ))),
    }
}

fn source_input(
    roots: &PackageRoots,
    owner_namespace: &str,
    cfg: &Config,
    raw_name: &str,
    opts: &RunGraphOptions,
) -> mlua::Result<InitialEmission> {
    let resolved = roots
        .name_resolver()
        .resolve(owner_namespace, raw_name)
        .map_err(mlua::Error::external)?;
    let raiser = cfg
        .raiser
        .get(&resolved)
        .ok_or_else(|| mlua::Error::external(format!("raiser `{raw_name}` is not declared")))?;
    let emission = match raiser {
        RaiserDecl::Cron { produces, .. } => {
            if opts.fixture.is_some() {
                return Err(mlua::Error::external(
                    "cron run_graph source does not accept a fixture",
                ));
            }
            cron_emission(
                &resolved,
                produces,
                TEST_OBSERVED_AT_MS,
                TEST_OBSERVED_AT_MS,
            )
        }
        RaiserDecl::FileWatch { glob, produces } => {
            let fixture = opts.fixture.as_ref().ok_or_else(|| {
                mlua::Error::external("file_watch run_graph source requires opts.fixture")
            })?;
            file_watch_fixture_emission(
                &resolved,
                glob,
                roots.host_root(),
                produces,
                fixture,
                TEST_OBSERVED_AT_MS,
            )
            .map_err(mlua::Error::external)?
        }
    };
    Ok(InitialEmission {
        event: emission.event,
        source: Some(emission.source),
        cron_payload: emission.cron_payload,
    })
}

fn event_input(
    lua: &Lua,
    roots: &PackageRoots,
    owner_namespace: &str,
    table: Table,
) -> mlua::Result<InitialEmission> {
    let raw_queue: String = table.get("queue")?;
    let queue = roots
        .name_resolver()
        .resolve(owner_namespace, &raw_queue)
        .map_err(mlua::Error::external)?;
    let payload = match table.get::<Value>("payload")? {
        Value::Nil => JsonValue::Null,
        value => lua.from_value(value)?,
    };
    let source = match table.get::<Value>("source_ref")? {
        Value::Nil => None,
        value => Some(source_ref_from_json(lua.from_value(value)?)?),
    };
    let ts = table
        .get::<Option<u64>>("ts")?
        .unwrap_or(TEST_OBSERVED_AT_MS);
    Ok(InitialEmission {
        event: Event { queue, payload, ts },
        source,
        cron_payload: None,
    })
}

fn source_ref_from_json(value: JsonValue) -> mlua::Result<SourceRef> {
    #[derive(Deserialize)]
    struct RawSourceRef {
        kind: String,
        reference: String,
    }
    let raw: RawSourceRef = serde_json::from_value(value).map_err(mlua::Error::external)?;
    if raw.reference.is_empty() {
        return Err(mlua::Error::external(
            "source_ref.reference must not be empty",
        ));
    }
    let kind = match raw.kind.as_str() {
        "file" | "file_watch" => SourceKind::File,
        "cron" => SourceKind::Cron,
        "git" => SourceKind::Git,
        "external" => SourceKind::External,
        other => {
            return Err(mlua::Error::external(format!(
                "unsupported source_ref.kind `{other}`"
            )))
        }
    };
    Ok(SourceRef {
        kind,
        reference: raw.reference,
    })
}

fn lease_next_record(store: &DeliveryStore, now_ms: u64) -> mlua::Result<Option<DeliveryRecord>> {
    store
        .next_due_deterministic(now_ms, Duration::from_millis(TEST_LEASE_MS))
        .map_err(mlua::Error::external)
}

fn retry_policy(
    dept: &DepartmentDecl,
) -> anyhow::Result<Option<crate::supervise::delivery_types::RetryPolicy>> {
    dept.retry.as_ref().map(policy_from_decl).transpose()
}

fn policy_from_decl(
    decl: &RetryDecl,
) -> anyhow::Result<crate::supervise::delivery_types::RetryPolicy> {
    Ok(crate::supervise::delivery_types::RetryPolicy {
        max_attempts: decl.max_attempts,
        base: parse_duration(&decl.base)?,
        cap: parse_duration(&decl.cap)?,
    })
}

fn parse_duration(value: &str) -> anyhow::Result<Duration> {
    let Some(unit) = value.chars().last() else {
        anyhow::bail!("duration must not be empty");
    };
    let number = &value[..value.len().saturating_sub(1)];
    let amount: u64 = number.parse()?;
    let seconds = match unit {
        's' => amount,
        'm' => amount.saturating_mul(60),
        'h' => amount.saturating_mul(60 * 60),
        _ => anyhow::bail!("duration `{value}` must end with s/m/h"),
    };
    Ok(Duration::from_secs(seconds))
}

fn run_record(
    cache: &TestRunCache,
    roots: &PackageRoots,
    mock_commands: MockCommandState,
    mock_observe: MockObserveState,
    dept: &DepartmentDecl,
    record: &DeliveryRecord,
    runtime_log_dir: &Path,
    coverage: Option<LuaCoverage>,
) -> mlua::Result<RecordRunOutcome> {
    let lua_path = if dept.lua.is_absolute() {
        dept.lua.clone()
    } else {
        roots.host_root().join(&dept.lua)
    };
    let outcome = run_department_core(
        cache,
        roots,
        &dept.owner_root,
        &dept.owner_namespace,
        mock_commands,
        mock_observe,
        &lua_path,
        serde_json::to_value(Event {
            queue: record.queue.clone(),
            payload: record.payload.clone(),
            ts: record.observed_at_ms,
        })
        .map_err(mlua::Error::external)?,
        DeptRunOptions::default_env().with_env_var(
            RUNTIME_LOG_DIR_ENV,
            runtime_log_dir.to_string_lossy().into_owned(),
        ),
        coverage,
    )?;
    Ok(RecordRunOutcome {
        exit_code: outcome.exit_code,
        error: outcome.error,
        raises: outcome
            .raises
            .into_iter()
            .map(|(queue, payload)| TraceRaised { queue, payload })
            .collect(),
    })
}

fn final_state(store: &DeliveryStore) -> mlua::Result<FinalState> {
    let snapshot = observe_snapshot(
        store,
        Path::new("<test-runtime>"),
        Path::new("<test-runtime>/delivery.redb"),
        &DeliveryObserveOptions {
            now_ms: TEST_OBSERVED_AT_MS,
            limit: MAX_TRACE_DELIVERIES,
            since: None,
            dead_letter_page: None,
            current_subscriber_queues: None,
            projection: Default::default(),
        },
    )
    .map_err(mlua::Error::external)?;
    let pending = snapshot
        .queues
        .iter()
        .map(|queue| queue.depth)
        .sum::<usize>();
    Ok(FinalState {
        pending,
        deliveries: snapshot.deliveries.len(),
        dead_letters: snapshot.dead_letters.len(),
    })
}

fn trace_table(lua: &Lua, trace: RunGraphTrace) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("status", trace.status)?;
    let steps = lua.create_table()?;
    for (idx, step) in trace.steps.into_iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set("step", step.step)?;
        entry.set("delivery_id", step.delivery_id)?;
        entry.set("queue", step.queue)?;
        entry.set("consumer", step.consumer)?;
        entry.set("status", step.status)?;
        entry.set("exit_code", step.exit_code)?;
        if let Some(error) = step.error {
            entry.set("error", error)?;
        }
        entry.set("raises", raises_table(lua, step.raises)?)?;
        steps.set(idx + 1, entry)?;
    }
    table.set("steps", steps)?;
    table.set("final", final_table(lua, trace.final_state)?)?;
    Ok(table)
}

fn raises_table(lua: &Lua, raises: Vec<TraceRaised>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (idx, raised) in raises.into_iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set("queue", raised.queue)?;
        entry.set("payload", lua.to_value(&raised.payload)?)?;
        table.set(idx + 1, entry)?;
    }
    Ok(table)
}

fn final_table(lua: &Lua, state: FinalState) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("pending", state.pending)?;
    table.set("deliveries", state.deliveries)?;
    table.set("dead_letters", state.dead_letters)?;
    Ok(table)
}

#[derive(Debug)]
struct RunGraphOptions {
    max_steps: usize,
    fixture: Option<std::path::PathBuf>,
}

impl RunGraphOptions {
    fn from_lua(host_root: &Path, opts: Option<Table>) -> mlua::Result<Self> {
        let Some(opts) = opts else {
            return Ok(Self {
                max_steps: DEFAULT_MAX_STEPS,
                fixture: None,
            });
        };
        let max_steps_opt = opts.get::<Option<usize>>("max_steps")?;
        let max_deliveries_opt = opts.get::<Option<usize>>("max_deliveries")?;
        if max_steps_opt.is_some()
            && max_deliveries_opt.is_some()
            && max_steps_opt != max_deliveries_opt
        {
            return Err(mlua::Error::external(
                "run_graph max_steps and max_deliveries must match when both are set",
            ));
        }
        let max_steps = max_steps_opt
            .or(max_deliveries_opt)
            .unwrap_or(DEFAULT_MAX_STEPS);
        if max_steps == 0 {
            return Err(mlua::Error::external(
                "run_graph max_steps/max_deliveries must be > 0",
            ));
        }
        let fixture = opts
            .get::<Option<String>>("fixture")?
            .map(|path| host_root.join(path));
        Ok(Self { max_steps, fixture })
    }
}

struct InitialEmission {
    event: Event,
    source: Option<SourceRef>,
    cron_payload: Option<JsonValue>,
}

#[derive(Default)]
struct RunGraphTrace {
    status: String,
    steps: Vec<TraceStep>,
    final_state: FinalState,
}

struct TraceStep {
    step: usize,
    delivery_id: String,
    queue: String,
    consumer: String,
    status: String,
    exit_code: i32,
    error: Option<String>,
    raises: Vec<TraceRaised>,
}

struct RecordRunOutcome {
    exit_code: i32,
    error: Option<String>,
    raises: Vec<TraceRaised>,
}

#[derive(Clone)]
struct TraceRaised {
    queue: String,
    payload: JsonValue,
}

#[derive(Default)]
struct FinalState {
    pending: usize,
    deliveries: usize,
    dead_letters: usize,
}

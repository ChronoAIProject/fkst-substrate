//! SDK: `fkst.observe([opts]) -> table`.
//!
//! This is the in-process adapter for the same generic observe snapshot used by the
//! `fkst-framework observe --json` CLI. Packages interpret the returned engine facts;
//! this module must not encode package-specific idle, board, audit, or workflow meaning.

use fkst_common::DURABLE_ROOT_ENV;
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const OBSERVE_SECTIONS: &[&str] = &["queues", "errors", "events", "entities"];

#[derive(Clone, Default)]
pub(crate) struct MockObserveState {
    snapshot: Arc<Mutex<Option<JsonValue>>>,
}

impl MockObserveState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set(&self, snapshot: JsonValue) -> mlua::Result<()> {
        *self
            .snapshot
            .lock()
            .map_err(|_| mlua::Error::runtime("mock observe state lock poisoned"))? =
            Some(snapshot);
        Ok(())
    }

    pub(crate) fn reset(&self) -> mlua::Result<()> {
        *self
            .snapshot
            .lock()
            .map_err(|_| mlua::Error::runtime("mock observe state lock poisoned"))? = None;
        Ok(())
    }

    fn snapshot(&self) -> mlua::Result<Option<JsonValue>> {
        Ok(self
            .snapshot
            .lock()
            .map_err(|_| mlua::Error::runtime("mock observe state lock poisoned"))?
            .clone())
    }
}

pub(crate) fn register(lua: &Lua, mock_observe: Option<MockObserveState>) -> mlua::Result<()> {
    let fkst = fkst_table(lua)?;
    fkst.set("observe", {
        lua.create_function(move |lua, opts: Option<Table>| {
            let opts = ObserveSdkOptions::from_lua(opts)?;
            let snapshot = match &mock_observe {
                Some(mock) => match mock.snapshot()? {
                    Some(snapshot) => snapshot,
                    None => real_observe(&opts)?,
                },
                None => real_observe(&opts)?,
            };
            let snapshot = opts.apply(snapshot)?;
            lua.to_value(&snapshot)
        })?
    })?;
    lua.globals().set("fkst", fkst)?;
    Ok(())
}

pub(crate) fn register_test(
    lua: &Lua,
    test: &Table,
    mock_observe: MockObserveState,
) -> mlua::Result<()> {
    test.set("mock_observe", {
        lua.create_function(move |lua, snapshot: Value| {
            let snapshot: JsonValue = lua.from_value(snapshot)?;
            mock_observe.set(snapshot)
        })?
    })?;
    Ok(())
}

fn real_observe(opts: &ObserveSdkOptions) -> mlua::Result<JsonValue> {
    let durable_root = std::env::var_os(DURABLE_ROOT_ENV)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            mlua::Error::external(format!("{DURABLE_ROOT_ENV} must be set for fkst.observe"))
        })?;
    let snapshot =
        crate::observe::snapshot_for_durable_root(PathBuf::from(durable_root), opts.limit)
            .map_err(mlua::Error::external)?;
    serde_json::to_value(snapshot).map_err(mlua::Error::external)
}

fn fkst_table(lua: &Lua) -> mlua::Result<Table> {
    match lua.globals().get::<Value>("fkst")? {
        Value::Table(table) => Ok(table),
        Value::Nil => lua.create_table(),
        _ => Err(mlua::Error::runtime(
            "global fkst exists and is not a table",
        )),
    }
}

#[derive(Debug)]
struct ObserveSdkOptions {
    limit: usize,
    include: Option<BTreeSet<String>>,
    since: Option<String>,
}

impl ObserveSdkOptions {
    fn from_lua(opts: Option<Table>) -> mlua::Result<Self> {
        let Some(opts) = opts else {
            return Ok(Self {
                limit: 500,
                include: None,
                since: None,
            });
        };
        reject_unknown_options(&opts)?;
        let limit = opts.get::<Option<usize>>("limit")?.unwrap_or(500);
        let limit = crate::observe::validate_limit(limit).map_err(mlua::Error::external)?;
        let include = parse_include(opts.get::<Option<Table>>("include")?)?;
        let since = opts.get::<Option<String>>("since")?;
        if since.as_deref().is_some_and(str::is_empty) {
            return Err(mlua::Error::external(
                "fkst.observe since must not be empty",
            ));
        }
        Ok(Self {
            limit,
            include,
            since,
        })
    }

    fn apply(&self, mut snapshot: JsonValue) -> mlua::Result<JsonValue> {
        if let Some(since) = &self.since {
            apply_since(&mut snapshot, since)?;
        }
        if let Some(include) = &self.include {
            snapshot = apply_include(snapshot, include)?;
        }
        Ok(snapshot)
    }
}

fn reject_unknown_options(opts: &Table) -> mlua::Result<()> {
    for pair in opts.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(mlua::Error::external(
                "fkst.observe option keys must be strings",
            ));
        };
        let key = key.to_str()?;
        if !matches!(key.as_ref(), "limit" | "include" | "since") {
            return Err(mlua::Error::external(format!(
                "unknown fkst.observe option `{key}`"
            )));
        }
    }
    Ok(())
}

fn parse_include(include: Option<Table>) -> mlua::Result<Option<BTreeSet<String>>> {
    let Some(include) = include else {
        return Ok(None);
    };
    let mut sections = BTreeSet::new();
    for value in include.sequence_values::<String>() {
        let section = value?;
        if !OBSERVE_SECTIONS.contains(&section.as_str()) {
            return Err(mlua::Error::external(format!(
                "unsupported fkst.observe include section `{section}`"
            )));
        }
        sections.insert(section);
    }
    Ok(Some(sections))
}

fn apply_since(snapshot: &mut JsonValue, since: &str) -> mlua::Result<()> {
    let object = snapshot.as_object_mut().ok_or_else(|| {
        mlua::Error::external("fkst.observe snapshot must be an object when since is set")
    })?;
    trim_entries_after_cursor(object.get_mut("deliveries"), since, "deliveries")?;
    trim_entries_after_cursor(object.get_mut("dead_letters"), since, "dead_letters")?;
    Ok(())
}

fn trim_entries_after_cursor(
    entries: Option<&mut JsonValue>,
    since: &str,
    field: &str,
) -> mlua::Result<()> {
    let Some(entries) = entries else {
        return Ok(());
    };
    let entries = entries.as_array_mut().ok_or_else(|| {
        mlua::Error::external(format!(
            "fkst.observe snapshot field `{field}` must be an array"
        ))
    })?;
    let Some(cursor) = entries
        .iter()
        .position(|entry| entry.get("delivery_id").and_then(JsonValue::as_str) == Some(since))
    else {
        return Ok(());
    };
    entries.drain(0..=cursor);
    Ok(())
}

fn apply_include(snapshot: JsonValue, include: &BTreeSet<String>) -> mlua::Result<JsonValue> {
    let object = snapshot.as_object().ok_or_else(|| {
        mlua::Error::external("fkst.observe snapshot must be an object when include is set")
    })?;
    let mut filtered = JsonMap::new();
    copy_if_present(&mut filtered, object, "schema_version");
    copy_if_present(&mut filtered, object, "generated_at_ms");
    if include.contains("entities") {
        copy_if_present(&mut filtered, object, "source");
        copy_if_present(&mut filtered, object, "limits");
        copy_if_present(&mut filtered, object, "truncated");
    }
    if include.contains("queues") {
        copy_if_present(&mut filtered, object, "queues");
    }
    if include.contains("events") {
        copy_if_present(&mut filtered, object, "deliveries");
    }
    if include.contains("errors") {
        copy_if_present(&mut filtered, object, "dead_letters");
    }
    Ok(JsonValue::Object(filtered))
}

fn copy_if_present(
    target: &mut JsonMap<String, JsonValue>,
    source: &serde_json::Map<String, JsonValue>,
    key: &str,
) {
    if let Some(value) = source.get(key) {
        target.insert(key.to_string(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_rejects_business_options() {
        let lua = Lua::new();
        register(&lua, None).unwrap();

        let err = lua
            .load("return fkst.observe({ idle = true })")
            .eval::<Value>()
            .unwrap_err();

        assert!(err.to_string().contains("unknown fkst.observe option"));
    }

    #[test]
    fn mock_observe_returns_snapshot() {
        let lua = Lua::new();
        let mock = MockObserveState::new();
        register(&lua, Some(mock.clone())).unwrap();
        mock.set(serde_json::json!({
            "schema_version": 1,
            "queues": [{"queue": "input", "depth": 1}]
        }))
        .unwrap();

        let value: JsonValue = lua
            .from_value(lua.load("return fkst.observe()").eval().unwrap())
            .unwrap();

        assert_eq!(value["queues"][0]["queue"], "input");
    }

    #[test]
    fn observe_rejects_unsupported_include_sections() {
        let lua = Lua::new();
        register(&lua, None).unwrap();

        let err = lua
            .load("return fkst.observe({ include = { 'idle' } })")
            .eval::<Value>()
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("unsupported fkst.observe include section"));
    }

    #[test]
    fn observe_applies_include_and_since_to_generic_snapshot_sections() {
        let lua = Lua::new();
        let mock = MockObserveState::new();
        register(&lua, Some(mock.clone())).unwrap();
        mock.set(serde_json::json!({
            "schema_version": 1,
            "generated_at_ms": 10,
            "source": {"durable_root": "/tmp/fkst"},
            "limits": {"max_deliveries": 10, "max_dead_letters": 10},
            "truncated": {"deliveries": false, "dead_letters": false},
            "queues": [{"queue": "input", "depth": 2}],
            "deliveries": [
                {"delivery_id": "delivery-one"},
                {"delivery_id": "delivery-two"}
            ],
            "dead_letters": [
                {"delivery_id": "dead-one"}
            ]
        }))
        .unwrap();

        let value: JsonValue = lua
            .from_value(
                lua.load("return fkst.observe({ include = { 'events' }, since = 'delivery-one' })")
                    .eval()
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["deliveries"][0]["delivery_id"], "delivery-two");
        assert!(value.get("queues").is_none());
        assert!(value.get("dead_letters").is_none());
        assert!(value.get("source").is_none());
    }
}

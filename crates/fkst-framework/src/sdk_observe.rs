//! SDK: `fkst.observe([opts]) -> table`.
//!
//! This is the in-process adapter for the same generic observe snapshot used by the
//! `fkst-framework observe --json` CLI. Packages interpret the returned engine facts;
//! this module must not encode package-specific idle, board, audit, or workflow meaning.

use fkst_common::DURABLE_ROOT_ENV;
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::{BTreeMap, BTreeSet};
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
                    Some(snapshot) => opts.apply_to_mock(snapshot)?,
                    None => {
                        return Err(mlua::Error::external(
                            "fkst.observe is not mocked in test mode",
                        ))
                    }
                },
                None => real_observe(&opts)?,
            };
            let snapshot = opts.apply_include(snapshot)?;
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
    if let Some(lineage) = &opts.lineage {
        let result = crate::observe::lineage_for_durable_root(PathBuf::from(durable_root), lineage)
            .map_err(mlua::Error::external)?;
        serde_json::to_value(result).map_err(mlua::Error::external)
    } else {
        let snapshot = crate::observe::snapshot_for_durable_root(
            PathBuf::from(durable_root),
            &opts.snapshot_options(),
        )
        .map_err(mlua::Error::external)?;
        serde_json::to_value(snapshot).map_err(mlua::Error::external)
    }
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
    page: Option<crate::observe::DeadLetterPageRequest>,
    lineage: Option<crate::observe::LineageObserveRequest>,
}

impl ObserveSdkOptions {
    fn from_lua(opts: Option<Table>) -> mlua::Result<Self> {
        let Some(opts) = opts else {
            return Ok(Self {
                limit: crate::observe::DEFAULT_LIMIT,
                include: None,
                since: None,
                page: None,
                lineage: None,
            });
        };
        reject_unknown_options(&opts)?;
        let lineage = parse_lineage(opts.get::<Option<Table>>("lineage")?)?;
        let has_snapshot_options = opts.contains_key("limit")?
            || opts.contains_key("include")?
            || opts.contains_key("since")?
            || opts.contains_key("page")?;
        if lineage.is_some() && has_snapshot_options {
            return Err(mlua::Error::external(
                "fkst.observe lineage cannot be combined with snapshot options",
            ));
        }
        let limit = opts
            .get::<Option<usize>>("limit")?
            .unwrap_or(crate::observe::DEFAULT_LIMIT);
        let limit = crate::observe::validate_limit(limit).map_err(mlua::Error::external)?;
        let include = parse_include(opts.get::<Option<Table>>("include")?)?;
        let since = opts.get::<Option<String>>("since")?;
        crate::observe::validate_since(since.as_deref()).map_err(mlua::Error::external)?;
        let page = parse_page(opts.get::<Option<Table>>("page")?, since.as_deref())?;
        Ok(Self {
            limit,
            include,
            since,
            page,
            lineage,
        })
    }

    fn snapshot_options(&self) -> crate::observe::ObserveSnapshotOptions {
        crate::observe::ObserveSnapshotOptions {
            limit: self.limit,
            since: self.since.clone(),
            page: self.page.clone(),
        }
    }

    fn apply_to_mock(&self, mut snapshot: JsonValue) -> mlua::Result<JsonValue> {
        if self.lineage.is_some() {
            return Ok(snapshot);
        }
        if let Some(page_request) = &self.page {
            let page = crate::observe::validate_dead_letter_page(
                Some(page_request),
                self.since.as_deref(),
            )
            .map_err(mlua::Error::external)?
            .expect("page request must produce page options");
            apply_dead_letter_page(&mut snapshot, &page, self.limit)?;
        }
        if let Some(since) = &self.since {
            apply_since(&mut snapshot, since)?;
        }
        apply_limit(&mut snapshot, self.limit)?;
        Ok(snapshot)
    }

    fn apply_include(&self, mut snapshot: JsonValue) -> mlua::Result<JsonValue> {
        if self.lineage.is_some() {
            return Ok(snapshot);
        }
        if let Some(include) = &self.include {
            snapshot = apply_include(snapshot, include)?;
        }
        Ok(snapshot)
    }
}

fn apply_dead_letter_page(
    snapshot: &mut JsonValue,
    page: &crate::observe::DeadLetterPageOptions,
    limit: usize,
) -> mlua::Result<()> {
    let object = snapshot.as_object_mut().ok_or_else(|| {
        mlua::Error::external("fkst.observe snapshot must be an object when page is set")
    })?;
    let mut keyed = {
        let entries = object
            .get_mut("dead_letters")
            .ok_or_else(|| {
                mlua::Error::external(
                    "fkst.observe snapshot field `dead_letters` is required when page is set",
                )
            })?
            .as_array_mut()
            .ok_or_else(|| {
                mlua::Error::external("fkst.observe snapshot field `dead_letters` must be an array")
            })?;
        entries
            .drain(..)
            .map(|entry| {
                let dead_at_ms = entry
                    .get("dead_at_ms")
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| {
                        mlua::Error::external(
                            "fkst.observe dead_letters page entry requires integer dead_at_ms",
                        )
                    })?;
                let delivery_id = entry
                    .get("delivery_id")
                    .and_then(JsonValue::as_str)
                    .filter(|delivery_id| !delivery_id.is_empty())
                    .ok_or_else(|| {
                        mlua::Error::external(
                            "fkst.observe dead_letters page entry requires delivery_id",
                        )
                    })?
                    .to_string();
                Ok((dead_at_ms, delivery_id, entry))
            })
            .collect::<mlua::Result<Vec<_>>>()?
    };
    keyed.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    if let Some(after) = &page.after {
        keyed.retain(|entry| {
            (entry.0, entry.1.as_str()) > (after.dead_at_ms, after.delivery_id.as_str())
        });
    }
    let has_more = keyed.len() > limit;
    keyed.truncate(limit);
    let next = if has_more {
        keyed
            .last()
            .map(|entry| {
                crate::observe::encode_dead_letter_cursor(&crate::observe::DeadLetterPageCursor {
                    dead_at_ms: entry.0,
                    delivery_id: entry.1.clone(),
                })
                .map_err(mlua::Error::external)
            })
            .transpose()?
    } else {
        None
    };
    *object
        .get_mut("dead_letters")
        .expect("dead_letters field was validated") =
        JsonValue::Array(keyed.into_iter().map(|(_, _, entry)| entry).collect());
    let mut page_result = JsonMap::new();
    page_result.insert(
        "section".to_string(),
        JsonValue::String("dead_letters".to_string()),
    );
    if let Some(next) = next {
        page_result.insert("next".to_string(), JsonValue::String(next));
    }
    object.insert("page".to_string(), JsonValue::Object(page_result));
    update_truncated(object.get_mut("truncated"), false, has_more, false)?;
    Ok(())
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
        if !matches!(
            key.as_ref(),
            "limit" | "include" | "since" | "page" | "lineage"
        ) {
            return Err(mlua::Error::external(format!(
                "unknown fkst.observe option `{key}`"
            )));
        }
    }
    Ok(())
}

fn parse_lineage(
    lineage: Option<Table>,
) -> mlua::Result<Option<crate::observe::LineageObserveRequest>> {
    let Some(lineage) = lineage else {
        return Ok(None);
    };
    reject_nested_options(&lineage, &["queue", "dept", "source_ref"], "lineage")?;
    let queue = required_nonempty_string(&lineage, "queue", "fkst.observe lineage")?;
    let dept = required_nonempty_string(&lineage, "dept", "fkst.observe lineage")?;
    let source_ref = match lineage.get::<Value>("source_ref")? {
        Value::Table(source_ref) => source_ref,
        _ => {
            return Err(mlua::Error::external(
                "fkst.observe lineage source_ref must be a table",
            ))
        }
    };
    reject_nested_options(&source_ref, &["kind", "ref"], "lineage source_ref")?;
    let kind = required_nonempty_string(&source_ref, "kind", "fkst.observe lineage source_ref")?;
    let reference =
        required_nonempty_string(&source_ref, "ref", "fkst.observe lineage source_ref")?;
    let kind = match kind.as_str() {
        "file" | "file_watch" => crate::supervise::delivery_types::SourceKind::File,
        "cron" => crate::supervise::delivery_types::SourceKind::Cron,
        "git" => crate::supervise::delivery_types::SourceKind::Git,
        "external" => crate::supervise::delivery_types::SourceKind::External,
        _ => {
            return Err(mlua::Error::external(format!(
                "fkst.observe lineage source_ref kind `{kind}` is unsupported"
            )))
        }
    };
    Ok(Some(crate::observe::LineageObserveRequest {
        queue,
        dept,
        source_ref: crate::supervise::delivery_types::SourceRef { kind, reference },
    }))
}

fn reject_nested_options(table: &Table, allowed: &[&str], label: &str) -> mlua::Result<()> {
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(mlua::Error::external(format!(
                "fkst.observe {label} keys must be strings"
            )));
        };
        let key = key.to_str()?;
        if !allowed.contains(&key.as_ref()) {
            return Err(mlua::Error::external(format!(
                "unknown fkst.observe {label} option `{key}`"
            )));
        }
    }
    Ok(())
}

fn required_nonempty_string(table: &Table, key: &str, label: &str) -> mlua::Result<String> {
    match table.get::<Value>(key)? {
        Value::String(value) if !value.as_bytes().is_empty() => Ok(value.to_str()?.to_string()),
        _ => Err(mlua::Error::external(format!(
            "{label} {key} must be a non-empty string"
        ))),
    }
}

fn parse_page(
    page: Option<Table>,
    since: Option<&str>,
) -> mlua::Result<Option<crate::observe::DeadLetterPageRequest>> {
    let Some(page) = page else {
        return Ok(None);
    };
    for pair in page.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(mlua::Error::external(
                "fkst.observe page keys must be strings",
            ));
        };
        let key = key.to_str()?;
        if !matches!(key.as_ref(), "section" | "after") {
            return Err(mlua::Error::external(format!(
                "unknown fkst.observe page option `{key}`"
            )));
        }
    }
    let section = match page.get::<Value>("section")? {
        Value::String(section) => section.to_str()?.to_string(),
        _ => {
            return Err(mlua::Error::external(
                "fkst.observe page section must be a string",
            ))
        }
    };
    let after = match page.get::<Value>("after")? {
        Value::Nil => None,
        Value::String(after) => Some(after.to_str()?.to_string()),
        _ => {
            return Err(mlua::Error::external(
                "observe dead-letter cursor invalid: after must be a string",
            ))
        }
    };
    let request = crate::observe::DeadLetterPageRequest { section, after };
    crate::observe::validate_dead_letter_page(Some(&request), since)
        .map_err(mlua::Error::external)?;
    Ok(Some(request))
}

fn parse_include(include: Option<Table>) -> mlua::Result<Option<BTreeSet<String>>> {
    let Some(include) = include else {
        return Ok(None);
    };
    let mut ordered_sections = BTreeMap::new();
    for pair in include.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let Value::Integer(index) = key else {
            return Err(mlua::Error::external(
                "fkst.observe include keys must be positive contiguous integers",
            ));
        };
        let index = usize::try_from(index).map_err(|_| {
            mlua::Error::external("fkst.observe include keys must be positive contiguous integers")
        })?;
        if index == 0 {
            return Err(mlua::Error::external(
                "fkst.observe include keys must be positive contiguous integers",
            ));
        }
        let Value::String(section) = value else {
            return Err(mlua::Error::external(
                "fkst.observe include values must be strings",
            ));
        };
        let section = section.to_str()?.to_string();
        if !OBSERVE_SECTIONS.contains(&section.as_str()) {
            return Err(mlua::Error::external(format!(
                "unsupported fkst.observe include section `{section}`"
            )));
        }
        ordered_sections.insert(index, section);
    }
    for (expected, actual) in (1..=ordered_sections.len()).zip(ordered_sections.keys()) {
        if expected != *actual {
            return Err(mlua::Error::external(
                "fkst.observe include keys must be positive contiguous integers",
            ));
        }
    }
    Ok(Some(ordered_sections.into_values().collect()))
}

fn apply_since(snapshot: &mut JsonValue, since: &str) -> mlua::Result<()> {
    let object = snapshot.as_object_mut().ok_or_else(|| {
        mlua::Error::external("fkst.observe snapshot must be an object when since is set")
    })?;
    trim_entries_after_cursor(object.get_mut("deliveries"), since, "deliveries")?;
    trim_entries_after_cursor(object.get_mut("dead_letters"), since, "dead_letters")?;
    Ok(())
}

fn apply_limit(snapshot: &mut JsonValue, limit: usize) -> mlua::Result<()> {
    let object = snapshot.as_object_mut().ok_or_else(|| {
        mlua::Error::external("fkst.observe snapshot must be an object when limit is set")
    })?;
    let deliveries_truncated = truncate_array(object.get_mut("deliveries"), limit, "deliveries")?;
    let dead_letters_truncated =
        truncate_array(object.get_mut("dead_letters"), limit, "dead_letters")?;
    let terminal_suppressions_truncated = truncate_array(
        object.get_mut("terminal_suppressions"),
        limit,
        "terminal_suppressions",
    )?;
    update_limits(object.get_mut("limits"), limit)?;
    update_truncated(
        object.get_mut("truncated"),
        deliveries_truncated,
        dead_letters_truncated,
        terminal_suppressions_truncated,
    )?;
    Ok(())
}

fn truncate_array(
    entries: Option<&mut JsonValue>,
    limit: usize,
    field: &str,
) -> mlua::Result<bool> {
    let Some(entries) = entries else {
        return Ok(false);
    };
    let entries = entries.as_array_mut().ok_or_else(|| {
        mlua::Error::external(format!(
            "fkst.observe snapshot field `{field}` must be an array"
        ))
    })?;
    if entries.len() <= limit {
        return Ok(false);
    }
    entries.truncate(limit);
    Ok(true)
}

fn update_limits(limits: Option<&mut JsonValue>, limit: usize) -> mlua::Result<()> {
    let Some(limits) = limits else {
        return Ok(());
    };
    let limits = limits.as_object_mut().ok_or_else(|| {
        mlua::Error::external("fkst.observe snapshot field `limits` must be an object")
    })?;
    limits.insert("max_deliveries".to_string(), JsonValue::from(limit));
    limits.insert("max_dead_letters".to_string(), JsonValue::from(limit));
    limits.insert(
        "max_terminal_suppressions".to_string(),
        JsonValue::from(limit),
    );
    Ok(())
}

fn update_truncated(
    truncated: Option<&mut JsonValue>,
    deliveries_truncated: bool,
    dead_letters_truncated: bool,
    terminal_suppressions_truncated: bool,
) -> mlua::Result<()> {
    let Some(truncated) = truncated else {
        return Ok(());
    };
    let truncated = truncated.as_object_mut().ok_or_else(|| {
        mlua::Error::external("fkst.observe snapshot field `truncated` must be an object")
    })?;
    let deliveries_truncated = existing_truncated(truncated, "deliveries")? || deliveries_truncated;
    let dead_letters_truncated =
        existing_truncated(truncated, "dead_letters")? || dead_letters_truncated;
    let terminal_suppressions_truncated =
        existing_truncated(truncated, "terminal_suppressions")? || terminal_suppressions_truncated;
    truncated.insert(
        "deliveries".to_string(),
        JsonValue::Bool(deliveries_truncated),
    );
    truncated.insert(
        "dead_letters".to_string(),
        JsonValue::Bool(dead_letters_truncated),
    );
    truncated.insert(
        "terminal_suppressions".to_string(),
        JsonValue::Bool(terminal_suppressions_truncated),
    );
    Ok(())
}

fn existing_truncated(truncated: &JsonMap<String, JsonValue>, field: &str) -> mlua::Result<bool> {
    match truncated.get(field) {
        Some(JsonValue::Bool(value)) => Ok(*value),
        Some(_) => Err(mlua::Error::external(format!(
            "fkst.observe snapshot truncated field `{field}` must be a boolean"
        ))),
        None => Ok(false),
    }
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
    copy_if_present(&mut filtered, object, "page");
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
        copy_if_present(&mut filtered, object, "terminal_suppressions");
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

    fn assert_observe_rejected(chunk: &str, expected: &str) {
        let lua = Lua::new();
        register(&lua, None).unwrap();

        let err = lua.load(chunk).eval::<Value>().unwrap_err();

        assert!(err.to_string().contains(expected), "{err}");
    }

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
    fn mock_observe_returns_bounded_lineage_result() {
        let lua = Lua::new();
        let mock = MockObserveState::new();
        register(&lua, Some(mock.clone())).unwrap();
        mock.set(serde_json::json!({
            "live_delivery": {
                "delivery_id": "live-one",
                "queue": "target.queue",
                "dept": "target-dept"
            },
            "terminal_dead_letter": {
                "delivery_id": "dead-one",
                "queue": "target.queue",
                "dept": "target-dept",
                "attempts": 2,
                "permanent": true,
                "replayable": false
            }
        }))
        .unwrap();

        let value: JsonValue = lua
            .from_value(
                lua.load(
                    r#"
return fkst.observe({
  lineage = {
    queue = "target.queue",
    dept = "target-dept",
    source_ref = { kind = "external", ref = "owner/repo#issue/42" },
  },
})
"#,
                )
                .eval()
                .unwrap(),
            )
            .unwrap();

        assert_eq!(value["live_delivery"]["delivery_id"], "live-one");
        assert_eq!(value["terminal_dead_letter"]["delivery_id"], "dead-one");
        assert!(value.get("truncated").is_none());
    }

    #[test]
    fn lineage_observe_rejects_snapshot_options() {
        assert_observe_rejected(
            r#"
return fkst.observe({
  lineage = {
    queue = "target.queue",
    dept = "target-dept",
    source_ref = { kind = "external", ref = "owner/repo#issue/42" },
  },
  limit = 1,
})
"#,
            "fkst.observe lineage cannot be combined with snapshot options",
        );
    }

    #[test]
    fn lineage_observe_rejects_unknown_nested_options() {
        assert_observe_rejected(
            r#"
return fkst.observe({
  lineage = {
    queue = "target.queue",
    dept = "target-dept",
    source_ref = { kind = "external", ref = "owner/repo#issue/42" },
    retry = true,
  },
})
"#,
            "unknown fkst.observe lineage option `retry`",
        );
        assert_observe_rejected(
            r#"
return fkst.observe({
  lineage = {
    queue = "target.queue",
    dept = "target-dept",
    source_ref = {
      kind = "external",
      ref = "owner/repo#issue/42",
      version = 1,
    },
  },
})
"#,
            "unknown fkst.observe lineage source_ref option `version`",
        );
    }

    #[test]
    fn observe_fails_closed_in_test_mode_without_mock() {
        let lua = Lua::new();
        let mock = MockObserveState::new();
        register(&lua, Some(mock)).unwrap();

        let err = lua
            .load("return fkst.observe()")
            .eval::<Value>()
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("fkst.observe is not mocked in test mode"));
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
    fn observe_rejects_mapped_include_sections() {
        let lua = Lua::new();
        register(&lua, None).unwrap();

        let err = lua
            .load("return fkst.observe({ include = { idle = true } })")
            .eval::<Value>()
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("fkst.observe include keys must be positive contiguous integers"));
    }

    #[test]
    fn observe_rejects_sparse_include_sections() {
        let lua = Lua::new();
        register(&lua, None).unwrap();

        let err = lua
            .load("return fkst.observe({ include = { [2] = 'queues' } })")
            .eval::<Value>()
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("fkst.observe include keys must be positive contiguous integers"));
    }

    #[test]
    fn observe_rejects_malformed_dead_letter_cursor() {
        let lua = Lua::new();
        register(&lua, None).unwrap();

        let err = lua
            .load("return fkst.observe({ page = { section = 'dead_letters', after = 'bad' } })")
            .eval::<Value>()
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("observe dead-letter cursor invalid"));
    }

    #[test]
    fn observe_rejects_page_combined_with_since() {
        assert_observe_rejected(
            "return fkst.observe({ since = 'dead-one', page = { section = 'dead_letters' } })",
            "fkst.observe page cannot be combined with since",
        );
    }

    #[test]
    fn observe_rejects_unsupported_page_section() {
        assert_observe_rejected(
            "return fkst.observe({ page = { section = 'deliveries' } })",
            "fkst.observe page section must be `dead_letters`",
        );
    }

    #[test]
    fn observe_rejects_unknown_page_option() {
        assert_observe_rejected(
            "return fkst.observe({ page = { section = 'dead_letters', restart = true } })",
            "unknown fkst.observe page option `restart`",
        );
    }

    #[test]
    fn observe_rejects_non_string_page_cursor() {
        assert_observe_rejected(
            "return fkst.observe({ page = { section = 'dead_letters', after = 42 } })",
            "observe dead-letter cursor invalid: after must be a string",
        );
    }

    #[test]
    fn observe_rejects_limit_above_maximum_before_page_read() {
        assert_observe_rejected(
            "return fkst.observe({ limit = 10001, page = { section = 'dead_letters' } })",
            "observe limit must be between 1 and 10000",
        );
    }

    #[test]
    fn mock_observe_applies_since_before_limit() {
        let lua = Lua::new();
        let mock = MockObserveState::new();
        register(&lua, Some(mock.clone())).unwrap();
        mock.set(serde_json::json!({
            "schema_version": 1,
            "limits": {"max_deliveries": 10, "max_dead_letters": 10},
            "truncated": {"deliveries": false, "dead_letters": false},
            "deliveries": [
                {"delivery_id": "delivery-one"},
                {"delivery_id": "delivery-two"},
                {"delivery_id": "delivery-three"}
            ],
            "dead_letters": []
        }))
        .unwrap();

        let value: JsonValue = lua
            .from_value(
                lua.load("return fkst.observe({ since = 'delivery-one', limit = 1 })")
                    .eval()
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(value["deliveries"].as_array().unwrap().len(), 1);
        assert_eq!(value["deliveries"][0]["delivery_id"], "delivery-two");
        assert_eq!(value["limits"]["max_deliveries"], 1);
        assert!(value["truncated"]["deliveries"].as_bool().unwrap());
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

    #[test]
    fn observe_errors_include_terminal_suppressions() {
        let lua = Lua::new();
        let mock = MockObserveState::new();
        register(&lua, Some(mock.clone())).unwrap();
        mock.set(serde_json::json!({
            "schema_version": 1,
            "deliveries": [{"delivery_id": "live-one"}],
            "dead_letters": [{"delivery_id": "dead-one"}],
            "terminal_suppressions": [{"delivery_id": "suppressed-one"}]
        }))
        .unwrap();

        let value: JsonValue = lua
            .from_value(
                lua.load("return fkst.observe({ include = { 'errors' } })")
                    .eval()
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(value["dead_letters"][0]["delivery_id"], "dead-one");
        assert_eq!(
            value["terminal_suppressions"][0]["delivery_id"],
            "suppressed-one"
        );
        assert!(value.get("deliveries").is_none());
    }

    #[test]
    fn mock_observe_limits_terminal_suppressions_independently() {
        let lua = Lua::new();
        let mock = MockObserveState::new();
        register(&lua, Some(mock.clone())).unwrap();
        mock.set(serde_json::json!({
            "schema_version": 1,
            "limits": {
                "max_deliveries": 10,
                "max_dead_letters": 10,
                "max_terminal_suppressions": 10
            },
            "truncated": {
                "deliveries": false,
                "dead_letters": false,
                "terminal_suppressions": false
            },
            "deliveries": [],
            "dead_letters": [],
            "terminal_suppressions": [
                {"delivery_id": "suppressed-one"},
                {"delivery_id": "suppressed-two"}
            ]
        }))
        .unwrap();

        let value: JsonValue = lua
            .from_value(
                lua.load("return fkst.observe({ limit = 1, include = { 'errors', 'entities' } })")
                    .eval()
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(value["terminal_suppressions"].as_array().unwrap().len(), 1);
        assert_eq!(value["limits"]["max_terminal_suppressions"], 1);
        assert!(value["truncated"]["terminal_suppressions"]
            .as_bool()
            .unwrap());
    }

    #[test]
    fn mock_observe_pages_dead_letters_by_durable_order() {
        let lua = Lua::new();
        let mock = MockObserveState::new();
        register(&lua, Some(mock.clone())).unwrap();
        mock.set(serde_json::json!({
            "schema_version": 1,
            "generated_at_ms": 10,
            "source": {"durable_root": "/tmp/fkst"},
            "limits": {"max_deliveries": 10, "max_dead_letters": 10},
            "truncated": {"deliveries": false, "dead_letters": false},
            "queues": [],
            "deliveries": [],
            "dead_letters": [
                {"delivery_id": "dead-c", "dead_at_ms": 11},
                {"delivery_id": "dead-b", "dead_at_ms": 10},
                {"delivery_id": "dead-a", "dead_at_ms": 10}
            ]
        }))
        .unwrap();

        let first: JsonValue = lua
            .from_value(
                lua.load(
                    "return fkst.observe({ limit = 2, include = { 'errors', 'entities' }, page = { section = 'dead_letters' } })",
                )
                .eval()
                .unwrap(),
            )
            .unwrap();
        let cursor = first["page"]["next"].as_str().unwrap();
        let second: JsonValue = lua
            .from_value(
                lua.load(format!(
                    "return fkst.observe({{ limit = 2, include = {{ 'errors', 'entities' }}, page = {{ section = 'dead_letters', after = '{cursor}' }} }})"
                ))
                .eval()
                .unwrap(),
            )
            .unwrap();

        assert_eq!(
            first["dead_letters"],
            serde_json::json!([
                {"delivery_id": "dead-a", "dead_at_ms": 10},
                {"delivery_id": "dead-b", "dead_at_ms": 10}
            ])
        );
        assert_eq!(second["dead_letters"][0]["delivery_id"], "dead-c");
        assert!(second["page"].get("next").is_none());
    }
}

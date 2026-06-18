//! SDK: durable visible-intent barrier for non-idempotent effects.

use anyhow::{anyhow, bail, Result};
use fkst_common::DurableLayout;
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use nix::fcntl::{flock, FlockArg};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: &str = "1";
const INTENT_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("intent_by_id");
const INTENT_BY_EFFECT_KEY: TableDefinition<&str, &str> =
    TableDefinition::new("intent_by_effect_key");
const RESULT_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("result_by_id");
const RESULT_BY_INTENT_ID: TableDefinition<&str, &str> =
    TableDefinition::new("result_by_intent_id");
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct IntentRecord {
    intent_id: String,
    edge: String,
    generation: String,
    effect_kind: String,
    effect_key: String,
    declared_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ResultRecord {
    result_id: String,
    intent_id: String,
    effect_key: String,
    result: JsonValue,
    written_at_ms: u64,
}

struct IntentStore {
    db: Database,
}

impl IntentStore {
    fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        let write = db.begin_write()?;
        {
            write.open_table(INTENT_BY_ID)?;
            write.open_table(INTENT_BY_EFFECT_KEY)?;
            write.open_table(RESULT_BY_ID)?;
            write.open_table(RESULT_BY_INTENT_ID)?;
            let mut meta = write.open_table(META)?;
            meta.insert("schema_version", SCHEMA_VERSION)?;
        }
        write.commit()?;
        Ok(Self { db })
    }

    fn declare_intent(
        &self,
        edge: String,
        generation: String,
        effect_kind: String,
        effect_key: String,
        now_ms: u64,
    ) -> Result<IntentRecord> {
        validate_token("edge", &edge)?;
        validate_token("generation", &generation)?;
        validate_token("effect_kind", &effect_kind)?;
        validate_effect_key(&effect_key)?;
        let intent_id = intent_id(&edge, &generation, &effect_kind, &effect_key);
        let record = IntentRecord {
            intent_id: intent_id.clone(),
            edge,
            generation,
            effect_kind,
            effect_key: effect_key.clone(),
            declared_at_ms: now_ms,
        };
        let write = self.db.begin_write()?;
        {
            let mut by_id = write.open_table(INTENT_BY_ID)?;
            let mut by_effect_key = write.open_table(INTENT_BY_EFFECT_KEY)?;
            let existing_id = by_effect_key
                .get(effect_key.as_str())?
                .map(|value| value.value().to_string());
            if let Some(existing_id) = existing_id {
                let Some(existing) = read_intent(&by_id, &existing_id)? else {
                    bail!("intent index points to missing intent_id: {existing_id}");
                };
                if existing == record {
                    drop(by_id);
                    drop(by_effect_key);
                    write.commit()?;
                    return Ok(existing);
                }
                bail!(
                    "effect_key `{}` already belongs to intent `{}`",
                    effect_key,
                    existing.intent_id
                );
            }
            by_id.insert(intent_id.as_str(), serde_json::to_vec(&record)?.as_slice())?;
            by_effect_key.insert(effect_key.as_str(), intent_id.as_str())?;
        }
        write.commit()?;
        Ok(record)
    }

    fn intent(&self, intent_id: &str) -> Result<Option<IntentRecord>> {
        let read = self.db.begin_read()?;
        let by_id = read.open_table(INTENT_BY_ID)?;
        read_intent(&by_id, intent_id)
    }

    fn result_for_intent(&self, intent_id: &str) -> Result<Option<ResultRecord>> {
        let read = self.db.begin_read()?;
        let by_intent = read.open_table(RESULT_BY_INTENT_ID)?;
        let by_id = read.open_table(RESULT_BY_ID)?;
        let Some(result_id) = by_intent.get(intent_id)? else {
            return Ok(None);
        };
        read_result(&by_id, result_id.value())
    }

    fn write_result(
        &self,
        intent_id: String,
        result: JsonValue,
        now_ms: u64,
    ) -> Result<ResultRecord> {
        let write = self.db.begin_write()?;
        let record = {
            let by_intent = write.open_table(RESULT_BY_INTENT_ID)?;
            let existing_id = by_intent
                .get(intent_id.as_str())?
                .map(|value| value.value().to_string());
            if let Some(existing_id) = existing_id {
                let by_id = write.open_table(RESULT_BY_ID)?;
                let Some(existing) = read_result(&by_id, &existing_id)? else {
                    bail!("result index points to missing result_id: {existing_id}");
                };
                drop(by_id);
                drop(by_intent);
                write.commit()?;
                return Ok(existing);
            }
            drop(by_intent);

            let by_id = write.open_table(INTENT_BY_ID)?;
            let Some(intent) = read_intent(&by_id, &intent_id)? else {
                bail!("intent `{intent_id}` is not visible");
            };
            drop(by_id);

            let result_id = format!("{}:result", intent.intent_id);
            let record = ResultRecord {
                result_id: result_id.clone(),
                intent_id: intent.intent_id,
                effect_key: intent.effect_key,
                result,
                written_at_ms: now_ms,
            };
            let mut by_id = write.open_table(RESULT_BY_ID)?;
            let mut by_intent = write.open_table(RESULT_BY_INTENT_ID)?;
            by_id.insert(result_id.as_str(), serde_json::to_vec(&record)?.as_slice())?;
            by_intent.insert(record.intent_id.as_str(), result_id.as_str())?;
            record
        };
        write.commit()?;
        Ok(record)
    }
}

pub(crate) fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "declare_intent",
        lua.create_function(
            |lua, (edge, generation, effect_kind, effect_key): (String, String, String, String)| {
                let store = open_store().map_err(mlua::Error::external)?;
                let record = store
                    .declare_intent(edge, generation, effect_kind, effect_key, now_ms())
                    .map_err(mlua::Error::external)?;
                intent_to_table(lua, &record)
            },
        )?,
    )?;

    lua.globals().set(
        "wait_until_intent_visible",
        lua.create_function(|lua, intent_id: String| {
            validate_intent_id(&intent_id).map_err(mlua::Error::external)?;
            let store = open_store().map_err(mlua::Error::external)?;
            let record = store
                .intent(&intent_id)
                .map_err(mlua::Error::external)?
                .ok_or_else(|| {
                    mlua::Error::external(format!("intent `{intent_id}` is not visible"))
                })?;
            intent_to_table(lua, &record)
        })?,
    )?;

    lua.globals().set(
        "write_result_marker",
        lua.create_function(|lua, (intent_id, result): (String, Value)| {
            validate_intent_id(&intent_id).map_err(mlua::Error::external)?;
            let result_json: JsonValue = lua.from_value(result).map_err(mlua::Error::external)?;
            let store = open_store().map_err(mlua::Error::external)?;
            let record = store
                .write_result(intent_id, result_json, now_ms())
                .map_err(mlua::Error::external)?;
            result_to_table(lua, &record)
        })?,
    )?;

    lua.globals().set(
        "wait_until_result_visible",
        lua.create_function(|lua, result_id: String| {
            let intent_id = result_id
                .strip_suffix(":result")
                .ok_or_else(|| mlua::Error::external("result_id must end with `:result`"))?;
            validate_intent_id(intent_id).map_err(mlua::Error::external)?;
            let store = open_store().map_err(mlua::Error::external)?;
            let record = store
                .result_for_intent(intent_id)
                .map_err(mlua::Error::external)?
                .ok_or_else(|| {
                    mlua::Error::external(format!("result `{result_id}` is not visible"))
                })?;
            result_to_table(lua, &record)
        })?,
    )?;

    lua.globals().set(
        "derive_next_transition_from_visible_result",
        lua.create_function(|lua, result_id: String| {
            let result: Table = lua
                .globals()
                .get::<Function>("wait_until_result_visible")?
                .call(result_id)?;
            result.get::<Value>("result")
        })?,
    )?;

    lua.globals().set(
        "perform_or_recover_effect",
        lua.create_function(
            |lua, (intent_id, effect_key, recover, perform): (String, String, Function, Function)| {
                validate_intent_id(&intent_id).map_err(mlua::Error::external)?;
                validate_effect_key(&effect_key).map_err(mlua::Error::external)?;
                let layout = DurableLayout::from_env().map_err(mlua::Error::external)?;
                let _lock =
                    acquire_intent_lock(&layout.intent_lock_dir(), &intent_id).map_err(mlua::Error::external)?;
                let store = IntentStore::open(layout.intent_db_path()).map_err(mlua::Error::external)?;
                let intent = store
                    .intent(&intent_id)
                    .map_err(mlua::Error::external)?
                    .ok_or_else(|| mlua::Error::external(format!("intent `{intent_id}` is not visible")))?;
                if intent.effect_key != effect_key {
                    return Err(mlua::Error::external(format!(
                        "intent `{intent_id}` does not match effect_key `{effect_key}`"
                    )));
                }
                if let Some(result) = store
                    .result_for_intent(&intent_id)
                    .map_err(mlua::Error::external)?
                {
                    return lua.to_value(&result.result);
                }
                let recovered: Value = recover.call(effect_key.clone())?;
                if !matches!(recovered, Value::Nil) {
                    let recovered_json: JsonValue =
                        lua.from_value(recovered).map_err(mlua::Error::external)?;
                    let result = store
                        .write_result(intent_id, recovered_json, now_ms())
                        .map_err(mlua::Error::external)?;
                    return lua.to_value(&result.result);
                }
                let performed: Value = perform.call(effect_key)?;
                let performed_json: JsonValue =
                    lua.from_value(performed).map_err(mlua::Error::external)?;
                let result = store
                    .write_result(intent_id, performed_json, now_ms())
                    .map_err(mlua::Error::external)?;
                lua.to_value(&result.result)
            },
        )?,
    )?;
    Ok(())
}

fn open_store() -> Result<IntentStore> {
    let layout = DurableLayout::from_env()?;
    IntentStore::open(layout.intent_db_path())
}

fn read_intent(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    intent_id: &str,
) -> Result<Option<IntentRecord>> {
    let Some(bytes) = table.get(intent_id)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(bytes.value())?))
}

fn read_result(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    result_id: &str,
) -> Result<Option<ResultRecord>> {
    let Some(bytes) = table.get(result_id)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(bytes.value())?))
}

fn intent_to_table(lua: &Lua, record: &IntentRecord) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("intent_id", record.intent_id.clone())?;
    table.set("edge", record.edge.clone())?;
    table.set("generation", record.generation.clone())?;
    table.set("effect_kind", record.effect_kind.clone())?;
    table.set("effect_key", record.effect_key.clone())?;
    table.set("declared_at_ms", record.declared_at_ms)?;
    Ok(table)
}

fn result_to_table(lua: &Lua, record: &ResultRecord) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("result_id", record.result_id.clone())?;
    table.set("intent_id", record.intent_id.clone())?;
    table.set("effect_key", record.effect_key.clone())?;
    table.set("result", lua.to_value(&record.result)?)?;
    table.set("written_at_ms", record.written_at_ms)?;
    Ok(table)
}

fn intent_id(edge: &str, generation: &str, effect_kind: &str, effect_key: &str) -> String {
    format!(
        "{edge}/{generation}/{effect_kind}/{}",
        stable_hash(effect_key)
    )
}

fn stable_hash(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn validate_token(name: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{name} must not be empty");
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        Ok(())
    } else {
        bail!("{name} must match [A-Za-z0-9_.-]+")
    }
}

fn validate_effect_key(value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("effect_key must not be empty");
    }
    if value.len() > 1024 {
        bail!("effect_key must be at most 1024 bytes");
    }
    if value.chars().any(|ch| ch.is_control()) {
        bail!("effect_key must not contain control characters");
    }
    Ok(())
}

fn validate_intent_id(value: &str) -> Result<()> {
    let segments = value.split('/').collect::<Vec<_>>();
    if segments.len() != 4 {
        bail!("intent_id must have four path segments");
    }
    validate_token("edge", segments[0])?;
    validate_token("generation", segments[1])?;
    validate_token("effect_kind", segments[2])?;
    if segments[3].len() != 16 || !segments[3].bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("intent_id hash segment must be 16 hex digits");
    }
    Ok(())
}

fn acquire_intent_lock(root: &Path, intent_id: &str) -> Result<File> {
    let path = root.join(intent_id).join("=lock");
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("intent lock target '{}' has no parent", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    flock(file.as_raw_fd(), FlockArg::LockExclusive)?;
    Ok(file)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn declare_intent_is_idempotent_for_same_effect_key() {
        let root = tempdir().unwrap();
        let store = IntentStore::open(root.path().join("intent.redb")).unwrap();

        let first = store
            .declare_intent(
                "ready-implementing".to_string(),
                "issue-1".to_string(),
                "codex".to_string(),
                "issue/1/attempt".to_string(),
                10,
            )
            .unwrap();
        let second = store
            .declare_intent(
                "ready-implementing".to_string(),
                "issue-1".to_string(),
                "codex".to_string(),
                "issue/1/attempt".to_string(),
                10,
            )
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(store.intent(&first.intent_id).unwrap(), Some(first));
    }

    #[test]
    fn conflicting_effect_key_owner_fails_closed() {
        let root = tempdir().unwrap();
        let store = IntentStore::open(root.path().join("intent.redb")).unwrap();
        store
            .declare_intent(
                "edge-a".to_string(),
                "gen".to_string(),
                "codex".to_string(),
                "same-key".to_string(),
                10,
            )
            .unwrap();

        let err = store
            .declare_intent(
                "edge-b".to_string(),
                "gen".to_string(),
                "codex".to_string(),
                "same-key".to_string(),
                11,
            )
            .unwrap_err();

        assert!(format!("{err:#}").contains("already belongs to intent"));
    }

    #[test]
    fn write_result_marker_is_idempotent() {
        let root = tempdir().unwrap();
        let store = IntentStore::open(root.path().join("intent.redb")).unwrap();
        let intent = store
            .declare_intent(
                "edge".to_string(),
                "gen".to_string(),
                "comment".to_string(),
                "comment-key".to_string(),
                10,
            )
            .unwrap();

        let first = store
            .write_result(intent.intent_id.clone(), serde_json::json!({"id": 1}), 20)
            .unwrap();
        let second = store
            .write_result(intent.intent_id.clone(), serde_json::json!({"id": 2}), 21)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            store
                .result_for_intent(&intent.intent_id)
                .unwrap()
                .unwrap()
                .result,
            serde_json::json!({"id": 1})
        );
    }
}

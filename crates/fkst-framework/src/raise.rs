//! SDK: `raise(queue_name, payload)` — buffers in-process; emit RAISED stdout line on exit.
//!
//! Raise is best-effort, at-most-once, and derived-only. Durable intent goes through filesystem.
//! Emit format = `RAISED: <base64-url-encoded JSON [{queue, payload}, ...]>`.

use base64::Engine;
use mlua::{Lua, LuaSerdeExt, Result};
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::sync::{Arc, Mutex};

use crate::path_resolver::NameResolver;

#[derive(Serialize, Debug, Clone)]
struct RaisedEntry {
    queue: String,
    payload: JsonValue,
}

#[derive(Clone, Default)]
pub struct RaiseBuffer(Arc<Mutex<Vec<RaisedEntry>>>);

impl RaiseBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn snapshot(&self) -> Vec<(String, JsonValue)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|entry| (entry.queue.clone(), entry.payload.clone()))
            .collect()
    }

    pub(crate) fn push(&self, queue: String, payload: JsonValue) {
        self.0.lock().unwrap().push(RaisedEntry { queue, payload });
    }

    pub fn emit_stdout(&self) {
        let entries = self.0.lock().unwrap().clone();
        if entries.is_empty() {
            return;
        }
        let json = serde_json::to_string(&entries).expect("serialize raise entries");
        let b64 = base64::engine::general_purpose::URL_SAFE.encode(json.as_bytes());
        println!("RAISED: {}", b64);
    }
}

pub fn register(
    lua: &Lua,
    buf: RaiseBuffer,
    resolver: NameResolver,
    owner_namespace: String,
) -> Result<()> {
    let buf_clone = buf.clone();
    lua.globals().set(
        "raise",
        lua.create_function(move |lua, (queue, payload): (String, mlua::Value)| {
            let queue = resolver
                .resolve(&owner_namespace, &queue)
                .map_err(mlua::Error::external)?;
            let p_json: JsonValue = lua.from_value(payload).unwrap_or(JsonValue::Null);
            buf_clone.push(queue, p_json);
            Ok(())
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn register_raise_and_json(lua: &Lua, buf: RaiseBuffer) {
        crate::sdk_json::register(lua).unwrap();
        register(
            lua,
            buf,
            NameResolver::new(["pkg".to_string()]),
            "pkg".to_string(),
        )
        .unwrap();
    }

    #[test]
    fn raise_adds_to_buffer() {
        let lua = Lua::new();
        let buf = RaiseBuffer::new();
        register(
            &lua,
            buf.clone(),
            NameResolver::new(["pkg".to_string()]),
            "pkg".to_string(),
        )
        .unwrap();
        lua.load(r#"raise("done", {n=42})"#).exec().unwrap();
        let entries = buf.0.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].queue, "done");
        assert_eq!(entries[0].payload, serde_json::json!({"n": 42}));
    }

    #[test]
    fn multiple_raises_buffered() {
        let lua = Lua::new();
        let buf = RaiseBuffer::new();
        register(
            &lua,
            buf.clone(),
            NameResolver::new(["pkg".to_string()]),
            "pkg".to_string(),
        )
        .unwrap();
        lua.load(
            r#"
            raise("q1", {})
            raise("q2", {a=1})
            raise("q1", "string-payload")
        "#,
        )
        .exec()
        .unwrap();
        let entries = buf.0.lock().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].queue, "q1");
        assert_eq!(entries[2].queue, "q1");
    }

    #[test]
    fn raise_preserves_json_decoded_empty_array_payload_field() {
        let lua = Lua::new();
        let buf = RaiseBuffer::new();
        register_raise_and_json(&lua, buf.clone());

        lua.load(r#"raise("done", {items = json.decode("[]")})"#)
            .exec()
            .unwrap();

        let entries = buf.0.lock().unwrap();
        assert_eq!(
            entries[0].payload,
            serde_json::json!({
                "items": []
            })
        );
    }

    #[test]
    fn raise_serializes_bare_empty_table_payload_field_as_object() {
        let lua = Lua::new();
        let buf = RaiseBuffer::new();
        register_raise_and_json(&lua, buf.clone());

        lua.load(r#"raise("done", {items = {}})"#).exec().unwrap();

        let entries = buf.0.lock().unwrap();
        assert_eq!(
            entries[0].payload,
            serde_json::json!({
                "items": {}
            })
        );
    }

    #[test]
    fn raise_serializes_non_empty_sequence_payload_field_as_array() {
        let lua = Lua::new();
        let buf = RaiseBuffer::new();
        register_raise_and_json(&lua, buf.clone());

        lua.load(r#"raise("done", {items = {"a"}})"#)
            .exec()
            .unwrap();

        let entries = buf.0.lock().unwrap();
        assert_eq!(
            entries[0].payload,
            serde_json::json!({
                "items": ["a"]
            })
        );
    }

    #[test]
    fn raise_serializes_non_empty_map_payload_field_as_object() {
        let lua = Lua::new();
        let buf = RaiseBuffer::new();
        register_raise_and_json(&lua, buf.clone());

        lua.load(r#"raise("done", {items = {name = "a"}})"#)
            .exec()
            .unwrap();

        let entries = buf.0.lock().unwrap();
        assert_eq!(
            entries[0].payload,
            serde_json::json!({
                "items": {
                    "name": "a"
                }
            })
        );
    }

    #[test]
    fn namespaced_raise_qualifies_bare_and_rejects_unknown_namespace() {
        let lua = Lua::new();
        let buf = RaiseBuffer::new();
        register(
            &lua,
            buf.clone(),
            NameResolver::new(["pkg".to_string(), "host".to_string()]),
            "pkg".to_string(),
        )
        .unwrap();

        lua.load(
            r#"
            raise("done", {n=1})
            raise("pkg.done", {n=2})
        "#,
        )
        .exec()
        .unwrap();

        let err = lua.load(r#"raise("missing.done", {})"#).exec().unwrap_err();
        assert!(err.to_string().contains("unknown namespace"), "got: {err}");

        let entries = buf.0.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].queue, "pkg.done");
        assert_eq!(entries[1].queue, "pkg.done");
    }

    #[test]
    fn empty_buffer_emits_nothing() {
        let buf = RaiseBuffer::new();
        // Just verify no panic; output goes to real stdout in tests so we can't capture cleanly.
        buf.emit_stdout();
    }
}

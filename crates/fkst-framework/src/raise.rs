//! SDK: `raise(queue_name, payload)` — buffers in-process; emit RAISED stdout line on exit.
//!
//! Raise is best-effort, at-most-once, and derived-only. Durable intent goes through filesystem.
//! Emit format = `RAISED: <base64-url-encoded JSON [{queue, payload}, ...]>`.

use base64::Engine;
use mlua::{Lua, LuaSerdeExt, Result};
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::sync::{Arc, Mutex};

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

pub fn register(lua: &Lua, buf: RaiseBuffer) -> Result<()> {
    let buf_clone = buf.clone();
    lua.globals().set(
        "raise",
        lua.create_function(move |lua, (queue, payload): (String, mlua::Value)| {
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

    #[test]
    fn raise_adds_to_buffer() {
        let lua = Lua::new();
        let buf = RaiseBuffer::new();
        register(&lua, buf.clone()).unwrap();
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
        register(&lua, buf.clone()).unwrap();
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
    fn empty_buffer_emits_nothing() {
        let buf = RaiseBuffer::new();
        // Just verify no panic; output goes to real stdout in tests so we can't capture cleanly.
        buf.emit_stdout();
    }
}

//! SDK: JSON helpers.
//!
//! JSON is the engine's own wire format: events arrive as JSON and `raise` emits JSON.
//! `json.decode` exposes the same serde_json + mlua LuaSerdeExt conversion already used
//! internally (see mlua_init.rs json_to_lua) so Lua handlers can parse JSON strings from
//! subprocess output, files, or external tools. Decode-only by deliberate surface boundary:
//! Lua values already leave the engine as JSON through `raise`.

use mlua::{Lua, LuaSerdeExt, Result};

pub fn register(lua: &Lua) -> Result<()> {
    let json = lua.create_table()?;
    json.set(
        "decode",
        lua.create_function(|lua, text: String| {
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| mlua::Error::external(format!("json.decode invalid-json: {e}")))?;
            lua.to_value(&value)
        })?,
    )?;
    lua.globals().set("json", json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    #[test]
    fn object_decodes_to_table() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let value: i64 = lua.load(r#"return json.decode('{"a":1}').a"#).eval().unwrap();
        assert_eq!(value, 1);
    }

    #[test]
    fn array_decodes_to_one_indexed_sequence() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let value: i64 = lua.load(r#"return json.decode('[1,2,3]')[2]"#).eval().unwrap();
        assert_eq!(value, 2);
    }

    #[test]
    fn scalars_decode_correctly() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let ok: bool = lua
            .load(
                r#"
                return json.decode('"x"') == "x"
                    and json.decode('42') == 42
                    and json.decode('true') == true
                "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn malformed_input_raises_json_decode_error() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let err = lua.load(r#"return json.decode("{bad")"#).eval::<mlua::Value>().unwrap_err();
        assert!(err.to_string().contains("json.decode invalid-json:"));
    }

    #[test]
    fn empty_string_raises_json_decode_error() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let err = lua.load(r#"return json.decode("")"#).eval::<mlua::Value>().unwrap_err();
        assert!(err.to_string().contains("json.decode invalid-json:"));
    }

    #[test]
    fn json_encode_is_not_registered() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let absent: bool = lua.load(r#"return json.encode == nil"#).eval().unwrap();
        assert!(absent);
    }

    #[test]
    fn nested_object_and_array_decode() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let value: i64 = lua
            .load(r#"return json.decode('{"a":{"b":[10,20,30]}}').a.b[2]"#)
            .eval()
            .unwrap();
        assert_eq!(value, 20);
    }

    #[test]
    fn json_null_decodes_to_serde_null_sentinel_not_lua_nil() {
        // `null` follows the same LuaSerdeExt conversion as event injection
        // (mlua_init::json_to_lua), yielding the null sentinel rather than Lua nil.
        let lua = Lua::new();
        register(&lua).unwrap();
        let is_lua_nil: bool = lua.load(r#"return json.decode("null") == nil"#).eval().unwrap();
        assert!(!is_lua_nil);
    }
}

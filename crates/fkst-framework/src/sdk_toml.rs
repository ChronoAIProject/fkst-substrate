//! SDK: TOML helpers.
//!
//! `toml.decode` mirrors the `json.decode` adapter shape: parse through the
//! engine-owned TOML parser and convert into Lua through LuaSerdeExt.

use mlua::{Lua, LuaSerdeExt, Result};

pub fn register(lua: &Lua) -> Result<()> {
    let toml = lua.create_table()?;
    toml.set(
        "decode",
        lua.create_function(|lua, text: String| {
            let value: toml::Value = toml::from_str(&text)
                .map_err(|e| mlua::Error::external(format!("toml.decode invalid-toml: {e}")))?;
            lua.to_value(&value)
        })?,
    )?;
    lua.globals().set("toml", toml)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    #[test]
    fn representative_values_decode_to_lua() {
        let lua = Lua::new();
        register(&lua).unwrap();

        let ok: bool = lua
            .load(
                r#"
                local result = toml.decode([=[
title = "example"
enabled = true
count = 42
ratio = 3.5
tags = ["sdk", "toml"]

[owner]
name = "framework"
]=])
                return result.title == "example"
                    and result.enabled == true
                    and result.count == 42
                    and result.ratio == 3.5
                    and result.tags[1] == "sdk"
                    and result.tags[2] == "toml"
                    and result.owner.name == "framework"
                "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn arrays_decode_to_one_indexed_sequences() {
        let lua = Lua::new();
        register(&lua).unwrap();

        let value: i64 = lua
            .load(r#"return toml.decode("items = [10, 20, 30]\n").items[2]"#)
            .eval()
            .unwrap();
        assert_eq!(value, 20);
    }

    #[test]
    fn table_decodes_to_nested_lua_table() {
        let lua = Lua::new();
        register(&lua).unwrap();

        lua.load(
            r#"
            local result = toml.decode("[a]\nb = 1\n")
            assert(result.a.b == 1)
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn empty_document_decodes_to_empty_table() {
        let lua = Lua::new();
        register(&lua).unwrap();

        let ok: bool = lua
            .load(
                r#"
                local result = toml.decode("")
                return type(result) == "table" and next(result) == nil
                "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn malformed_input_raises_toml_decode_error() {
        let lua = Lua::new();
        register(&lua).unwrap();

        let err = lua
            .load(r#"return toml.decode("title =")"#)
            .eval::<mlua::Value>()
            .unwrap_err();
        assert!(err.to_string().contains("toml.decode invalid-toml:"));
    }

    #[test]
    fn toml_encode_is_not_registered() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let absent: bool = lua.load(r#"return toml.encode == nil"#).eval().unwrap();
        assert!(absent);
    }
}

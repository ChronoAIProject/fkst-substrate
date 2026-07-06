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
    fn toml_encode_is_not_registered() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let absent: bool = lua.load(r#"return toml.encode == nil"#).eval().unwrap();
        assert!(absent);
    }
}

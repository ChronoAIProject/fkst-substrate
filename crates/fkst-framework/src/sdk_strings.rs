//! SDK: UTF-8 string helpers.

use mlua::{Lua, Result};

pub fn register(lua: &Lua) -> Result<()> {
    lua.globals().set(
        "truncate_utf8",
        lua.create_function(|_, (value, max_bytes): (mlua::String, i64)| {
            truncate_utf8(value, max_bytes)
        })?,
    )?;
    Ok(())
}

fn truncate_utf8(value: mlua::String, max_bytes: i64) -> Result<String> {
    if max_bytes < 0 {
        return Err(mlua::Error::external(
            "truncate_utf8 max_bytes must be non-negative",
        ));
    }

    let value = value
        .to_str()
        .map_err(|_| mlua::Error::external("truncate_utf8 input must be valid UTF-8"))?;
    let max_bytes = max_bytes as usize;
    if max_bytes >= value.len() {
        return Ok(value.to_string());
    }

    let end = value.floor_char_boundary(max_bytes);
    Ok(value[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_truncate(lua: &Lua, value: &str, max_bytes: i64) -> mlua::Result<String> {
        let truncate: mlua::Function = lua.globals().get("truncate_utf8").unwrap();
        truncate.call((lua.create_string(value)?, max_bytes))
    }

    #[test]
    fn ascii_within_at_and_over_cap() {
        let lua = Lua::new();
        register(&lua).unwrap();

        assert_eq!(eval_truncate(&lua, "abcdef", 3).unwrap(), "abc");
        assert_eq!(eval_truncate(&lua, "abcdef", 6).unwrap(), "abcdef");
        assert_eq!(eval_truncate(&lua, "abcdef", 9).unwrap(), "abcdef");
    }

    #[test]
    fn cjk_split_backs_off_to_previous_char() {
        let lua = Lua::new();
        register(&lua).unwrap();

        assert_eq!(eval_truncate(&lua, "标题", 4).unwrap(), "标");
    }

    #[test]
    fn emoji_boundary_keeps_complete_char() {
        let lua = Lua::new();
        register(&lua).unwrap();

        assert_eq!(eval_truncate(&lua, "a😀b", 5).unwrap(), "a😀");
    }

    #[test]
    fn cap_below_first_multibyte_char_returns_empty() {
        let lua = Lua::new();
        register(&lua).unwrap();

        assert_eq!(eval_truncate(&lua, "标", 2).unwrap(), "");
    }

    #[test]
    fn cap_zero_returns_empty() {
        let lua = Lua::new();
        register(&lua).unwrap();

        assert_eq!(eval_truncate(&lua, "abc", 0).unwrap(), "");
        assert_eq!(eval_truncate(&lua, "标", 0).unwrap(), "");
    }

    #[test]
    fn exact_boundary_equals_char_end_keeps_char() {
        let lua = Lua::new();
        register(&lua).unwrap();

        assert_eq!(eval_truncate(&lua, "标a", 3).unwrap(), "标");
    }

    #[test]
    fn invalid_utf8_input_returns_error() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let truncate: mlua::Function = lua.globals().get("truncate_utf8").unwrap();
        let invalid = lua.create_string([0xff]).unwrap();

        let err = truncate.call::<String>((invalid, 1)).unwrap_err();
        assert!(err.to_string().contains("input must be valid UTF-8"));
    }

    #[test]
    fn negative_max_bytes_returns_error() {
        let lua = Lua::new();
        register(&lua).unwrap();

        let err = eval_truncate(&lua, "abc", -1).unwrap_err();
        assert!(err.to_string().contains("max_bytes must be non-negative"));
    }
}

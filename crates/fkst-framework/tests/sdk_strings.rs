// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/sdk_strings.rs"]
mod sdk_strings;

use mlua::Lua;

#[test]
fn truncate_utf8_is_callable_from_lua() {
    let lua = Lua::new();
    sdk_strings::register(&lua).unwrap();

    let value: String = lua
        .load(r#"return truncate_utf8("issue 标题 😀", 12)"#)
        .eval()
        .unwrap();

    assert_eq!(value, "issue 标题");
}

#[test]
fn truncate_utf8_rejects_invalid_lua_string() {
    let lua = Lua::new();
    sdk_strings::register(&lua).unwrap();

    let err = lua
        .load(r#"return truncate_utf8(string.char(255), 1)"#)
        .eval::<String>()
        .unwrap_err();

    assert!(err.to_string().contains("input must be valid UTF-8"));
}

//! SDK: `log.info(msg)`, `log.warn(msg)`, `log.error(msg)` — all write to stderr.

use mlua::{Lua, Result};

pub fn register(lua: &Lua) -> Result<()> {
    let log = lua.create_table()?;
    log.set(
        "info",
        lua.create_function(|_, msg: String| {
            eprintln!("[info] {}", msg);
            Ok(())
        })?,
    )?;
    log.set(
        "warn",
        lua.create_function(|_, msg: String| {
            eprintln!("[warn] {}", msg);
            Ok(())
        })?,
    )?;
    log.set(
        "error",
        lua.create_function(|_, msg: String| {
            eprintln!("[error] {}", msg);
            Ok(())
        })?,
    )?;
    lua.globals().set("log", log)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    #[test]
    fn log_table_exists_after_register() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let log: mlua::Table = lua.globals().get("log").unwrap();
        assert!(log.get::<mlua::Function>("info").is_ok());
        assert!(log.get::<mlua::Function>("warn").is_ok());
        assert!(log.get::<mlua::Function>("error").is_ok());
    }

    #[test]
    fn log_info_callable_from_lua() {
        let lua = Lua::new();
        register(&lua).unwrap();
        // Just verify no error on call.
        lua.load(r#"log.info("test message")"#).exec().unwrap();
    }
}

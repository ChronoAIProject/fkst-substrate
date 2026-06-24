//! SDK: read-only durable delivery observe snapshot.

use fkst_common::DurableLayout;
use mlua::{Lua, LuaSerdeExt, Result, Table, Value};

pub(crate) fn register(lua: &Lua) -> Result<()> {
    let fkst = match lua.globals().get::<Value>("fkst")? {
        Value::Table(table) => table,
        Value::Nil => lua.create_table()?,
        _ => {
            return Err(mlua::Error::RuntimeError(
                "global fkst is not a table".to_string(),
            ))
        }
    };
    fkst.set(
        "observe",
        lua.create_function(move |lua, opts: Option<Table>| {
            let limit = observe_limit(opts)?;
            let layout = DurableLayout::from_env().map_err(mlua::Error::external)?;
            let snapshot = crate::observe::snapshot(layout.durable_root().to_path_buf(), limit)
                .map_err(mlua::Error::external)?;
            lua.to_value(&snapshot)
        })?,
    )?;
    lua.globals().set("fkst", fkst)?;
    Ok(())
}

fn observe_limit(opts: Option<Table>) -> Result<usize> {
    let Some(opts) = opts else {
        return Ok(crate::observe::default_limit());
    };
    let value: Value = opts.get("limit")?;
    match value {
        Value::Nil => Ok(crate::observe::default_limit()),
        Value::Integer(value) if value >= 0 => {
            crate::observe::validate_limit(value as usize).map_err(mlua::Error::external)
        }
        Value::Number(value) if value.is_finite() && value.fract() == 0.0 && value >= 0.0 => {
            crate::observe::validate_limit(value as usize).map_err(mlua::Error::external)
        }
        _ => Err(mlua::Error::external(
            "observe limit must be a positive integer",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_limit_defaults_without_options() {
        assert_eq!(observe_limit(None).unwrap(), 500);
    }

    #[test]
    fn observe_limit_rejects_zero() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("limit", 0).unwrap();
        let err = observe_limit(Some(opts)).unwrap_err();
        assert!(err.to_string().contains("observe limit must be between"));
    }
}

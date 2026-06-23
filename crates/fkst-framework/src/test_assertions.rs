use mlua::{Function, Lua, Table, Value};

pub(crate) fn register(lua: &Lua, test: &Table) -> mlua::Result<()> {
    test.set(
        "eq",
        lua.create_function(
            |lua, (actual, expected, msg): (Value, Value, Option<String>)| {
                if lua_values_equal(lua, actual.clone(), expected.clone())? {
                    return Ok(());
                }
                Err(assertion_error(
                    "eq",
                    msg,
                    format!(
                        "expected {}, got {}",
                        display_value(expected),
                        display_value(actual)
                    ),
                ))
            },
        )?,
    )?;
    test.set(
        "is_true",
        lua.create_function(|_, (value, msg): (Value, Option<String>)| {
            if !matches!(value, Value::Nil | Value::Boolean(false)) {
                return Ok(());
            }
            Err(assertion_error(
                "is_true",
                msg,
                format!("got {}", display_value(value)),
            ))
        })?,
    )?;
    test.set(
        "raises",
        lua.create_function(|_, (func, msg): (Function, Option<String>)| {
            match func.call::<()>(()) {
                Ok(()) => Err(assertion_error(
                    "raises",
                    msg,
                    "function did not raise".to_string(),
                )),
                Err(_) => Ok(()),
            }
        })?,
    )?;
    test.set(
        "is_nil",
        lua.create_function(|_, (value, msg): (Value, Option<String>)| {
            if matches!(value, Value::Nil) {
                return Ok(());
            }
            Err(assertion_error(
                "is_nil",
                msg,
                format!("got {}", display_value(value)),
            ))
        })?,
    )?;
    Ok(())
}

fn lua_values_equal(lua: &Lua, actual: Value, expected: Value) -> mlua::Result<bool> {
    lua.globals()
        .get::<Function>("rawequal")?
        .call::<bool>((actual, expected))
}

fn assertion_error(name: &str, msg: Option<String>, detail: String) -> mlua::Error {
    let prefix = match msg {
        Some(msg) if !msg.is_empty() => format!("{name}: {msg}: "),
        _ => format!("{name}: "),
    };
    mlua::Error::runtime(format!("{prefix}{detail}"))
}

fn display_value(value: Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(value) => value.to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => match value.to_str() {
            Ok(value) => format!("{value:?}"),
            Err(_) => "<non-utf8-string>".to_string(),
        },
        Value::Table(_) => "<table>".to_string(),
        Value::Function(_) => "<function>".to_string(),
        Value::Thread(_) => "<thread>".to_string(),
        Value::UserData(_) => "<userdata>".to_string(),
        Value::LightUserData(_) => "<lightuserdata>".to_string(),
        Value::Error(err) => format!("<error:{err}>"),
        Value::Other(_) => "<other>".to_string(),
    }
}

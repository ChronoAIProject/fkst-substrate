//! SDK: host-owned restricted Lua source loading.
//!
//! The primitive evaluates small declarative Lua sources in a fresh Lua VM with no
//! standard libraries loaded beyond the base library that Lua needs to execute a
//! chunk. Callers grant capabilities explicitly through `bindings`; returned data is
//! copied back only when it is plain scalar/table data.

use std::collections::{BTreeSet, HashSet};

use mlua::{
    ChunkMode, Function, Lua, LuaOptions, LuaSerdeExt, MultiValue, Result, StdLib, Table, Value,
};

const DEFAULT_MODE: &str = "text";

const FORBIDDEN_BINDING_KEYS: &[&str] = &[
    "_G",
    "coroutine",
    "debug",
    "dofile",
    "getmetatable",
    "io",
    "load",
    "loadfile",
    "loadstring",
    "module",
    "os",
    "package",
    "rawequal",
    "rawget",
    "rawlen",
    "rawset",
    "require",
    "setmetatable",
    "string",
];

pub fn register(lua: &Lua) -> Result<()> {
    lua.globals().set(
        "restricted_lua_load",
        lua.create_function(|lua, opts: Table| restricted_lua_load(lua, opts))?,
    )?;
    Ok(())
}

fn restricted_lua_load(host_lua: &Lua, opts: Table) -> Result<Value> {
    let source: mlua::String = opts.get("source").map_err(|err| {
        mlua::Error::external(format!("restricted_lua invalid-options: source: {err}"))
    })?;
    let source = source.as_bytes().to_vec();
    let mode = parse_mode(opts.get::<Option<String>>("mode")?)?;
    let name = opts
        .get::<Option<String>>("name")?
        .unwrap_or_else(|| "restricted_lua".to_string());
    let bindings = opts.get::<Option<Table>>("bindings")?;

    let sandbox = Lua::new_with(StdLib::NONE, LuaOptions::new())
        .map_err(|err| mlua::Error::external(format!("restricted_lua init-error: {err}")))?;
    let env = sandbox.create_table()?;

    if let Some(bindings) = bindings {
        let mut visited = HashSet::new();
        copy_bindings_table(host_lua, &sandbox, &bindings, &env, &mut visited)?;
    }

    let value = sandbox
        .load(source)
        .set_name(name)
        .set_mode(mode)
        .set_environment(env)
        .eval::<Value>()
        .map_err(|err| {
            let class = if is_syntax_error(&err) {
                "compile-error"
            } else {
                "runtime-error"
            };
            mlua::Error::external(format!("restricted_lua {class}: {err}"))
        })?;

    let mut visited = HashSet::new();
    copy_plain_value(&sandbox, host_lua, value, "return", &mut visited)
        .map_err(|err| mlua::Error::external(format!("restricted_lua invalid-return: {err}")))
}

fn parse_mode(mode: Option<String>) -> Result<ChunkMode> {
    match mode.as_deref().unwrap_or(DEFAULT_MODE) {
        "text" => Ok(ChunkMode::Text),
        "bytecode" => Ok(ChunkMode::Binary),
        other => Err(mlua::Error::external(format!(
            "restricted_lua invalid-options: mode must be `text` or `bytecode`, got `{other}`"
        ))),
    }
}

fn is_syntax_error(err: &mlua::Error) -> bool {
    match err {
        mlua::Error::SyntaxError { .. } => true,
        mlua::Error::CallbackError { cause, .. } => is_syntax_error(cause),
        _ => false,
    }
}

fn copy_bindings_table(
    host_lua: &Lua,
    sandbox: &Lua,
    from: &Table,
    to: &Table,
    visited: &mut HashSet<*const std::ffi::c_void>,
) -> Result<()> {
    let pointer = from.to_pointer();
    if !visited.insert(pointer) {
        return Err(mlua::Error::external(
            "restricted_lua invalid-bindings: bindings table is recursive",
        ));
    }

    for pair in from.pairs::<Value, Value>() {
        let (key, value) = pair?;
        validate_binding_key(&key)?;
        let copied_key =
            copy_plain_value(host_lua, sandbox, key, "binding key", visited).map_err(|err| {
                mlua::Error::external(format!("restricted_lua invalid-bindings: {err}"))
            })?;
        let copied_value = copy_binding_value(host_lua, sandbox, value, visited)?;
        to.raw_set(copied_key, copied_value)?;
    }

    visited.remove(&pointer);
    Ok(())
}

fn validate_binding_key(key: &Value) -> Result<()> {
    let Value::String(name) = key else {
        return Ok(());
    };
    let Ok(name) = name.to_str() else {
        return Err(mlua::Error::external(
            "restricted_lua invalid-bindings: binding keys must be valid UTF-8 when strings",
        ));
    };
    if FORBIDDEN_BINDING_KEYS
        .iter()
        .any(|forbidden| name.as_ref() == *forbidden)
    {
        return Err(mlua::Error::external(format!(
            "restricted_lua invalid-bindings: binding `{name}` is reserved"
        )));
    }
    Ok(())
}

fn copy_binding_value(
    host_lua: &Lua,
    sandbox: &Lua,
    value: Value,
    visited: &mut HashSet<*const std::ffi::c_void>,
) -> Result<Value> {
    match value {
        Value::Function(function) => bridge_function(host_lua, sandbox, function),
        other => copy_plain_value(host_lua, sandbox, other, "binding", visited).map_err(|err| {
            mlua::Error::external(format!("restricted_lua invalid-bindings: {err}"))
        }),
    }
}

fn bridge_function(host_lua: &Lua, sandbox: &Lua, function: Function) -> Result<Value> {
    let host_lua = host_lua.clone();
    let bridged = sandbox.create_function(move |sandbox_lua, args: MultiValue| {
        let mut to_host_visited = HashSet::new();
        let host_args = args
            .into_iter()
            .enumerate()
            .map(|(idx, value)| {
                copy_plain_value(
                    sandbox_lua,
                    &host_lua,
                    value,
                    &format!("function argument {}", idx + 1),
                    &mut to_host_visited,
                )
            })
            .collect::<Result<Vec<_>>>()
            .map_err(|err| mlua::Error::external(format!("restricted_lua invalid-call: {err}")))?;

        let result = function.call::<MultiValue>(MultiValue::from_vec(host_args));
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                return Err(mlua::Error::external(format!(
                    "restricted_lua runtime-error: granted binding failed: {}",
                    compact_error(&err)
                )));
            }
        };

        let mut to_sandbox_visited = HashSet::new();
        let copied = result
            .into_iter()
            .enumerate()
            .map(|(idx, value)| {
                copy_plain_value(
                    &host_lua,
                    sandbox_lua,
                    value,
                    &format!("function return {}", idx + 1),
                    &mut to_sandbox_visited,
                )
            })
            .collect::<Result<Vec<_>>>()
            .map_err(|err| {
                mlua::Error::external(format!("restricted_lua invalid-call-return: {err}"))
            })?;
        Ok(MultiValue::from_vec(copied))
    })?;
    Ok(Value::Function(bridged))
}

fn copy_plain_value(
    from_lua: &Lua,
    to_lua: &Lua,
    value: Value,
    context: &str,
    visited: &mut HashSet<*const std::ffi::c_void>,
) -> Result<Value> {
    match value {
        Value::Nil => Ok(Value::Nil),
        Value::Boolean(value) => Ok(Value::Boolean(value)),
        Value::Integer(value) => Ok(Value::Integer(value)),
        Value::Number(value) => Ok(Value::Number(value)),
        Value::String(value) => Ok(Value::String(to_lua.create_string(value.as_bytes())?)),
        Value::Table(table) => copy_plain_table(from_lua, to_lua, table, context, visited),
        other => Err(mlua::Error::external(format!(
            "{context} contains unsupported {}",
            other.type_name()
        ))),
    }
}

fn copy_plain_table(
    _from_lua: &Lua,
    to_lua: &Lua,
    table: Table,
    context: &str,
    visited: &mut HashSet<*const std::ffi::c_void>,
) -> Result<Value> {
    let pointer = table.to_pointer();
    if !visited.insert(pointer) {
        return Err(mlua::Error::external(format!(
            "{context} contains recursive table"
        )));
    }

    let to_table = to_lua.create_table()?;
    let mut int_keys = BTreeSet::new();
    let mut pair_count = 0usize;
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        pair_count += 1;
        if let Value::Integer(i) = key {
            if i > 0 {
                int_keys.insert(i as usize);
            }
            let copied_value = copy_plain_value(_from_lua, to_lua, value, context, visited)?;
            to_table.raw_set(i, copied_value)?;
        } else {
            let copied_key = copy_plain_value(_from_lua, to_lua, key, context, visited)?;
            let copied_value = copy_plain_value(_from_lua, to_lua, value, context, visited)?;
            to_table.raw_set(copied_key, copied_value)?;
        }
    }

    if pair_count > 0 && int_keys.len() == pair_count {
        let expected = 1..=pair_count;
        if int_keys.iter().copied().eq(expected) {
            to_table.set_metatable(Some(to_lua.array_metatable()));
        }
    }

    visited.remove(&pointer);
    Ok(Value::Table(to_table))
}

fn compact_error(err: &mlua::Error) -> String {
    let mut msg = err.to_string();
    msg = msg.replace('\n', " ");
    if msg.len() > 240 {
        let mut end = 240;
        while !msg.is_char_boundary(end) {
            end -= 1;
        }
        msg.truncate(end);
        msg.push_str("...");
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(source: &str) -> Result<(Lua, Value)> {
        let lua = Lua::new();
        register(&lua)?;
        let value = lua
            .load("return restricted_lua_load(...)")
            .call(lua.create_table_from([("source", source)])?)?;
        Ok((lua, value))
    }

    fn err(source: &str) -> String {
        eval(source).unwrap_err().to_string()
    }

    #[test]
    fn allows_plain_declarative_table_return() {
        let (_lua, value) = eval(
            r#"
            return {
              name = "gate",
              enabled = true,
              threshold = 2,
              terms = { "a", "b" },
            }
            "#,
        )
        .unwrap();
        let Value::Table(table) = value else {
            panic!("expected table");
        };
        assert_eq!(table.get::<String>("name").unwrap(), "gate");
        assert!(table.get::<bool>("enabled").unwrap());
        assert_eq!(table.get::<i64>("threshold").unwrap(), 2);
        let terms: Table = table.get("terms").unwrap();
        assert_eq!(terms.get::<String>(2).unwrap(), "b");
    }

    #[test]
    fn forbidden_ambient_capabilities_are_unreachable() {
        let (_lua, value) = eval(
            r#"
            return {
              require = require,
              load = load,
              loadstring = loadstring,
              global = _G,
              debug = debug,
              package = package,
              rawget = rawget,
              rawset = rawset,
              rawequal = rawequal,
              rawlen = rawlen,
              getmetatable = getmetatable,
              setmetatable = setmetatable,
              io = io,
              os = os,
              coroutine = coroutine,
              string_dump = string and string.dump,
            }
            "#,
        )
        .unwrap();
        let Value::Table(table) = value else {
            panic!("expected table");
        };
        for key in [
            "require",
            "load",
            "loadstring",
            "global",
            "debug",
            "package",
            "rawget",
            "rawset",
            "rawequal",
            "rawlen",
            "getmetatable",
            "setmetatable",
            "io",
            "os",
            "coroutine",
            "string_dump",
        ] {
            assert!(
                matches!(table.raw_get::<Value>(key).unwrap(), Value::Nil),
                "{key}"
            );
        }

        let value_metatable = err(r#"return ("").dump"#);
        assert!(
            value_metatable.contains("restricted_lua runtime-error:"),
            "{value_metatable}"
        );
    }

    #[test]
    fn text_mode_rejects_bytecode_by_default() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let bytecode = lua.load("return 42").into_function().unwrap().dump(false);
        let opts = lua
            .create_table_from([("source", lua.create_string(bytecode).unwrap())])
            .unwrap();
        let err = lua
            .load("return restricted_lua_load(...)")
            .call::<Value>(opts)
            .unwrap_err()
            .to_string();
        assert!(err.contains("restricted_lua compile-error:"), "{err}");
    }

    #[test]
    fn bytecode_mode_is_explicit() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let bytecode = lua.load("return 42").into_function().unwrap().dump(false);
        let opts = lua.create_table().unwrap();
        opts.set("source", lua.create_string(bytecode).unwrap())
            .unwrap();
        opts.set("mode", "bytecode").unwrap();

        let value: i64 = lua
            .load("return restricted_lua_load(...)")
            .call(opts)
            .unwrap();
        assert_eq!(value, 42);
    }

    #[test]
    fn compile_and_runtime_errors_are_classified() {
        let compile = err("return {");
        assert!(
            compile.contains("restricted_lua compile-error:"),
            "{compile}"
        );

        let runtime = err("error('boom')");
        assert!(
            runtime.contains("restricted_lua runtime-error:"),
            "{runtime}"
        );
    }

    #[test]
    fn binding_names_cannot_grant_forbidden_ambient_keys() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let bindings = lua.create_table_from([("require", 1)]).unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("source", "return require").unwrap();
        opts.set("bindings", bindings).unwrap();

        let err = lua
            .load("return restricted_lua_load(...)")
            .call::<Value>(opts)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("restricted_lua invalid-bindings: binding `require` is reserved"),
            "{err}"
        );
    }

    #[test]
    fn explicit_function_bindings_are_capabilities() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let bindings = lua.create_table().unwrap();
        bindings
            .set(
                "plus_one",
                lua.create_function(|_, value: i64| Ok(value + 1)).unwrap(),
            )
            .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("source", "return plus_one(41)").unwrap();
        opts.set("bindings", bindings).unwrap();

        let value: i64 = lua
            .load("return restricted_lua_load(...)")
            .call(opts)
            .unwrap();
        assert_eq!(value, 42);
    }

    #[test]
    fn returned_functions_are_rejected() {
        let err = err("return function() end");
        assert!(
            err.contains("restricted_lua invalid-return: return contains unsupported function"),
            "{err}"
        );
    }

    #[test]
    fn recursive_return_tables_are_rejected() {
        let err = err("local t = {}; t.self = t; return t");
        assert!(
            err.contains("restricted_lua invalid-return: return contains recursive table"),
            "{err}"
        );
    }

    #[test]
    fn restricted_load_does_not_mutate_host_string_metatable() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let dump_is_available: bool = lua.load(r#"return ("").dump ~= nil"#).eval().unwrap();
        assert!(dump_is_available);

        let err = lua
            .load("return restricted_lua_load({ source = [[ return ('').dump ]] })")
            .eval::<Value>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("restricted_lua runtime-error:"), "{err}");

        let dump_is_still_available: bool = lua.load(r#"return ("").dump ~= nil"#).eval().unwrap();
        assert!(dump_is_still_available);
    }

    #[test]
    fn granted_binding_errors_are_bounded_on_utf8_boundary() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let bindings = lua.create_table().unwrap();
        bindings
            .set(
                "explode",
                lua.create_function(|_, ()| Err::<(), _>(mlua::Error::external("é".repeat(300))))
                    .unwrap(),
            )
            .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("source", "return explode()").unwrap();
        opts.set("bindings", bindings).unwrap();

        let err = lua
            .load("return restricted_lua_load(...)")
            .call::<Value>(opts)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("restricted_lua runtime-error: granted binding failed:"),
            "{err}"
        );
        assert!(err.contains("..."), "{err}");
    }
}

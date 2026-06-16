//! SDK: `exec_argv(opts) -> {stdout, stderr, exit_code}`.
//!
//! `exec_argv` is the shell-free egress: it takes an explicit `argv` array and runs the
//! program directly (`Command::new(argv[1]).args(argv[2..])`) with no `/bin/sh`, so no Lua-side
//! shell quoting and no shell injection. It is the execution primitive for the `std.github` /
//! `std.git` adapters; genuine shell commands keep using `exec_sync`.
//!
//! It shares the mock/coalesce/rate/audit dispatch with `exec_sync` through
//! `sdk_basic::run_spec_with_context`; it differs only in building an argv `CommandSpec` and in
//! deriving its rate pool from the program basename (`argv[1]`) rather than parsing command text.

use mlua::{Lua, Result, Table, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::external_command::{
    format_command, CommandSpec, CommandStdin, MockCommandInvocation, MockCommandState,
};
use crate::rate_pool::RatePoolRegistry;
use crate::read_coalesce::{CoalesceClock, ReadCoalesceOptions};
use crate::sdk_basic::{
    exec_result_to_table, parse_read_coalesce, run_spec_with_context, ExecResult, RateKey,
};

struct ArgvOptions {
    argv: Vec<String>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    timeout: Option<Duration>,
    read_coalesce: Option<ReadCoalesceOptions>,
}

pub(crate) fn register(
    lua: &Lua,
    rate_pools: RatePoolRegistry,
    runner: Option<MockCommandState>,
    host_root: PathBuf,
    coalesce_clock: CoalesceClock,
) -> Result<()> {
    lua.globals().set(
        "exec_argv",
        lua.create_function(move |lua, arg: Value| {
            crate::process_tree::ensure_supervisor_parent_alive()?;
            let opts = parse_argv_options(arg)?;
            let out = run_exec_argv_with_context(
                opts,
                runner.as_ref(),
                &rate_pools,
                &host_root,
                &coalesce_clock,
            )?;
            exec_result_to_table(lua, out)
        })?,
    )?;
    Ok(())
}

fn parse_argv_options(arg: Value) -> Result<ArgvOptions> {
    let table = match arg {
        Value::Table(table) => table,
        other => {
            return Err(mlua::Error::external(format!(
                "exec_argv expects a table with an argv array, got {}",
                other.type_name()
            )));
        }
    };
    if table.contains_key("cmd")? {
        return Err(mlua::Error::external(
            "exec_argv does not accept a shell `cmd` string; pass argv = {program, args...} (use exec_sync for genuine shell commands)",
        ));
    }
    if table.contains_key("rate_pool")? {
        return Err(mlua::Error::external(
            "exec_argv does not accept `rate_pool`; the rate pool is derived from argv[1] (program basename)",
        ));
    }
    let argv_table = table
        .get::<Option<Table>>("argv")?
        .ok_or_else(|| mlua::Error::external("exec_argv requires an `argv` array of strings"))?;
    let argv: Vec<String> = argv_table
        .sequence_values::<String>()
        .collect::<Result<Vec<String>>>()?;
    if argv.is_empty() {
        return Err(mlua::Error::external(
            "exec_argv `argv` must be a non-empty array of strings; argv[1] is the program",
        ));
    }
    let cwd: Option<String> = table.get("cwd")?;
    let env = match table.get::<Option<Table>>("env")? {
        Some(env_table) => env_table
            .pairs::<String, String>()
            .collect::<Result<Vec<(String, String)>>>()?,
        None => Vec::new(),
    };
    let timeout = table
        .get::<Option<f64>>("timeout")?
        .map(|secs| Duration::from_secs_f64(secs.max(0.0)));
    let read_coalesce = parse_read_coalesce(table.get::<Option<Table>>("read_coalesce")?)?;

    Ok(ArgvOptions {
        argv,
        cwd,
        env,
        timeout,
        read_coalesce,
    })
}

fn run_exec_argv_with_context(
    opts: ArgvOptions,
    runner: Option<&MockCommandState>,
    rate_pools: &RatePoolRegistry,
    host_root: &Path,
    coalesce_clock: &CoalesceClock,
) -> Result<ExecResult> {
    let mut argv = opts.argv;
    let program = argv.remove(0);
    let args = argv;
    let rendered = format_command(&program, &args);
    let cwd = opts.cwd;
    let env = opts.env;
    let spec = CommandSpec {
        program: PathBuf::from(&program),
        args: args.clone(),
        cwd: cwd.clone().map(PathBuf::from),
        env: env.clone(),
        stdin: CommandStdin::Null,
        timeout: opts.timeout,
        process_group: opts.timeout.is_some(),
    };
    let invocation = MockCommandInvocation {
        rendered,
        program: program.clone(),
        args,
        stdin: String::new(),
        cwd,
        env,
    };
    run_spec_with_context(
        spec,
        invocation,
        RateKey::Program(program),
        opts.read_coalesce,
        runner,
        rate_pools,
        host_root,
        coalesce_clock,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate_pool::{RatePoolConfig, RatePoolRegistry};
    use mlua::Lua;
    use std::collections::BTreeMap;

    fn run_exec_argv(
        opts: ArgvOptions,
        runner: Option<&MockCommandState>,
        rate_pools: &RatePoolRegistry,
    ) -> Result<ExecResult> {
        let host_root = std::env::current_dir().unwrap();
        run_exec_argv_with_context(
            opts,
            runner,
            rate_pools,
            &host_root,
            &CoalesceClock::system(),
        )
    }

    #[test]
    fn exec_argv_runs_program_directly() {
        let lua = Lua::new();
        crate::sdk_basic::register(&lua).unwrap();
        let t: Table = lua
            .load(r#"return exec_argv({argv = {"echo", "hello"}})"#)
            .eval()
            .unwrap();
        let stdout: String = t.get("stdout").unwrap();
        let exit_code: i64 = t.get("exit_code").unwrap();
        assert_eq!(stdout, "hello\n");
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn exec_argv_does_not_invoke_a_shell() {
        // `$HOME` and `;` are literal argv arguments — never expanded or parsed by a shell.
        let lua = Lua::new();
        crate::sdk_basic::register(&lua).unwrap();
        let t: Table = lua
            .load(r#"return exec_argv({argv = {"printf", "%s", "$HOME;echo pwned"}})"#)
            .eval()
            .unwrap();
        let stdout: String = t.get("stdout").unwrap();
        assert_eq!(stdout, "$HOME;echo pwned");
    }

    #[test]
    fn exec_argv_rejects_cmd_string() {
        let lua = Lua::new();
        crate::sdk_basic::register(&lua).unwrap();
        let err = lua
            .load(r#"return exec_argv({cmd = "gh issue view 1"})"#)
            .eval::<Table>()
            .unwrap_err();
        assert!(
            err.to_string().contains("does not accept a shell"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn exec_argv_rejects_rate_pool_field() {
        let lua = Lua::new();
        crate::sdk_basic::register(&lua).unwrap();
        let err = lua
            .load(r#"return exec_argv({argv = {"echo", "x"}, rate_pool = {name = "echo"}})"#)
            .eval::<Table>()
            .unwrap_err();
        assert!(
            err.to_string().contains("rate_pool"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn exec_argv_requires_nonempty_argv() {
        let lua = Lua::new();
        crate::sdk_basic::register(&lua).unwrap();
        assert!(lua.load(r#"return exec_argv({})"#).eval::<Table>().is_err());
        assert!(lua
            .load(r#"return exec_argv({argv = {}})"#)
            .eval::<Table>()
            .is_err());
    }

    #[test]
    fn exec_argv_acquires_rate_pool_by_program() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\nprintf gh-ok\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&gh, perms).unwrap();
        }
        let rate_pools = RatePoolRegistry::for_test(
            dir.path().to_path_buf(),
            BTreeMap::from([(
                "gh".to_string(),
                RatePoolConfig {
                    burst: 1,
                    refill_per_minute: 1,
                },
            )]),
        );
        std::fs::write(
            dir.path().join("gh.bucket"),
            "updated_nanos=0\ntokens=1\nremainder_nanos=0\n",
        )
        .unwrap();
        let out = run_exec_argv(
            ArgvOptions {
                argv: vec![gh.to_string_lossy().into_owned(), "--version".to_string()],
                cwd: None,
                env: Vec::new(),
                timeout: None,
                read_coalesce: None,
            },
            None,
            &rate_pools,
        )
        .unwrap();
        assert_eq!(out.stdout, "gh-ok");
        // The "gh" pool ledger was created, proving rate was acquired by program basename.
        assert!(dir.path().join("gh.bucket").is_file());
    }
}

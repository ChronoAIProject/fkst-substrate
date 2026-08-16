//! SDK: `env_read(name) -> string`.

use mlua::{Lua, Result};

use crate::external_command::{
    MockCommandInvocation, MockCommandPlan, MockCommandResult, MockCommandState,
};

pub(crate) fn register(lua: &Lua) -> Result<()> {
    register_with_runner(lua, None)
}

pub(crate) fn register_with_runner(lua: &Lua, runner: Option<MockCommandState>) -> Result<()> {
    register_with_runner_and_parent_check(
        lua,
        runner,
        crate::process_tree::ensure_supervisor_parent_alive,
    )
}

fn register_with_runner_and_parent_check(
    lua: &Lua,
    runner: Option<MockCommandState>,
    parent_alive: fn() -> Result<()>,
) -> Result<()> {
    lua.globals().set(
        "env_read",
        lua.create_function(move |_, name: String| {
            parent_alive()?;
            let command = format!(r#"printf %s "${name}""#);
            if let Some(runner) = runner.as_ref() {
                let invocation = MockCommandInvocation {
                    rendered: command.clone(),
                    program: "/bin/sh".to_string(),
                    args: vec!["-c".to_string(), command],
                    stdin: String::new(),
                    cwd: None,
                    env: Vec::new(),
                };
                return match runner.prepare(invocation.clone())? {
                    MockCommandPlan::Return(result) => Ok(mock_env_value(result)),
                    MockCommandPlan::Record => {
                        let value = process_env_value(&name);
                        runner.record(
                            invocation,
                            MockCommandResult {
                                stdout: value.clone(),
                                stderr: String::new(),
                                exit_code: 0,
                            },
                        )?;
                        Ok(value)
                    }
                };
            }

            Ok(process_env_value(&name))
        })?,
    )?;
    Ok(())
}

fn process_env_value(name: &str) -> String {
    std::env::var_os(name)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn mock_env_value(result: MockCommandResult) -> String {
    if result.exit_code == 0 {
        result.stdout
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{register, register_with_runner, register_with_runner_and_parent_check};
    use crate::external_command::{
        CommandCassetteMode, CommandCassetteOptions, MockCommandResult, MockCommandState,
    };
    use mlua::Lua;

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn env_read_consumes_matching_mock_and_records_command_call() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set("FKST_ENV_READ_TEST", "process-value");
        let lua = Lua::new();
        let runner = MockCommandState::new();
        runner
            .push_mock(
                r#"printf %s "$FKST_ENV_READ_TEST""#.to_string(),
                MockCommandResult {
                    stdout: "mock-value".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                },
            )
            .unwrap();

        register_with_runner(&lua, Some(runner.clone())).unwrap();
        let result: String = lua
            .load(r#"return env_read("FKST_ENV_READ_TEST")"#)
            .eval()
            .unwrap();

        assert_eq!(result, "mock-value");
        let calls = runner.calls().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].rendered, r#"printf %s "$FKST_ENV_READ_TEST""#);
        assert_eq!(calls[0].program, "/bin/sh");
        assert_eq!(
            calls[0].args,
            vec![
                "-c".to_string(),
                r#"printf %s "$FKST_ENV_READ_TEST""#.to_string(),
            ]
        );
    }

    #[test]
    fn env_read_rejects_an_orphaned_department_before_reading_environment() {
        fn parent_lost() -> mlua::Result<()> {
            Err(mlua::Error::external("supervisor parent lost: test parent"))
        }

        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set("FKST_ENV_READ_TEST", "must-not-read");
        let lua = Lua::new();
        register_with_runner_and_parent_check(&lua, None, parent_lost).unwrap();

        let error = lua
            .load(r#"return env_read("FKST_ENV_READ_TEST")"#)
            .eval::<String>()
            .unwrap_err()
            .to_string();

        assert!(error.contains("supervisor parent lost: test parent"));
        assert!(!error.contains("must-not-read"));
    }

    #[test]
    fn env_read_without_matching_mock_fails_closed() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set("FKST_ENV_READ_TEST", "process-value");
        let lua = Lua::new();
        let runner = MockCommandState::new();

        register_with_runner(&lua, Some(runner.clone())).unwrap();
        let error = lua
            .load(r#"return env_read("FKST_ENV_READ_TEST")"#)
            .eval::<String>()
            .unwrap_err()
            .to_string();

        assert!(error.contains(r#"unmocked external command: printf %s "$FKST_ENV_READ_TEST""#));
        assert!(runner.calls().unwrap().is_empty());
    }

    #[test]
    fn env_read_without_runner_reads_process_env() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set("FKST_ENV_READ_TEST", "process-value");
        let lua = Lua::new();

        register(&lua).unwrap();
        let result: String = lua
            .load(r#"return env_read("FKST_ENV_READ_TEST")"#)
            .eval()
            .unwrap();

        assert_eq!(result, "process-value");
    }

    #[test]
    fn env_read_maps_nonzero_mock_to_empty_string() {
        let lua = Lua::new();
        let runner = MockCommandState::new();
        runner
            .push_mock(
                r#"printf %s "$FKST_ENV_READ_TEST""#.to_string(),
                MockCommandResult {
                    stdout: "must-not-leak".to_string(),
                    stderr: "failure".to_string(),
                    exit_code: 1,
                },
            )
            .unwrap();

        register_with_runner(&lua, Some(runner.clone())).unwrap();
        let result: String = lua
            .load(r#"return env_read("FKST_ENV_READ_TEST")"#)
            .eval()
            .unwrap();

        assert_eq!(result, "");
        assert_eq!(runner.calls().unwrap().len(), 1);
    }

    #[test]
    fn env_read_records_and_replays_command_cassette() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set("FKST_ENV_READ_TEST", "recorded-value");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env.json");

        let record_lua = Lua::new();
        let record_runner = MockCommandState::new();
        record_runner
            .start_cassette(CommandCassetteOptions {
                path: path.clone(),
                mode: CommandCassetteMode::Record,
                redactions: Vec::new(),
            })
            .unwrap();
        register_with_runner(&record_lua, Some(record_runner.clone())).unwrap();
        let recorded: String = record_lua
            .load(r#"return env_read("FKST_ENV_READ_TEST")"#)
            .eval()
            .unwrap();
        assert_eq!(recorded, "recorded-value");
        record_runner.finish_cassette().unwrap();

        std::env::set_var("FKST_ENV_READ_TEST", "changed-value");
        let replay_lua = Lua::new();
        let replay_runner = MockCommandState::new();
        replay_runner
            .start_cassette(CommandCassetteOptions {
                path,
                mode: CommandCassetteMode::Replay,
                redactions: Vec::new(),
            })
            .unwrap();
        register_with_runner(&replay_lua, Some(replay_runner.clone())).unwrap();
        let replayed: String = replay_lua
            .load(r#"return env_read("FKST_ENV_READ_TEST")"#)
            .eval()
            .unwrap();
        assert_eq!(replayed, "recorded-value");
        replay_runner.finish_cassette().unwrap();
    }
}

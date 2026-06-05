use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub(crate) struct MockCommandState {
    inner: Arc<Mutex<MockCommandInner>>,
}

#[derive(Debug, Default)]
struct MockCommandInner {
    mocks: Vec<MockCommand>,
    calls: Vec<MockCommandCall>,
}

#[derive(Clone, Debug)]
struct MockCommand {
    pattern: String,
    result: MockCommandResult,
}

#[derive(Clone, Debug)]
pub(crate) struct MockCommandResult {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
}

#[derive(Clone, Debug)]
pub(crate) struct MockCommandCall {
    pub(crate) rendered: String,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) stdin: String,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
}

impl MockCommandState {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockCommandInner::default())),
        }
    }

    pub(crate) fn reset(&self) -> mlua::Result<()> {
        let mut inner = self.lock()?;
        inner.mocks.clear();
        inner.calls.clear();
        Ok(())
    }

    pub(crate) fn push_mock(&self, pattern: String, result: MockCommandResult) -> mlua::Result<()> {
        let mut inner = self.lock()?;
        inner.mocks.push(MockCommand { pattern, result });
        Ok(())
    }

    pub(crate) fn calls(&self) -> mlua::Result<Vec<MockCommandCall>> {
        let inner = self.lock()?;
        Ok(inner.calls.clone())
    }

    pub(crate) fn execute(
        &self,
        rendered: String,
        program: String,
        args: Vec<String>,
        stdin: String,
    ) -> mlua::Result<MockCommandResult> {
        let mut inner = self.lock()?;
        let index = inner
            .mocks
            .iter()
            .position(|mock| {
                rendered.starts_with(&mock.pattern) || rendered.contains(&mock.pattern)
            })
            .ok_or_else(|| {
                mlua::Error::external(format!("unmocked external command: {rendered}"))
            })?;
        let mock = inner.mocks.remove(index);
        inner.calls.push(MockCommandCall {
            rendered,
            program,
            args,
            stdin,
            stdout: mock.result.stdout.clone(),
            stderr: mock.result.stderr.clone(),
            exit_code: mock.result.exit_code,
        });
        Ok(mock.result)
    }

    fn lock(&self) -> mlua::Result<std::sync::MutexGuard<'_, MockCommandInner>> {
        self.inner
            .lock()
            .map_err(|_| mlua::Error::external("mock command state lock is poisoned"))
    }
}

pub(crate) fn format_command(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_string())
        .chain(args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

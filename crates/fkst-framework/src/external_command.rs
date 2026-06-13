use anyhow::Result;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const AUDIT_EXCERPT_LIMIT: usize = 4096;
#[cfg(test)]
type AuditSink = Arc<dyn Fn(&str) + Send + Sync + 'static>;
#[cfg(test)]
thread_local! {
    static AUDIT_SINK: std::cell::RefCell<Option<AuditSink>> = std::cell::RefCell::new(None);
}

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

#[derive(Clone, Debug)]
pub(crate) struct CommandSpec {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) process_group: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct AuditedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) exit_code: i32,
    pub(crate) timed_out: bool,
}

pub(crate) fn run_audited(spec: CommandSpec) -> Result<AuditedOutput> {
    let rendered = format_command(&spec.program.to_string_lossy(), &spec.args);
    let result = run_audited_inner(&spec);
    match &result {
        Ok(output) => emit_audit_line(&rendered, output),
        Err(err) => emit_audit_error_line(&rendered, err),
    }
    result
}

fn run_audited_inner(spec: &CommandSpec) -> Result<AuditedOutput> {
    match spec.timeout {
        Some(timeout) => run_audited_with_timeout(spec, timeout),
        None => {
            let output = build_command(spec).output()?;
            Ok(AuditedOutput {
                stdout: output.stdout,
                stderr: output.stderr,
                exit_code: output.status.code().unwrap_or(-1),
                timed_out: false,
            })
        }
    }
}

fn run_audited_with_timeout(spec: &CommandSpec, timeout: Duration) -> Result<AuditedOutput> {
    let mut command = build_command(spec);
    let mut child = command.spawn()?;
    let child_pid = child.id();
    let _registration = if spec.process_group {
        Some(crate::process_tree::sdk_process_groups().register(child_pid))
    } else {
        None
    };
    let stdout_reader = child
        .stdout
        .take()
        .map(read_pipe_in_thread)
        .ok_or_else(|| anyhow::anyhow!("failed to capture stdout"))?;
    let stderr_reader = child
        .stderr
        .take()
        .map(read_pipe_in_thread)
        .ok_or_else(|| anyhow::anyhow!("failed to capture stderr"))?;

    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(AuditedOutput {
                stdout: join_pipe_reader(stdout_reader)?,
                stderr: join_pipe_reader(stderr_reader)?,
                exit_code: status.code().unwrap_or(-1),
                timed_out: false,
            });
        }

        if start.elapsed() >= timeout {
            if spec.process_group {
                kill_process_group(child_pid);
            } else {
                let _ = child.kill();
            }
            let _ = child.wait();
            return Ok(AuditedOutput {
                stdout: join_pipe_reader(stdout_reader)?,
                stderr: join_pipe_reader(stderr_reader)?,
                exit_code: 124,
                timed_out: true,
            });
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

fn build_command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if spec.process_group {
        make_process_group_leader(&mut command);
    }
    command
}

#[cfg(unix)]
fn make_process_group_leader(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn make_process_group_leader(_command: &mut Command) {}

#[cfg(unix)]
fn kill_process_group(child_pid: u32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    let _ = killpg(Pid::from_raw(child_pid as i32), Signal::SIGKILL);
}

#[cfg(not(unix))]
fn kill_process_group(_child_pid: u32) {}

fn read_pipe_in_thread<R>(mut pipe: R) -> JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        pipe.read_to_end(&mut buf)?;
        Ok(buf)
    })
}

fn join_pipe_reader(reader: JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("pipe reader thread panicked"))?
        .map_err(Into::into)
}

fn emit_audit_line(rendered: &str, output: &AuditedOutput) {
    emit_audit(&audit_line(
        rendered,
        output.exit_code,
        output.timed_out,
        &output.stdout,
        &output.stderr,
    ));
}

fn emit_audit_error_line(rendered: &str, err: &anyhow::Error) {
    let stderr = err.to_string();
    emit_audit(&audit_line(rendered, -1, false, b"", stderr.as_bytes()));
}

fn emit_audit(line: &str) {
    #[cfg(test)]
    AUDIT_SINK.with(|slot| {
        if let Some(sink) = slot.borrow().as_ref() {
            sink(line);
        }
    });
    eprintln!("{line}");
}

#[cfg(test)]
pub(crate) fn install_audit_sink_for_test(sink: AuditSink) -> AuditSinkGuard {
    let previous = AUDIT_SINK.with(|slot| slot.replace(Some(sink)));
    AuditSinkGuard { previous }
}

#[cfg(test)]
pub(crate) struct AuditSinkGuard {
    previous: Option<AuditSink>,
}

#[cfg(test)]
impl Drop for AuditSinkGuard {
    fn drop(&mut self) {
        AUDIT_SINK.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

pub(crate) fn audit_line(
    rendered: &str,
    exit_code: i32,
    timed_out: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    format!(
        "LEVEL=info EVENT=external_command CMD={} EXIT={} TIMED_OUT={} STDOUT_BYTES={} STDERR_BYTES={} STDOUT_EXCERPT={} STDERR_EXCERPT={}",
        escape_value(rendered),
        exit_code,
        timed_out,
        stdout.len(),
        stderr.len(),
        escaped_excerpt(stdout, AUDIT_EXCERPT_LIMIT),
        escaped_excerpt(stderr, AUDIT_EXCERPT_LIMIT)
    )
}

fn excerpt(bytes: &[u8], limit: usize) -> String {
    let bounded = if bytes.len() > limit {
        &bytes[..limit]
    } else {
        bytes
    };
    String::from_utf8_lossy(bounded).to_string()
}

fn escaped_excerpt(bytes: &[u8], limit: usize) -> String {
    truncate_escaped_field(&escape_value(&excerpt(bytes, limit)), limit)
}

fn truncate_escaped_field(value: &str, limit: usize) -> String {
    const MARKER: &str = "...";
    if value.len() <= limit {
        return value.to_string();
    }
    if limit <= MARKER.len() {
        return MARKER[..limit].to_string();
    }
    let content_limit = limit - MARKER.len();
    let mut end = 0;
    for (idx, ch) in value.char_indices() {
        let next = idx + ch.len_utf8();
        if next > content_limit {
            break;
        }
        end = next;
    }
    format!("{}{}", &value[..end], MARKER)
}

pub(crate) fn shim_audit_line(rendered: &str, exit_code: i32, timed_out: bool) -> String {
    format!(
        "LEVEL=info ENGINE_VER={} PKG_VER={} EVENT=external_command CMD={} EXIT={} TIMED_OUT={}",
        escape_value(&crate::provenance::current_engine_ver()),
        escape_value(&crate::provenance::current_pkg_ver()),
        escape_value(rendered),
        exit_code,
        timed_out
    )
}

pub(crate) fn append_shim_audit_line(line: &str) {
    let Some(runtime_root) =
        std::env::var_os("FKST_RUNTIME_ROOT").filter(|value| !value.is_empty())
    else {
        return;
    };
    let path = PathBuf::from(runtime_root)
        .join("logs")
        .join("external-command-audit.log");
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

pub(crate) fn escape_value(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ' ' => escaped.push_str("\\s"),
            '=' => escaped.push_str("\\="),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub(crate) struct TracingKeyValueFormatter;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for TracingKeyValueFormatter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut visitor = TracingFieldVisitor::default();
        event.record(&mut visitor);
        write!(
            writer,
            "TIMESTAMP={} LEVEL={}",
            rfc3339_utc_now(),
            event.metadata().level()
        )?;
        for (key, value) in visitor.fields {
            if key == "message" {
                continue;
            }
            write!(writer, " {}={}", key, escape_value(&value))?;
        }
        write!(writer, " MSG={}", escape_message(&visitor.message))?;
        writeln!(writer)
    }
}

fn escape_message(value: &str) -> String {
    value.replace('\r', "\\r").replace('\n', "\\n")
}

fn rfc3339_utc_now() -> String {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    rfc3339_from_unix(elapsed.as_secs())
}

fn rfc3339_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let seconds_of_day = secs % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

#[derive(Default)]
struct TracingFieldVisitor {
    message: String,
    fields: Vec<(&'static str, String)>,
}

impl tracing::field::Visit for TracingFieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = rendered.trim_matches('"').to_string();
        } else {
            self.fields.push((field.name(), rendered));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name(), value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.push((field.name(), value.to_string()));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.push((field.name(), value.to_string()));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields.push((field.name(), value.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_excerpt_is_bounded_and_single_line_escaped() {
        let stdout = format!("{}x\n", "a".repeat(5000));
        let line = audit_line("gh issue list", 0, false, stdout.as_bytes(), b"err\r\nnext");
        assert!(!line.contains('\n'), "{line}");
        assert!(!line.contains('\r'), "{line}");
        let excerpt = line
            .split("STDOUT_EXCERPT=")
            .nth(1)
            .unwrap()
            .split(" STDERR_EXCERPT=")
            .next()
            .unwrap();
        assert!(excerpt.len() <= AUDIT_EXCERPT_LIMIT, "{}", excerpt.len());
        assert!(line.contains(r"STDERR_EXCERPT=err\r\nnext"), "{line}");
    }

    #[test]
    fn audit_line_captures_exit_timeout_and_byte_counts() {
        let line = audit_line("sh -c false", 124, true, b"out", b"err");
        assert!(line.contains("EVENT=external_command"), "{line}");
        assert!(!line.contains("ENGINE_VER="), "{line}");
        assert!(!line.contains("PKG_VER="), "{line}");
        assert!(line.contains("EXIT=124"), "{line}");
        assert!(line.contains("TIMED_OUT=true"), "{line}");
        assert!(line.contains("STDOUT_BYTES=3"), "{line}");
        assert!(line.contains("STDERR_BYTES=3"), "{line}");
    }

    #[test]
    fn shim_audit_line_omits_captured_excerpts() {
        let line = shim_audit_line("gh issue list", 17, false);
        assert!(line.contains("EVENT=external_command"), "{line}");
        assert!(line.contains("ENGINE_VER="), "{line}");
        assert!(line.contains("PKG_VER="), "{line}");
        assert!(line.contains("CMD=gh\\sissue\\slist"), "{line}");
        assert!(line.contains("EXIT=17"), "{line}");
        assert!(!line.contains("STDOUT_EXCERPT"), "{line}");
        assert!(!line.contains("STDERR_EXCERPT"), "{line}");
    }

    #[test]
    fn tracing_message_keeps_spaces_readable() {
        assert_eq!(
            escape_message("schema validation passed"),
            "schema validation passed"
        );
        assert_eq!(escape_message("a\r\nb"), r"a\r\nb");
    }

    #[test]
    fn tracing_formatter_omits_process_provenance_fields() {
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .event_format(TracingKeyValueFormatter)
            .with_writer(SharedBuffer(output.clone()))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(event = "startup", "framework started");
        });

        let line = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(line.contains(" event=startup "), "{line}");
        assert!(!line.contains(" ENGINE_VER="), "{line}");
        assert!(!line.contains(" PKG_VER="), "{line}");
        assert!(!line.contains(" PKG_VERS="), "{line}");
    }

    #[derive(Clone)]
    struct SharedBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    struct SharedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWriter(self.0.clone())
        }
    }

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}

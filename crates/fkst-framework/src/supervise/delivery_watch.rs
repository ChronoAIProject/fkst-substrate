//! Slow operation watchdog for the durable delivery store.

use std::time::{Duration, Instant};
use tracing::warn;

const STORE_OP_WARN_AFTER: Duration = Duration::from_millis(250);

pub(crate) struct StoreOpWatch<'a> {
    op: &'static str,
    dept: String,
    started: Instant,
    now: Box<dyn Fn() -> Instant + 'static>,
    _marker: std::marker::PhantomData<&'a str>,
}

impl<'a> StoreOpWatch<'a> {
    pub(crate) fn new(op: &'static str, dept: &'a str) -> Self {
        Self::with_clock(op, dept, Instant::now(), Instant::now)
    }

    pub(crate) fn with_clock<F>(op: &'static str, dept: &'a str, started: Instant, now: F) -> Self
    where
        F: Fn() -> Instant + 'static,
    {
        Self {
            op,
            dept: dept.to_string(),
            started,
            now: Box::new(now),
            _marker: std::marker::PhantomData,
        }
    }

    pub(crate) fn set_dept(&mut self, dept: &str) {
        self.dept.clear();
        self.dept.push_str(dept);
    }
}

impl Drop for StoreOpWatch<'_> {
    fn drop(&mut self) {
        let now = (self.now)();
        let Some(warning) = watchdog_warning(self.op, &self.dept, self.started, now) else {
            return;
        };
        warn!(
            op = warning.op,
            dept = %warning.dept,
            elapsed_ms = warning.elapsed_ms,
            "durable delivery store operation exceeded watchdog threshold"
        );
    }
}

struct WatchdogWarning<'a> {
    op: &'static str,
    dept: &'a str,
    elapsed_ms: u64,
}

fn watchdog_warning<'a>(
    op: &'static str,
    dept: &'a str,
    started: Instant,
    now: Instant,
) -> Option<WatchdogWarning<'a>> {
    let elapsed = now.saturating_duration_since(started);
    if elapsed > STORE_OP_WARN_AFTER {
        Some(WatchdogWarning {
            op,
            dept,
            elapsed_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWriter(self.0.clone())
        }
    }

    fn capture_warns(f: impl FnOnce()) -> String {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .finish();

        tracing::subscriber::with_default(subscriber, f);

        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn watchdog_warns_past_threshold_with_op_and_dept() {
        let started = Instant::now();
        let now = started + STORE_OP_WARN_AFTER + Duration::from_millis(1);

        let logs = capture_warns(|| {
            let _watch = StoreOpWatch::with_clock("lease", "worker", started, move || now);
        });

        assert!(logs.contains("op=\"lease\""), "{logs}");
        assert!(logs.contains("dept=worker"), "{logs}");
        assert!(logs.contains("durable delivery store operation exceeded watchdog threshold"));
    }
}

//! Fanout uses explicit `Vec<mpsc::Sender>` per queue. NOT `tokio::sync::broadcast`.
//!
//! Physical fanout is shared by single-consumer and explicit-fanout queues; the
//! startup validator decides how many active consumers each queue may have.
//! Producer (raiser or upstream dept) calls `send(queue, event)`: we look up the
//! Vec<Sender> for that queue, then `try_send` a clone of the event to each
//! subscriber. Slow consumer drops affect only that subscriber. Unknown queues
//! warn and drop the event.

use fkst_common::Event;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tracing::warn;

#[derive(Clone)]
pub struct Fanout {
    inner: Arc<RwLock<HashMap<String, Vec<mpsc::Sender<Event>>>>>,
}

impl Fanout {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a subscriber to `queue`. Returns the Receiver the subscriber owns.
    pub async fn subscribe(&self, queue: &str, capacity: usize) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(capacity);
        let mut map = self.inner.write().await;
        map.entry(queue.to_string()).or_default().push(tx);
        rx
    }

    /// Send to all subscribers of `queue`.
    ///
    /// Slow or closed consumers warn and drop only that subscriber's delivery.
    /// Unknown queues warn and drop the event. Best-effort delivery still returns
    /// Ok after these drops.
    pub fn send(&self, queue: &str, event: Event) -> anyhow::Result<()> {
        let map = self
            .inner
            .try_read()
            .map_err(|_| anyhow::anyhow!("fanout map lock contention"))?;
        if let Some(senders) = map.get(queue) {
            for sender in senders {
                match sender.try_send(event.clone()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        warn!(queue = %queue, "consumer mailbox full, dropping event for slow subscriber");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        warn!(queue = %queue, "consumer mailbox closed (dept dropped receiver)");
                    }
                }
            }
        } else {
            warn!(queue = %queue, "unknown queue, dropping event");
        }
        Ok(())
    }
}

impl Default for Fanout {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
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

    #[tokio::test]
    async fn one_subscriber_receives_event() {
        let f = Fanout::new();
        let mut rx = f.subscribe("tick", 10).await;
        let ev = Event::new("tick", Value::Null);
        f.send("tick", ev.clone()).unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.queue, "tick");
    }

    #[tokio::test]
    async fn two_subscribers_each_receive_event_independently() {
        let f = Fanout::new();
        let mut rx1 = f.subscribe("tick", 10).await;
        let mut rx2 = f.subscribe("tick", 10).await;
        let ev = Event::new("tick", serde_json::json!({"n": 42}));
        f.send("tick", ev.clone()).unwrap();
        let g1 = rx1.recv().await.unwrap();
        let g2 = rx2.recv().await.unwrap();
        assert_eq!(g1.payload, serde_json::json!({"n": 42}));
        assert_eq!(g2.payload, serde_json::json!({"n": 42}));
    }

    #[tokio::test]
    async fn slow_consumer_does_not_block_fast_consumer() {
        let f = Fanout::new();
        let _slow_rx = f.subscribe("tick", 1).await; // cap 1 — fills fast
        let mut fast_rx = f.subscribe("tick", 100).await;
        // Send 5 events; slow consumer drops 4, fast consumer receives all 5.
        for n in 0..5 {
            f.send("tick", Event::new("tick", serde_json::json!({"n": n})))
                .unwrap();
        }
        let mut fast_count = 0;
        while let Ok(Some(_)) =
            tokio::time::timeout(std::time::Duration::from_millis(10), fast_rx.recv()).await
        {
            fast_count += 1;
        }
        assert_eq!(fast_count, 5, "fast consumer should see all 5 events");
    }

    #[tokio::test]
    async fn closed_consumer_does_not_block_fast_consumer() {
        let f = Fanout::new();
        let closed_rx = f.subscribe("tick", 10).await;
        let mut fast_rx = f.subscribe("tick", 10).await;
        drop(closed_rx);

        f.send("tick", Event::new("tick", Value::Null)).unwrap();

        let got = fast_rx.recv().await.unwrap();
        assert_eq!(got.queue, "tick");
    }

    #[tokio::test]
    async fn send_to_unknown_queue_warns_and_returns_ok() {
        let f = Fanout::new();
        let ev = Event::new("ghost", Value::Null);
        let logs = capture_warns(|| f.send("ghost", ev).unwrap());

        assert!(logs.contains("unknown queue, dropping event"), "{logs}");
        assert!(logs.contains("queue=ghost"), "{logs}");
    }
}

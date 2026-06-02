//! Event flowing from a Raiser through a Fanout to a department.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One event in the pub-sub system.
///
/// `queue` is the queue name the producer wrote to (and a Fanout duplicates to N subscribers).
/// `payload` is opaque JSON; the receiving Lua department picks fields it understands.
/// `ts` is unix milliseconds the producer stamped on creation; used for latency measurement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub queue: String,
    pub payload: Value,
    pub ts: u64,
}

impl Event {
    /// Construct an Event with `ts` set to current time.
    pub fn new(queue: impl Into<String>, payload: Value) -> Self {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_millis() as u64;
        Self {
            queue: queue.into(),
            payload,
            ts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrips_serde_json() {
        let e = Event::new("tick", serde_json::json!({"n": 1}));
        let j = serde_json::to_string(&e).unwrap();
        let e2: Event = serde_json::from_str(&j).unwrap();
        assert_eq!(e.queue, e2.queue);
        assert_eq!(e.payload, e2.payload);
        assert_eq!(e.ts, e2.ts);
    }

    #[test]
    fn event_new_sets_ts_to_recent_unix_ms() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let e = Event::new("x", Value::Null);
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            e.ts >= before && e.ts <= after,
            "ts {} not in [{}, {}]",
            e.ts,
            before,
            after
        );
    }
}

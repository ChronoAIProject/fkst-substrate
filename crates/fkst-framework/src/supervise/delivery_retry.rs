//! Durable delivery retry delay and error helpers.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

pub(crate) const ERROR_EXCERPT_LIMIT: usize = 500;
const JITTER_DIVISOR: u128 = 4;

pub(crate) fn backoff_delay(base: Duration, cap: Duration, attempt: u64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(63);
    let multiplier = 1_u128 << exponent;
    let millis = base.as_millis().saturating_mul(multiplier);
    let capped = millis.min(cap.as_millis());
    Duration::from_millis(capped.min(u64::MAX as u128) as u64)
}

pub(crate) fn bounded_jitter(delivery_id: &str, attempt: u64, base: Duration) -> Duration {
    let max = (base.as_millis() / JITTER_DIVISOR).min(u64::MAX as u128) as u64;
    if max == 0 {
        return Duration::ZERO;
    }
    let mut hasher = DefaultHasher::new();
    delivery_id.hash(&mut hasher);
    attempt.hash(&mut hasher);
    Duration::from_millis(hasher.finish() % (max + 1))
}

pub(crate) fn error_excerpt(error: &str) -> String {
    let mut excerpt = error.replace('\r', "\\r").replace('\n', "\\n");
    if excerpt.len() > ERROR_EXCERPT_LIMIT {
        let boundary = excerpt
            .char_indices()
            .map(|(idx, _)| idx)
            .take_while(|idx| *idx <= ERROR_EXCERPT_LIMIT)
            .last()
            .unwrap_or(0);
        excerpt.truncate(boundary);
    }
    excerpt
}

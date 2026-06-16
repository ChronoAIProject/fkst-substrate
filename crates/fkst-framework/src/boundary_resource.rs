//! Static boundary-resource registry and adapter error taxonomy.
//!
//! The registry is an audit anchor: every entry names the mediated adapter,
//! grant, meter, budget/backpressure posture, and typed error contract for an
//! external resource class the framework can touch.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BoundaryErrorClass {
    QuotaExhausted,
    AuthDegraded,
    ProviderUnavailable,
    ProviderThrottle,
}

impl BoundaryErrorClass {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::QuotaExhausted => "quota-exhausted",
            Self::AuthDegraded => "auth-degraded",
            Self::ProviderUnavailable => "provider-unavailable",
            Self::ProviderThrottle => "provider-throttle",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BoundaryResourceEntry {
    pub(crate) id: &'static str,
    pub(crate) resource_type: &'static str,
    pub(crate) adapters: &'static str,
    pub(crate) grant: &'static str,
    pub(crate) meter: &'static str,
    pub(crate) budget: &'static str,
    pub(crate) errors: &'static str,
}

pub(crate) static BOUNDARY_RESOURCE_REGISTRY: &[BoundaryResourceEntry] = &[
    BoundaryResourceEntry {
        id: "codex.process",
        resource_type: "compute",
        adapters: "spawn_codex_sync,spawn_codex",
        grant: "codex-permit fcntl pool plus optional rate-pool shim",
        meter: "codex log, codex-permit lock witness, optional rate-pool ledger",
        budget: "FKST_CODEX_PERMIT_SLOTS, optional FKST_RATE_POOL_CODEX, FKST_CODEX_LOG_MAX_AGE, and optional FKST_CODEX_LOG_MAX_BYTES",
        errors: "quota-exhausted,auth-degraded,provider-unavailable,provider-throttle",
    },
    BoundaryResourceEntry {
        id: "shell.process",
        resource_type: "subprocess",
        adapters: "exec_sync",
        grant: "exec_sync adapter plus optional first-program rate pool",
        meter: "optional rate-pool ledger; test-mode command_calls",
        budget: "optional FKST_RATE_POOL_<PROGRAM> and per-call timeout",
        errors: "quota-exhausted,auth-degraded,provider-unavailable,provider-throttle",
    },
    BoundaryResourceEntry {
        id: "argv.process",
        resource_type: "subprocess",
        adapters: "exec_argv",
        grant: "exec_argv adapter (direct program argv, no shell) plus optional argv[1] program rate pool",
        meter: "optional rate-pool ledger; test-mode command_calls",
        budget: "optional FKST_RATE_POOL_<PROGRAM> and per-call timeout",
        errors: "quota-exhausted,auth-degraded,provider-unavailable,provider-throttle",
    },
    BoundaryResourceEntry {
        id: "git.process",
        resource_type: "external-source",
        adapters: "git_log_count,git_log_grep,count_worktrees,list_orphan_worktrees,setup_worktree",
        grant: "git -C host-root adapter plus optional FKST_RATE_POOL_GIT",
        meter: "optional rate-pool ledger; test-mode command_calls",
        budget: "optional FKST_RATE_POOL_GIT",
        errors: "quota-exhausted,auth-degraded,provider-unavailable,provider-throttle",
    },
    BoundaryResourceEntry {
        id: "runtime.filesystem",
        resource_type: "storage",
        adapters: "RuntimeLayout,file,with_lock,once,cache_set,cache_get,cache_expire,setup_worktree",
        grant: "RuntimeLayout path mediation and runtime-key validation",
        meter: "host filesystem inspection under declared runtime directories",
        budget: "host filesystem posture; RuntimeLayout confines engine scratch",
        errors: "provider-unavailable",
    },
    BoundaryResourceEntry {
        id: "wall-clock",
        resource_type: "time",
        adapters: "now,exec_sync timeout,codex timeout,rate-pool refill",
        grant: "adapter-owned SystemTime/Instant calls",
        meter: "returned timestamps, timeout fields, and rate-pool ledger timestamps",
        budget: "per-call timeout and configured rate-pool refill",
        errors: "provider-unavailable",
    },
];

pub(crate) fn class_for_adapter_failure(kind: &str, message: &str) -> BoundaryErrorClass {
    if let Some(class) = classify_text(message) {
        return class;
    }
    match kind {
        "permit" => BoundaryErrorClass::QuotaExhausted,
        "rate_pool" | "timeout" | "spawn" | "stdin" | "wait" | "thread" => {
            BoundaryErrorClass::ProviderUnavailable
        }
        _ => BoundaryErrorClass::ProviderUnavailable,
    }
}

pub(crate) fn classify_process_output(
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) -> Option<BoundaryErrorClass> {
    if exit_code == 0 {
        return None;
    }
    classify_text(&format!("{stdout}\n{stderr}"))
}

fn classify_text(text: &str) -> Option<BoundaryErrorClass> {
    let lower = text.to_ascii_lowercase();
    if contains_any(
        &lower,
        &[
            "secondary rate limit",
            "abuse detection",
            "too many requests",
            "429",
            "throttl",
        ],
    ) {
        return Some(BoundaryErrorClass::ProviderThrottle);
    }
    if contains_any(
        &lower,
        &[
            "quota exceeded",
            "quota-exhausted",
            "rate limit exceeded",
            "api rate limit",
            "resource exhausted",
        ],
    ) {
        return Some(BoundaryErrorClass::QuotaExhausted);
    }
    if contains_any(
        &lower,
        &[
            "401",
            "bad credentials",
            "requires authentication",
            "authentication failed",
            "auth failed",
            "unauthorized",
            "keyring",
        ],
    ) {
        return Some(BoundaryErrorClass::AuthDegraded);
    }
    if contains_any(
        &lower,
        &[
            "503",
            "502",
            "504",
            "service unavailable",
            "provider unavailable",
            "connection refused",
            "timed out",
            "timeout",
        ],
    ) {
        return Some(BoundaryErrorClass::ProviderUnavailable);
    }
    None
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_enumerates_current_framework_boundary_resources() {
        let ids = BOUNDARY_RESOURCE_REGISTRY
            .iter()
            .map(|entry| entry.id)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(ids.contains("codex.process"));
        assert!(ids.contains("shell.process"));
        assert!(ids.contains("argv.process"));
        assert!(ids.contains("git.process"));
        assert!(ids.contains("runtime.filesystem"));
        assert!(ids.contains("wall-clock"));
    }

    #[test]
    fn classifier_keeps_auth_distinct_from_quota() {
        assert_eq!(
            classify_process_output(1, "", "HTTP 401 from keyring; rate limit unknown"),
            Some(BoundaryErrorClass::AuthDegraded)
        );
        assert_eq!(
            classify_process_output(1, "", "secondary rate limit"),
            Some(BoundaryErrorClass::ProviderThrottle)
        );
        assert_eq!(
            classify_process_output(1, "", "HTTP 403 secondary rate limit"),
            Some(BoundaryErrorClass::ProviderThrottle)
        );
        assert_eq!(
            classify_process_output(1, "", "API rate limit exceeded"),
            Some(BoundaryErrorClass::QuotaExhausted)
        );
        assert_eq!(
            classify_process_output(1, "", "HTTP 403 API rate limit exceeded"),
            Some(BoundaryErrorClass::QuotaExhausted)
        );
    }
}

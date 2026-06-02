//! Known-good swap owner for CAS, candidate checkout, health observation, and rollback witnesses.

use anyhow::{Context, Result};
use fkst_common::{RuntimeKind, RuntimeLayout};
use nix::fcntl::{flock, FlockArg};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const KNOWN_GOOD_REF: &str = "refs/known-good";
const NULL_SHA: &str = "0000000000000000000000000000000000000000";
const HEALTH_LOCK: &str = "known-good-health.lock";
const DEFAULT_HEALTH_WINDOW_SECONDS: u64 = 300;
const DEFAULT_HEALTH_POLL_MS: u64 = 1_000;
const CRASH_LOOP_ROLLBACK_THRESHOLD: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapEvidence {
    pub conformance: Option<String>,
    pub self_test: Option<String>,
    pub review: Option<String>,
}

impl SwapEvidence {
    fn require_complete(&self) -> std::result::Result<(), KnownGoodError> {
        if missing(&self.conformance) {
            return Err(KnownGoodError::ConformanceFail);
        }
        if !evidence_passed(self.conformance.as_deref().unwrap_or_default()) {
            return Err(KnownGoodError::ConformanceFail);
        }
        if missing(&self.self_test) {
            return Err(KnownGoodError::SelfTestFail(
                "self-test-fail:missing".to_string(),
            ));
        }
        if !evidence_passed(self.self_test.as_deref().unwrap_or_default()) {
            return Err(KnownGoodError::SelfTestFail(self_test_failure_class(
                self.self_test.as_deref().unwrap_or_default(),
            )));
        }
        self.require_review()
    }

    fn require_review(&self) -> std::result::Result<(), KnownGoodError> {
        if missing(&self.review) {
            return Err(KnownGoodError::ReviewEvidenceMissing);
        }
        Ok(())
    }

    fn summary(&self) -> String {
        format!(
            "conformance={} self_test={} review={}",
            self.conformance.as_deref().unwrap_or("missing"),
            self.self_test.as_deref().unwrap_or("missing"),
            self.review.as_deref().unwrap_or("missing")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthGateOptions {
    pub window: Duration,
    pub poll_interval: Duration,
    pub supervisor_log: Option<PathBuf>,
}

impl Default for HealthGateOptions {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(DEFAULT_HEALTH_WINDOW_SECONDS),
            poll_interval: Duration::from_millis(DEFAULT_HEALTH_POLL_MS),
            supervisor_log: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthVerdict {
    pub evidence_line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionTarget {
    pub integration_ref: String,
    pub candidate_sha: String,
    pub old_known_good_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionOutcome {
    Promoted {
        integration_ref: String,
        old_known_good_sha: String,
        candidate_sha: String,
        health: HealthVerdict,
    },
    Bootstrapped {
        integration_ref: String,
        candidate_sha: String,
    },
    NoOp {
        integration_ref: String,
        sha: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum KnownGoodError {
    IntegrationRefUnresolved(String),
    KnownGoodRefMissing,
    KnownGoodRefUnreadable(String),
    KnownGoodRefAlreadyExists(String),
    CandidateNotDescendant {
        old_known_good_sha: String,
        candidate_sha: String,
    },
    CandidateMoved {
        expected_candidate_sha: String,
        actual_candidate_sha: String,
    },
    SelfTestFail(String),
    ConformanceFail,
    ReviewEvidenceMissing,
    ReviewEvidenceExternal,
    HealthGateFailed {
        class: String,
        evidence_line: String,
    },
    UpdateRefConflict,
    UpdateRefIo(String),
}

impl KnownGoodError {
    pub fn class(&self) -> String {
        match self {
            Self::IntegrationRefUnresolved(_) => "integration-ref-unresolved".to_string(),
            Self::KnownGoodRefMissing => "known-good-ref-missing".to_string(),
            Self::KnownGoodRefUnreadable(_) => "known-good-ref-unreadable".to_string(),
            Self::KnownGoodRefAlreadyExists(_) => "known-good-ref-already-exists".to_string(),
            Self::CandidateNotDescendant { .. } => "candidate-not-descendant".to_string(),
            Self::CandidateMoved { .. } => "candidate-moved".to_string(),
            Self::SelfTestFail(class) => class.clone(),
            Self::ConformanceFail => "conformance-fail".to_string(),
            Self::ReviewEvidenceMissing => "review-evidence-missing".to_string(),
            Self::ReviewEvidenceExternal => "review-evidence-external".to_string(),
            Self::HealthGateFailed { class, .. } => class.clone(),
            Self::UpdateRefConflict => "update-ref-conflict".to_string(),
            Self::UpdateRefIo(_) => "update-ref-io".to_string(),
        }
    }

    pub fn health_evidence(&self) -> Option<&str> {
        match self {
            Self::HealthGateFailed { evidence_line, .. } => Some(evidence_line),
            _ => None,
        }
    }
}

pub struct KnownGoodRef {
    root: PathBuf,
    integration_ref: String,
}

impl KnownGoodRef {
    pub fn new(root: PathBuf, integration_ref: Option<String>) -> Result<Self> {
        let integration_ref =
            resolve_integration_ref(&root, integration_ref).context("resolve integration ref")?;
        Ok(Self {
            root,
            integration_ref,
        })
    }

    pub fn read_promotion_target(&self) -> std::result::Result<PromotionTarget, KnownGoodError> {
        let candidate_sha = self
            .rev_parse_commit(&self.integration_ref)
            .map_err(|stderr| KnownGoodError::IntegrationRefUnresolved(stderr))?;
        let old_known_good_sha = self.read_existing_known_good()?;
        Ok(PromotionTarget {
            integration_ref: self.integration_ref.clone(),
            candidate_sha,
            old_known_good_sha,
        })
    }

    pub fn promote_with_health_options(
        &self,
        evidence: &SwapEvidence,
        trigger: &str,
        health_options: &HealthGateOptions,
    ) -> std::result::Result<PromotionOutcome, KnownGoodError> {
        let target = self.read_promotion_target()?;
        self.promote_target_with_health_options(evidence, &target, trigger, health_options)
    }

    pub fn promote_target_with_health_options(
        &self,
        evidence: &SwapEvidence,
        target: &PromotionTarget,
        trigger: &str,
        health_options: &HealthGateOptions,
    ) -> std::result::Result<PromotionOutcome, KnownGoodError> {
        if target.candidate_sha == target.old_known_good_sha {
            return Ok(PromotionOutcome::NoOp {
                integration_ref: target.integration_ref.clone(),
                sha: target.candidate_sha.clone(),
            });
        }

        evidence.require_complete()?;
        let actual_candidate = self
            .rev_parse_commit(&target.integration_ref)
            .map_err(KnownGoodError::IntegrationRefUnresolved)?;
        if actual_candidate != target.candidate_sha {
            return Err(KnownGoodError::CandidateMoved {
                expected_candidate_sha: target.candidate_sha.clone(),
                actual_candidate_sha: actual_candidate,
            });
        }
        self.require_descendant(&target.old_known_good_sha, &target.candidate_sha)?;
        let _lock = self.acquire_health_lock(health_options)?;
        let msg = format!(
            "event=known-good-swap result=promoted trigger={} integration_ref={} old_known_good={} candidate={} {} {}",
            trigger,
            target.integration_ref,
            target.old_known_good_sha,
            target.candidate_sha,
            evidence.summary(),
            "swap=before-health-observe"
        );
        self.update_known_good(&target.candidate_sha, &target.old_known_good_sha, &msg)?;
        self.checkout_detached(&target.candidate_sha)?;
        let health = self.observe_candidate_health(evidence, target, trigger, health_options)?;
        Ok(PromotionOutcome::Promoted {
            integration_ref: target.integration_ref.clone(),
            old_known_good_sha: target.old_known_good_sha.clone(),
            candidate_sha: target.candidate_sha.clone(),
            health,
        })
    }

    pub fn bootstrap(
        &self,
        evidence: &SwapEvidence,
        trigger: &str,
    ) -> std::result::Result<PromotionOutcome, KnownGoodError> {
        evidence.require_complete()?;
        match self.read_existing_known_good() {
            Ok(existing) => return Err(KnownGoodError::KnownGoodRefAlreadyExists(existing)),
            Err(KnownGoodError::KnownGoodRefMissing) => {}
            Err(err) => return Err(err),
        }
        let candidate_sha = self
            .rev_parse_commit(&self.integration_ref)
            .map_err(|stderr| KnownGoodError::IntegrationRefUnresolved(stderr))?;
        let msg = format!(
            "event=known-good-swap result=bootstrapped trigger={} integration_ref={} old_known_good={} candidate={} {}",
            trigger,
            self.integration_ref,
            NULL_SHA,
            candidate_sha,
            evidence.summary()
        );
        self.update_known_good(&candidate_sha, NULL_SHA, &msg)?;
        Ok(PromotionOutcome::Bootstrapped {
            integration_ref: self.integration_ref.clone(),
            candidate_sha,
        })
    }

    fn read_existing_known_good(&self) -> std::result::Result<String, KnownGoodError> {
        let output = git(
            &self.root,
            [
                "rev-parse",
                "--verify",
                &format!("{KNOWN_GOOD_REF}^{{commit}}"),
            ],
        );
        match output {
            Ok(sha) => Ok(sha),
            Err(stderr) => {
                if stderr.contains("Needed a single revision")
                    || stderr.contains("unknown revision")
                    || stderr.contains("not a valid object name")
                {
                    Err(KnownGoodError::KnownGoodRefMissing)
                } else {
                    Err(KnownGoodError::KnownGoodRefUnreadable(stderr))
                }
            }
        }
    }

    fn rev_parse_commit(&self, reference: &str) -> std::result::Result<String, String> {
        git(
            &self.root,
            ["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
        )
    }

    fn require_descendant(
        &self,
        old_known_good_sha: &str,
        candidate_sha: &str,
    ) -> std::result::Result<(), KnownGoodError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .arg("merge-base")
            .arg("--is-ancestor")
            .arg(old_known_good_sha)
            .arg(candidate_sha)
            .output()
            .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
        if output.status.success() {
            Ok(())
        } else if output.status.code() == Some(1) {
            Err(KnownGoodError::CandidateNotDescendant {
                old_known_good_sha: old_known_good_sha.to_string(),
                candidate_sha: candidate_sha.to_string(),
            })
        } else {
            Err(KnownGoodError::UpdateRefIo(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }

    fn update_known_good(
        &self,
        new_sha: &str,
        expected_old_sha: &str,
        message: &str,
    ) -> std::result::Result<(), KnownGoodError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .arg("update-ref")
            .arg("--create-reflog")
            .arg("-m")
            .arg(message)
            .arg(KNOWN_GOOD_REF)
            .arg(new_sha)
            .arg(expected_old_sha)
            .output()
            .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
        if output.status.success() {
            return Ok(());
        }

        if expected_old_sha != NULL_SHA {
            if let Ok(current) = self.read_existing_known_good() {
                if current != expected_old_sha {
                    return Err(KnownGoodError::UpdateRefConflict);
                }
            }
        } else if self.read_existing_known_good().is_ok() {
            return Err(KnownGoodError::UpdateRefConflict);
        }

        Err(KnownGoodError::UpdateRefIo(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }

    fn acquire_health_lock(
        &self,
        options: &HealthGateOptions,
    ) -> std::result::Result<File, KnownGoodError> {
        let layout = RuntimeLayout::from_env()
            .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
        let locks_dir = layout.runtime_dir(RuntimeKind::Locks);
        std::fs::create_dir_all(locks_dir)
            .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
        let lock_path = layout
            .runtime_path(RuntimeKind::Locks, HEALTH_LOCK)
            .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
        match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
            Ok(()) => Ok(file),
            Err(_) => {
                let line = health_evidence_line(HealthEvidence {
                    result: "fail",
                    trigger: "unknown",
                    integration_ref: "unknown",
                    old_known_good_sha: "unknown",
                    candidate_sha: "unknown",
                    window: options.window,
                    failure_class: "health-lock-contention",
                });
                let _ = append_health_log(options.supervisor_log.as_deref(), &line);
                Err(KnownGoodError::HealthGateFailed {
                    class: "health-lock-contention".to_string(),
                    evidence_line: line,
                })
            }
        }
    }

    fn checkout_detached(&self, sha: &str) -> std::result::Result<(), KnownGoodError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .arg("checkout")
            .arg("--detach")
            .arg("-q")
            .arg(sha)
            .output()
            .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(KnownGoodError::UpdateRefIo(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }

    fn observe_candidate_health(
        &self,
        evidence: &SwapEvidence,
        target: &PromotionTarget,
        trigger: &str,
        options: &HealthGateOptions,
    ) -> std::result::Result<HealthVerdict, KnownGoodError> {
        self.observe_candidate_health_with_runtime(
            evidence,
            target,
            trigger,
            options,
            &RealHealthRuntime,
        )
    }

    fn observe_candidate_health_with_runtime(
        &self,
        evidence: &SwapEvidence,
        target: &PromotionTarget,
        trigger: &str,
        options: &HealthGateOptions,
        runtime: &dyn HealthRuntime,
    ) -> std::result::Result<HealthVerdict, KnownGoodError> {
        let mut log_cursor = match supervisor_log_len(options.supervisor_log.as_deref()) {
            Ok(len) => len,
            Err(_) => {
                return self.health_fail(
                    target,
                    trigger,
                    options,
                    "supervisor-observation-interrupted",
                );
            }
        };
        append_health_log(
            options.supervisor_log.as_deref(),
            &health_evidence_line(HealthEvidence {
                result: "start",
                trigger,
                integration_ref: &target.integration_ref,
                old_known_good_sha: &target.old_known_good_sha,
                candidate_sha: &target.candidate_sha,
                window: options.window,
                failure_class: "none",
            }),
        )?;

        if missing(&evidence.conformance) {
            return self.health_fail(target, trigger, options, "conformance-missing");
        }
        if !evidence_passed(evidence.conformance.as_deref().unwrap_or_default()) {
            return self.health_fail(target, trigger, options, "conformance-fail");
        }
        if missing(&evidence.self_test) {
            return self.health_fail(target, trigger, options, "self-test-fail:missing");
        }
        if !evidence_passed(evidence.self_test.as_deref().unwrap_or_default()) {
            let class = self_test_failure_class(evidence.self_test.as_deref().unwrap_or_default());
            return self.health_fail(target, trigger, options, &class);
        }

        let started = runtime.now();
        loop {
            if self
                .rev_parse_commit(&target.integration_ref)
                .map_err(KnownGoodError::IntegrationRefUnresolved)?
                != target.candidate_sha
            {
                return self.health_fail(target, trigger, options, "ref-drift");
            }
            if self.read_existing_known_good()? != target.candidate_sha {
                return self.health_fail(target, trigger, options, "ref-drift");
            }
            match scan_health_log(options.supervisor_log.as_deref(), log_cursor) {
                Ok(HealthLogScan::Failure {
                    cursor,
                    failure_class,
                }) => {
                    log_cursor = cursor;
                    let _ = log_cursor;
                    return self.health_fail(target, trigger, options, &failure_class);
                }
                Ok(HealthLogScan::NoFailure(cursor)) => {
                    log_cursor = cursor;
                }
                Err(_) => {
                    return self.health_fail(
                        target,
                        trigger,
                        options,
                        "supervisor-observation-interrupted",
                    );
                }
            }

            let elapsed = runtime.now().saturating_duration_since(started);
            if elapsed >= options.window {
                let line = health_evidence_line(HealthEvidence {
                    result: "pass",
                    trigger,
                    integration_ref: &target.integration_ref,
                    old_known_good_sha: &target.old_known_good_sha,
                    candidate_sha: &target.candidate_sha,
                    window: options.window,
                    failure_class: "none",
                });
                append_health_log(options.supervisor_log.as_deref(), &line)?;
                return Ok(HealthVerdict {
                    evidence_line: line,
                });
            }
            runtime.sleep(std::cmp::min(
                options.poll_interval,
                options.window - elapsed,
            ));
        }
    }

    fn health_fail(
        &self,
        target: &PromotionTarget,
        trigger: &str,
        options: &HealthGateOptions,
        failure_class: &str,
    ) -> std::result::Result<HealthVerdict, KnownGoodError> {
        let line = health_evidence_line(HealthEvidence {
            result: "fail",
            trigger,
            integration_ref: &target.integration_ref,
            old_known_good_sha: &target.old_known_good_sha,
            candidate_sha: &target.candidate_sha,
            window: options.window,
            failure_class,
        });
        append_health_log(options.supervisor_log.as_deref(), &line)?;
        let (class, evidence_line) = if should_rollback_health_failure(failure_class) {
            let rollback =
                self.rollback_failed_candidate(target, trigger, options, failure_class)?;
            (
                rollback.failure_class,
                format!("{line}\n{}", rollback.evidence_line),
            )
        } else {
            (failure_class.to_string(), line)
        };
        Err(KnownGoodError::HealthGateFailed {
            class,
            evidence_line,
        })
    }

    fn rollback_failed_candidate(
        &self,
        target: &PromotionTarget,
        trigger: &str,
        options: &HealthGateOptions,
        original_failure_class: &str,
    ) -> std::result::Result<RollbackEvidence, KnownGoodError> {
        let current_known_good = self.read_existing_known_good()?;
        if current_known_good != target.candidate_sha {
            let line = rollback_evidence_line(RollbackEvidenceFields {
                failure_class: "ref-drift",
                original_failure_class,
                candidate_sha: &target.candidate_sha,
                known_good_sha: &current_known_good,
                trigger,
                action: "none",
                reusable: "no",
            });
            append_health_log(options.supervisor_log.as_deref(), &line)?;
            return Ok(RollbackEvidence {
                failure_class: "ref-drift".to_string(),
                evidence_line: line,
            });
        }

        let msg = rollback_evidence_line(RollbackEvidenceFields {
            failure_class: original_failure_class,
            original_failure_class,
            candidate_sha: &target.candidate_sha,
            known_good_sha: &target.old_known_good_sha,
            trigger,
            action: "ref-rollback",
            reusable: "yes",
        });
        self.update_known_good(&target.old_known_good_sha, &target.candidate_sha, &msg)?;

        if self.has_checkout_changes(options.supervisor_log.as_deref())? {
            let line = rollback_evidence_line(RollbackEvidenceFields {
                failure_class: "rollback-blocked-dirty-worktree",
                original_failure_class,
                candidate_sha: &target.candidate_sha,
                known_good_sha: &target.old_known_good_sha,
                trigger,
                action: "blocked",
                reusable: "no",
            });
            append_health_log(options.supervisor_log.as_deref(), &line)?;
            return Ok(RollbackEvidence {
                failure_class: "rollback-blocked-dirty-worktree".to_string(),
                evidence_line: line,
            });
        }

        let effective_failure_class =
            if rollback_count(options.supervisor_log.as_deref(), &target.candidate_sha) + 1
                >= CRASH_LOOP_ROLLBACK_THRESHOLD
            {
                "crash-loop"
            } else {
                original_failure_class
            };
        self.checkout_detached(&target.old_known_good_sha)?;
        let line = rollback_evidence_line(RollbackEvidenceFields {
            failure_class: effective_failure_class,
            original_failure_class,
            candidate_sha: &target.candidate_sha,
            known_good_sha: &target.old_known_good_sha,
            trigger,
            action: "checkout-known-good",
            reusable: "yes",
        });
        append_health_log(options.supervisor_log.as_deref(), &line)?;
        Ok(RollbackEvidence {
            failure_class: effective_failure_class.to_string(),
            evidence_line: line,
        })
    }

    fn has_checkout_changes(
        &self,
        supervisor_log: Option<&Path>,
    ) -> std::result::Result<bool, KnownGoodError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .arg("status")
            .arg("--porcelain")
            .output()
            .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(porcelain_path)
                .any(|path| !self.ignored_dirty_path(path, supervisor_log)))
        } else {
            Err(KnownGoodError::UpdateRefIo(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }

    fn ignored_dirty_path(&self, path: &str, supervisor_log: Option<&Path>) -> bool {
        let Some(supervisor_log) = supervisor_log else {
            return false;
        };
        let normalized = supervisor_log
            .strip_prefix(&self.root)
            .unwrap_or(supervisor_log)
            .to_string_lossy()
            .trim_start_matches("./")
            .to_string();
        path == normalized
    }
}

struct RollbackEvidence {
    failure_class: String,
    evidence_line: String,
}

trait HealthRuntime {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

struct RealHealthRuntime;

impl HealthRuntime for RealHealthRuntime {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn resolve_integration_ref(root: &Path, explicit: Option<String>) -> Result<String> {
    if let Some(value) = nonempty(explicit) {
        return Ok(value);
    }
    if let Some(value) = nonempty(std::env::var("FKST_INTEGRATION_BRANCH").ok()) {
        return Ok(value);
    }
    if let Some(value) = read_env_file_value(&root.join("fkst.env"), "FKST_INTEGRATION_BRANCH")? {
        return Ok(value);
    }
    let tunable = root.join("tunables/integration_branch.txt");
    if let Ok(content) = std::fs::read_to_string(&tunable) {
        if let Some(value) = nonempty(Some(content)) {
            return Ok(value);
        }
    }
    anyhow::bail!(
        "integration-branch-unconfigured: missing FKST_INTEGRATION_BRANCH or tunables/integration_branch.txt"
    )
}

fn read_env_file_value(path: &Path, key: &str) -> Result<Option<String>> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    for line in content.lines() {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(value) = line.strip_prefix(&format!("{key}=")) else {
            continue;
        };
        if let Some(value) = nonempty(Some(value.to_string())) {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn missing(value: &Option<String>) -> bool {
    value.as_deref().map(str::trim).unwrap_or("").is_empty()
}

fn nonempty(value: Option<String>) -> Option<String> {
    let value = value?.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn git<const N: usize>(root: &Path, args: [&str; N]) -> std::result::Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

struct HealthEvidence<'a> {
    result: &'a str,
    trigger: &'a str,
    integration_ref: &'a str,
    old_known_good_sha: &'a str,
    candidate_sha: &'a str,
    window: Duration,
    failure_class: &'a str,
}

fn health_evidence_line(evidence: HealthEvidence<'_>) -> String {
    let reusable_by = if evidence.result == "pass" {
        "known-good-promote,auto-rollback"
    } else {
        "auto-rollback"
    };
    let rollback_owner = if evidence.result == "fail" {
        " rollback_owner=issue-63"
    } else {
        ""
    };
    format!(
        "KNOWN_GOOD_HEALTH:{} action=known-good-health-observe result={} trigger_source={} candidate_sha={} old_known_good_sha={} integration_branch={} attempt_id={}:{}:{} window_s={} self_test={} conformance={} reusable_by={} failure_class={} failure_owner=known-good-health-gate{}",
        evidence.result,
        evidence.result,
        evidence.trigger,
        evidence.candidate_sha,
        evidence.old_known_good_sha,
        evidence.integration_ref,
        evidence.integration_ref,
        evidence.old_known_good_sha,
        evidence.candidate_sha,
        evidence.window.as_secs(),
        if evidence.result == "pass" { "pass" } else { "unknown" },
        if evidence.result == "pass" { "pass" } else { "unknown" },
        reusable_by,
        evidence.failure_class,
        rollback_owner
    )
}

fn append_health_log(path: Option<&Path>, line: &str) -> std::result::Result<(), KnownGoodError> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))?;
    writeln!(file, "{line}").map_err(|err| KnownGoodError::UpdateRefIo(err.to_string()))
}

fn supervisor_log_len(path: Option<&Path>) -> std::io::Result<u64> {
    let Some(path) = path else {
        return Ok(0);
    };
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err),
    }
}

enum HealthLogScan {
    NoFailure(u64),
    Failure { cursor: u64, failure_class: String },
}

fn scan_health_log(path: Option<&Path>, cursor: u64) -> std::io::Result<HealthLogScan> {
    let Some(path) = path else {
        return Ok(HealthLogScan::NoFailure(cursor));
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HealthLogScan::NoFailure(cursor));
        }
        Err(err) => return Err(err),
    };
    let len = file.metadata()?.len();
    let start = std::cmp::min(cursor, len);
    file.seek(SeekFrom::Start(start))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    for line in text.lines() {
        if let Some(class) = framework_failure_class(line) {
            return Ok(HealthLogScan::Failure {
                cursor: len,
                failure_class: class,
            });
        }
    }
    Ok(HealthLogScan::NoFailure(len))
}

fn evidence_passed(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value == "pass"
        || value.ends_with(":pass")
        || value.ends_with("=pass")
        || value.contains(" result=pass")
        || value.contains(" status=pass")
}

fn self_test_failure_class(value: &str) -> String {
    if let Some(rest) = value.split("SELF_TEST_FAILED:").nth(1) {
        let class = rest
            .split(|c: char| c.is_whitespace() || c == ':')
            .next()
            .unwrap_or("unknown")
            .trim();
        if !class.is_empty() {
            return format!("self-test-fail:{class}");
        }
    }
    "self-test-fail".to_string()
}

fn framework_failure_class(line: &str) -> Option<String> {
    for token in [
        "framework-spawn-fail",
        "framework-stall-timeout",
        "supervisor-observation-interrupted",
    ] {
        if line.contains(token) {
            return Some(token.to_string());
        }
    }
    if let Some(rest) = line.split("framework-exit-nonzero:").nth(1) {
        let code = rest.split_whitespace().next().unwrap_or("unknown");
        return Some(format!("framework-exit-nonzero:{code}"));
    }
    if line.contains("exit_code=") && !line.contains("exit_code=0") {
        let code = line
            .split("exit_code=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or("unknown");
        return Some(format!("framework-exit-nonzero:{code}"));
    }
    None
}

fn porcelain_path(line: &str) -> Option<&str> {
    line.get(3..).map(str::trim).filter(|path| !path.is_empty())
}

fn should_rollback_health_failure(failure_class: &str) -> bool {
    !matches!(
        failure_class,
        "conformance-missing" | "conformance-fail" | "self-test-fail" | "self-test-fail:missing"
    ) && !failure_class.starts_with("self-test-fail:")
}

struct RollbackEvidenceFields<'a> {
    failure_class: &'a str,
    original_failure_class: &'a str,
    candidate_sha: &'a str,
    known_good_sha: &'a str,
    trigger: &'a str,
    action: &'a str,
    reusable: &'a str,
}

fn rollback_evidence_line(fields: RollbackEvidenceFields<'_>) -> String {
    format!(
        "event=known-good-rollback failure_class={} original_failure_class={} candidate={} known_good={} trigger={} action={} reusable={} trial_ref=none failure_owner=known-good-auto-rollback",
        fields.failure_class,
        fields.original_failure_class,
        fields.candidate_sha,
        fields.known_good_sha,
        fields.trigger,
        fields.action,
        fields.reusable
    )
}

fn rollback_count(path: Option<&Path>, candidate_sha: &str) -> usize {
    let Some(path) = path else {
        return 0;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let needle = format!("candidate={candidate_sha}");
    text.lines()
        .filter(|line| line.contains("event=known-good-rollback") && line.contains(&needle))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framework_failure_class_reads_exit_and_stall_facts() {
        assert_eq!(
            framework_failure_class(
                "event=framework-failed dept=evolve exit_code=7 timed_out=false elapsed_ms=42 stderr=boom"
            ),
            Some("framework-exit-nonzero:7".to_string())
        );
        assert_eq!(
            framework_failure_class(
                "event=framework-stall-timeout dept=evolve stall_window=600s elapsed_ms=600000"
            ),
            Some("framework-stall-timeout".to_string())
        );
        assert_eq!(framework_failure_class("event=other"), None);
    }

    #[test]
    fn rollback_evidence_line_preserves_failure_owner_fields() {
        let line = rollback_evidence_line(RollbackEvidenceFields {
            failure_class: "rollback-blocked-dirty-worktree",
            original_failure_class: "framework-exit-nonzero:7",
            candidate_sha: "candidate-sha",
            known_good_sha: "known-good-sha",
            trigger: "reconcile",
            action: "blocked",
            reusable: "no",
        });

        assert!(line.contains("event=known-good-rollback"));
        assert!(line.contains("failure_class=rollback-blocked-dirty-worktree"));
        assert!(line.contains("original_failure_class=framework-exit-nonzero:7"));
        assert!(line.contains("candidate=candidate-sha"));
        assert!(line.contains("known_good=known-good-sha"));
        assert!(line.contains("action=blocked"));
        assert!(line.contains("failure_owner=known-good-auto-rollback"));
    }

    #[test]
    fn self_test_failure_class_keeps_specific_socket() {
        assert_eq!(
            self_test_failure_class("SELF_TEST_FAILED:permit-pool: boom"),
            "self-test-fail:permit-pool"
        );
        assert_eq!(self_test_failure_class("plain failure"), "self-test-fail");
    }

    #[test]
    fn should_rollback_only_framework_runtime_failures() {
        assert!(should_rollback_health_failure("framework-exit-nonzero:7"));
        assert!(should_rollback_health_failure("framework-stall-timeout"));
        assert!(!should_rollback_health_failure("conformance-fail"));
        assert!(should_rollback_health_failure("known-good-ref-unreadable"));
    }
}

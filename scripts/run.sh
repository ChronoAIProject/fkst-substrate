#!/usr/bin/env bash
# Repository-local command wrapper.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# github-devloop's implement local-iteration gate passes
# FKST_LOCAL_ITERATION_RESULT_FILE and reads exactly one line from it:
#
#   FKST_LOCAL_ITERATION_RESULT:v2:<VERDICT>:<FAULT_CLASS>
#
# A missing, multi-line, or unrecognised marker resolves to UNKNOWN, because a
# raw non-zero exit carries no domain meaning under POSIX. Emitting nothing is
# therefore not neutral: a *successful* verification is also read as UNKNOWN,
# which the package checkpoints and replays forever, so a strand never
# terminates even when its work is green (fkst-substrate#317).
#
# Only classes this script can establish are emitted. verify.sh currently
# signals every gate failure with a bare `exit 1`, so a failing verification
# stays UNKNOWN rather than being guessed at as SEMANTIC — misreporting an
# infrastructure fault as "your diff is wrong" would be worse than the honest
# UNKNOWN it replaces. Instrumenting verify.sh per gate is the remaining work.
result_file="${FKST_LOCAL_ITERATION_RESULT_FILE:-}"
# Unset before invoking any child so a nested runner cannot clobber this
# result file, mirroring fkst-packages' local_iteration_result_arm.
unset FKST_LOCAL_ITERATION_RESULT_FILE

# The result file is an INTERNAL nesting channel: a parent runner sets it so a
# nested runner's verdict can be merged instead of racing the parent's own. The
# consumer that actually classifies the attempt is harvest, which reads the
# marker off the process's stdout/stderr and never opens that file
# (fkst-packages local_iteration_result.lua:65-66, from_command). harvest
# invokes this gate WITHOUT setting FKST_LOCAL_ITERATION_RESULT_FILE, so a
# file-only emitter writes into a channel nobody reads and every verdict stays
# UNKNOWN. Mirror the reference emitter exactly: file when nesting, else stderr.
emit_local_iteration_result() {
  local marker
  marker="$(printf 'FKST_LOCAL_ITERATION_RESULT:v2:%s' "$1")"
  if [ -n "$result_file" ]; then
    printf '%s\n' "$marker" > "$result_file"
  else
    printf '%s\n' "$marker" >&2
  fi
}

run_verification() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "scripts/run.sh: cargo not found on PATH" >&2
    emit_local_iteration_result "FAIL:TOOLCHAIN"
    return 1
  fi
  local status=0
  "$repo/scripts/verify.sh" || status=$?
  # verify.sh exits with a distinct code per gate so the failure can be typed.
  # Every one of these gates fails because the tree under test is wrong — a
  # compile error, a failing test, an oversized supervisor, an unparseable
  # script, a conformance violation. That is SEMANTIC. An exit code outside the
  # known set is genuinely unclassifiable and stays UNKNOWN rather than being
  # guessed at, since misreporting an infrastructure fault as "your diff is
  # wrong" is worse than admitting ignorance.
  # Only gates that cannot fail for an environmental reason may accuse the tree.
  # The audit, shell-syntax, self-test, conformance and lua-test gates each ran
  # to a verdict about the tree, so their failure is SEMANTIC.
  #
  # `cargo build` (12) and `cargo test` (13) are excluded on purpose: each
  # conflates "ran and rejected the tree" with "could not run" — lock
  # contention, target-dir/IO failure, a registry fetch. #327 observed exactly
  # that: #314's base probe reported base_exit=12 -> SEMANTIC for
  # 171c7395, while CI on that same sha was green three times over. Until the
  # two are told apart they stay UNKNOWN, because an honest UNKNOWN costs a
  # redrive while a false SEMANTIC parks a healthy strand on an accusation.
  case "$status" in
    0)              emit_local_iteration_result "PASS:NONE" ;;
    10|11|14|15|16) emit_local_iteration_result "FAIL:SEMANTIC" ;;
    *)              emit_local_iteration_result "UNKNOWN:UNKNOWN" ;;
  esac
  return "$status"
}

case "${1:-test}" in
  test)
    shift || true
    if (($#)); then
      echo "scripts/run.sh test: unexpected arguments: $*" >&2
      emit_local_iteration_result "FAIL:CONFIGURATION"
      exit 2
    fi
    run_verification
    ;;
  test-affected)
    # The github-devloop implement local-iteration verify invokes
    # `scripts/run.sh test-affected <affected-paths...>` as the scoped pre-check
    # contract (fkst-packages scopes to affected packages for speed). This is a
    # Rust repo with no per-file affected-test scoping, so accept and ignore the
    # affected-path arguments and run the full verification. Honouring the
    # contract (a real pass/fail) is required: without a `test-affected` case the
    # command falls through to the usage error below (exit 2), which the implement
    # reads as `local-iteration-attribution-indeterminate` and fails closed,
    # blocking every substrate-pipeline engine change from developing.
    shift || true
    run_verification
    ;;
  *)
    echo "usage: scripts/run.sh {test|test-affected}" >&2
    emit_local_iteration_result "FAIL:CONFIGURATION"
    exit 2
    ;;
esac

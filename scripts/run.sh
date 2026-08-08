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

emit_local_iteration_result() {
  [ -n "$result_file" ] || return 0
  printf 'FKST_LOCAL_ITERATION_RESULT:v2:%s\n' "$1" > "$result_file"
}

run_verification() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "scripts/run.sh: cargo not found on PATH" >&2
    emit_local_iteration_result "FAIL:TOOLCHAIN"
    return 1
  fi
  local status=0
  "$repo/scripts/verify.sh" || status=$?
  if [ "$status" -eq 0 ]; then
    emit_local_iteration_result "PASS:NONE"
  else
    emit_local_iteration_result "UNKNOWN:UNKNOWN"
  fi
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

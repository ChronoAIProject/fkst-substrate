#!/usr/bin/env bash
# Repository-local command wrapper.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "${1:-test}" in
  test)
    shift || true
    if (($#)); then
      echo "scripts/run.sh test: unexpected arguments: $*" >&2
      exit 2
    fi
    "$repo/scripts/verify.sh"
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
    "$repo/scripts/verify.sh"
    ;;
  *)
    echo "usage: scripts/run.sh {test|test-affected}" >&2
    exit 2
    ;;
esac

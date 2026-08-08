#!/usr/bin/env bash
# Canonical verification gates for fkst-substrate. CI runs this exact script,
# so local and CI verification stay identical. Run from the repo root.
set -euo pipefail

# Each gate exits with its own code so scripts/run.sh can map affirmative
# evidence to a typed FKST_LOCAL_ITERATION_RESULT fault class. Cargo diagnostics
# and test-process outcomes are classified from structured output; an ambiguous
# non-zero result remains UNKNOWN. CI only distinguishes zero from non-zero.
GATE_AUDIT=10                # supervisor LOC audit
GATE_SHELL=11                # shell syntax
GATE_BUILD_SEMANTIC=12       # compiler rejected the tree
GATE_TEST_SEMANTIC=13        # compiler or started test process rejected the tree
GATE_SELFTEST=14             # framework startup self-test
GATE_CONFORMANCE=15          # package conformance
GATE_LUATEST=16              # package Lua tests
GATE_BUILD_INFRASTRUCTURE=17 # cargo build could not start
GATE_TEST_INFRASTRUCTURE=18  # cargo test or its test process could not start
GATE_BUILD_UNKNOWN=19        # cargo build failed without affirmative evidence
GATE_TEST_UNKNOWN=20         # cargo test failed without affirmative evidence
GATE_SCRIPT_TEST=21          # verification-script regression tests
GATE_TOOLCHAIN=22            # required verification tool is unavailable

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Verification runs framework commands directly, not as supervisor children.
unset FKST_SUPERVISOR_PID

command -v python3 >/dev/null 2>&1 || {
  echo "scripts/verify.sh: python3 not found on PATH" >&2
  exit "$GATE_TOOLCHAIN"
}

# Tier I audit gate: supervisor must stay readable in one sitting.
loc=$(find crates/fkst-supervisor/src -name '*.rs' -exec cat {} + | wc -l)
[ "$loc" -le 150 ] || { echo "supervisor exceeds 150 LOC audit gate: $loc" >&2; exit "$GATE_AUDIT"; }

# Shell syntax gate: operator convenience scripts must stay parseable.
for s in scripts/*.sh; do bash -n "$s" || exit "$GATE_SHELL"; done

PYTHONDONTWRITEBYTECODE=1 \
  python3 -m unittest discover -s scripts/tests -p 'test_*.py' \
  || exit "$GATE_SCRIPT_TEST"

run_cargo_gate() {
  local semantic_code="$1"
  local infrastructure_code="$2"
  local unknown_code="$3"
  shift 3
  local status=0
  python3 "$repo/scripts/cargo_gate.py" "$@" || status=$?
  case "$status" in
    0)  return 0 ;;
    20) return "$semantic_code" ;;
    21) return "$infrastructure_code" ;;
    *)  return "$unknown_code" ;;
  esac
}

run_cargo_gate \
  "$GATE_BUILD_SEMANTIC" "$GATE_BUILD_INFRASTRUCTURE" "$GATE_BUILD_UNKNOWN" \
  build --workspace || exit "$?"
run_cargo_gate \
  "$GATE_TEST_SEMANTIC" "$GATE_TEST_INFRASTRUCTURE" "$GATE_TEST_UNKNOWN" \
  test --workspace -- --test-threads=1 || exit "$?"

# Scratch outside the source tree: package root is read-only at runtime.
scratch=$(mktemp -d); trap 'rm -rf "$scratch"' EXIT

# Startup self-test needs a writable runtime root for its codex permit pool.
env -u FKST_SUPERVISOR_PID \
  FKST_RUNTIME_ROOT="$scratch/runtime" \
  target/debug/fkst-framework --self-test || exit "$GATE_SELFTEST"

# Conformance + Lua tests run against a writable copy of the minimal package.
host="$scratch/host"; mkdir -p "$host"
cp -R examples/minimal-package/. "$host/"
env -u FKST_SUPERVISOR_PID \
  target/debug/fkst-framework conformance --project-root "$host" --package-root "$host" \
  || exit "$GATE_CONFORMANCE"
env -u FKST_SUPERVISOR_PID \
  target/debug/fkst-framework test --project-root "$host" --package-root "$host" \
  || exit "$GATE_LUATEST"

#!/usr/bin/env bash
# Canonical verification gates for fkst-substrate. CI runs this exact script,
# so local and CI verification stay identical. Run from the repo root.
set -euo pipefail

# Each gate exits with its own code so scripts/run.sh can map the failure to a
# typed FKST_LOCAL_ITERATION_RESULT fault class. A bare `exit 1` cannot be
# classified, and the consumer deliberately resolves an unclassifiable non-zero
# exit to UNKNOWN, which the devloop checkpoints and replays forever rather than
# terminating the strand (fkst-substrate#317). CI only distinguishes zero from
# non-zero, so these codes do not change its verdict.
GATE_AUDIT=10      # supervisor LOC audit
GATE_SHELL=11      # shell syntax
GATE_BUILD=12      # cargo build
GATE_TEST=13       # cargo test
GATE_SELFTEST=14   # framework startup self-test
GATE_CONFORMANCE=15 # package conformance
GATE_LUATEST=16    # package Lua tests

# Verification runs framework commands directly, not as supervisor children.
unset FKST_SUPERVISOR_PID

# Tier I audit gate: supervisor must stay readable in one sitting.
loc=$(find crates/fkst-supervisor/src -name '*.rs' -exec cat {} + | wc -l)
[ "$loc" -le 150 ] || { echo "supervisor exceeds 150 LOC audit gate: $loc" >&2; exit "$GATE_AUDIT"; }

# Shell syntax gate: operator convenience scripts must stay parseable.
for s in scripts/*.sh; do bash -n "$s" || exit "$GATE_SHELL"; done

cargo build --workspace || exit "$GATE_BUILD"
cargo test --workspace -- --test-threads=1 || exit "$GATE_TEST"

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

#!/usr/bin/env bash
# Canonical verification gates for fkst-substrate. CI runs this exact script,
# so local and CI verification stay identical. Run from the repo root.
set -euo pipefail

unset FKST_SUPERVISOR_PID

# Tier I audit gate: supervisor must stay readable in one sitting.
loc=$(find crates/fkst-supervisor/src -name '*.rs' -exec cat {} + | wc -l)
[ "$loc" -le 150 ] || { echo "supervisor exceeds 150 LOC audit gate: $loc" >&2; exit 1; }

# Shell syntax gate: operator convenience scripts must stay parseable.
for s in scripts/*.sh; do bash -n "$s"; done

cargo build --workspace
cargo test --workspace -- --test-threads=1

# Scratch outside the source tree: package root is read-only at runtime.
scratch=$(mktemp -d); trap 'rm -rf "$scratch"' EXIT

# Startup self-test needs a writable runtime root for its codex permit pool.
FKST_RUNTIME_ROOT="$scratch/runtime" target/debug/fkst-framework --self-test

# Conformance + Lua tests run against a writable copy of the minimal package.
host="$scratch/host"; mkdir -p "$host"
cp -R examples/minimal-package/. "$host/"
target/debug/fkst-framework conformance --project-root "$host" --package-root "$host"
target/debug/fkst-framework test --project-root "$host" --package-root "$host"

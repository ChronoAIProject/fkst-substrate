# fkst-substrate

`fkst-substrate` is a stable supervised event, SDK, and process substrate. It provides the process-root supervisor, framework runtime, shared types, Lua SDK, event dispatch, runtime layout, worktree/lock/codex process contracts, and conformance gates. Business Lua packages, department topology, host workflow policy, release policy, and product-specific behavior live outside this repository and are injected through package roots or a host root.

The default integration branch is `dev`.

## Governing Practice

This README follows docs-as-code and single-source-of-truth practice: it is a source-verified entrypoint, not a second specification. Stable identity and contract details belong in `SPEC.md`, engine architecture belongs in `docs/architecture.md`, and executable behavior belongs in `crates/`, `examples/`, and `scripts/verify.sh`.

## Verification

The canonical local and CI gate is:

```sh
scripts/verify.sh
```

`scripts/run.sh test` is the repository-local wrapper for the same gate:

```sh
scripts/run.sh test
```

As of this tree, `scripts/verify.sh` runs these checks from the repository root:

- Tier I supervisor audit: all Rust source under `crates/fkst-supervisor/src` must be at most 150 lines.
- Shell syntax audit for `scripts/*.sh`.
- `cargo build --workspace`.
- `cargo test --workspace -- --test-threads=1`.
- `target/debug/fkst-framework --self-test` with a scratch `FKST_RUNTIME_ROOT`.
- `target/debug/fkst-framework conformance --project-root <scratch-host> --package-root <scratch-host>`.
- `target/debug/fkst-framework test --project-root <scratch-host> --package-root <scratch-host>`.

CI runs `./scripts/verify.sh` in `.github/workflows/ci.yml`. The coverage job is explicitly non-gating visualization.

## Workspace Layout

The Rust workspace currently contains four crates:

| crate | role |
|---|---|
| `crates/fkst-supervisor` | Tier I process root. It locates and spawns `fkst-framework supervise`, inherits stdout/stderr, handles process-level signals, waits, and reaps. It does not depend on `fkst-common` or `fkst-framework`. |
| `crates/fkst-common` | Shared engine types such as config, events, validation, and `RuntimeLayout`. |
| `crates/fkst-framework` | Tier III runtime, CLI, graph scan, source runner, dispatch, Lua SDK, boundary adapters, self-test, package tests, and conformance. |
| `crates/fkst-update` | Standalone verify-and-swap deploy client for externally produced GitHub Release artifacts. It is not in the supervisor/framework hot path. |

There is no business Lua package crate in this repository. `examples/minimal-package/` is an engine fixture.

## Framework CLI

`fkst-framework` currently exposes these user-facing commands:

```text
fkst-framework run <lua> --project-root <path> --package-root <path> [--package-root <path> ...] [--owner-namespace <id>] --event <json>
fkst-framework supervise --project-root <path> --framework-bin <path> [--package-root <path> ...]
fkst-framework conformance --project-root <path> [--package-root <path> ...] [--config <path>]
fkst-framework config --project-root <path> [--package-root <path> ...]
fkst-framework boundary-resources
fkst-framework rate-acquire <pool>
fkst-framework rate-exec <pool> -- <program> [args...]
fkst-framework host lock --project-root <path> [--package-root <path> ...]
fkst-framework test --project-root <path> [--package-root <path> ...] [--report-json <path>] [--coverage <dir>]
fkst-framework deps [lock|fetch] --project-root <path> [--package-root <path> ...] [--json] [--locked]
fkst-framework manifest composed-deps --manifest <path>
fkst-framework init-package-repo [--ref <substrate-ref>] [--force]
fkst-framework observe --durable-root <path> [--json] [--limit <n>]
fkst-framework --self-test [--coverage <dir>]
```

The internal `__codex-worker` command is not package-facing API.

## Configuration Registry

Engine operation configuration is declared by the static typed registry in `crates/fkst-framework/src/config_registry.rs`. Resolution order is process environment, host `fkst.env`, then an operational default. `HostFact` entries have no default and fail closed when absent. The registry is read-only: it has no set, apply, watch, YAML, DSL, manifest, plugin, or `tunables/*.txt` compatibility layer.

Current registry entries:

| name | env key | kind | type | default / required |
|---|---|---|---|---|
| `queue_capacity` | `FKST_QUEUE_CAPACITY` | `Operational` | `usize` | default `16` |
| `department_default_stall_window` | `FKST_DEPARTMENT_DEFAULT_STALL_WINDOW` | `Operational` | `duration-string` | default `30s`; Department delivery lease window |
| `codex_permit_slots` | `FKST_CODEX_PERMIT_SLOTS` | `Operational` | `usize` | default `20` |
| `max_in_flight_per_dept` | `FKST_MAX_IN_FLIGHT_PER_DEPT` | `Operational` | `usize` | default `16` |
| `durable_admission_burst_per_dept` | `FKST_DURABLE_ADMISSION_BURST_PER_DEPT` | `Operational` | `usize` | default `1` |
| `subscriber_absent_delivery_budget` | `FKST_SUBSCRIBER_ABSENT_DELIVERY_BUDGET` | `Operational` | `duration-string` | default `168h`; continuous absence before DLQ |
| `rate_pool_root` | `FKST_RATE_POOL_ROOT` | `Operational` | `string` | default `~/.fkst/rate-pools` |
| `retry_default_max_attempts` | `FKST_RETRY_DEFAULT_MAX_ATTEMPTS` | `Operational` | `usize` | default `5` |
| `retry_default_base` | `FKST_RETRY_DEFAULT_BASE` | `Operational` | `duration-string` | default `60s` |
| `retry_default_cap` | `FKST_RETRY_DEFAULT_CAP` | `Operational` | `duration-string` | default `30m` |
| `candidate_prefix` | `FKST_CANDIDATE_PREFIX` | `HostFact` | `string` | required |
| `candidate_from_sep` | `FKST_CANDIDATE_FROM_SEP` | `HostFact` | `string` | required |

Read-only introspection:

```sh
target/debug/fkst-framework config \
  --project-root "$PWD/examples/minimal-package" \
  --package-root "$PWD/examples/minimal-package"
```

## Boundary Resources

Boundary resources follow capability-security practice: no ambient authority. Every external resource class the framework can touch must be listed in `crates/fkst-framework/src/boundary_resource.rs` and mediated through an adapter, grant, meter, budget/backpressure posture, and typed error contract.

Current boundary resource ids are:

- `codex.process`
- `shell.process`
- `argv.process`
- `git.process`
- `runtime.filesystem`
- `wall-clock`

Read-only introspection:

```sh
target/debug/fkst-framework boundary-resources
```

Classified adapter failures use `error_class` values from the same registry: `quota-exhausted`, `auth-degraded`, `provider-unavailable`, and `provider-throttle`.

## Runtime Model

The event flow is:

```text
source -> fanout -> route -> spawn fkst-framework run -> pipeline(event) -> RAISED -> dispatch
```

Sources are declared by `raisers/*.lua`. Departments are `departments/<dept>/main.lua` files that return `M.spec` and implement `pipeline(event)`. A delivered event has the standard shape `Event { queue, payload, ts }`, where `ts` is Unix milliseconds generated by the runtime on real dispatch.

Package roots are supplied by repeated `--package-root`, `FKST_PACKAGE_ROOTS`, or the legacy single `FKST_PACKAGE_ROOT`. `FKST_PACKAGE_ROOTS` uses the platform path-list separator. When both `FKST_PACKAGE_ROOTS` and `FKST_PACKAGE_ROOT` are set without explicit `--package-root`, the framework fails closed.

Queue names are package-local. In composed multi-root graphs, bare queue names resolve inside the owner namespace and cross-package consumption must use `pkg.queue`. A folded single package/host root preserves legacy flat byte output for queue names.

`FKST_RUNTIME_ROOT` is engine scratch for `worktrees`, `codex-permits`, `locks`, `logs`, `marks`, and `cache`. Durable business facts must come from git refs/commits/branches, explicit host filesystem facts, or external sources. Runtime scratch is not accepted release state, package state, rollback state, or a durable business database.

Reliable delivery state uses `FKST_DURABLE_ROOT` and the framework delivery store. It is a delivery lease, retry, and DLQ ledger, not an entity fact store.

If a pending durable delivery's queue has no current subscriber, supervise records `subscriber_absent_since_ms` in the delivery row. If the subscriber returns before `FKST_SUBSCRIBER_ABSENT_DELIVERY_BUDGET` elapses, the delivery resumes normally and the absence clock is cleared. If the absence remains continuous past the budget, the supervisor sweep moves the delivery to the replayable DLQ with `error_excerpt` set to `subscriber-absent`.

## Lua SDK Surface

The fixed production Lua SDK surface is anchored in `SPEC.md`; Rust-registered runtime primitives are wired through `crates/fkst-framework/src/mlua_init.rs`. Current surface names include:

```text
pipeline
source
raise
spawn_codex_sync
spawn_codex
await_all
exec_sync
exec_argv
with_lock
once
cache_set
cache_get
cache_expire
graph_json
t
restricted_lua_load
truncate_utf8
git_log_count
git_log_grep
count_worktrees
list_orphan_worktrees
setup_worktree
file.read
file.write
file.exists
file.list
json.decode
toml.decode
log.info
log.warn
log.error
now
fkst.codex_runs
fkst.observe
```

`json` is decode-only. `fkst.test` is registered only by `fkst-framework test`; production `run`, `supervise`, `--self-test`, and conformance do not expose the test table.

See `SPEC.md` for the normative SDK contract and `docs/architecture.md` for operational details.

## Minimal Package Fixture

`examples/minimal-package/` is the repository fixture for package-root loading and graph validation. It contains:

- a cron raiser `tick`;
- a `producer` Department that consumes `tick` and raises `example_event`;
- a `consumer` Department that consumes `example_event` and logs the full standard event;
- Lua tests under `tests/*_test.lua`.

Manual fixture check:

```sh
cargo build --workspace
repo="$PWD"
tmp_host="$(mktemp -d)"
cp -R examples/minimal-package/. "$tmp_host/"

target/debug/fkst-framework conformance \
  --project-root "$tmp_host" \
  --package-root "$tmp_host"

target/debug/fkst-framework test \
  --project-root "$tmp_host" \
  --package-root "$tmp_host"

(
  cd "$tmp_host" &&
  "$repo/target/debug/fkst-framework" run \
    "$tmp_host/departments/producer/main.lua" \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --owner-namespace "$(basename "$tmp_host")" \
    --event '{"queue":"tick","payload":{"raiser":"tick"}}'
)
```

`run --event` injects one pipeline directly; it does not go through `supervise` routing.

## Package Repository Scaffold

`fkst-framework init-package-repo [--ref <substrate-ref>] [--force]` materializes an engine-owned package-repository scaffold in the current git repository. It writes templates such as `scripts/run.sh`, `scripts/check_repo.py`, `.github/workflows/ci.yml`, `env.example`, `.fkst-substrate-ref`, `.gitignore` entries, and a minimal `README.md` pointer.

The command is idempotent. Identical files are reported as `UNCHANGED`; missing files are created; local edits to owned template files are refused unless `--force` is passed. `.gitignore` updates are append-only for scaffold entries. The command does not touch `packages/`, git history, or remotes.

With no `--ref`, the scaffold pins the running engine binary's build-time source revision. Operators can pass `--ref <substrate-ref>` for an explicit release tag or commit.

## Install And Update

`scripts/install.sh` is an operator convenience script, not engine surface. It builds the workspace in release mode, installs `fkst-supervisor`, `fkst-framework`, and `fkst-update` into `$FKST_HOME/bin` (default `~/fkst/bin`), creates the package root and runtime root, and generates `$FKST_HOME/bin/fkst-run`.

```sh
scripts/install.sh
FKST_HOME=/opt/fkst FKST_PACKAGE_ROOT=/srv/fkst-packages scripts/install.sh
```

The generated `fkst-run` launcher sets `FKST_PACKAGE_ROOT`, `FKST_RUNTIME_ROOT`, and `FKST_FRAMEWORK_BIN`, then executes `fkst-supervisor` from the package root. The engine binaries themselves carry no default package root.

`fkst-update` is a standalone deploy client:

```sh
fkst-update
fkst-update --tag v0.1.0
fkst-update --bin-dir /opt/fkst/bin
```

It downloads the externally produced `fkst-<target>.tar.gz` and `SHA256SUMS` from GitHub Releases, verifies the SHA-256 checksum, and atomically swaps `fkst-supervisor` and `fkst-framework` in the selected bin directory. It does not build from source, maintain known-good state, implement rollback, run health gates, run canaries, signal a supervisor, or restart processes.

The release producer is `.github/workflows/release.yml`, triggered by `v*` tags.

For a source checkout tracking `dev`, update by running:

```sh
git pull --ff-only
scripts/verify.sh
scripts/install.sh
```

## Release Boundary

Accepted release state is an external release-pipeline fact, not engine runtime state. The recommended external chain is build, test, `--self-test`, conformance, signed artifact, deploy, canary, and rollback policy. `fkst-substrate` does not own runtime accepted-state or rollback.

## Documentation

- `CLAUDE.md` and `AGENTS.md`: repository governance and engine philosophy.
- `SPEC.md`: Tier II identity anchor and normative contract surface.
- `docs/architecture.md`: detailed engine architecture.
- `crates/fkst-framework/src/config_registry.rs`: source-owned engine operation registry.
- `crates/fkst-framework/src/boundary_resource.rs`: source-owned boundary-resource registry.
- `scripts/verify.sh`: canonical verification gate used by local development and CI.

Before consuming this repository as a stable substrate, rely on the current `scripts/verify.sh` result and the local conformance result, not on README prose alone.

⟦AI:FKST⟧

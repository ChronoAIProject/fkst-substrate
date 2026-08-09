# fkst-substrate

`fkst-substrate` is the stable supervised event, SDK, and process substrate for FKST. It ships the process-root supervisor, framework runtime, common engine types, Lua SDK, event dispatch, runtime layout, worktree/lock/codex boundaries, and conformance gates. Business packages, Department topology, host workflow policy, deployment replacement, and release policy live outside this repository.

The default integration branch is `dev`. `SPEC.md` is the normative engine contract, `docs/architecture.md` is the descriptive implementation account, and `docs/package-repo-contract.md` states package-author obligations.

## Install

Build and install the engine binaries with:

```sh
scripts/install.sh
FKST_HOME=/opt/fkst FKST_PACKAGE_ROOT=/srv/fkst-packages scripts/install.sh
```

The script installs `fkst-supervisor`, `fkst-framework`, and `fkst-update` into `$FKST_HOME/bin` (default `~/fkst/bin`) and generates `fkst-run`. The generated launcher selects the shipped `fkst-supervisor` entry topology. Direct `fkst-framework supervise` is also an engine-supported topology; selecting and replacing either topology is operator policy, not an engine guarantee. See the outcome matrix in `SPEC.md` and the process sequence in `docs/architecture.md`.

`fkst-update` downloads externally produced release artifacts and `SHA256SUMS`, verifies SHA-256, and atomically replaces installed engine binaries. It does not restart a process, select a deployment topology, maintain accepted state, or implement rollback, health gates, or canaries.

## Verification

The canonical local and CI gate is:

```sh
scripts/verify.sh
```

The repository-local wrapper runs the same gate:

```sh
scripts/run.sh test
```

The gate audits the Tier I supervisor size and shell syntax, runs verification-script regression tests, builds and tests the Rust workspace, runs `fkst-framework --self-test`, and exercises conformance and package-test entrypoints against a scratch host. CI invokes `./scripts/verify.sh` from `.github/workflows/ci.yml`.

Read-only inspection commands include:

```sh
fkst-framework config --project-root <host> --package-root <package>
fkst-framework boundary-resources
fkst-framework observe --durable-root <path> --json --limit 100
```

The full CLI and runtime mechanisms are described in `docs/architecture.md`; stable surface and failure guarantees are anchored in `SPEC.md`.

⟦AI:FKST⟧

# Package Repository Contract

This document states package-author obligations. Engine topology, signal handling, delivery mechanics, and runtime-generation outcomes are defined by `SPEC.md` and described in `docs/architecture.md`; they are not repeated here.

## Package Boundary

A package supplies `departments/`, `raisers/`, owner-scoped Lua modules, fixtures, and assets through an installed package root. Package identity is its canonical root basename. Names must match `[A-Za-z0-9_-]+`; cross-package queues use the explicit `pkg.queue` form. Package code must not assume it can require a sibling package or mutate its installed package root at runtime.

A Department exports `M.spec` and `pipeline(event)`. It receives `Event { queue, payload, ts }`. Each call runs in a fresh Lua state with no lifecycle hook, shared memory, persistent Person identity, or direct Person-to-Person channel.

## Durable Facts

Package decisions that must survive process death must be derived from one of these authoritative boundaries:

- git refs, commits, and branches;
- an explicit host-filesystem fact outside engine scratch;
- an authoritative external source.

`FKST_RUNTIME_ROOT` content, including worktrees, locks, permits, logs, marks, cache, and codex-adoption traces, is engine scratch. Logs are evidence for debugging, not reconciliation input. A reliable delivery row in `FKST_DURABLE_ROOT` is durable delivery intent, not a package entity or accepted-release fact.

Reliable payloads must remain small control messages. Entity content belongs in an authoritative boundary and should be re-read at consumption time through a stable `source_ref`. Package code must not create an inbox, completion database, cursor, accepted-state marker, or rollback record under the package root or runtime root.

## Replay And Idempotence

Reliable delivery is at-least-once-until-ACK, not exactly-once. A process can die after an external effect but before its delivery is acknowledged. An expired lease can then be delivered again, and old execution may overlap replay. Package authors must therefore:

- make every repeatable external mutation idempotent at the authoritative destination;
- use a stable operation key or authoritative completion fact before issuing a non-idempotent effect;
- treat `event.ts`, lease timing, logs, cache entries, and `once` markers as non-authoritative;
- tolerate the same `pipeline(event)` running more than once and in overlapping runtime generations;
- re-read current facts instead of relying on a previous Lua state, in-memory cursor, or unaccepted raised output;
- keep failure visible through a classified external fact, commit, host file, or engine log appropriate to the boundary.

`M.spec.ephemeral` explicitly opts a consumed queue out of reliable delivery. Package authors must tolerate loss of ephemeral work and must not depend on its redelivery; the normative termination outcome is defined only by the matrix in `SPEC.md`. `M.spec.retry = false` disables failure retry but does not make a reliable subscription ephemeral. `M.spec.stall_window` is a lease/renewal window, not a child completion deadline.

`once` and cache operations can suppress redundant work within available scratch, but clearing scratch, changing hosts, or reliable replay can bypass prior scratch. They cannot establish durable completion. Worktree-backed codex adoption can reconnect to the same engine-owned codex work record, but its trace is not package business truth and does not remove the package's idempotence obligation.

## Raised Events

`raise(queue, payload)` produces derived output. For reliable input, raised delivery inherits the authoritative `source_ref`; missing required provenance fails closed and prevents the parent ACK. Package code must assume a raised event may have been published before the parent ACK and may be encountered again through replay. It must not assume child stdout can cross an event-runtime generation; authenticated frames are accepted only by the runtime that created the invocation.

## Package Verification

Run package tests and conformance against the actual host and package roots:

```sh
fkst-framework test --project-root <HOST> --package-root <PACKAGE> --report-json <PATH>
fkst-framework conformance --project-root <HOST> --package-root <PACKAGE>
```

Tests must cover duplicate delivery, replay after an effect but before completion, stale scratch, and any operation whose destination lacks native idempotency. External commands in `fkst-framework test` must use the test-only mock/cassette boundary; unmocked commands fail closed.

⟦AI:FKST⟧

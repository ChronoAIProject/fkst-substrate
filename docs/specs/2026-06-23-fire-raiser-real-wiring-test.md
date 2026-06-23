# `fkst.test.fire_raiser` — test the REAL producer→consumer wiring (engine primitive)

Status: DESIGN — sshx adversarial (minimal/structural/delete codex triplet + ChatGPT Pro), converged `implement`.
Date: 2026-06-23
Repo: fkst-substrate (engine).

## 1. Problem (the engine-level root of an incident class)

The test surface is only `fkst.test.{run_department, mock_command, command_calls}`. `run_department`
**injects a hand-built event** into one department — so tests prove "given an IDEAL event, the
department logic runs" (CAN-RUN) but never "the REAL declared raiser emits a payload its consumer
ACCEPTS" (AUTO-TRIGGER). This masks the «ideal-trigger masks real-wiring-failure» class:

- Incident #1361: a cron raiser (`type=cron`, `produces=archaudit_tick`) emits the engine GENERIC
  cron tick; the consumer required a custom schema only a *fixture builder* produced → every real
  tick failed `unknown-schema` (terminal) → **0 production output**, while the ideal-injected test
  stayed GREEN.

The package-side producer-liveness contract (#1362) cannot truly PREVENT this, because its fixture
can still **hand-build an ideal payload**. Only the engine firing the REAL declared raiser makes the
fixture honest. This is a **framework-stable-public-part** concern: the wiring-test capability is
generic to every package, so it belongs in the engine.

## 2. Harness (prior art)

- **Consumer-driven contract testing** (Pact): the consumer is verified against what the producer
  ACTUALLY emits, not a hand-written stand-in.
- **"Don't mock what you are testing"**: the thing under test here IS the producer→consumer wiring,
  so the test must NOT substitute a synthetic payload for the real source emission.
- **Integration vs unit boundary**: `run_department` is the unit (department logic given an event);
  `fire_raiser` is the integration (the real source→route→consume edge).

## 3. Design principle (the converged invariant)

> A producer-liveness test's evidence must be **provably from a declared source firing**, never from
> a synthetic payload. The engine fires the real raiser and routes its actual emitted payload to the
> consumer; the test asserts acceptance + downstream effects on that real evidence.

Three layers (kept strictly separated — the unanimous triplet boundary):

| Layer | Owns | Must NOT own |
|---|---|---|
| **Engine (substrate)** | `fire_raiser`: resolve the declared raiser, FIRE it, capture its REAL emitted payload, route through fanout/graph to the consumer dept(s), execute, return a **trace/evidence** | any business schema, source kind semantics, retry policy, host topology, dashboard, durable fact |
| **Conformance (substrate, structural-only)** | the #1362 producer-liveness requirement that the liveness evidence **came from `fire_raiser`** (a hand-built payload is non-conformant) | understanding business schemas — purely structural |
| **Package** | domain assertions (consumer accepts? what downstream raises?), `file_watch` fixture files, the #1362 policy mapping declared raisers→required liveness tests | — |

## 4. Engine API — a SEPARATE primitive (not a `run_department` mode)

ChatGPT Pro's keystone: it must be a **separate primitive**, NOT `run_department(..., mode="fire_raiser")`
— because the entire safety property is *"this evidence came from a declared source, not a synthetic
payload."* A mode on `run_department` would still accept a caller payload and defeat the property.

```
-- Lua test surface (illustrative; exact spelling settled in implementation)
local trace = fkst.test.fire_raiser("<raiser_name>", { fixture = <file_watch fixture | nil for cron> })
--   trace.source_payload   -- the REAL payload the engine emitted (captured, not supplied)
--   trace.routed_to        -- the consumer department(s) the graph routed it to
--   trace.consumer_result  -- accepted? error? the consumer's outcome
--   trace.raised           -- downstream events the consumer raised
```

Behavior:
1. Resolve the declared raiser from the graph (`RaiserDecl::Cron` / `FileWatch`, `sdk_graph.rs`).
2. **Fire it for real** — produce the engine's actual emitted payload to the `produces` queue (the
   generic cron tick for cron; the file event for `file_watch`, driven by the package fixture file).
   The payload is CAPTURED, never caller-supplied.
3. Route it through fanout/graph (`event_fanout.rs`) to the consuming department(s).
4. Execute the consumer(s) reusing the lower-level `run_department` dispatch machinery (`test_runner.rs`).
5. Return the **trace** (evidence) for the test to assert on.

Determinism / side-effects (the GPT Pro design risk): firing a real cron raiser must be deterministic
in test mode — the engine supplies the tick payload directly (no wall-clock wait, no real scheduler);
`file_watch` is driven by an explicit package fixture file; external `gh`/`git`/`codex` stay mocked
via the existing `mock_command`/ports fakes. No real network/clock/scheduler enters the test.

## 5. Conformance composition (the PREVENT)

The #1362 producer-liveness conformance gains ONE structural requirement: a producer's liveness
evidence must reference a `fire_raiser` trace for that producer's declared raiser. A fixture that
hand-builds a payload (no `fire_raiser` trace) is **non-conformant** → CI red. This makes the
«ideal-trigger masks real-wiring-failure» class structurally impossible, not merely test-able:
`fire_raiser` is the DETECT capability; the conformance-requires-`fire_raiser` is the PREVENT.

The conformance stays **structural** (it checks the evidence came from `fire_raiser`); it does NOT
parse business schemas — those assertions live in the package test.

## 6. Migration

This implementation PR covers step 1 and the substrate self-test in step 2 only. The #1362
producer-liveness conformance hook and package fixture migration remain separate follow-ups.

1. Add `fkst.test.fire_raiser` (engine), reusing `run_department`'s dispatch; cron + file_watch.
2. `cargo test` + a substrate self-test proving: a cron raiser whose consumer rejects the real tick
   FAILS `fire_raiser` (the #1361 shape), and one that accepts it passes (mirrors the package fix).
3. Add the #1362 conformance structural hook (evidence-from-`fire_raiser`); shrink-only allowlist
   while packages migrate their producer-liveness fixtures to use it.
4. Package side (separate, fkst-packages): migrate producer-liveness fixtures (archaudit audit, idle
   producers, …) to `fire_raiser`; the package owns the domain assertions.

## 7. Non-goals

- No business-schema awareness in the engine or conformance (packages own domain assertions).
- Not a `run_department` mode (the synthetic-payload escape hatch is the bug).
- No real scheduler/clock/network in tests (deterministic fire; fixtures for file_watch).
- No change to production raiser/source runtime behavior (test-mode capability only).

## 8. Adversarial record

`sshx`: minimal/structural/delete codex triplet + ChatGPT Pro, `implement`.

- **minimal**: engine does graph-route-selection + consumer execution + reports evidence; package owns
  fixtures/assertions/required-liveness-edges. No production SDK/source-kind/dashboard/durable concept added.
- **structural**: engine emits readable `source_fire` evidence; packages own domain assertions + the
  #1362 policy mapping raisers→required liveness tests. No business schema/retry/topology in substrate.
- **delete**: conformance owns ONLY the structural requirement that producer-liveness evidence came
  from `fire_raiser`; it must not understand business schemas (keep the engine thin).
- **ChatGPT Pro**: a SEPARATE primitive (`drive_source`/`fire_raiser`), reusing only lower-level
  routing internally — NOT a `run_department` mode, because the safety property is "evidence from a
  declared source, not a synthetic payload." Flagged the determinism/side-effect risk → deterministic
  test-mode fire + fixtures.

```
[goal: test the REAL producer→consumer wiring]
   │ resolved-by
   ▼
[fire_raiser engine primitive] ──must-be──▶ [SEPARATE primitive, not run_department mode]  ◀─agree─ ChatGPT Pro
   │ depends-on                                   │ (else synthetic-payload escape defeats it)
   ▼                                              │
[engine: fire real raiser → real payload → route → consumer → trace]  ◀─agree─ minimal/structural/delete
   │ enables
   ▼
[#1362 conformance: evidence MUST come from fire_raiser]  ──PREVENT──▶ [ideal-trigger masking impossible]
   │ boundary (unanimous)
   ▼
[engine+conformance = structural only; package = domain assertions]
```

Meta-judge `implement`: unanimous on the separate-primitive + thin-engine + structural-conformance +
package-owns-domain boundary; ChatGPT Pro's separate-primitive keystone resolves the only design fork
(mode-vs-primitive). Every node resolved; no open conflict edge.

⟦AI:FKST⟧

# fkst-substrate spec: library-dependency primitive (companion to fkst-packages ADR 0001)

Status: in-implementation (2026-06-22). Derived via sshx triplet (minimal/structural/delete) + ChatGPT Pro, converged.

## Goal
Implement the engine side of ADR 0001: a unit (`package` | `library`) declares its code dependencies in a per-unit `fkst.toml`; the engine enforces declared require-scope (a unit may `require` only its own modules + the PUBLIC exports of libraries it DIRECTLY declares; undeclared/ambiguous = fail-closed). Replaces the per-package `std` symlink entirely. This is a CLEAN BREAKING change developed on an isolated branch — NO backward-compat / dual-mode / legacy fallback (per repo doctrine «不要历史兼容性，删就删干净»). The engine simply requires + enforces manifests; ALL fixtures + the real workspace adopt the new model; migration = merge the complete, tested change.

## Resolver design (the hard part — per ChatGPT Pro)
- **Per-unit `_ENV`-bound `require` closure**, NOT a package.searchers entry and NOT manifest-generated package.path. Lua checks `package.loaded[name]` BEFORE searchers, so a bare searcher cannot isolate cross-scope cache; package.path order hides ambiguity and over-exposes files. Bind `require` lexically per unit via mlua `Chunk::set_environment` (a proxy `_ENV` whose `require = require_for(unit)`, metatable `__index/__newindex = shared_globals`); lexical (not a dynamic current-unit stack) because a library function may `require` lazily after its loader returned.
- **Resolution** for caller unit U + logical module M: candidates = U's own modules matching M + PUBLIC modules matching M from each library in `U.lib_deps` (DIRECT edges only — package→A→B: A may require B, package may not). `0 candidates → deny`, `1 → resolve`, `2+ → deny ambiguous`.
- **Cache key** = `\x1ffkst:<provider-unit-id>:<logical-module>` (keyed by PROVIDER unit + module, not caller) so same-named private modules don't poison each other; one shared-library instance per department PROCESS (departments are already separate processes → package.loaded already per-process).
- The custom loader executes the chunk with the ORIGINAL logical name as `...`, set_name("@path") for tracebacks.
- **Enforce mode replaces searchers**: `package.searchers = { engine_preload_searcher?, fkst_exact_file_searcher }`; remove default Lua/C/all-in-one searchers; package.path empty in enforce mode (owner-only retained in legacy/compat). Rust/C modules = fixed engine preloads, no package.cpath.
- Native `loadfile` and `dofile` are not reachable in unit environments. `load` stays available for in-memory chunks, but its default environment reaches the engine's erroring raw global `require`, not Lua's native package searchers.

## Exports = physical layout (NOT manifest globs — avoids a 2nd drifting API inventory)
```
libraries/<lib>/fkst.toml + public/<modtree> + private/<modtree>
```
- lib resolving its OWN modules sees public+private; a DIRECT consumer sees public only; a non-consumer sees neither.
- Manifest `[visibility] allow = [...]` answers a DIFFERENT question: which units may DECLARE this library (consumer allowlist).
- **Build the module index at startup; DO NOT follow symlinks** (critical: the legacy per-package `std` symlink stays present but must NOT appear as an owner-private candidate). Reject at index time: duplicate library names, `foo.lua` + `foo/init.lua`, public/private duplicate logical names, visible collisions in any unit scope.
- v1 simplification: `std` is all-public (no private/), so a library without a public/private split = all-public.

## Manifest format (TOML; keep v1 minimal)
Per-unit `fkst.toml`: `kind = "package"|"library"`, `name`, `[code] root`, `[lib_deps]` (declared libraries), `[event_deps]` (composed.deps formalized: sibling packages referenced via consumes/produces/fanout/raise for composed conformance). Library adds `[library] name/stable_id/version`, `[visibility] allow=[...]`. Workspace `fkst.workspace.toml` discovers units + registries; `fkst.lock` pins (workspace-internal = "workspace").

## Phases (file-by-file, fkst-framework crate)
- **A. Manifest + catalog (legacy-safe)**: `Cargo.toml` +toml; NEW `manifest.rs` (~500: parse fkst.toml/workspace/lock, types, UnitGraph/catalog, discover by walking up to fkst.workspace.toml, **legacy mode = no manifests → today's symlink behavior**); `path_resolver.rs` (~180: optional UnitGraph in PackageRoots, manifest-aware RequireScope). NO behavior change yet (legacy fallback). 
- **B. Scoped resolver + enforcement**: NEW `lua_require.rs` (~700: per-unit `_ENV` require closures, catalog resolve 0/1/2+, canonical cache keys, replace searchers, index build no-follow-symlink); `mlua_init.rs` (~150: compile each unit chunk with unit-bound _ENV + RequireScope); `supervise/graph_scan.rs` (~120: same scope during M.spec eval); `test_runner.rs`/`main.rs` call sites.
- **C. `fkst-framework deps` validator**: NEW `deps_cli.rs` (~450: render+validate DAG acyclic, declared lib exists, visibility, exports exist, no ambiguous, no orphan, declared lib_deps == actual require literals via a small Rust scanner = G-STD-DEP equivalent, event_deps == composed.deps).

## Packages side (fkst-packages)
All units get `fkst.toml`; `std` declared as a library (kind=library, all-public, visibility=public); `event_deps` mirrored from `composed.deps`; `lib_deps` from G-STD-DEP-derived actual usage. **Per-package `std` symlink REMOVED** (replaced by manifest+resolver); composed.deps may stay as the source `event_deps` mirrors until G-rules retarget. Restructure `std` to the library layout (all-public).

**DI inversion (Candidate C, oracle-confirmed) — how shared `std.devloop_*` consumes consumer-private registries under the resolver.** Shared library code MUST NOT require a consumer package's private modules (and MUST NOT be handed a `require`/loader/prefix/package_name — that is "capability laundering," a disguised reach-in). Instead each package's `core/devloop_wiring.lua` resolves its OWN private modules (restart marker_fields / replay_payload_fields / transitions / liveness_signal_producers / prompts / gate DSL sources) with the package's own lexical `require`, and passes the resolved VALUES into `std` installs (`install(M, resolved)`), keyed by SEMANTIC ROLE rather than package-private module-name strings. `std.registry` keeps only pure builders over already-loaded values (sorted/unique/key-match/dup validation). Gate DSL stays single-source: the package's `core/gates/<name>.lua` RETURNS its DSL source string, wiring `require`s it, and the monotone-gate-DSL ratchet validates that same returned string. Visibility = authority: `std` never possesses the capability to reach a consumer's private namespace; the std-dependency-model ratchet makes a std→package require a CI violation.

## Migration / merge-together (clean break, test-then-migrate)
No dual-mode. Build the complete clean change on the two branches; ALL tests (engine fixtures + package conformance/tests) pass on the NEW model (manifests everywhere, symlinks removed). THEN migrate as one coordinated step. Because the clean-break packages branch REMOVES the `std` symlinks (the old, pre-resolver engine cannot load it) and PINS `.fkst/substrate-ref` to the new engine commit, the packages side always builds the resolver-aware engine and has NO broken intermediate window (it does not depend on the old engine ignoring new files). Sequence: (1) merge the substrate engine PR to substrate-dev; (2) bump the packages `.fkst/substrate-ref` from the branch commit to the post-merge substrate-dev HEAD so packages tracks mainline rather than a branch commit; (3) merge the packages PR to dev. The point: zero permanent compat code in the engine; the only "ordering" is this deliberate sequence, not a runtime fallback.


## OUT of scope (clean follow-ups)
- **Devloop library extraction** (`std.devloop_*` → a named `devloop` library): now UNBLOCKED. The DI inversion that was its prerequisite IS DONE on the packages side (Candidate C, 2026-06-22): `std.devloop_*` no longer reach into consumer-private modules — each package's `core/devloop_wiring.lua` resolves its own private restart/liveness/prompt/gate modules and passes resolved VALUES (keyed by semantic role) into `std`, mechanically enforced by the std-dependency-model ratchet. Extracting `std.devloop_*` into a named `devloop` library is the remaining follow-up, using the now-working primitive.
- Versioning / registry / cross-repo third-party consumption (workspace-internal needs none).

⟦AI:FKST⟧

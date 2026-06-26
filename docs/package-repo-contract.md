# engine ↔ package-repo contract

本文是 package-repo 作者面向 fkst-substrate 当前引擎的契约索引。它不替代 `SPEC.md` 与 `docs/architecture.md`：身份边界以 `SPEC.md` 为准，运行架构以 `docs/architecture.md` 为准，可靠投递设计背景见 `docs/durable-delivery-design.md`。本文只把 package 作者必须遵守的引擎接口集中到一处，并标明哪些规则由引擎强制、哪些仍是 doctrine 或 package 测试责任。

本文依据当前仓库源码与文档：`SPEC.md`、`CLAUDE.md`、`docs/architecture.md`、`docs/durable-delivery-design.md`、`crates/fkst-framework/src/main.rs`、`crates/fkst-framework/src/test_runner.rs`、`crates/fkst-framework/src/supervise/graph_scan.rs`、`crates/fkst-framework/src/host_conformance.rs`、`crates/fkst-common/src/validation.rs`。

## 1. 什么是 fkst package-repo

一个 fkst package-repo 提供一个或多个 package root。package root 是引擎扫描与运行 Lua graph 的输入目录；它不是 Rust crate。当前 library-dependency contract 要求每个 package root 是 `fkst.workspace.toml` 发现的 manifest unit；显式传入且位于 `--project-root` 外部的 platform package root 使用自身向上发现的 workspace catalog，而不是由 host workspace 拥有。标准布局如下：

```text
<PACKAGE_ROOT>/
├── core.lua
├── departments/
│   └── <dept>/
│       ├── main.lua
│       └── *_test.lua
├── locales/
│   ├── en.lua
│   └── <locale>.lua
├── raisers/
│   └── <raiser>.lua
└── tests/
    └── *_test.lua
```

`core.lua` 是 package 自己可 `require` 的共享库约定；引擎只通过 package root 设置 `package.path`，不会特殊识别 `core.lua`。`departments/<dept>/main.lua` 是 Department entrypoint，必须返回含 `M.spec` 的 table，并定义全局 `pipeline(event)`。`raisers/<raiser>.lua` 返回 source declaration。`locales/*.lua` 是 package-owned key catalog content for `t(key[, vars])`; `en.lua` is the reference catalog. `tests/*_test.lua` 与 `departments/<dept>/*_test.lua` 由 `fkst-framework test` 发现；runner 不递归扫描其它目录。

flat package 指 `package-root == host-root` 且只有一个 graph root；它保持 LegacyFlat：裸 queue 名、`Event.queue`、`RAISED`、delivery id 和 `source_ref` 字节仍是裸名。composed packages 指多个 package root 与 host root 组合成一张 composed graph；此时 package root basename 是 namespace，裸 queue 名按 owner namespace 归一化为 `<pkg>.<queue>` 或 `host.<queue>`，跨包消费必须显式写 `pkg.queue`。详见 `SPEC.md` 的“身份边界 / SDK surface”和 `docs/architecture.md` §4。

Composed package event dependencies are declared canonically in `fkst.toml` under `[event_deps] packages = [...]`. Package-repo scripts that need to ask whether a manifest is composed, and which package roots to pass as composed dependencies, must call `fkst-framework manifest composed-deps --manifest <path>`. That command parses exactly one manifest with the engine manifest parser, prints composed package deps one per line in declared order, exits `10` for a valid non-composed manifest, and exits `1` for a missing, unreadable, or malformed manifest.

`composed.deps` is removed from the package-repo contract. It is not a dependency resolver, version resolver, override mechanism, ordering DSL, or cross-package `require` mechanism.

`persistence_class` is the package manifest's typed persistence declaration. `persistence_class = "saga"` derives the `saga_recovery` capability, which is required to call `once(key, fn)`. The declaration selects proof obligations instead of replacing them: Saga packages must include saga-shaped Department code (`require("workflow.saga")` and `.department`) and declare `[conformance].function` for restart-transition proof obligations. Non-Saga packages cannot use `once`.

Stateless generator packages declare intent in their package `fkst.toml`, not authority. A package with `persistence_class = "stateless_generator"` must declare `[generator]`; `[generator].suggested_output_roots` is optional advisory package metadata, and `[generator].package_input_roots` grants read-only package-bundled inputs relative to the package owner root. Effective write and host/project read authority is granted only by the host workspace manifest:

```toml
[generators.<package-namespace>]
output_roots = ["dist"]
project_input_roots = ["content"]
```

`output_roots` and `project_input_roots` are relative to the host root. `output_roots = ["."]` is rejected unless the same host grant sets `allow_host_source_mutation = true`. The removed package-side `[generator].output_roots` and `[generator].input_roots` fields are not accepted.

## 2. 固定 Lua SDK surface

当前 production Lua SDK surface 是：

```text
pipeline(event)
source
raise(queue, payload)
spawn_codex_sync(opts)
spawn_codex(opts)
fkst.codex_runs()
fkst.observe([opts])
exec_sync(cmd_or_opts)
exec_argv(opts)
await_all(handles)
with_lock(name, fn)
once(key, fn)
cache_get(key)
cache_set(key, value[, ttl_seconds])
cache_expire(key)
truncate_utf8(s, max_bytes)
graph_json()
t(key[, vars])
restricted_lua_load(opts)
git_log_count(grep, since)
git_log_grep(grep, since)
count_worktrees()
list_orphan_worktrees(prefix)
setup_worktree(prefix)
file.read(path)
file.write(path, content)
file.exists(path)
json.decode(text)
log.info(msg)
log.warn(msg)
log.error(msg)
now()
```

`pipeline(event)` 与 `source` 是 package-side 约定，不是普通 SDK global。Rust 注册的 runtime primitive 来自 `mlua_init.rs` 调用的 `sdk_*` 模块与 `raise.rs`。`json` 只有 `json.decode`，没有 `json.encode`、`json.array` 或 schema 推断；需要空数组时用 `json.decode("[]")` 形成 array-tagged table。Pure data utilities are not automatically Rust primitives; §2.2 defines the decision order.

`exec_sync` and `exec_argv` are two distinct subprocess capabilities, not two ways to run the same thing. `exec_sync(cmd_or_opts)` lowers a command string to `/bin/sh -c` and is the genuine-shell primitive (env expansion, redirection, `&&`, builtins); its rate pool derives from the first shell word. `exec_argv({argv = {program, args...}, cwd?, env?, timeout?, read_coalesce?})` runs the program directly via `argv` with no shell — no Lua-side quoting, no shell injection — and is the egress that `gh`/`git` adapters (`std.github`/`std.git`) must use; its rate pool derives from `argv[1]`'s program basename. `exec_argv` rejects a `cmd` string and a `rate_pool` field. Both share the same mock/cassette/read-coalescing/rate/audit machinery and return `{stdout, stderr, exit_code, timed_out?, error_class?}`.

`t(key[, vars])` implements key-based localization using the current owner root only. It reads `locales/<locale>.lua`, where `<locale>` comes from `FKST_OUTPUT_LANG`, and falls back to `locales/en.lua` when the requested locale or key is missing. `vars` is an optional table of scalar values interpolated into `{name}` placeholders. Catalogs are plain flat Lua tables with stable string keys and literal UTF-8 string values; they are package content, not engine policy.

`restricted_lua_load({ source, bindings?, mode?, name? })` is a host-owned restricted loader for small declarative Lua sources. It evaluates `source` in a fresh Lua state with an empty `_ENV`; callers grant plain data or function capabilities explicitly through `bindings`. `mode` defaults to `"text"` and bytecode is accepted only with `mode = "bytecode"`. The chunk cannot reach ambient `require`, `load`, `loadstring`, `_G`, `debug`, `package`, raw table primitives, metatable access, `io`, `os`, coroutine primitives, `string.dump`, or value-metatable paths such as `("").dump`. Returned data and bridged function arguments / returns must be plain nil / boolean / number / string / table values; executable or VM-owned values fail closed with structured `restricted_lua` errors.

`locales/*.lua` is the sanctioned home for non-English prose literals. Source files outside `locales/` still follow the English-source rule. Machine protocol tokens, marker names, verdict sentinels and AI provenance sentinels are code, not prose; they must not appear in catalog keys or values. Conformance checks catalog completeness against `en`, rejects decode-helper-hidden literals in `locales/`, and rejects machine tokens in catalogs.

`truncate_utf8(s, max_bytes)` returns the longest prefix of `s` that is at most `max_bytes` bytes and ends on a UTF-8 character boundary, matching Rust `str::floor_char_boundary` semantics. It never emits a partial sequence; `max_bytes >= #s` returns `s` unchanged; `max_bytes` smaller than the first character returns the empty string; negative `max_bytes` is an argument error; invalid UTF-8 input is an argument error. This is the blessed replacement for package-side byte truncation.

`fkst.codex_runs()` is a read-only observability query for engine codex run records. It returns running and recent entries with `role`, `started_at`, `started_at_ms`, `timeout_seconds`, `lease_expires_at`, `lease_expires_at_ms`, `status` (`running`, `done`, or `failed`), bounded `output_tail`, and optional `exit_code`; it does not return runtime paths or unbounded stdout/stderr. The lease deadline is the run's own wall-clock timeout contract, derived as `started_at_ms + timeout_seconds * 1000`. `recent` is the last `50` completed codex runs by record time for bounded observability, not a time-bounded just-finished handoff window.

`fkst.observe([opts]) -> table` is the in-process form of `fkst-framework observe --json`. It returns generic durable delivery observe facts: source metadata, limits, truncation flags, queue depth state, live delivery entries, and DLQ entries, with payloads summarized by schema, dedup key, byte count, and digest rather than full bodies. `opts` is optional and may only narrow generic observe data with `limit`, `include = {"queues","errors","events","entities"}`, and `since = <delivery_id>`; `since` is applied before `limit` truncation. It must not encode business concepts such as idle, board, audit, skip, or workflow phases. Production reads `FKST_DURABLE_ROOT` and uses the same live owner-process socket / offline database projection as the CLI. Package-side helpers may interpret the generic facts, but the engine does not.

用户提纲里的 SDK 列表漏掉了当前已存在的 `list_orphan_worktrees(prefix)`。提纲中的其它 production primitive 均存在。没有发现额外 production SDK primitive。

### 2.1 Lua standard library surface

Every engine-constructed Lua context is created with mlua `Lua::new()`, which loads Lua 5.4 safe standard libraries. Packages can rely on this surface everywhere:

```text
base functions: assert, error, ipairs, next, pairs, pcall, print, rawequal, rawget, rawlen, rawset, select, tonumber, tostring, type, xpcall
coroutine
string
table
math
utf8: utf8.char, utf8.charpattern, utf8.codepoint, utf8.codes, utf8.len, utf8.offset
os: os.clock, os.date, os.difftime, os.time
package
```

`io`, `debug`, `ffi`, shell access, networking, and host filesystem authority are not stdlib capability commitments for package logic; use the documented framework SDK for host-authorized effects. A capability already covered by the stdlib is a documentation fact, not a feature request.

中文补充：stdlib（含 `utf8`）在所有引擎 Lua 上下文都已加载；stdlib 已覆盖的能力不构成新原语需求。

### 2.2 Layered capability doctrine（能力分层固定）

How a Lua-side capability need is satisfied, decided strictly in order:

1. **Lua stdlib first.** If the Lua 5.4 standard library covers it (see §2.1), use it directly; the only deliverable is documentation. Hand-rolling what the stdlib provides (e.g. byte arithmetic instead of `utf8.offset`) is a defect.
2. **Engine Lua prelude** for pure utilities beyond the stdlib: a small engine-vendored pure-Lua layer, loaded uniformly into EVERY context the engine constructs (production, test, conformance, graph-scan/spec-eval). Capabilities are written in Lua; presence is guaranteed by the engine; additions are curated wholesale (Redis/OpenResty battery model), never per incident. The prelude must not grant host authority, ambient filesystem access, subprocess access, network access, or durable fact storage.
3. **Rust primitives ONLY for**: host authority and side effects (raise / spawn / locks / cache / worktrees), performance, or fail-closed boundary enforcement (e.g. `json.decode` validation). A proposal for a new Rust primitive must state why layers 1–2 cannot carry it and what conformance or self-test evidence pins the boundary.

Prior art: Redis frozen battery set (cjson/cmsgpack/bit/struct/sha1hex), OpenResty curated batteries, Neovim vendored `vim.json`/`vim.lpeg`. The common split is host-authority effectful primitives from the host, pure data utilities from a curated, documented battery layer.

中文补充：顺序固定为「stdlib 优先 → 引擎纯 Lua prelude（全上下文统一加载，整批精选引入）→ Rust 原语只留 host 权威/性能/fail-closed 边界」。新增 Rust 原语的提案必须说明为何前两层无法承载。

`fkst-framework test` 额外注册 test-mode-only `fkst.test`：

```text
fkst.test.eq(actual, expected[, msg])
fkst.test.is_true(value[, msg])
fkst.test.is_nil(value[, msg])
fkst.test.raises(fn[, msg])
fkst.test.run_department(path, event[, opts])
fkst.test.mock_command(pattern, result)
fkst.test.mock_observe(snapshot)
fkst.test.with_command_cassette(opts, fn)
fkst.test.command_calls()
```

`fkst.test` 不存在于 `run`、`supervise`、`--self-test` 或 conformance production Lua state。`mock_command` 劫持 test mode 中的 `exec_sync`、codex SDK 与 git SDK 外部命令调用；未 mock 的外部命令 fail closed。`mock_observe(snapshot)` makes `fkst.observe()` use the provided deterministic raw snapshot for that test, including fresh Lua states spawned by `run_department`, `fire_raiser`, or `run_graph`; `fkst.observe(opts)` still applies `since`, `limit`, and `include` to the mock. Unmocked test-mode `fkst.observe()` fails closed.

`fkst.test.with_command_cassette({ path, mode, redact? }, fn)` is the bounded VCR-style contract-test surface for external commands. It only exists in `fkst-framework test`; production `run`, `supervise`, `--self-test`, and conformance do not register it. `mode` is `"record"` or `"replay"`. `path` is resolved relative to the owner package root unless absolute. During the callback, explicit `mock_command` entries still take precedence; otherwise `exec_sync`, codex SDK, and git SDK calls use the active cassette.

The cassette format is JSON schema `"fkst.test.command-cassette.v1"` with ordered `entries`. Each entry records the command boundary (`rendered`, `program`, `args`, `stdin`, optional `cwd`, sorted `env`) and result (`stdout`, `stderr`, `exit_code`). Replay is deterministic and fail-closed: entries are consumed in order, command boundary fields must match after redaction, unconsumed entries fail when the callback finishes, and replay never starts a real external process. Record mode starts the real command and atomically writes the cassette after the callback succeeds. `redact` is a list of `{ value, replacement? }`; every non-empty `value` is replaced in recorded command boundary fields and outputs, defaulting to `"<REDACTED>"`. Cassettes are package fixtures, not runtime facts or accepted state.

### 2.3 `exec_sync` read coalescing

`exec_sync(cmd_or_opts)` is force-fresh by default. A package may opt in only for external commands it knows are read-safe:

```lua
local result = exec_sync({
  cmd = "gh issue list --json number,title,updatedAt",
  read_coalesce = {
    key = "github/issues/owner-repo/open",
    ttl_seconds = 30,
  },
})
```

`read_coalesce.key` uses the runtime key contract in §5. `ttl_seconds` must be a positive finite number and is clamped by the engine to at most 300 seconds. The engine fingerprints all result-affecting inputs it controls: the caller key, resolved `/bin/sh -c` command/argv, exact execution cwd, sorted effective environment, and timeout. Coalescing is only allowed for stdin-less commands; if a future command path carries stdin bytes or inherits stdin, the engine bypasses read coalescing and runs fresh. A fresh success cache hit returns `{stdout, stderr, exit_code, timed_out?, error_class?}` before named rate-pool acquisition and before spawning the command. On miss, the engine takes a per-fingerprint flock under `<RT>/locks`, rechecks, then acquires any matching rate-pool token and executes the command. Only `exit_code == 0` is cached; non-zero exits re-run on the next call.

This is a bounded thundering-herd suppression primitive, not automatic command classification. Never use it for writes or read commands whose caller requires force-fresh data; omit `read_coalesce` for those calls. In `fkst-framework test`, mocked `exec_sync` bypasses read coalescing entirely, so `fkst.test.command_calls()` still records each mocked call.

## 3. Event model

事件流是：

```text
source -> fanout -> route -> spawn -> RAISED
```

`cron` 与 `file_watch` source 由 `raisers/*.lua` 静态声明。Department 的 `M.spec` 静态声明：

```lua
M.spec = {
  consumes = {...},
  produces = {...},
  published_seam = {...},
  fanout = {...},
  ephemeral = {...},
  stall_window = "30s",
  retry = { max_attempts = 5, base = "60s", cap = "30m" },
}
```

`M.spec.published_seam` declares consumed queues that this package exposes as public sibling entry points. Graph scan requires each entry to be owned and consumed by the declaring Department, and requires every sibling Department or Raiser `produces` target to be either an own-package queue or one of these published entries. `M.spec.retry` 可省略、设为 `false`，或设为 table；`retry=true` 当前被拒绝。`M.spec.stall_window` 是可靠投递 lease 与续租窗口，不是 framework child 无输出 kill deadline。当前没有 `M.spec` 级别的 per-dept codex timeout knob；codex timeout 是每次 `spawn_codex_sync(opts)` / `spawn_codex(opts)` 的 `opts.timeout`，默认 3600 秒。全局 codex 并发上限来自 `FKST_CODEX_PERMIT_SLOTS` / `codex_permit_slots`，不是 per-dept timeout。

Department 收到的事件形状是：

```text
Event { queue, payload, ts }
```

`ts` 是 Unix 毫秒。Department 没有 lifecycle hook、共享内存或持久 agent state；同一个 `pipeline(event)` 跑两次就是两次独立调用。Person 是一次 `codex exec` 子进程，只能通过 Department 使用 `spawn_codex_sync`、`spawn_codex`、`await_all` 组织。

## 4. Reliable delivery contract

可靠投递默认启用。Department 可用 `M.spec.ephemeral = {"queue"}` 将本 Department 对指定 consumed queue 的订阅降级为非可靠。`M.spec.retry = false` 表示失败不重试，不表示非可靠投递；`M.spec.retry = { ... }` 可覆盖 `max_attempts`、`base`、`cap` 的任意子集。

可靠语义是 at-least-once-until-ack，不是 exactly-once。可靠 delivery 使用 redb store 记录 delivery lease、fencing、retry、backoff 与 DLQ；`Fanout::send` 在可靠路径只作进程内唤醒。成功条件是 framework child exit 0 且所有 `RAISED` publish 成功，然后 ack；spawn error、非零退出、codex timeout 或 `RAISED` publish 失败进入 retry；达到 max attempts 后写 dead 表，并 best-effort publish `dead_letter`。

可靠 event 必须有 `source_ref = {kind, ref}`（Lua 协议字段名是 `ref`；如 github-proxy 的 `{kind="external", ref="<repo>#<type>/<number>"}`）。cron 的 `source_ref` 由 raiser 名和 slot 派生；file_watch 的 `source_ref` 由绝对路径和稳定 change version 派生；Department `RAISED` 进入可靠 queue 时继承上游 reliable delivery 的 `source_ref`。如果 reliable publish 缺 `source_ref`，引擎 fail closed，上游 delivery 不 ack。

可靠 payload 有 64 KiB 上限，超过时 fail closed。宪法规则是：大内容永远不要序列化进 reliable payload。reliable payload 只放小控制字段；实体内容由 `source_ref` 指向的 git、外部源或明确 host filesystem fact 重新读取。`docs/durable-delivery-design.md` 把当前实现称为 bounded payload 过渡，但 package 作者应按 trigger-only + consume-time source lookup 设计。

有可靠订阅时，真实 `supervise` 必须设置 `FKST_DURABLE_ROOT`；纯 `ephemeral` graph 不需要打开 durable store。`FKST_DURABLE_ROOT` 不属于 `<RT>`，落点是 operator 管理的一等持久边界。

## 5. Runtime key contract

`once(key, fn)`、`cache_get(key)`、`cache_set(key, value[, ttl_seconds])`、`cache_expire(key)` 与 `with_lock(name, fn)` 使用同一 runtime key 合约。key / name 必须是可读的相对 filesystem path，不是 hex 编码。

规则：非空；相对路径；允许 `/` 表示目录；每个 segment 非空、最长 255 bytes、只含 `[A-Za-z0-9._-]`，且不能是全点 segment；禁止 leading `/`、trailing `/`、`//`、反斜杠、NUL 和绝对路径。校验后的 key 保持为 `<RT>/{locks,marks,cache}/<key>/` 目录路径，engine 在该目录下写 reserved leaf file（`=lock` / `=mark` / `=value`）；`=` 不在合法 key segment 字符集内，因此不会与有效 key 冲突。

`with_lock` 使用 `<RT>/locks/<name>/=lock`；`once` 使用 `<RT>/locks/once/<key>/=lock` 作为内部锁，并在成功执行后写 `<RT>/marks/<key>/=mark`；`cache` 读写 `<RT>/cache/<key>/=value`。marker 和 cache 都是 scratch，不是 durable truth；需要 read-compare-write 原子性时，package 应外层使用 `with_lock`。

## 6. Fact-source doctrine

跨 pipeline 的稳定事实只能来自 git refs / commits / branches、外部源和明确 host filesystem fact。内存队列、raise buffer、Lua table、subprocess handle、agent 判断、logs、`<RT>/locks`、`<RT>/marks`、`<RT>/cache` 都不是实体业务真相。

package source tree 在运行期承载代码、fixture 和 asset，不承载“为活过崩溃”的业务状态。`<RT>` 是 scratch：worktrees、codex permits、locks、logs、marks、cache。可靠 delivery store 只承载在途 delivery 账本，不承载 accepted state、rollback state 或实体内容真相。

恢复模型是 raiser 从 git / external source / explicit host fact 重新推导，再 enqueue；下游 Department 通过 source-derived `dedup_key` 或 git / external fact 幂等处理重复。没有 durable fact，就不要把“进程内觉得发生了”当事实。

### 6.1 Host command rate pools

Named rate pools are host posture env facts for external command pressure, not package API. A host may set `FKST_RATE_POOL_<NAME>=<burst>,<refill_per_minute>` such as `FKST_RATE_POOL_GH=50,50`; when a real external command's program basename matches `<NAME>` case-insensitively, the engine acquires one token before spawning it. `exec_sync("gh ...")` and git SDK calls go through the in-process adapter; Codex subprocesses still use the existing `codex-permits` fcntl pool for Codex process concurrency, and their `PATH` is prepended with generated rate shims so codex-internal commands like `gh` consume the same named bucket. `fkst.test.mock_command` bypasses rate pools and does not consume tokens.

Pool ledgers live under `FKST_RATE_POOL_ROOT`, default `~/.fkst/rate-pools`. This root is deliberately outside `FKST_RUNTIME_ROOT` so independent `supervise` instances on the same host share one command posture. Invalid pool definitions fail closed at startup/config parsing. Packages must not depend on ledger file contents as business facts.

## 7. CLI subcommand contract

当前 `fkst-framework` CLI surface 来自 `crates/fkst-framework/src/main.rs`：

```text
fkst-framework --self-test [--coverage <dir>]
fkst-framework conformance --project-root <path> [--package-root <path> ...]
fkst-framework test --project-root <path> [--package-root <path> ...] [--report-json <path>] [--coverage <dir>]
fkst-framework run <lua> --project-root <path> --package-root <path> [--package-root <path> ...] [--owner-namespace <id>] --event <json>
fkst-framework supervise --project-root <path> --framework-bin <path> [--package-root <path> ...]
fkst-framework config --project-root <path> [--package-root <path> ...]
fkst-framework init-package-repo [--ref <substrate-ref>] [--force]
```

`--self-test` 运行引擎自检。`conformance` 支持 flat single-root 与 composed multi-root，通过 `--project-root` 和可重复 `--package-root` 形成 host + package graph。`test` 发现 `<ROOT>/departments/*/*_test.lua` 与 `<ROOT>/tests/*_test.lua`；`--report-json <path>` 写 schema 为 `fkst.test.report.v1` 的机器报告，条目身份是 `owner_namespace`、`file`、`name`。stdout 的 `PASS` / `FAIL` / summary 行只是 human / compatibility surface，不是 authoritative inventory。

`--coverage <dir>` is opt-in engine-owned Lua line coverage. It installs an `mlua` line hook only for that test run, writes `<dir>/coverage.json` as `{ "<file>": { "covered_lines": [n] } }`, and writes `<dir>/lcov.info` with line-only `DA` records. It excludes `*_test.lua` and generated `=fkst:<purpose>` chunks, and it names engine-loaded chunks as `@<owner-root-relative-path>` so `debug.source()` maps back to package source files. The surface is line-granularity only; branch/condition coverage and mutation evidence remain outside the engine coverage primitive. Hooks are applied to the main Lua state and threads created through standard `coroutine.create` during coverage runs. Without `--coverage`, no coverage hook is installed. `fkst-framework --self-test --coverage <dir>` runs the normal self-test and then the same Lua test runner against the current directory as a folded host/package root before writing these artifacts.

`run` 执行一个 Lua entrypoint。无 `--owner-namespace` 时，只在单一 package root 可唯一确定 owner namespace 的情况下默认；多个 `--package-root` 时必须传 `--owner-namespace <id>`。当前 `run` 明确拒绝 `FKST_PACKAGE_ROOTS` env；应通过可重复 `--package-root` 传 composed namespace catalog。

`supervise` 扫描 package roots 与 host root，构造一张 composed graph，spawn consumer/source runtime，并用 `--framework-bin` 指定 child `fkst-framework run` binary。`config` 是只读自省命令，不是 package 行为入口，但它属于当前 CLI surface。

`init-package-repo` is a deterministic scaffold generator for package or host repositories. It runs inside the target git repository, writes engine-owned templates for `scripts/run.sh`, `scripts/check_repo.py`, `.github/workflows/ci.yml`, `env.example`, `.fkst-substrate-ref`, `.gitignore` entries and a minimal `README.md` pointer, and prints a converge report. Identical files are a no-op; differing owned template files are refused unless `--force` is passed; `.gitignore` only receives missing scaffold entries. The command does not touch `packages/`, git history or remotes. When `--ref` is omitted, `.fkst-substrate-ref` uses the running binary's build-time source revision.

## 8. Conformance

当前 `fkst-framework conformance` 的直接 check id 是：

```text
runtime-layout
project-layout
locale-catalogs
graph-scan
department-non-empty
schema-validation
```

`graph-scan` 会执行 package root / host root 扫描、`package.lua` removed surface 拒绝、`M.spec` unknown fields 拒绝、`retry` 解析、`published_seam` capability validation、namespace 解析、queue 归一化和 owner-scoped `package.path`。每个 graph root 用 fresh Lua state，package owner 只看自己的 root；host owner 可看 host + packages；`--package-root` 不是跨包 `require` 授权。

`locale-catalogs` validates each graph root's `locales/` directory when present. It requires `en.lua` as the reference if any locale catalog exists, requires every non-`en` catalog to cover all `en` keys, and rejects decode-helper-hidden literals or machine protocol tokens in catalogs.

`schema-validation` 会检查 queue capacity、raiser / department queue 引用、Department lua 文件存在、`ephemeral` 必须属于 `consumes`、`stall_window` 后缀、`retry` 数值与 duration、孤立 queue、多消费者 fanout、同 Department consume+produce 同 queue 必须 fanout，以及“只消费 ephemeral queue 的 Department 不能 produce 到 reliable downstream”这一 reliable `source_ref` 传播规则。

需要特别澄清：当前 `host_conformance.rs` 没有名为 fixed-SDK-surface 的独立 check。固定 SDK surface 由 `SPEC.md` 锚定，并由 `--self-test` / Rust tests 覆盖；package-repo 的 release gate 应同时跑 `fkst-framework --self-test`、`fkst-framework test` 和 `fkst-framework conformance`。

## 9. Enforcement map

| Contract rule | Enforcement |
|---|---|
| package root 来自可重复 `--package-root`、`FKST_PACKAGE_ROOTS` 或 `FKST_PACKAGE_ROOT` | engine `PackageRoots::resolve` / `resolve_run` |
| package root 必须由 manifest catalog 拥有；显式外部 `--package-root` 使用自身 workspace catalog，内部 root 使用 host workspace catalog | engine path resolver / graph scan / run / test runner |
| `FKST_PACKAGE_ROOTS` 与 `FKST_PACKAGE_ROOT` 无显式 `--package-root` 时互斥 | engine path resolver |
| `run` 不接受 `FKST_PACKAGE_ROOTS` env | engine `resolve_run` |
| package basename / Department / Raiser / queue segment name 字符集 | engine graph scan / path resolver |
| duplicate package root 或 duplicate package basename | engine path resolver |
| independent host root 下 package basename 不能是 `host` | engine path resolver |
| `package.lua`、`FKST_STDLIB_ROOT`、`FKST_RUNTIME_PACKAGE_ROOT`、`FKST_GRAPH_ROOTS` removed surface | engine graph scan / path resolver |
| owner-scoped `package.path` 与 package-root require isolation | engine graph scan / run / test-mode runner |
| stateless generator package metadata can only request roots; effective `output_roots` / `project_input_roots` come from host `[generators.<package-namespace>]`, and `output_roots = ["."]` requires `allow_host_source_mutation = true` | engine manifest parser / run / test-mode runner / confined file SDK |
| `M.spec` unknown fields 拒绝 | engine graph scan |
| `M.spec.consumes` / `produces` / `fanout` queue 解析 | engine graph scan |
| `M.spec.published_seam` 必须引用本 package consumed queue；sibling Department / Raiser `produces` 只能指向 own queues 或 sibling published seam | engine graph scan |
| `M.spec.ephemeral` 必须引用 consumed queue | engine schema validation |
| `M.spec.retry` 只能 nil / false / table，`retry=true` 拒绝 | engine graph scan |
| `retry.max_attempts > 0`，`base` / `cap` 是正 `s/m/h` duration 且 `cap >= base` | engine schema validation |
| queue capacity > 0 | engine schema validation |
| queue 引用必须存在，queue 不能孤立；consumed-only queues are warnings in flat single-root conformance and failures in composed conformance / runtime startup | engine schema validation |
| 多消费者或同 Department consume+produce 同 queue 必须 fanout | engine schema validation |
| Department lua 文件必须存在 | engine schema validation |
| source kind 只支持 `cron` 与 `file_watch` | engine graph scan / serde parse |
| reliable subscription 需要 `FKST_DURABLE_ROOT` | engine supervise startup |
| reliable publish 必须带 `source_ref` | engine delivery router |
| reliable payload <= 64 KiB | engine delivery router |
| 大内容不进 reliable payload，只传 `source_ref` 和小控制字段 | doctrine-only + 64 KiB bound；package tests / review |
| reliable retry / lease / fencing / DLQ | engine delivery store / consumer |
| `once` / `cache` / `with_lock` runtime key 是相对可读 path | engine `validate_runtime_key` |
| `json` decode-only | engine SDK registration / self-test |
| `t(key[, vars])` owner-root locale catalog lookup and `en` fallback | engine SDK registration / self-test / Rust tests |
| `locales/*.lua` completeness, no decode-helper-hidden literals, no machine tokens | engine conformance `locale-catalogs` |
| `fkst.test.*` 不泄漏到 production | engine test-mode registration + Rust tests |
| machine test inventory 只能来自 `--report-json` 的 `fkst.test.report.v1` | engine test runner |
| stdout `PASS` lines 不作为 authoritative inventory | doctrine + test runner contract |
| no lifecycle hooks / no shared memory / same pipeline run independent | engine process model + doctrine |
| cross-pipeline truth 只来自 git / external source / explicit host fact | doctrine-only；review 与 package tests |
| downstream idempotency by `dedup_key` | package `*_test.lua` / reviewer doctrine；engine 不理解业务 dedup schema |
| composed package deps are declared in `fkst.toml [event_deps] packages`; `composed.deps` is not a contract surface | engine manifest parser + `fkst-framework manifest composed-deps --manifest <path>` |
| fixed production SDK surface | `SPEC.md` + `--self-test` + Rust tests；当前 host conformance 无独立 check |

## 10. 站起一个新 package-repo

新 package-repo 应把 package root 通过 `--package-root <path>` 显式传给 engine；需要组合多个 package 时重复传 `--package-root`。每个 package root 需要有 `fkst.toml` 并被某个 `fkst.workspace.toml` catalog 发现；host repo 如果没有自有 Lua unit，可以使用最小 `fkst.workspace.toml`：

```toml
[workspace]
units = []
```

不要发明额外 dependency resolver 或跨包 `require`。

版本固定方式是 git source-ref pin：tag 或 SHA。当前契约不是 semver-published SDK，也不是多 binary distribution matrix。package-repo 的 wrapper 可以 pin fkst-substrate 的 tag/SHA，构建或引用对应 `fkst-framework`，再运行：

```text
fkst-framework --self-test
fkst-framework test --project-root <HOST> --package-root <PACKAGE> --report-json <PATH>
fkst-framework conformance --project-root <HOST> --package-root <PACKAGE>
```

真实 `supervise` 若存在 reliable subscription，operator 必须提供 `FKST_DURABLE_ROOT`。`FKST_RUNTIME_ROOT` 仍是 scratch；清空 `<RT>` 不应丢失实体业务真相。

⟦AI:FKST⟧

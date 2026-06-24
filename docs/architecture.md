# fkst-substrate 引擎架构

本库是稳定发布的受监督事件 / SDK / 进程衬底；业务 Lua package 不在本库内，由独立仓库或 host 通过 `FKST_PACKAGE_ROOTS` / `FKST_PACKAGE_ROOT` / `--package-root` 注入。

本文只描述 fkst-substrate 当前引擎事实。源码权威在 `crates/`；`README.md` 说明验证命令；`SPEC.md` 是身份锚点；业务部门拓扑、host 运行流程和具体研发策略不属于本文。

## 0. 仓库目录结构

```text
fkst-substrate/
├── Cargo.toml / Cargo.lock
├── CLAUDE.md
├── AGENTS.md -> CLAUDE.md
├── SPEC.md
├── README.md
├── crates/
│   ├── fkst-supervisor/src/main.rs
│   ├── fkst-common/src/
│   │   ├── lib.rs config.rs event.rs
│   │   ├── runtime_layout.rs
│   │   ├── error.rs validation.rs
│   └── fkst-framework/src/
│       ├── main.rs
│       ├── config_registry.rs
│       ├── path_resolver.rs raise.rs mlua_init.rs
│       ├── sdk_basic.rs sdk_codex.rs sdk_fs.rs sdk_git.rs sdk_log.rs
│       ├── sdk_mark.rs sdk_cache.rs
│       ├── host_conformance.rs runtime_context.rs self_test.rs
│       └── supervise/
│           ├── mod.rs graph_scan.rs source_runner.rs
│           └── event_fanout.rs consumer.rs spawner.rs raised.rs delivery_router.rs delivery_store.rs delivery_types.rs
├── examples/minimal-package/
└── docs/architecture.md
```

`examples/minimal-package/` 是引擎自带的 package-root fixture：单个 cron source 产生 `tick`，producer Department 消费 `tick` 并 `raise("example_event", payload)`，consumer Department 消费 `example_event` 并打印标准 `Event{queue,payload,ts}`。它用于证明 `--package-root` 能被独立加载、通过 graph validation、直接触发 pipeline，并且 producer payload 可被 consumer 标准事件消费；真实 routing 由 framework 自身测试覆盖。

## 1. 三层稳定性与三级公司

Tier I 是 `crates/fkst-supervisor`。它是进程根，负责定位 framework binary、启动 `fkst-framework supervise`、等待退出、处理 signal、reap 子进程。它不扫描图、不解析事件、不知道 Department/Raiser。

Tier II 是 `SPEC.md` 与 conformance。它定义系统身份、边界和不可绕过的检查。

Tier III 是 `crates/fkst-framework`、`crates/fkst-common` 和注入的 Lua graph。framework/common 是引擎代码；Lua package/host 是外部行为层。

运行时公司结构只有三级：

- Company：supervisor + `fkst-framework supervise` + composed graph。
- Department：`departments/<dept>/main.lua`，暴露 `M.spec` 与 `pipeline(event)`。
- Person：一次 `codex exec` 子进程，由 `spawn_codex_sync` 或 `spawn_codex` 创建。

## 2. Crate 依赖图

```text
fkst-supervisor
  deps: tokio, nix, tracing, tracing-subscriber, anyhow
  internal deps: none
  runtime edge: spawn fkst-framework supervise

fkst-common
  deps: serde, serde_json, thiserror, anyhow
  provides: Config, Event, RuntimeLayout, validation

fkst-framework
  deps: fkst-common, mlua, notify, tokio, ulid, base64, nix, serde, serde_json,
        tracing, tracing-subscriber, anyhow
  provides: CLI, graph scan, source runner, fanout, consumer, spawner,
            RAISED parser, Lua SDK, self-test, host conformance
```

workspace 只包含这三个 crate。没有业务 Lua package crate，也没有 package manifest crate。

## 3. CLI Surface

`fkst-framework` 当前 surface：

```text
fkst-framework run <lua> --project-root <path> --package-root <path> ... --owner-namespace <id> --event <json>
fkst-framework supervise --project-root <path> [--package-root <path> ...] --framework-bin <path>
fkst-framework conformance --project-root <path> [--package-root <path> ...]
fkst-framework config --project-root <path> [--package-root <path> ...]
fkst-framework boundary-resources
fkst-framework test --project-root <path> [--package-root <path> ...] [--report-json <path>] [--coverage <dir>]
fkst-framework --self-test [--coverage <dir>]
```

`fkst-supervisor` 没有业务子命令；它只把当前目录作为 host root，启动 `fkst-framework supervise`。

`fkst-framework test` 是 Tier III test-mode runner，不启动 supervise，不执行 dispatcher，也不把 `fkst.test` 加入 production Lua SDK。它用 `PackageRoots::resolve` 得到 package roots input set 和 host root，只发现两类文件：

```text
<ROOT>/departments/*/*_test.lua
<ROOT>/tests/*_test.lua
```

runner 不全树递归，不扫描 `raisers/` 或 `fkst/`。每个测试文件在独立 Lua state 中执行，先注册 production SDK 以便测试可 `require` package 模块和调用固定 SDK，再注册 test-mode `fkst.test` 表。测试文件必须返回 table；runner 只执行排序后的 `test_*` key。单个测试失败后继续执行同文件其余测试和后续文件，最后输出 `N passed, M failed`。退出码语义是：全部通过返回 0；存在测试失败返回 1；`--report-json` 写入失败等基础设施错误返回 2。`test` 仍不启动 router 或 supervise。

`--report-json <path>` 在所有测试运行结束后用临时文件加原子 rename 写入 JSON 报告，schema 为 `fkst.test.report.v1`：

```json
{
  "schema": "fkst.test.report.v1",
  "summary": { "passed": 1, "failed": 0 },
  "tests": [
    {
      "owner_namespace": "pkg",
      "file": "tests/example_test.lua",
      "name": "test_example",
      "status": "pass"
    }
  ]
}
```

失败条目额外包含 `error`。加载或 eval 测试文件失败时，报告条目使用 `name = "<load>"` 且计入 failed。报告条目来自 Rust 侧枚举出的测试文件和 `test_*` key；每个条目的身份是 `owner_namespace`、`file`、`name` 三元组，不提供可被分隔符碰撞污染的拼接 `id`。Lua `print` 不能向报告注入伪造测试；stdout 的 `PASS` / `FAIL` / summary 行保留为 legacy human / compatibility surface，不是 authoritative machine channel。

`--coverage <dir>` is an opt-in engine-owned Lua line coverage mode for the test runner. It installs an `mlua` `HookTriggers::EVERY_LINE` hook only for the covered run, names engine-loaded file chunks as `@<owner-root-relative-path>`, and writes `<dir>/coverage.json` plus `<dir>/lcov.info` after the full run. `coverage.json` has the shape `{ "<file>": { "covered_lines": [1, 2] } }`; `lcov.info` emits only `TN`, `SF`, `DA`, and `end_of_record` records. This is honest line coverage only: branch, condition, and mutation evidence are outside this surface. Files ending in `*_test.lua` and generated chunks named `=fkst:<purpose>` are excluded from production coverage. Hooks are also applied to threads created through the standard `coroutine.create` table during coverage runs. Without `--coverage`, no hook is installed and the runner keeps the normal zero-coverage-overhead path.

`fkst-framework --self-test --coverage <dir>` first runs the normal engine self-test, then runs the same Lua test runner against the current directory as the folded host/package root and writes the same coverage artifacts. Plain `--self-test` remains the startup self-test and does not run package Lua tests.

明确非目标：当前 test runner 不向 package author 提供 router、可靠投递或 supervise 的 Lua 原语；不承担 Lua stray-global 或 unused-local lint；不使用随机 sentinel 或专用 fd 分离测试输出；不沙箱恶意 Lua。它的范围是运行受信 package 的 Lua 测试、提供 Rust 枚举来源的机器报告，并保持 stdout 兼容面。

`fkst.test` 包含 `eq(actual, expected[, msg])`、`is_true(value[, msg])`、`raises(fn[, msg])`、`is_nil(value[, msg])` 四个断言，以及 test-mode-only `run_department(path, event[, opts])`。`run_department` 用 fresh Lua state 注册 production SDK 和独立 `RaiseBuffer`，再通过正常 department runner 注入 `event`；它返回 `{ exit_code = int, raises = { { queue = string, payload = table }, ... } }`。queue 解析与 production 一致，唯一例外是 `run_department` 会记录但不投递 subject department 在 `M.spec.produces` 中声明的 qualified queue raise。每个测试文件按所属 graph root 隔离执行；相对 `path` 按该测试文件所属的 owner package root 解析，运行期 `package.path` 也只指向该 owner root。绝对 `path` 仍按绝对路径处理。`opts.cwd`、`opts.env`、`opts.path_prepend` 只作用于该次执行并随后恢复。

`fkst.test.mock_command(pattern, result)`、`fkst.test.with_command_cassette(opts, fn)` 与 `fkst.test.command_calls()` 只在 test mode 注册。mock runner 劫持 `exec_sync`、codex SDK 与 git SDK 的外部命令调用；渲染命令行按前缀或子串匹配，mock 按注册顺序一次性消费，未 mock 且无 active cassette 的外部命令 fail closed 且不启动真实进程。`command_calls()` 返回每次调用的 rendered、program、args、stdin、cwd、env、stdout、stderr 与 exit_code。`run_department` 创建的 fresh Lua state 与测试文件共享同一个 mock/cassette state；每个 test function 开始前清空 mocks、active cassette 与 calls。`setup_worktree` 在 test mode 也经过同一 git mock runner，但 mock 不模拟 worktree filesystem 副作用。

`fkst.test.with_command_cassette({ path, mode, redact? }, fn)` is a bounded VCR-style integration-test fixture. `mode = "record"` starts the real command for unmocked external calls and atomically writes JSON schema `"fkst.test.command-cassette.v1"` after `fn` succeeds. `mode = "replay"` reads that schema and consumes ordered entries without spawning real external commands. Each cassette entry stores command boundary fields (`rendered`, `program`, `args`, `stdin`, optional `cwd`, sorted `env`) plus `stdout`, `stderr`, and `exit_code`. Replay fail-closes on boundary mismatch, exhausted entries, unused entries, unsupported schema, or malformed cassette. `redact` entries replace exact non-empty values in both command boundary and output fields before record/replay comparison; default replacement is `"<REDACTED>"`. Cassette files are package-owned test fixtures, not `<RT>` state, logs, durable delivery facts, or accepted-state records.

## 4. Package Root 与 Host Root

`PackageRoots::resolve` 产生 package roots input set 和一个 host root：

```text
显式可重复 --package-root <path> 优先
否则 FKST_PACKAGE_ROOTS，按平台 path list 分隔符解析
否则旧单值 FKST_PACKAGE_ROOT
host root 来自显式 --project-root
同时设置 FKST_PACKAGE_ROOTS 与 FKST_PACKAGE_ROOT 且没有显式 --package-root 时 fail closed
```

package identity 是 canonical package-root basename。package id、Department name、Raiser name 与 queue 段名都必须匹配 `[A-Za-z0-9_-]+`；`.` 只作为 `pkg.queue` 限定符。两个 package root 的 basename 相同 fail closed；独立 host root 存在时，package basename `host` fail closed，因为 `host` 是固定 host namespace。

`run` 模式可接收多个 `--package-root`，这些 root 只提供 composed namespace catalog；`--owner-namespace` 选择 owner root，并按 owner root 构造 Lua `require` roots。host owner 可使用 `[host]+packages`，package owner 只使用自己的 package root。`--package-root` 不是跨包 `require` 授权。`supervise`、`test`、`conformance`、`config` 可接收多个 package root。如果某个 `<PKG> == <HOST>`，该 graph root 折叠为 `PackageAndHost`。否则扫描顺序是 package roots 后 host root。每个 graph root 使用 fresh Lua state，`package.path` 只指向当前 root；运行期 Department 和测试文件的相对路径按所属 owner package root 解析，不存在 package manifest、依赖、order、override 或跨包 require。重复 package root 在 canonicalize 后拒绝；同名 Department 或 Raiser 只要 owner namespace 不同即可共存。`package.lua` 是被移除的 surface，存在即拒绝启动。

queue 是包内命名空间。多 graph-root 组合时，裸 queue 名按 owner namespace 归一化为 `<pkg>.<queue>` 或 `host.<queue>`；跨包消费必须显式写 `pkg.queue`。折叠单包使用 LegacyFlat：裸名输出仍是裸名，同包限定名解析回裸名，`RAISED`、`Event.queue`、delivery id 与 source reference 字节保持单包旧形态。

合法 graph 输入：

```text
<PKG>/departments/<dept>/main.lua
<PKG>/raisers/*.lua
<PKG_N>/departments/<dept>/main.lua
<PKG_N>/raisers/*.lua
<HOST>/departments/<dept>/main.lua
<HOST>/raisers/*.lua
```

每个 Department `main.lua` 必须 return table，其中 `M.spec` 只能解析为：

```lua
{
  consumes = {...},
  produces = {...},
  fanout = {...},
  stall_window = "30s"
}
```

`M.spec` 只接受 `consumes`、`produces`、`fanout`、`stall_window`；未知字段 fail-closed。`stall_window` 是可靠投递 lease window，不是 framework child 的无输出 kill deadline。

每个 Raiser lua 文件 return 一个 source declaration，当前只支持：

```lua
{ type = "cron", interval = "10s", produces = "queue" }
{ type = "file_watch", glob = "host-root-relative/or/absolute/path/*.md", produces = "queue" }
```

`file_watch.glob` 可以是 host root 相对路径或绝对路径；相对路径由 engine 锚到 host root。它用于监听 host repo durable 文件或外部同步到 host 的文件，不支持 runtime scheme。

## 5. Engine Operation Registry

引擎操作 knob 统一由 `config_registry.rs` 的静态 typed registry 声明，并通过显式 host root 构造的 `ConfigContext` 解析。读取优先级是 process env → host `fkst.env` → operational 默认。registry 不读 cwd、`tunables/*.txt`，也没有 set/write/dynamic registration、YAML、DSL、manifest、plugin 或 dashboard 入口。

当前 registry 只有 11 项：

| name | env key | kind | type | default / required |
|---|---|---|---|---|
| `queue_capacity` | `FKST_QUEUE_CAPACITY` | Operational | `usize` | default `16` |
| `department_default_stall_window` | `FKST_DEPARTMENT_DEFAULT_STALL_WINDOW` | Operational | duration string | default `30s`, Department delivery lease |
| `codex_permit_slots` | `FKST_CODEX_PERMIT_SLOTS` | Operational | `usize` | default `20` |
| `max_in_flight_per_dept` | `FKST_MAX_IN_FLIGHT_PER_DEPT` | Operational | `usize` | default `16` |
| `durable_admission_burst_per_dept` | `FKST_DURABLE_ADMISSION_BURST_PER_DEPT` | Operational | `usize` | default `1` |
| `rate_pool_root` | `FKST_RATE_POOL_ROOT` | Operational | string | default `~/.fkst/rate-pools` |
| `retry_default_max_attempts` | `FKST_RETRY_DEFAULT_MAX_ATTEMPTS` | Operational | `usize` | default `5` |
| `retry_default_base` | `FKST_RETRY_DEFAULT_BASE` | Operational | duration string | default `60s` |
| `retry_default_cap` | `FKST_RETRY_DEFAULT_CAP` | Operational | duration string | default `30m` |
| `candidate_prefix` | `FKST_CANDIDATE_PREFIX` | HostFact | string | required |
| `candidate_from_sep` | `FKST_CANDIDATE_FROM_SEP` | HostFact | string | required |

`fkst-framework config --project-root <path> [--package-root <path>]` 是只读自省命令，逐项打印 env key、kind、type、default/required、resolved value/source 与 doc。HostFact 缺失时显示缺失，不会写配置或访问网络。

### 5.1 Boundary Resource Registry

边界资源遵循 capability security 的 no ambient authority 约束：engine 触达的外部资源必须在静态 registry 中枚举，并经 adapter grant、meter、budget/backpressure 与 typed error contract 访问。`fkst-framework boundary-resources` 是只读自省命令，当前条目为 `codex.process`、`shell.process`、`git.process`、`runtime.filesystem` 与 `wall-clock`。`exec_sync`、`spawn_codex_sync` 与 `spawn_codex` 对可分类边界失败返回 `error_class`，值域为 `quota-exhausted`、`auth-degraded`、`provider-unavailable`、`provider-throttle`。

## 6. Runtime I/O 与落点

`RuntimeKind` 固定 6 类。它们都是 runtime scratch 落点；`Marks` 只承载 `once` success marker，`Cache` 只承载 `cache_get` / `cache_set` / `cache_expire` 的 best-effort scratch KV。marker 和 cache 可以跨 tick 保留以减少重复执行或重复计算，但它们和 locks / permits 一样不是 durable 真相、不是 package 状态层、业务 schema、accepted-state 或 rollback state。可靠 delivery 状态不在 `<RT>`，而在 `FKST_DURABLE_ROOT` 下的 redb delivery store。`<RT>` 表示引擎 runtime root，相对值会锚到 `<HOST>`。

| RuntimeKind | 落点 | 用途 | 写入者(engine) | 读取者 |
|---|---|---|---|---|
| `Worktrees` | `<RT>/worktrees` | 隔离 worktree | `sdk_git::setup_worktree`(`git worktree add`) | `count_worktrees` / `list_orphan_worktrees` |
| `CodexPermits` | `<RT>/codex-permits` | `permit-*` fcntl codex 并发池 | `sdk_codex`(建池 + flock 占位) | `spawn_codex` 抢 permit |
| `Locks` | `<RT>/locks` | fcntl 锁文件 | `sdk_git::with_lock` | 同 — 跨 pipeline 互斥 |
| `Logs` | `<RT>/logs` | 过程日志 | `supervise::spawner`(framework-child;dept `log.*` 经 stderr 捕获于此) | 人手 / 调试,非 file_watch 输入 |
| `Marks` | `<RT>/marks` | per-key success marker | `sdk_mark::once` | `once` |
| `Cache` | `<RT>/cache` | best-effort scratch KV with optional ttl | `sdk_cache::cache_set` / `sdk_cache::cache_expire` | `sdk_cache::cache_get` |

说明:
- **engine 自己只写 scratch 结构事实**(worktree / permit / lock / log / mark / cache)。package 不访问 `<RT>`，也不把 `<RT>` 当 inbox、完成态或业务 schema 数据库。
- `RuntimeLayout` 只提供固定 runtime dir 解析，framework 先把相对 runtime root 锚到 `<HOST>` 再建路径。
- `with_lock`、`once` 与 `cache` 共用 runtime key 合约：key / name 必须是非空相对 filesystem path，`/` 表示目录；每个 segment 非空、最长 255 bytes、匹配 `[A-Za-z0-9._-]+`，且不是全点 segment（如 `.` / `..` / `...`）；禁止 leading / trailing `/`、`//`、反斜杠、NUL 与绝对路径。校验后的 key 保持为 `<RT>/{locks,marks,cache}/<key>/` 目录路径，engine 在该目录下写 reserved leaf file（`=lock` / `=mark` / `=value`），形成可人工浏览的目录树，不做 byte hex 编码。`=` 不在合法 key segment 字符集内，因此不会与有效 key 冲突。`locks/once/` 是 `once` 内部锁的保留子目录，不属于 `with_lock` 用户锁命名空间。
- `file_watch` 只接受 host-root 相对或绝对 glob；不支持 runtime scheme。
- codex log **不属** `RuntimeKind`/`<RT>`:`sdk_codex` 把它落到 `FKST_RUNTIME_LOG_DIR` 或平台默认目录(如 `~/Library/Logs/fkst`)下的 `codex/`。它与 `<RT>/logs` 同属 process-trace scratch(可 grep、非事实源),但落点不同,`supervise` 也不给 framework child 注入 `FKST_RUNTIME_LOG_DIR`。每次 `spawn_codex_sync` / `spawn_codex` 会在写入本次 log 前 best-effort 修剪同一个 `codex/` 目录中的旧 `.log` 文件：`FKST_CODEX_LOG_MAX_AGE` 默认 `48h`，`0` 或空值表示关闭年龄修剪；`FKST_CODEX_LOG_MAX_BYTES` 为空或 `0` 表示不启用容量上限，启用时优先删除最旧 log；当前请求的 log path 永远豁免，删除或扫描失败只写 warning。
- With `worktree`, `spawn_codex_sync` writes prompt, stdout, stderr, a trusted effect receipt log, effect marker, result marker, and completion status into writable runtime scratch at `<runtime-log-root>/codex-adoption/<key>/`; `key` is derived from optional `dedup_key`, `worktree`, and prompt/context identity. While holding the key's `run.lock`, the framework writes and rereads a visible `intent` status before allowing the detached `fkst-framework __codex-worker` to claim `running` and execute the real `codex exec`; the worker must also claim visible intent under the same lock, preventing duplicate effects if the parent exits after spawn but before the running record is visible. After `codex exec` returns, the worker first appends a `CODEX_EFFECT:` keyed receipt to the adoption-local trusted receipt log, then publishes the adoption-local keyed effect marker and result marker; redrive can recover completed status from the result marker, effect marker, or trusted `effect_key` receipt without repeating the effect. The human/debug Codex `log_path` continues to mix status with untrusted stdout/stderr output, so it is not a recovery receipt source. Re-delivery of the same work unit checks this handoff first: completed reads the result, running waits for the same worker/codex, and stale intent or dead running owner redrives and claims before effect without storm-spawning `codex`. Different worktrees do not adopt each other even with the same `dedup_key`, prompt, and context; different `dedup_key` or prompt/context under the same worktree also remain distinct. This directory is process-trace scratch outside `RuntimeLayout`, used for supervisor-generation adoption; it is not written into the worktree and does not carry package business schema, accepted-state, or rollback state. Without `worktree`, `spawn_codex_sync` keeps pipe/wait semantics.
- engine **不写** runtime 持久状态；accepted-state / rollback 是外部 release pipeline 的事实，见 §13。

## 7. 运行态数据流

```text
source
  cron tick or file_watch event
    ↓
Fanout::send(queue, Event)
    ↓
Vec<mpsc::Sender<Event>>
    ↓
consumer inbox
    ↓
spawn fkst-framework run <department main.lua> --project-root <HOST> --package-root <PKG_A> --package-root <PKG_B> ... --owner-namespace <id> --event <json>
    ↓
single Lua state + owner-scoped package.path + pipeline(event)
    ↓
SDK calls: file/json/git/lock/worktree/exec/codex/log/now/raise
    ↓
optional stdout line: RAISED: <base64-url-json>
    ↓
parse_raised from stdout tail
    ↓
Fanout::send(raised.queue, raised_event)
```

Department 收到的标准事件是 `Event{queue,payload,ts}`，其中 `ts` 是 Unix 毫秒。

`consumer.rs` 为每个 Department 的每个 consumed queue 建 receiver，再汇入该 Department 的 inbox。每个事件 spawn 一个 framework child，不是在 supervisor 进程内直接调用 Lua。framework child 的 stdout/stderr 会写到 `<RT>/logs/framework-child/` 下的具名 log；dept 的 `log.*` 以结构化行写 stderr，并由这个具名 log 捕获。RAISED 解析不依赖 log 文件，而是解析 captured stdout。

`raise` 不落盘。它通过 `LuaSerdeExt` 的 `lua.from_value` 将 Lua payload 转为 JSON 后进入 stdout `RAISED:` 协议。bare Lua empty table 没有数组 / 对象意图标记，序列化为 JSON object `{}`；由 `json.decode("[]")` 构造的 array-tagged empty table 会保持为 JSON array `[]`。需要可能为空的数组字段时，package 必须显式构造 array-tagged table；engine 不根据字段名或 schema 推断空表形态。

需要 durable intent 或完成态事实时，package/host 必须显式写入 git commit、host repo 文件或外部源，再由 package controller 通过 `cron` / `file_watch` 重新引入事件。

## 8. 瞬时队列与恢复模型

内存队列是瞬时队列。它只存在于当前 `fkst-framework supervise` 进程和 supervisor 生命周期内；进程挂掉、supervisor 重启或 host 迁移时队列内容丢失。engine 不把 queue 当 durable message state,不跨机同步队列,也不引入 MQ broker。

durable 真相来自可观测事实：git commit、明确的 host filesystem fact 与外部源（例如 GitHub issue）。真正跨机或长期保留的完成态事实应进入 git commit 或外部源。`<RT>` 只是一轮运行的一次性 scratch，`locks` 也不是 durable 真相——`with_lock` 只是进程死即释放的**处理中租约/协调事实**，不承载完成态。engine 只提供 `file`、`file_watch`、cron、git/worktree 和 `with_lock` 等原语，不拥有业务部门、inbox schema、完成判定、重试策略或幂等语义。

恢复模型：package controller 用 cron / file_watch 读取 durable 源，推导未完成工作，并重新 enqueue 对应事件。崩溃等价于从 0 重来；in-flight 事件丢失后，下一拍从 durable 源重新推导。幂等由 package controller 保证；engine 只负责把重新派生的事件送入当前内存队列。

可靠 delivery 默认启用。Department 可用 `M.spec.ephemeral = {"queue"}` 将本 Department 对指定 consumed queue 的订阅降级为非可靠；`M.spec.retry = false` 只表示失败不重试，不再表示非可靠投递。可靠订阅启动时必须有 `FKST_DURABLE_ROOT`，缺失 fail-closed。`DeliveryRouter` 对可靠订阅写入 redb delivery store 后再唤醒 consumer；ephemeral 订阅仍直接走 `Fanout::send`。可靠 delivery 的 source event 必须带 `SourceRef{kind, reference}`，cron 由 raiser 名派生，file_watch 由绝对路径派生；Department `RAISED` 进入可靠 queue 时继承上游 source_ref，缺失则 publish fail-closed，上游 delivery 不 ack 并进入 retry。

consumer 的可靠路径由定时 tick 和 Fanout 唤醒触发，调用 delivery store `lease` 取 due 或过期 lease 的记录，构造标准 `Event{queue,payload,ts}` 后 spawn framework。exit 0 且所有 RAISED publish 成功才 `ack`；spawn error、stall、非零退出或 RAISED publish 失败都调用 `retry`。retry 达到 `max_attempts` 时 delivery 移入 redb dead 表，并 best-effort 经 Router publish `dead_letter` 通知；当前 delivery 自身来自 `dead_letter` 时抑制再次发送 `dead_letter`，避免自环。`Fanout` 在可靠路径只承担进程内唤醒，不再承载可靠事实。

engine 维护 durable 在途 delivery state，但它不是实体业务真相、accepted state 或 rollback state。`处理中` 可以是 redb delivery lease、`with_lock` 租约（进程死后 fcntl lock 自动释放）或 worktree 等可观测事实；完成态仍是 commit、明确的 host filesystem fact 或外部源事实。

## 9. SDK Surface

固定 surface：

| Surface | 当前实现 |
|---|---|
| `pipeline(event)` | Department Lua 入口约定 |
| `source` | Raiser Lua return declaration 约定 |
| `raise(queue, payload)` | `raise.rs`，进程内 buffer，退出 stdout `RAISED:` |
| `spawn_codex_sync(opts)` | `sdk_codex.rs`，同步 `codex exec` |
| `spawn_codex(opts)` | `sdk_codex.rs`，返回 pipeline-local handle |
| `await_all(handles)` | `sdk_codex.rs`，join handles，防跨 pipeline/重复消费 |
| `exec_sync(cmd|opts)` | `sdk_basic.rs`，运行 `/bin/sh -c`，可选 cwd/env/timeout/read_coalesce |
| `with_lock(name, fn)` | `sdk_git.rs`，fcntl exclusive flock |
| `once(key, fn)` | `sdk_mark.rs`，locked per-key marker，成功后写入 scratch marker |
| `cache_set(key, value[, ttl_seconds])` | `sdk_cache.rs`，best-effort scratch KV 原子覆盖写，支持可选 expiry metadata |
| `cache_get(key)` | `sdk_cache.rs`，best-effort scratch KV 读取，缺失 / 过期 / malformed / unreadable 返回 nil |
| `cache_expire(key)` | `sdk_cache.rs`，best-effort scratch KV 显式删除，缺失视为成功 |
| `graph_json()` | `sdk_graph.rs`，只读 composed graph JSON snapshot |
| `restricted_lua_load(opts)` | `sdk_restricted_lua.rs`, fresh Lua state restricted source loader |
| `git_log_count(grep, since)` | `sdk_git.rs`，调用 `git log --grep --since --oneline` |
| `git_log_grep(grep, since)` | `sdk_git.rs`，调用 `git log --format=%H` |
| `count_worktrees()` | `sdk_git.rs`，解析 `git worktree list --porcelain` |
| `list_orphan_worktrees(prefix)` | `sdk_git.rs`，列 `<RT>/worktrees/<prefix>*` linked worktree |
| `setup_worktree(prefix)` | `sdk_git.rs`，创建 `<RT>/worktrees/<prefix>-<ULID>` 和 candidate branch |
| `file.read/write/exists` | `sdk_fs.rs` |
| `log.info/warn/error` | `sdk_log.rs`，结构化行写 stderr，由 supervise 捕获进 framework-child log |
| `now()` | `sdk_basic.rs`，Unix seconds |

`json` surface 只包含 `json.decode`，不包含 `json.encode` 或 `json.array`。`json.decode` 产生的 JSON array table 会带有 `LuaSerdeExt` 可识别的数组标记，因此 `json.decode("[]")` 经 `raise` 仍是 `[]`；裸 `{}` 经 `raise` 是 `{}`。非空 sequence 序列化为 JSON array，非空 map 序列化为 JSON object。

`restricted_lua_load({ source, bindings?, mode?, name? })` evaluates small declarative Lua sources in a fresh restricted Lua state. The restricted chunk receives an empty `_ENV` plus caller-supplied `bindings`, defaults to text-only loading, and accepts bytecode only with explicit `mode = "bytecode"`. Ambient VM capabilities such as `require`, `load`, `_G`, `debug`, `package`, raw table primitives, metatable access, `io`, `os`, coroutine APIs, `string.dump`, and `("").dump` are unreachable. Results are copied back only as plain nil / boolean / number / string / table data; compile/runtime failures and non-plain returns fail closed with structured `restricted_lua` errors.

`graph_json()` 是显式授权的只读 topology introspection。只有当前 Department 的 `M.spec.graph_json = true` 时可调用；未声明授权时调用失败。它按当前 fixed package roots input set 与 host root 重新扫描并验证 composed graph，返回 `fkst.graph.v1` JSON string。schema 包含 `nodes` 与 `edges`：raiser nodes 带 `source`，queue nodes 带 `fanout`，department nodes 带 `consumes`、`produces`、`ephemeral`、`stall_window` 与 materialized `retry` metadata；edges 表示 raiser→queue、queue→department 和 department→queue。node `id` 与 edge endpoint 使用 `kind:canonical_name` 形态，避免同名 raiser / queue / department 在图渲染时碰撞。输出排序稳定，不包含 `lua` path、`owner_root`、queue capacity 或 runtime state。

`fkst.observe([opts])` is the in-process Lua primitive for the existing `fkst-framework observe --json` durable delivery snapshot. `opts.limit` defaults to 500 and must be within 1..10000. It resolves `FKST_DURABLE_ROOT`, uses the live observe socket when available, otherwise opens `<DURABLE>/delivery.redb` read-only through the same offline path, and returns the existing `DeliveryObserveSnapshot` model as Lua data. This keeps observe data behind an engine-owned capability boundary without shelling out to the engine binary and without adding observe semantics beyond the CLI contract.

`M.spec.retry` 默认启用；`retry=false` 表示失败不重试；`retry={...}` 支持 `max_attempts`、`base`、`cap` 子集覆盖。全局默认由 registry 的 `retry_default_max_attempts`、`retry_default_base`、`retry_default_cap` 提供。可靠 / 非可靠投递由 `M.spec.ephemeral` 决定，不由 retry 决定。可靠路径不依赖 payload dedup key、runtime marker 或 retry scratch 文件；delivery store 使用 `delivery_id`、`lease_generation` 和 redb 事务提供 fencing、ack、retry 和 dead 表。

`spawn_codex_sync` 与 `spawn_codex` 使用整体 wall-clock `timeout`，默认 3600 秒；stdout/stderr 输出只被捕获，不延长 timeout。`spawn_codex` 的 handle 只能由同一个 Lua pipeline 的 `await_all` 消费。单 handle 等待用 `await_all({handle})`。失败结果保留既有 `error_kind`，同时在可分类时提供 `error_class`；provider 非零退出可直接返回 `error_class` 而不制造 adapter `error_kind`。没有 sleep timer、first-result fanout、provider abstraction 或动态 SDK extension。

## 10. 事件与队列机制

`Config` 包含 `queue`、`raiser`、`department`、`limits`。Queue 有 `capacity` 与 `fanout`。Raiser 只有 `Cron` 与 `FileWatch`。Department 有 `lua`、`owner_root`、`owner_namespace`、`consumes`、`produces`、`ephemeral`、`stall_window`、可选 `retry`。`stall_window` 用作可靠投递 lease 与续租窗口，不流入 framework child kill deadline。多 graph-root 下，Config 的 queue、raiser、department key 都是 canonical name；折叠单包仍是裸名。

validation 规则：

- `limits.global_codex_processes > 0`
- 每个 queue `capacity > 0`
- 每个 Raiser produces 必须引用已声明 queue
- 每个 Department consumes/produces 必须引用已声明 queue
- Department lua 文件必须存在
- stall_window 必须以 `s`、`m` 或 `h` 结尾，语义是 Department delivery lease
- queue 不能孤立
- 非 fanout queue 不能有多个 consumer
- Department consume 和 produce 同一 queue 时，该 queue 必须 fanout

`M.spec.fanout` 的语义是 queue contract，不是广播主题。声明者必须在同一 `M.spec` 的 `consumes` 或 `produces` 中引用该 queue；否则 graph scan 拒绝。fanout queue 的物理实现仍是 `Vec<mpsc::Sender<Event>>`，只是 validator 允许多个 active consumer。

## 11. 并发与进程边界

supervisor 使用 current-thread tokio runtime，spawn `fkst-framework supervise` 并把 stdout/stderr 继承出去。收到 interrupt/terminate 时 supervisor 返回对应 exit code，不 signal event runtime。它最后 best-effort reap children。

supervise 运行在 current-thread tokio runtime 内，但每个 Department event 都会 spawn 一个 framework child process。framework child 是新的 process group leader，并运行到自然退出。Department `M.spec.stall_window` 是可靠投递 lease 与续租窗口，不是 codex kill deadline。

The supervise process boundary follows process-supervision practice: each `fkst-framework run` child has one engine owner that observes exit and reaps it, and that observation is the source for delivery ack or retry. Blocking `wait` and blocking pipe I/O are not allowed on the core async runtime thread; stdout/stderr capture and child exit observation must not starve cron ticks, file-watch ticks, reliable wakes, lease renewal, retry, or dead-letter maintenance. Department spawn admission is bounded before a reliable lease is consumed, so lack of capacity leaves delivery due for a later dispatch pass instead of creating an unobservable leased backlog.

Codex SDK 也把 `codex exec` 放入 process group。`spawn_codex_sync` 与 `spawn_codex` 使用整体 wall-clock `timeout`，默认 3600 秒；只有总运行时间超过 timeout 时才 kill process group，stdout/stderr 输出只被捕获，不延长 timeout。permit 池使用 fcntl lock file，不是内存 semaphore。permit 数来自 registry 的 `codex_permit_slots`：env 或 host `fkst.env` 可覆盖，未设置时默认 `20`。

`with_lock(name, fn)` 是跨 pipeline 互斥 primitive。它把校验后的锁名解析到 `<RT>/locks/<name>/=lock` 并打开，获取 exclusive flock，执行 Lua function，释放 file handle。进程死时 lock 自动释放。

`once(key, fn)` 是 best-effort per-key de-bounce primitive。它把校验后的 key 作为相对路径，在 `<RT>/locks/once/<key>/=lock` 上获取 exclusive flock 后检查 `<RT>/marks/<key>/=mark`；marker 存在则返回 `false`，不存在则执行 `fn`，成功后写 marker 并返回 `true`。marker 是 host-local scratch，不是 durable state；runtime root 被清空或换 host 后，`fn` 会重新运行，这由 at-least-once、下游 / package 幂等和从 durable 源重新推导来容纳。`fn` 失败时不写 marker，后续调用会重试。

`once` 决策通过 engine log 观察：`once decision=skip-marked key=...` 与 `once decision=ran-marked key=...` 提供可 grep trail。marker 内容只含人工提示（`key`、`marked_at`），不被解析；LIVE lock holder 用 `lsof <RT>/locks/once/<key>/=lock` 查看。

`cache_set(key, value[, ttl_seconds])` / `cache_get(key)` / `cache_expire(key)` 是 best-effort scratch KV primitive。它们把校验后的 key 作为相对路径读写 `<RT>/cache/<key>/=value`；`cache_set` 用 temp file + rename 原子覆盖写入带 expiry metadata 的 byte-exact string value，`ttl_seconds` 缺省或 nil 表示不过期，正数表示按 wall-clock deadline 过期；`cache_get` 命中返回 string，缺失、过期、malformed 或 unreadable 时返回 nil，过期文件会 best-effort lazy evict；`cache_expire` 显式删除 key，缺失视为成功。`cache_set`、`cache_get` 与 `cache_expire` 都使用 `<RT>/locks/cache/<key>/=lock` per-key flock 串行化单个 key 的文件操作。cache 是 host-local scratch，不是 durable state；runtime root 被清空或换 host 后，`cache_get` 返回 nil，调用者必须从 durable source 重新推导；需要 read-compare-write 原子性时由调用者外层使用 `with_lock`。

## 12. Git 与 Worktree Primitives

`setup_worktree` 会创建 candidate branch。所有 git SDK 命令使用 `git -C <HOST> ...`，不依赖 framework launcher cwd。branch 前缀和 from separator 是 HostFact，来自 `FKST_CANDIDATE_PREFIX` / `FKST_CANDIDATE_FROM_SEP` 或 host `fkst.env`，缺失时 fail-closed。

具体 integration branch、candidate topology、host ref 命名、push/pull 策略属于 package/host，不是 substrate 固定事实。

## 13. 发布边界

fkst-substrate 的 accepted release state 是外部 release pipeline 的事实。推荐外部链路是 build → test → `--self-test` → conformance → 签名 artifact → deploy → canary / 回退策略。

engine 无 runtime accepted-state/回退，发布安全是外部策略。

⟦AI:FKST⟧

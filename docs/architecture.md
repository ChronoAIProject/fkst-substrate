# fkst-substrate 引擎架构

本库是稳定发布的受监督事件 / SDK / 进程衬底；业务 Lua package 不在本库内，由独立仓库或 host 通过 `FKST_PACKAGE_ROOT` / `--package-root` 注入。

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
│           └── event_fanout.rs consumer.rs spawner.rs raised.rs retry_state.rs retry_sweep.rs
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
fkst-framework run <lua> --project-root <path> [--package-root <path>] --event <json>
fkst-framework supervise --project-root <path> [--package-root <path>] --framework-bin <path>
fkst-framework conformance --project-root <path> [--package-root <path>]
fkst-framework config --project-root <path> [--package-root <path>]
fkst-framework test --project-root <path> [--package-root <path>]
fkst-framework --self-test
```

`fkst-supervisor` 没有业务子命令；它只把当前目录作为 host root，启动 `fkst-framework supervise`。

`fkst-framework test` 是 Tier III test-mode runner，不启动 supervise，不执行 dispatcher，也不把 `fkst.test` 加入 production Lua SDK。它用 `PackageRoots::resolve` 得到 package root 和 host root，只发现两类文件：

```text
<ROOT>/departments/*/*_test.lua
<ROOT>/tests/*_test.lua
```

runner 不全树递归，不扫描 `raisers/` 或 `fkst/`。每个测试文件在独立 Lua state 中执行，先注册 production SDK 以便测试可 `require` package 模块和调用固定 SDK，再注册 test-mode `fkst.test` 表。测试文件必须返回 table；runner 只执行排序后的 `test_*` key。单个测试失败后继续执行同文件其余测试和后续文件，最后输出 `N passed, M failed`；`M > 0` 时退出码非 0。

`fkst.test` 包含 `eq(actual, expected[, msg])`、`is_true(value[, msg])`、`raises(fn[, msg])`、`is_nil(value[, msg])` 四个断言，以及 test-mode-only `run_department(path, event[, opts])`。`run_department` 用 fresh Lua state 注册 production SDK 和独立 `RaiseBuffer`，再通过正常 department runner 注入 `event`；它返回 `{ exit_code = int, raises = { { queue = string, payload = table }, ... } }`。相对 `path` 按 package root 解析，`opts.cwd`、`opts.env`、`opts.path_prepend` 只作用于该次执行并随后恢复。它是最小 Lua 单测工具，不提供 describe/it、hook、fixture、mock、stub 或测试框架 DSL。除非明确验证真实 CLI 路径，Lua 单测不应调用 codex。

## 4. Package Root 与 Host Root

`PackageRoots::resolve` 产生一个 package root 和一个 host root：

```text
--package-root <path> 优先
否则 FKST_PACKAGE_ROOT
host root 来自显式 --project-root
run 模式未传 --project-root 时可从 Lua 路径推断
```

如果 `<PKG> == <HOST>`，graph root 只有一个 `PackageAndHost`。否则扫描顺序是 package root 后 host root。重复 Department 名或 Raiser 名直接拒绝启动。`package.lua` 是被移除的 surface，存在即拒绝启动。

合法 graph 输入：

```text
<PKG>/departments/<dept>/main.lua
<PKG>/raisers/*.lua
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

`M.spec` 只接受 `consumes`、`produces`、`fanout`、`stall_window`；未知字段 fail-closed。

每个 Raiser lua 文件 return 一个 source declaration，当前只支持：

```lua
{ type = "cron", interval = "10s", produces = "queue" }
{ type = "file_watch", glob = "host-root-relative/or/absolute/path/*.md", produces = "queue" }
```

`file_watch.glob` 可以是 host root 相对路径或绝对路径；相对路径由 engine 锚到 host root。它用于监听 host repo durable 文件或外部同步到 host 的文件，不支持 runtime scheme。

## 5. Engine Operation Registry

引擎操作 knob 统一由 `config_registry.rs` 的静态 typed registry 声明，并通过显式 host root 构造的 `ConfigContext` 解析。读取优先级是 process env → host `fkst.env` → operational 默认。registry 不读 cwd、`tunables/*.txt`，也没有 set/write/dynamic registration、YAML、DSL、manifest、plugin 或 dashboard 入口。

当前 registry 只有 8 项：

| name | env key | kind | type | default / required |
|---|---|---|---|---|
| `queue_capacity` | `FKST_QUEUE_CAPACITY` | Operational | `usize` | default `16` |
| `department_default_stall_window` | `FKST_DEPARTMENT_DEFAULT_STALL_WINDOW` | Operational | duration string | default `30s` |
| `codex_permit_slots` | `FKST_CODEX_PERMIT_SLOTS` | Operational | `usize` | default `20` |
| `retry_default_max_attempts` | `FKST_RETRY_DEFAULT_MAX_ATTEMPTS` | Operational | `usize` | default `5` |
| `retry_default_base` | `FKST_RETRY_DEFAULT_BASE` | Operational | duration string | default `60s` |
| `retry_default_cap` | `FKST_RETRY_DEFAULT_CAP` | Operational | duration string | default `30m` |
| `candidate_prefix` | `FKST_CANDIDATE_PREFIX` | HostFact | string | required |
| `candidate_from_sep` | `FKST_CANDIDATE_FROM_SEP` | HostFact | string | required |

`fkst-framework config --project-root <path> [--package-root <path>]` 是只读自省命令，逐项打印 env key、kind、type、default/required、resolved value/source 与 doc。HostFact 缺失时显示缺失，不会写配置或访问网络。

## 6. Runtime I/O 与落点

`RuntimeKind` 固定 8 类。它们都是 runtime scratch 落点；`Marks` 承载 `once` 与引擎自动可靠重试的 per-(dept,event) success marker，`Cache` 只承载 `cache_get` / `cache_set` 的 best-effort scratch KV，`Retry` / `Dead` 只承载 `M.spec.retry` 驱动的 scratch 重投递记录与终止摘要。marker、cache、retry 和 dead 可以跨 tick 保留以减少重复执行或重投递失败事件，但它们和 locks / permits 一样不是 durable 真相、不是 package 状态层、业务 schema、accepted-state 或 rollback state。`<RT>` 表示引擎 runtime root，相对值会锚到 `<HOST>`。

| RuntimeKind | 落点 | 用途 | 写入者(engine) | 读取者 |
|---|---|---|---|---|
| `Worktrees` | `<RT>/worktrees` | 隔离 worktree | `sdk_git::setup_worktree`(`git worktree add`) | `count_worktrees` / `list_orphan_worktrees` |
| `CodexPermits` | `<RT>/codex-permits` | `permit-*` fcntl codex 并发池 | `sdk_codex`(建池 + flock 占位) | `spawn_codex` 抢 permit |
| `Locks` | `<RT>/locks` | fcntl 锁文件 | `sdk_git::with_lock` | 同 — 跨 pipeline 互斥 |
| `Logs` | `<RT>/logs` | 过程日志 | `supervise::spawner`(framework-child;dept `log.*` 经 stderr 捕获于此) | 人手 / 调试,非 file_watch 输入 |
| `Marks` | `<RT>/marks` | per-key success marker | `sdk_mark::once` / `supervise::retry_state` | `once` / consumer retry / retry sweeper marker check |
| `Cache` | `<RT>/cache` | best-effort scratch KV | `sdk_cache::cache_set` | `sdk_cache::cache_get` |
| `Retry` | `<RT>/retry` | reliable retry scratch record | `supervise::retry_state` | `supervise::retry_sweep` |
| `Dead` | `<RT>/dead` | reliable terminal failure scratch summary | `supervise::retry_state` | 人手 / 调试,非事实源 |

说明:
- **engine 自己只写 scratch 结构事实**(worktree / permit / lock / log / mark / cache / retry / dead)。package 不访问 `<RT>`，也不把 `<RT>` 当 inbox、完成态或业务 schema 数据库。
- `RuntimeLayout` 只提供固定 runtime dir 解析，framework 先把相对 runtime root 锚到 `<HOST>` 再建路径。
- `with_lock`、`once` 与 `cache` 共用 runtime key 合约：key / name 必须是非空相对 filesystem path，`/` 表示目录；每个 segment 非空、匹配 `[A-Za-z0-9._-]+`，且不是 `.` 或 `..`；禁止 leading / trailing `/`、`//`、反斜杠、NUL 与绝对路径。校验后的 key 直接 join 到 `<RT>/{locks,marks,cache}/<key>`，形成可人工浏览的目录树，不做 byte hex 编码。`locks/once/` 是 `once` 内部锁的保留子目录，`locks/reliable/` 是引擎自动可靠重试内部锁的保留子目录，它们都不属于 `with_lock` 用户锁命名空间。
- `file_watch` 只接受 host-root 相对或绝对 glob；不支持 runtime scheme。
- codex log **不属** `RuntimeKind`/`<RT>`:`sdk_codex` 把它落到 `FKST_RUNTIME_LOG_DIR` 或平台默认目录(如 `~/Library/Logs/fkst`)下的 `codex/`。它与 `<RT>/logs` 同属 process-trace scratch(可 grep、非事实源),但落点不同,`supervise` 也不给 framework child 注入 `FKST_RUNTIME_LOG_DIR`。
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
spawn fkst-framework run <department main.lua> --project-root <HOST> --package-root <PKG> --event <json>
    ↓
single Lua state + pipeline(event)
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

`raise` 不落盘。需要 durable intent 或完成态事实时，package/host 必须显式写入 git commit、host repo 文件或外部源，再由 package controller 通过 `cron` / `file_watch` 重新引入事件。

## 8. 瞬时队列与恢复模型

内存队列是瞬时队列。它只存在于当前 `fkst-framework supervise` 进程和 supervisor 生命周期内；进程挂掉、supervisor 重启或 host 迁移时队列内容丢失。engine 不把 queue 当 durable message state,不跨机同步队列,也不引入 MQ broker。

durable 真相来自可观测事实：git commit、明确的 host filesystem fact 与外部源（例如 GitHub issue）。真正跨机或长期保留的完成态事实应进入 git commit 或外部源。`<RT>` 只是一轮运行的一次性 scratch，`locks` 也不是 durable 真相——`with_lock` 只是进程死即释放的**处理中租约/协调事实**，不承载完成态。engine 只提供 `file`、`file_watch`、cron、git/worktree 和 `with_lock` 等原语，不拥有业务部门、inbox schema、完成判定、重试策略或幂等语义。

恢复模型：package controller 用 cron / file_watch 读取 durable 源，推导未完成工作，并重新 enqueue 对应事件。崩溃等价于从 0 重来；in-flight 事件丢失后，下一拍从 durable 源重新推导。幂等由 package controller 保证；engine 只负责把重新派生的事件送入当前内存队列。

supervise 内置 retry sweeper 是 scratch 重投递修复，不是新 source kind。Department 默认启用可靠重试，使用 registry 的 `retry_default_max_attempts`、`retry_default_base`、`retry_default_cap`。`M.spec.retry = false` 显式关闭；`M.spec.retry = { ... }` 覆盖 `max_attempts`、`base`、`cap` 的任意子集，缺失字段从全局默认补齐。只消费 `dead_letter` 队列的 Department 未显式声明 retry table 时默认关闭；同时消费 `dead_letter` 和其它队列时必须显式声明 retry table 或 `false`。启用后，consumer 用 string `event.payload.dedup_key` 形成 `<dept>/<sanitized(dedup_key)>`；净化是有损映射，段内非法字符替换为 `-`，发生有损净化时 framework warn，host 应产出净化后仍唯一的 key；缺失或非 string 的 dedup_key 安静按普通事件跑一次，不写 marker/retry/dead。有效 key 在 spawn 前持 `<RT>/locks/reliable/<key>` 检查 marker 和 retry record；marker 存在则跳过，record 存在且 `due_at > now` 时跳过且不刷新 `due_at`、不改 `attempt`。只有无 record 或 `due_at <= now` 时才写入 `<RT>/retry/<key>`，其中 `attempt` 保留现有值或为 `0`，`generation` 递增，`due_at = now + stall_window + sweep_interval + margin`。这个 due_at 是 visibility-timeout，不是 execution-timeout；consumer 在 child 存活期间按小于 lease 的间隔续租，只更新同一 `generation` 的 `due_at`，不改 `attempt` 或其它字段。续租在 record 缺失、`generation` 不匹配或 marker 已存在时停止；child 完成后先停止续租，再做完成状态更新。成功 exit 0 写 `<RT>/marks/<key>` 并删除 retry；失败只在当代完成时递增 attempt，未达 `max_attempts` 写退避 due_at，达到上限写 `<RT>/dead/<key>` 并 best-effort 发送 `dead_letter`。当前事件来自 `dead_letter` 时只写 dead record，不再发送新的 `dead_letter`。sweeper 周期扫描 `<RT>/retry`，对 due_at 到期的记录持 `<RT>/locks/reliable/<key>` 后检查 `<RT>/marks/<key>`；marker 已存在则删除 retry，marker 不存在则把记录里的 payload 广播重投到原 queue。sweeper 不递增 attempt。consumer 和 child 活着时租约不会到期；supervisor/consumer 进程死亡后不再续租，租约过期后由 sweeper 重投。fanout queue 的消费者不得混合启用和未启用 retry。`<RT>/retry` 和 `<RT>/dead` 可清可丢，丢失后仍由 durable source 下轮重新推导兜底。整体语义是 at-least-once-until-success；`Fanout::send` 仍是 try_send best-effort，慢消费者仍可能 drop。

engine 不维护 durable message state。`处理中` 可以是 `with_lock` 租约（进程死后 fcntl lock 自动释放）或 worktree 等可观测事实；完成态是 commit、明确的 host filesystem fact 或外部源事实。engine 明确不提供 message state、ack / visibility timeout、状态队列或 durable broker。

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
| `exec_sync(cmd|opts)` | `sdk_basic.rs`，运行 `/bin/sh -c`，可选 cwd/env/timeout |
| `with_lock(name, fn)` | `sdk_git.rs`，fcntl exclusive flock |
| `once(key, fn)` | `sdk_mark.rs`，locked per-key marker，成功后写入 scratch marker |
| `cache_set(key, value)` | `sdk_cache.rs`，best-effort scratch KV 原子覆盖写 |
| `cache_get(key)` | `sdk_cache.rs`，best-effort scratch KV 读取，缺失返回 nil |
| `git_log_count(grep, since)` | `sdk_git.rs`，调用 `git log --grep --since --oneline` |
| `git_log_grep(grep, since)` | `sdk_git.rs`，调用 `git log --format=%H` |
| `count_worktrees()` | `sdk_git.rs`，解析 `git worktree list --porcelain` |
| `list_orphan_worktrees(prefix)` | `sdk_git.rs`，列 `<RT>/worktrees/<prefix>*` linked worktree |
| `setup_worktree(prefix)` | `sdk_git.rs`，创建 `<RT>/worktrees/<prefix>-<ULID>` 和 candidate branch |
| `file.read/write/exists` | `sdk_fs.rs` |
| `log.info/warn/error` | `sdk_log.rs`，结构化行写 stderr，由 supervise 捕获进 framework-child log |
| `now()` | `sdk_basic.rs`，Unix seconds |

`M.spec.retry` 默认启用；`retry=false` opt out；`retry={...}` 支持 `max_attempts`、`base`、`cap` 子集覆盖。全局默认由 registry 的 `retry_default_max_attempts`、`retry_default_base`、`retry_default_cap` 提供。启用后，缺失或非 string 的 `event.payload.dedup_key` 安静普通跑一次，不写 marker/retry/dead。有效 key 会净化为 `<dept>/<sanitized(dedup_key)>`：段内非 `[A-Za-z0-9._-]` 字符替换为 `-`，保留 `/` 作目录分层，再经 runtime key 校验；该映射有损，发生有损净化时 framework warn，host 应保证净化后 key 仍唯一。未到期 retry record 会跳过本次投递；到期投递写入新的 `generation`，child 存活期间续租当前 `generation`，完成只允许当前 `generation` 更新状态。它不做业务错误分类，不提供 exactly-once。

`spawn_codex` 的 handle 只能由同一个 Lua pipeline 的 `await_all` 消费。单 handle 等待用 `await_all({handle})`。没有 sleep timer、first-result fanout、provider abstraction 或动态 SDK extension。

## 10. 事件与队列机制

`Config` 包含 `queue`、`raiser`、`department`、`limits`。Queue 有 `capacity` 与 `fanout`。Raiser 只有 `Cron` 与 `FileWatch`。Department 有 `lua`、`consumes`、`produces`、`stall_window`、可选 `retry`。

validation 规则：

- `limits.global_codex_processes > 0`
- 每个 queue `capacity > 0`
- 每个 Raiser produces 必须引用已声明 queue
- 每个 Department consumes/produces 必须引用已声明 queue
- Department lua 文件必须存在
- stall_window 必须以 `s`、`m` 或 `h` 结尾
- queue 不能孤立
- 非 fanout queue 不能有多个 consumer
- Department consume 和 produce 同一 queue 时，该 queue 必须 fanout

`M.spec.fanout` 的语义是 queue contract，不是广播主题。声明者必须在同一 `M.spec` 的 `consumes` 或 `produces` 中引用该 queue；否则 graph scan 拒绝。fanout queue 的物理实现仍是 `Vec<mpsc::Sender<Event>>`，只是 validator 允许多个 active consumer。

## 11. 并发与进程边界

supervisor 使用 current-thread tokio runtime，spawn `fkst-framework supervise` 并把 stdout/stderr 继承出去。收到 interrupt/terminate 时 supervisor 返回对应 exit code，不 signal event runtime。它最后 best-effort reap children。

supervise 运行在 current-thread tokio runtime 内，但每个 Department event 都会 spawn 一个 framework child process。framework child 是新的 process group leader。stall window 内无 stdout/stderr 输出时，supervise 对 `-pgid` 发送 `SIGKILL`，使 framework child 及其 codex 子孙一起退出。

Codex SDK 也把 `codex exec` 放入 process group。stall 时 kill process group。permit 池使用 fcntl lock file，不是内存 semaphore。permit 数来自 registry 的 `codex_permit_slots`：env 或 host `fkst.env` 可覆盖，未设置时默认 `20`。

`with_lock(name, fn)` 是跨 pipeline 互斥 primitive。它把校验后的锁名解析到 `<RT>/locks/<name>` 并打开，获取 exclusive flock，执行 Lua function，释放 file handle。进程死时 lock 自动释放。

`once(key, fn)` 是 best-effort per-key de-bounce primitive。它把校验后的 key 作为相对路径，在 `<RT>/locks/once/<key>` 上获取 exclusive flock 后检查 `<RT>/marks/<key>`；marker 存在则返回 `false`，不存在则执行 `fn`，成功后写 marker 并返回 `true`。marker 是 host-local scratch，不是 durable state；runtime root 被清空或换 host 后，`fn` 会重新运行，这由 at-least-once、下游 / package 幂等和从 durable 源重新推导来容纳。`fn` 失败时不写 marker，后续调用会重试。

`once` 决策通过 engine log 观察：`once decision=skip-marked key=...` 与 `once decision=ran-marked key=...` 提供可 grep trail。marker 内容只含人工提示（`key`、`marked_at`），不被解析；LIVE lock holder 用 `lsof <RT>/locks/once/<key>` 查看。

`cache_set(key, value)` / `cache_get(key)` 是 best-effort scratch KV primitive。它们把校验后的 key 作为相对路径读写 `<RT>/cache/<key>`；`cache_set` 用 temp file + rename 原子覆盖写入 string value，`cache_get` 命中返回 string，缺失返回 nil。cache 是 host-local scratch，不是 durable state；runtime root 被清空或换 host 后，`cache_get` 返回 nil，调用者必须从 durable source 重新推导。cache 操作内部不加锁，last-writer-wins；需要 read-compare-write 原子性时由调用者外层使用 `with_lock`。

## 12. Git 与 Worktree Primitives

`setup_worktree` 会创建 candidate branch。所有 git SDK 命令使用 `git -C <HOST> ...`，不依赖 framework launcher cwd。branch 前缀和 from separator 是 HostFact，来自 `FKST_CANDIDATE_PREFIX` / `FKST_CANDIDATE_FROM_SEP` 或 host `fkst.env`，缺失时 fail-closed。

具体 integration branch、candidate topology、host ref 命名、push/pull 策略属于 package/host，不是 substrate 固定事实。

## 13. 发布边界

fkst-substrate 的 accepted release state 是外部 release pipeline 的事实。推荐外部链路是 build → test → `--self-test` → conformance → 签名 artifact → deploy → canary / 回退策略。

engine 无 runtime accepted-state/回退，发布安全是外部策略。

⟦AI:FKST⟧

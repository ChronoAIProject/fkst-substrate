# fkst-substrate 引擎架构

本库是 universal SDLC substrate 的引擎 trusted base；业务 Lua package 不在本库内，由独立仓库或 host 通过 `FKST_PACKAGE_ROOT` / `--package-root` 注入。

本文只描述 fkst-substrate 当前引擎事实。源码权威在 `crates/`；`README.md` 说明抽取状态；`SPEC.md` 是身份锚点；业务部门拓扑、host 运行流程和具体研发策略不属于本文。

## 0. 仓库目录结构(本仓库实际文件)

```
fkst-substrate/
├── Cargo.toml / Cargo.lock                    workspace(仅 3 crate)
├── CLAUDE.md                                  引擎治理与哲学不动点
├── AGENTS.md  → CLAUDE.md                     软链
├── SPEC.md                                    Tier II 身份锚点
├── README.md                                  抽取状态 + 独立运行命令
├── crates/
│   ├── fkst-supervisor/src/main.rs            Tier I,进程根(≤150 LOC)
│   ├── fkst-common/src/
│   │   ├── lib.rs config.rs event.rs
│   │   ├── runtime_layout.rs                  RuntimeKind / RuntimeLayout
│   │   ├── error.rs validation.rs
│   └── fkst-framework/src/
│       ├── main.rs                            CLI: run / supervise / known-good / conformance / --self-test
│       ├── path_resolver.rs raise.rs mlua_init.rs
│       ├── sdk_basic.rs sdk_codex.rs sdk_fs.rs sdk_git.rs sdk_log.rs
│       ├── known_good.rs host_conformance.rs self_test.rs
│       └── supervise/
│           ├── mod.rs graph_scan.rs source_runner.rs
│           └── event_fanout.rs consumer.rs spawner.rs raised.rs
├── examples/
│   └── minimal-package/                       引擎自带最小示例包(证明 package-root 契约)
│       ├── tunables/
│       │   ├── queue_capacity.txt
│       │   ├── department_default_timeout.txt
│       │   └── codex_permit_slots.txt
│       ├── raisers/tick.lua                   cron source → 队列 tick
│       └── departments/
│           ├── producer/main.lua              consume tick;produce + fanout work;raise + file witness
│           ├── consumer_a/main.lua            consume work;写 witness
│           └── consumer_b/main.lua            consume work;写 witness
└── docs/architecture.md                       本文
```

业务 Lua package 不在本仓;`examples/minimal-package/` 是引擎自带的、唯一随仓发布的 package,用于证明 `--package-root` 能被独立加载并通过图/fanout 契约 validation,且单个 producer pipeline 可 `run` 起来发出 `RAISED`,不含任何业务语义。完整 `tick → 路由 → fanout → consumer witness` 端到端需常驻 `supervise`,其 bounded smoke 为 deferred(见 `README.md`)。host 注入的 package 用同样的 `departments/` + `raisers/` + `tunables/` 形状,经 `FKST_PACKAGE_ROOT` 指向。

## 1. 三层稳定性与三级公司

fkst-substrate 有两套正交分层:稳定性分层回答“什么多稳定”,公司结构回答“运行时谁组织谁”。

```
Tier I  supervisor
  crate: crates/fkst-supervisor
  role: process root
  Company 的最内核: spawn framework, wait, handle signals, reap children.

Tier II  SPEC + conformance
  files: SPEC.md, conformance assets when present
  role: identity anchor
  定义系统是什么,约束 Tier III 如何变化.

Tier III  framework + common + injected package graph
  crates: crates/fkst-framework, crates/fkst-common
  package: <PKG>/departments, <PKG>/raisers, <PKG>/fkst, <PKG>/tunables
  host: <HOST>/departments, <HOST>/raisers, host-owned config and assets
  role: event runtime, Lua SDK, package graph loader, subprocess boundary.
```

三级公司映射:

```
Company
  supervisor + fkst-framework supervise + composed graph.
  负责 source, queue, fanout, first-match routing, framework child spawn, RAISED parse.
  不做业务。

Department
  <PKG|HOST>/departments/<dept>/main.lua.
  暴露 M.spec 与 pipeline(event).
  M.spec 声明 consumes, produces, fanout, timeout.
  无生命周期 hook,无共享内存,无持久状态。

Person
  一次 codex exec 子进程。
  由 spawn_codex_sync 或 spawn_codex 创建,由 await_all join.
  无身份、无记忆、无直接通信。
```

`crates/fkst-common` 是共享类型层，包含 `Config`、`Event`、`RuntimeKind`、`RuntimeLayout`、validation error 等。`crates/fkst-supervisor` 不依赖它；supervisor 与 framework 只通过 OS 进程和 stdout/stderr 连接。这个边界让 Tier I 小而可审计。

## 2. Crate 依赖图

```
fkst-supervisor
  deps: tokio, nix, tracing, tracing-subscriber, anyhow
  no internal crate deps
  runtime edge: spawn fkst-framework supervise

fkst-common
  deps: serde, serde_json, thiserror, anyhow
  provides: Config, QueueDecl, RaiserDecl, DepartmentDecl, Event, RuntimeLayout, validation

fkst-framework
  deps: fkst-common, mlua, notify, tokio, ulid, base64, nix, serde, serde_json,
        tracing, tracing-subscriber, anyhow
  provides: CLI, graph scan, source runner, fanout, consumer, spawner, RAISED parser,
            Lua SDK, known-good, self-test, host conformance bridge
```

workspace 只包含这三个 crate。没有业务 Lua package crate,也没有 package manifest crate。

## 3. 概念分层树

记号:

`<RT>` = `FKST_RUNTIME_ROOT`  
`<PKG>` = package root,来自 `FKST_PACKAGE_ROOT` 或 `--package-root`  
`<HOST>` = host root,即 `--project-root` 或当前 repo root  

```
L0  supervisor, crates/fkst-supervisor
  引入概念:进程存活
  crate: tokio, nix, tracing, anyhow
  OS: spawn, waitpid, SIGINT, SIGTERM, process_group
  外部程序: fkst-framework
  读: cwd, FKST_FRAMEWORK_BIN, signals, child exit
  写: stdout/stderr 继承到调用方日志;不写 runtime 状态文件

L1  common, crates/fkst-common
  引入概念:事件、启动图、runtime path kind、schema validation
  crate: serde, serde_json, thiserror, anyhow
  OS: 无持久 I/O
  外部程序: 无
  读: Event timestamp 使用系统时间
  写: 无,纯类型和校验逻辑

L2a  framework CLI 与 root resolver
  files: main.rs, path_resolver.rs
  引入概念:run, supervise, known-good, conformance, self-test; package root + host root
  读: FKST_PACKAGE_ROOT, --package-root, --project-root, rejected env 检查
  写: CLI stdout/stderr;不写业务状态

L2b  graph scan 与 source runner
  files: supervise/graph_scan.rs, source_runner.rs
  引入概念:静态 behavior graph, cron, file_watch
  crate: mlua 用于求值 M.spec 与 raisers; notify 用于 file_watch
  读: <PKG>/departments/*/main.lua, <PKG>/raisers/*.lua,
      <HOST>/departments/*/main.lua, <HOST>/raisers/*.lua,
      <PKG>/tunables/*.txt, <HOST>/fkst.env, env defaults
  写: in-memory Config;file_watch glob 可解析 runtime:// 到 <RT>

L2c  fanout 与 routing
  files: event_fanout.rs, consumer.rs, raised.rs
  引入概念:queue, Vec<mpsc::Sender<Event>>, first-match consumer, RAISED stdout protocol
  crate: tokio mpsc, base64
  读: Config, source events, framework child stdout/stderr
  写: <RT>/logs/framework-child/<dept>-*.log,
      in-memory queue delivery,
      RAISED events re-injected into fanout

L2d  Lua bridge 与 SDK
  files: mlua_init.rs, sdk_basic.rs, sdk_codex.rs, sdk_fs.rs, sdk_git.rs, sdk_log.rs, raise.rs
  引入概念:bounded substrate API, codex as subprocess, fcntl permit pool, worktree resource
  crate: mlua, nix, ulid, base64
  OS: process_group, flock, killpg, file read/write
  外部程序: codex, git, /bin/sh
  读: event JSON, Lua source, files requested by package, git log/worktree state,
      <RT>/codex-permits, <RT>/locks
  写: <RT>/worktrees/<prefix>-<ULID>,
      <RT>/codex-permits/permit-*,
      <RT>/locks/*,
      codex log dir from FKST_RUNTIME_LOG_DIR or user log default,
      stdout RAISED line,
      file.write target chosen by package/host

L2e  known-good 与 lifecycle
  files: known_good.rs, host_conformance.rs, self_test.rs
  引入概念:accepted framework ref, health observation, rollback witness, self-test
  外部程序: git, cargo/bash through conformance where configured
  读: refs/known-good, integration ref, supervisor log when health gate configured,
      conformance/self-test/review evidence
  写: refs/known-good with reflog,
      <RT>/locks/known-good-health.lock,
      checkout target,
      stdout evidence lines

L3  injected Lua package, not stored in this repository
  引入概念:host/business SDLC behavior
  位置: <PKG>/departments, <PKG>/raisers, <PKG>/fkst, <PKG>/tunables,
       optional <HOST>/departments and <HOST>/raisers
  读写: 只能经 L2 SDK 触达 git, filesystem, locks, subprocesses, logs and RAISED
```

依赖方向只能向内。supervisor 不知道 event；common 不知道 Lua；framework 不知道业务轮次、业务关卡或判断策略；package 只能通过固定 SDK 请求引擎能力。

## 4. Runtime I/O 与落点

`RuntimeKind` 固定七类:

| RuntimeKind | 目录 | 用途 |
|---|---|---|
| `Pipeline` | `<RT>/pipeline` | package/host 可用的 pipeline artifact 落点 |
| `Mailbox` | `<RT>/mailbox` | package/host 可用的 mailbox artifact 落点 |
| `EvolveRequests` | `<RT>/evolve-requests` | durable intent / request 文件落点 |
| `Worktrees` | `<RT>/worktrees` | `setup_worktree` 创建隔离 worktree |
| `CodexPermits` | `<RT>/codex-permits` | `permit-*` fcntl codex 并发池 |
| `Locks` | `<RT>/locks` | `with_lock` 与 known-good health lock |
| `Logs` | `<RT>/logs` | framework child logs;package 也可选择写本地 logs |

`RuntimeLayout::runtime_path(kind, relative)` 拒绝 parent traversal 和绝对 relative path。`runtime://` glob 解析只允许 file_watch 映射到 runtime kind；`runtime://logs` 是 local-only,不能作为 file_watch 输入。

Codex SDK 的日志目录不是 `RuntimeKind::Logs` 的唯一来源。它优先用 `FKST_RUNTIME_LOG_DIR`,否则落到用户日志目录，如 macOS `~/Library/Logs/fkst/codex` 或 `~/.local/state/fkst/codex`。这是过程日志，不是业务状态。

## 5. Package Root 与 Host Root

`PackageRoots::resolve` 产生一个 package root 和一个 host root:

```
--package-root <path> 优先
否则 FKST_PACKAGE_ROOT
host root 来自 --project-root 或 run 模式下 Lua 路径推断
```

如果 `<PKG> == <HOST>`,graph root 只有一个 `PackageAndHost`。否则扫描顺序是 package root 后 host root。重复 Department 名或 Raiser 名直接拒绝启动。`package.lua` 是被移除的 surface,存在即拒绝启动。

合法 graph 输入(`<dept>` / `*.lua` 为通配;本仓内的具体实例是 `examples/minimal-package/`):

```
<PKG>/departments/<dept>/main.lua      e.g. examples/minimal-package/departments/producer/main.lua
<PKG>/raisers/*.lua                    e.g. examples/minimal-package/raisers/tick.lua
<HOST>/departments/<dept>/main.lua     (host 注入,本仓不含)
<HOST>/raisers/*.lua                   (host 注入,本仓不含)
```

每个 Department `main.lua` 必须 return table,其中 `M.spec` 至少能解析为:

```
{
  consumes = {...},
  produces = {...},
  fanout = {...},
  timeout = "30s"
}
```

每个 Raiser lua 文件 return 一个 source declaration,当前只支持:

```
{ type = "cron", interval = "10s", produces = "queue" }
{ type = "file_watch", glob = "path-or-runtime://...", produces = "queue" }
```

队列不是 manifest 手写对象，而是从 Department consumes/produces 与 Raiser produces 的并集推导。capacity 来自 `FKST_QUEUE_CAPACITY`、`<HOST>/fkst.env` 或 `<PKG>/tunables/queue_capacity.txt`。Department 默认 timeout 和 codex permit slots 同理来自 env、`fkst.env` 或 package tunable。

## 6. 运行态数据流

机制层数据流:

```
source
  cron tick or file_watch event
    ↓
Fanout::send(queue, Event)
    ↓
Vec<mpsc::Sender<Event>>
    ↓
consumer inbox
    ↓
spawn fkst-framework run <department main.lua> --event <json>
    ↓
single Lua state + pipeline(event)
    ↓
SDK calls: file/git/lock/worktree/exec/codex/log/now/raise
    ↓
optional stdout line: RAISED: <base64-url-json>
    ↓
parse_raised from stdout tail
    ↓
Fanout::send(raised.queue, raised_event)
```

`consumer.rs` 为每个 Department 的每个 consumed queue 建 receiver,再汇入该 Department 的 inbox。每个事件 spawn 一个 framework child,不是在 supervisor 进程内直接调用 Lua。framework child 的 stdout/stderr 会写到 `<RT>/logs/framework-child/` 下的具名 log；RAISED 解析不依赖 log 文件，而是解析 captured stdout。

`raise` 不落盘。需要 durable intent 时，package/host 必须显式写文件到 `<RT>/evolve-requests`、`<RT>/pipeline`、`<RT>/mailbox` 或其它约定落点，再由 `file_watch` 或 scanner 重新引入事件。

## 7. SDK Surface

固定 surface:

| Surface | 当前实现 |
|---|---|
| `pipeline(event)` | Department Lua 入口约定 |
| `source` | Raiser Lua return declaration 约定 |
| `raise(queue, payload)` | `raise.rs`,进程内 buffer,退出 stdout `RAISED:` |
| `spawn_codex_sync(opts)` | `sdk_codex.rs`,同步 `codex exec` |
| `spawn_codex(opts)` | `sdk_codex.rs`,返回 pipeline-local handle |
| `await_all(handles)` | `sdk_codex.rs`,join handles,防跨 pipeline/重复消费 |
| `exec_sync(cmd|opts)` | `sdk_basic.rs`,运行 `/bin/sh -c`,可选 cwd/env/timeout |
| `with_lock(path, fn)` | `sdk_git.rs`,fcntl exclusive flock |
| `git_log_count(grep, since)` | `sdk_git.rs`,调用 `git log --grep --since --oneline` |
| `git_log_grep(grep, since)` | `sdk_git.rs`,调用 `git log --format=%H` |
| `count_worktrees()` | `sdk_git.rs`,解析 `git worktree list --porcelain` |
| `list_orphan_worktrees(prefix)` | `sdk_git.rs`,列 `<RT>/worktrees/<prefix>*` linked worktree |
| `setup_worktree(prefix)` | `sdk_git.rs`,创建 `<RT>/worktrees/<prefix>-<ULID>` 和 candidate branch |
| `file.read/write/exists` | `sdk_fs.rs` |
| `log.info/warn/error` | `sdk_log.rs`,写 stderr |
| `now()` | `sdk_basic.rs`,Unix seconds |

`spawn_codex` 的 handle 只能由同一个 Lua pipeline 的 `await_all` 消费。单 handle 等待用 `await_all({handle})`。没有 sleep timer、first-result fanout、业务 retry、provider abstraction 或动态 SDK extension。

`exec_sync` 的 timeout 会把 shell child 放入 process group,超时后 kill process group 并返回 `exit_code = 124` 与 `timed_out = true`。Codex stall window 语义不同:它是 no-output stall window,有 stdout/stderr 活动就继续等待自然退出。

## 8. 事件与队列机制

`Config` 包含 `queue`、`raiser`、`department`、`limits`。Queue 有 `capacity` 与 `fanout`。Raiser 只有 `Cron` 与 `FileWatch`。Department 有 `lua`、`consumes`、`produces`、`timeout`。

validation 规则:

- `limits.global_codex_processes > 0`
- 每个 queue `capacity > 0`
- 每个 Raiser produces 必须引用已声明 queue
- 每个 Department consumes/produces 必须引用已声明 queue
- Department lua 文件必须存在
- timeout 必须以 `s`、`m` 或 `h` 结尾
- queue 不能孤立
- 非 fanout queue 不能有多个 consumer
- Department consume 和 produce 同一 queue 时，该 queue 必须 fanout

`M.spec.fanout` 的语义是 queue contract,不是广播主题。声明者必须在同一 `M.spec` 的 `consumes` 或 `produces` 中引用该 queue；否则 graph scan 拒绝。fanout queue 的物理实现仍是 `Vec<mpsc::Sender<Event>>`,只是 validator 允许多个 active consumer。

`Fanout::send` 使用 `try_send`。`Full` 或 `Closed` 只影响该 consumer,并写 warn。unknown queue warn 后 drop。发送本身保持 best-effort。

## 9. RuntimeKind 目录

`RuntimeLayout` 只接受 `FKST_RUNTIME_ROOT` 或显式 root 构造。root 不能空，不能包含 parent traversal。每个 kind 的目录名固定:

```
pipeline
mailbox
evolve-requests
worktrees
codex-permits
locks
logs
```

`RuntimeKind::parse` 接受 underscore 和 dash 的外部写法:如 `evolve_requests` / `evolve-requests`, `codex_permits` / `codex-permits`。内部落盘目录使用 dash。

典型写入者:

| 落点 | 写入者 |
|---|---|
| `<RT>/worktrees/<prefix>-<ULID>` | `setup_worktree` |
| `<RT>/codex-permits/permit-*` | `sdk_codex::ensure_pool` |
| `<RT>/locks/*` | `with_lock`, known-good health lock |
| `<RT>/logs/framework-child/*.log` | `supervise::spawner` |
| `<RT>/pipeline`, `<RT>/mailbox`, `<RT>/evolve-requests` | injected package/host 经 `file.*` 或外部工具写入 |

引擎不会为 package 定义 pipeline/mailbox/evolve-requests 的业务 schema。它只提供 bounded path categories 和 source/file_watch 机制。

## 10. Git Ref Namespace 与 Known-good

引擎层固定承认:

```
refs/known-good
```

`known_good.rs` 负责读取 integration ref、确认 candidate 是 old known-good 的 descendant、要求 conformance/self-test/review evidence、获取 `<RT>/locks/known-good-health.lock`、`git update-ref --create-reflog` 推进 `refs/known-good`、checkout detached candidate、观察健康窗口，并在失败时写 rollback witness。

`KNOWN_GOOD_REF` 是 ref,不是 branch。它表示当前 accepted framework state。推进动作使用 CAS:old sha 必须匹配,否则报告 update-ref conflict。

`setup_worktree` 会创建 candidate branch。branch 前缀和 from separator 来自 `FKST_CANDIDATE_PREFIX` / `FKST_CANDIDATE_FROM_SEP` 或 package tunables。具体 integration branch、candidate topology、runtime hidden refs、push/pull 策略属于 package/host,不是 substrate 固定事实。

## 11. 并发与进程边界

supervisor 使用 current-thread tokio runtime,spawn `fkst-framework supervise` 并把 stdout/stderr 继承出去。收到 interrupt/terminate 时 supervisor 返回对应 exit code,不 signal event runtime。它最后 best-effort reap children。

supervise 运行在 current-thread tokio runtime 内,但每个 Department event 都会 spawn 一个 framework child process。framework child 是新的 process group leader。stall window 内无 stdout/stderr 输出时，supervise 对 `-pgid` 发送 `SIGKILL`,使 framework child 及其 codex 子孙一起退出。

Codex SDK 也把 `codex exec` 放入 process group。stall 时 kill process group。permit 池使用 fcntl lock file,不是内存 semaphore。默认 permit 数是 `20`,但 supervise 会通过 `FKST_CODEX_PERMIT_SLOTS` 把 host graph defaults 传给 framework child。

`with_lock(path, fn)` 是跨 pipeline 互斥 primitive。它打开 path,获取 exclusive flock,执行 Lua function,释放 file handle。进程死时 lock 自动释放。

## 12. Codex 调用契约

Codex 命令固定:

```
codex exec --dangerously-bypass-approvals-and-sandbox [--context <context>] [-C <worktree>] -
```

prompt 写入 stdin。写完后 stdin handle 被 drop,EOF 是请求结束。stdout/stderr 由 reader thread 持续读取,任何输出都会刷新 no-output stall window。

结果表包含:

```
stdout
stderr
exit_code
log_path
error_kind  -- only on framework-classified failure
error       -- only on framework-classified failure
```

log 内容必须包括 stdout、stderr、`EXIT=...`、`DONE_AT=...`、`CMD=...`、`STALL_WINDOW=...`。permit、spawn、stdin、wait、stall 等失败路径也必须写同一个 log path。

## 13. RAISED 协议

`raise(queue, payload)` buffer 的 entries 在 pipeline 退出前序列化为:

```
RAISED: <base64-url-encoded JSON [{queue, payload}, ...]>
```

`parse_raised` 从 stdout 最后向前找最后一行 `RAISED: `。多行时最后一行 wins。没有 RAISED 返回空列表。base64 或 JSON malformed 时 warn 并返回空,不让 supervisor crash。普通 log 行里包含 `RAISED:` 不会被误判,因为 parser 要求行首匹配。

## 14. 不属于引擎的内容

fkst-substrate 不包含:

- 具体业务 Department 名单或拓扑
- 具体 package 的策略部门
- 具体 package 的特殊 inbox 接线或演化通路
- host 代码托管流程、外部协调器或自演化 SOP
- package manifest DSL 或 package dependency graph
- web dashboard
- host runtime cleanup policy 的业务章节

这些可以由独立 package 或 host 构建在 SDK 之上，但不能写入 framework Rust 概念层。引擎只承认机制，不承认某个业务 package 的组织事实。

## 15. 设计边界摘要

引擎可变部分必须保持以下闭包:

- 事实源闭包:git refs + filesystem + fcntl locks + logs,无持久状态文件。
- 图闭包:package root + host root 的固定 `departments/` 与 `raisers/`,无 runtime dynamic registration。
- SDK 闭包:固定 Lua surface,新增函数需测试和 conformance。
- 进程闭包:supervisor -> framework child -> codex child,process group kill 只用于 stall/timeout。
- package 闭包:Lua/L3 来自独立 package,本库不携带业务 package。
- 概念闭包:Rust framework 不知道业务轮次、业务关卡、判断策略、重试策略或退避策略。

只要一个设计需要在这些闭包之外增加第二事实源、第二 dispatcher、第二状态层或业务概念下沉,它就不是 fkst-substrate 的合格引擎设计。

⟦AI:FKST⟧

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
│       ├── host_conformance.rs self_test.rs
│       └── supervise/
│           ├── mod.rs graph_scan.rs source_runner.rs
│           └── event_fanout.rs consumer.rs spawner.rs raised.rs
├── examples/minimal-package/
└── docs/architecture.md
```

`examples/minimal-package/` 是引擎自带的最小 reconciliation/control-loop 示例包，用于证明 `--package-root` 能被独立加载、通过图 validation，并且 scanner/worker pipeline 可独立 `run`。它演示事件瞬时、source→scanner→raise→worker→结构化日志、崩溃后由 cron/file_watch 重新扫描再推导；真实包的完成事实来自 git commit / 外部源 / 明确 host fact。

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
fkst-framework --self-test
```

`fkst-supervisor` 没有业务子命令；它只把当前目录作为 host root，启动 `fkst-framework supervise`。

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

每个 Department `main.lua` 必须 return table，其中 `M.spec` 至少能解析为：

```lua
{
  consumes = {...},
  produces = {...},
  fanout = {...},
  timeout = "30s"
}
```

每个 Raiser lua 文件 return 一个 source declaration，当前只支持：

```lua
{ type = "cron", interval = "10s", produces = "queue" }
{ type = "file_watch", glob = "host-root-relative/or/absolute/path/*.md", produces = "queue" }
```

`file_watch.glob` 可以是 host root 相对路径或绝对路径；相对路径由 engine 锚到 host root。它用于监听 host repo durable 文件或外部同步到 host 的文件，不支持已移除的 runtime scheme。

## 5. Engine Operation Registry

引擎操作 knob 统一由 `config_registry.rs` 的静态 typed registry 声明，并通过显式 host root 构造的 `ConfigContext` 解析。读取优先级是 process env → host `fkst.env` → operational 默认。registry 不读 cwd、`tunables/*.txt`，也没有 set/write/dynamic registration、YAML、DSL、manifest、plugin 或 dashboard 入口。

当前 registry 只有 5 项：

| name | env key | kind | type | default / required |
|---|---|---|---|---|
| `queue_capacity` | `FKST_QUEUE_CAPACITY` | Operational | `usize` | default `16` |
| `department_default_timeout` | `FKST_DEPARTMENT_DEFAULT_TIMEOUT` | Operational | duration string | default `30s` |
| `codex_permit_slots` | `FKST_CODEX_PERMIT_SLOTS` | Operational | `usize` | default `20` |
| `candidate_prefix` | `FKST_CANDIDATE_PREFIX` | HostFact | string | required |
| `candidate_from_sep` | `FKST_CANDIDATE_FROM_SEP` | HostFact | string | required |

`fkst-framework config --project-root <path> [--package-root <path>]` 是只读自省命令，逐项打印 env key、kind、type、default/required、resolved value/source 与 doc。HostFact 缺失时显示缺失，不会写配置或访问网络。

## 6. Runtime I/O 与落点

`RuntimeKind` 固定 4 类。它们都是一轮运行的一次性 scratch 落点，不是 package 状态层，也不是 durable 真相源。`<RT>` 表示引擎 runtime root，相对值会锚到 `<HOST>`。

| RuntimeKind | 落点 | 用途 | 写入者(engine) | 读取者 |
|---|---|---|---|---|
| `Worktrees` | `<RT>/worktrees` | 隔离 worktree | `sdk_git::setup_worktree`(`git worktree add`) | `count_worktrees` / `list_orphan_worktrees` |
| `CodexPermits` | `<RT>/codex-permits` | `permit-*` fcntl codex 并发池 | `sdk_codex`(建池 + flock 占位) | `spawn_codex` 抢 permit |
| `Locks` | `<RT>/locks` | fcntl 锁文件 | `sdk_git::with_lock` | 同 — 跨 pipeline 互斥 |
| `Logs` | `<RT>/logs` | 过程日志 | `supervise::spawner`(framework-child;dept `log.*` 经 stderr 捕获于此) | 人手 / 调试,非 file_watch 输入 |

说明:
- **engine 自己只写 scratch 结构事实**(worktree / permit / lock / log)。package 不访问 `<RT>`，也不把 `<RT>` 当 inbox、完成态或业务 schema 数据库。
- `RuntimeLayout` 只提供固定 runtime dir 解析，framework 先把相对 runtime root 锚到 `<HOST>` 再建路径。
- 已移除的 runtime scheme 不再是 graph surface；`file_watch` 只接受 host-root 相对或绝对 glob。
- codex log **不属** `RuntimeKind`/`<RT>`:`sdk_codex` 把它落到 `FKST_RUNTIME_LOG_DIR` 或平台默认目录(如 `~/Library/Logs/fkst`)下的 `codex/`。它与 `<RT>/logs` 同属 process-trace scratch(可 grep、非事实源),但落点不同,`supervise` 也不给 framework child 注入 `FKST_RUNTIME_LOG_DIR`。
- engine **不写** runtime 持久状态(无 `refs/known-good` / accepted-state / rollback —— 那是外部 release pipeline 的事实,见 §12)。

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
SDK calls: file/git/lock/worktree/exec/codex/log/now/raise
    ↓
optional stdout line: RAISED: <base64-url-json>
    ↓
parse_raised from stdout tail
    ↓
Fanout::send(raised.queue, raised_event)
```

`consumer.rs` 为每个 Department 的每个 consumed queue 建 receiver，再汇入该 Department 的 inbox。每个事件 spawn 一个 framework child，不是在 supervisor 进程内直接调用 Lua。framework child 的 stdout/stderr 会写到 `<RT>/logs/framework-child/` 下的具名 log；dept 的 `log.*` 以结构化行写 stderr，并由这个具名 log 捕获。RAISED 解析不依赖 log 文件，而是解析 captured stdout。

`raise` 不落盘。需要 durable intent 或完成事实时，package/host 必须显式写入 git commit、host repo 文件或外部源，再由 package controller 通过 `file_watch` 或 scanner 重新引入事件。

## 8. Reconciliation / Control-loop 事件模型

内存队列是瞬时队列。它只存在于当前 `fkst-framework supervise` 进程和 supervisor 生命周期内；进程挂掉、supervisor 重启或 host 迁移时队列内容丢失。engine 不把 queue 当 durable message state,不跨机同步队列,也不引入 MQ broker。

durable 真相来自可观测事实：git commit、明确的 host filesystem fact 与外部源（例如 GitHub issue）。真正跨机或长期保留的完成事实应进入 git commit 或外部源。`<RT>` 只是一轮运行的一次性 scratch，`locks` 也不是 durable 真相——`with_lock` 只是进程死即释放的**处理中租约/协调事实**，不承载完成态。engine 只提供 `file`、`file_watch`、cron、git/worktree 和 `with_lock` 等原语，不拥有 scanner 部门、inbox schema、done 判定、重试策略或幂等语义。

恢复模型是 control-loop：package controller 用 cron / file_watch scanner 扫描 durable 源，reconcile 未完成工作，并重新 enqueue 对应事件。崩溃等价于从 0 重来；in-flight 事件丢失后，下一拍从 durable 源重新推导。幂等由 package controller 保证；engine 只负责把重新 raised / scanned 的事件送入当前内存队列。

engine 不维护消息状态。`处理中` 可以是 `with_lock` 租约（进程死后 fcntl lock 自动释放）或 worktree 等可观测事实；`done` 是 commit、明确的 host filesystem fact 或外部源事实。engine 明确不提供 message state、ack / visibility timeout、dead-letter queue、状态队列或 durable broker。

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
| `git_log_count(grep, since)` | `sdk_git.rs`，调用 `git log --grep --since --oneline` |
| `git_log_grep(grep, since)` | `sdk_git.rs`，调用 `git log --format=%H` |
| `count_worktrees()` | `sdk_git.rs`，解析 `git worktree list --porcelain` |
| `list_orphan_worktrees(prefix)` | `sdk_git.rs`，列 `<RT>/worktrees/<prefix>*` linked worktree |
| `setup_worktree(prefix)` | `sdk_git.rs`，创建 `<RT>/worktrees/<prefix>-<ULID>` 和 candidate branch |
| `file.read/write/exists` | `sdk_fs.rs` |
| `log.info/warn/error` | `sdk_log.rs`，结构化行写 stderr，由 supervise 捕获进 framework-child log |
| `now()` | `sdk_basic.rs`，Unix seconds |

`spawn_codex` 的 handle 只能由同一个 Lua pipeline 的 `await_all` 消费。单 handle 等待用 `await_all({handle})`。没有 sleep timer、first-result fanout、业务 retry、provider abstraction 或动态 SDK extension。

## 10. 事件与队列机制

`Config` 包含 `queue`、`raiser`、`department`、`limits`。Queue 有 `capacity` 与 `fanout`。Raiser 只有 `Cron` 与 `FileWatch`。Department 有 `lua`、`consumes`、`produces`、`timeout`。

validation 规则：

- `limits.global_codex_processes > 0`
- 每个 queue `capacity > 0`
- 每个 Raiser produces 必须引用已声明 queue
- 每个 Department consumes/produces 必须引用已声明 queue
- Department lua 文件必须存在
- timeout 必须以 `s`、`m` 或 `h` 结尾
- queue 不能孤立
- 非 fanout queue 不能有多个 consumer
- Department consume 和 produce 同一 queue 时，该 queue 必须 fanout

`M.spec.fanout` 的语义是 queue contract，不是广播主题。声明者必须在同一 `M.spec` 的 `consumes` 或 `produces` 中引用该 queue；否则 graph scan 拒绝。fanout queue 的物理实现仍是 `Vec<mpsc::Sender<Event>>`，只是 validator 允许多个 active consumer。

## 11. 并发与进程边界

supervisor 使用 current-thread tokio runtime，spawn `fkst-framework supervise` 并把 stdout/stderr 继承出去。收到 interrupt/terminate 时 supervisor 返回对应 exit code，不 signal event runtime。它最后 best-effort reap children。

supervise 运行在 current-thread tokio runtime 内，但每个 Department event 都会 spawn 一个 framework child process。framework child 是新的 process group leader。stall window 内无 stdout/stderr 输出时，supervise 对 `-pgid` 发送 `SIGKILL`，使 framework child 及其 codex 子孙一起退出。

Codex SDK 也把 `codex exec` 放入 process group。stall 时 kill process group。permit 池使用 fcntl lock file，不是内存 semaphore。permit 数来自 registry 的 `codex_permit_slots`：env 或 host `fkst.env` 可覆盖，未设置时默认 `20`。

`with_lock(name, fn)` 是跨 pipeline 互斥 primitive。它把锁名解析到 `<RT>/locks/<name>` 并打开，获取 exclusive flock，执行 Lua function，释放 file handle。进程死时 lock 自动释放。

## 12. Git 与 Worktree Primitives

`setup_worktree` 会创建 candidate branch。所有 git SDK 命令使用 `git -C <HOST> ...`，不依赖 framework launcher cwd。branch 前缀和 from separator 是 HostFact，来自 `FKST_CANDIDATE_PREFIX` / `FKST_CANDIDATE_FROM_SEP` 或 host `fkst.env`，缺失时 fail-closed。

具体 integration branch、candidate topology、host ref 命名、push/pull 策略属于 package/host，不是 substrate 固定事实。

## 13. 发布边界

fkst-substrate 的 accepted release state 是外部 release pipeline 的事实。推荐外部链路是 build → test → `--self-test` → conformance → 签名 artifact → deploy → canary / 回退策略。

engine 无 runtime accepted-state/回退，发布安全是外部策略。

⟦AI:FKST⟧

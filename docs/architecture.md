# fkst-substrate 架构(引擎)

本文是 fkst **引擎**的结构地图:进程与层级如何映射、依赖如何从最小核向外分层、每层的对外资源与运行时 I/O、事件如何流转。本仓库只含引擎(trusted base);业务部门 / raiser / stdlib 是**独立的 Lua package**,经 `FKST_PACKAGE_ROOT` 注入,本库不含。invariant 权威定义见 `SPEC.md`,治理见 `CLAUDE.md`。

---

## 1. 三层稳定性 ↔ 三级公司

引擎用两套正交三分法描述自己:**三层稳定性**(按演化频率)与**三级公司**(按组织结构)。

```
┌───────────────────────────────────────────────────────────────────────────┐
│ Tier I  不可变 · ≤150 LOC · crates/fkst-supervisor/src/main.rs              │
│   supervisor 进程(launchd 拉起):spawn framework / 转发信号 / 记录 exit。   │
│   不做业务。对应 Level 1 进程根。                                            │
└───────────────────────────────────┬───────────────────────────────────────┘
                                     │ spawn(独立进程组,单次跑完即退)
                                     ▼
┌───────────────────────────────────────────────────────────────────────────┐
│ Tier III · Rust · auto-evolved · crates/fkst-framework/                     │
│   main.rs            CLI: run / supervise / known-good / --self-test         │
│   supervise/         graph_scan → source_runner → event_fanout              │
│                      → consumer → spawner → raised(RAISED: 协议)            │
│   mlua_init.rs       单 Lua state + 协程调度器                               │
│   sdk_basic/codex/fs/git/log.rs   暴露给 Lua 的固定 SDK surface              │
│   raise.rs           raise(q,payload):进程内 buffer,退出时 stdout 发 RAISED │
│   known_good.rs      refs/known-good CAS + 健康观察 + rollback                │
│   update.rs          framework 自 swap                                       │
│   host_conformance.rs / self_test.rs   改动合规 + conformance 触发           │
│   path_resolver.rs   解析 package root / host root / RuntimeLayout           │
│   (依赖 fkst-common: config / event / runtime_layout / validation)          │
└──────────────┬──────────────────────────────────────┬──────────────────────┘
               │ 路由 first-match                      │ SDK 调用(协程 yield 点)
               ▼                                        ▼
┌──────────────────────────────────────┐   ┌─────────────────────────────────┐
│ Tier III · Lua · 注入的 package graph │   │ Level 3 · codex CLI 子进程        │
│  (本库不含;FKST_PACKAGE_ROOT 注入)   │   │   spawn_codex_sync 起一个         │
│   departments/<X>/main.lua            │   │   codex exec,做一件事即退;        │
│   raisers/<X>.lua                     │   │   无记忆;实例间不通信。           │
│   Level 2 部门是唯一业务单元           │   └─────────────────────────────────┘
└──────────────────────────────────────┘

┌───────────────────────────────────────────────────────────────────────────┐
│ Tier II · fix-point · SPEC.md + conformance(gate 集随 package/upstream)     │
└───────────────────────────────────────────────────────────────────────────┘
```

构建产物:`fkst-supervisor`、`fkst-framework` 两个 binary。`fkst-common` 是被两者依赖的 lib。三级公司不变量:不能加层;Level 2 间无共享内存;Level 3 间无直接通信;Level 1 不做业务;部门无状态。

---

## 2. 依赖与概念分层(最小核 → 外层)

### 2.1 Crate 依赖图(编译期硬依赖)

```
   fkst-supervisor (Tier I, ~103 LOC)        fkst-framework (Tier III)
   deps: tokio nix tracing anyhow             deps: mlua notify tokio ulid
   ✗ 不依赖 fkst-common                            base64 nix serde tracing
   ✗ 不依赖 fkst-framework                              │
                                                        ▼
                                                 fkst-common (lib)
                                                 deps: serde serde_json
                                                       thiserror anyhow
```

**supervisor 零内部依赖**:不 link `fkst-common`,也不 link `fkst-framework`,只在运行时 `spawn(framework_bin)`。两 binary 经**进程边界 + stdout `RAISED:` 协议**通信,不经 Rust 类型耦合。换掉整个 framework,supervisor 一行不用改。

### 2.2 概念分层树(每层:概念 + 对外资源 + 运行时 I/O)

记号:`<RT>`=`FKST_RUNTIME_ROOT`、`<PKG>`=package root、`<HOST>`=host root。每节点 `[读←]` 输入、`[写→]` 产物落点。**依赖向内、概念向外、资源向外、演化频率向外**——四者同向,是可审计性的结构根源。

```
L0  Tier I supervisor  ── crates/fkst-supervisor
│   概念:进程存活(liveness)—— 起 framework 子进程,等它,转发信号;不杀在飞工作
│   crate: tokio·nix·tracing·anyhow   OS: spawn·waitpid·SIGINT/SIGTERM   外部程序: framework binary
│   读← env FKST_FRAMEWORK_BIN · 信号 · 子进程 exit code
│   写→ 自身 stdout/stderr(launchd 重定向)
│
├─ L1  共享词汇  ── crates/fkst-common
│   │   概念:把系统名词变类型(event / config / runtime path / 具名 error)
│   │   crate: serde·serde_json·thiserror·anyhow   注:supervisor 都不需要它
│   │   读← 系统时钟(Event.ts)   写→ 无 —— 纯数据类型,零文件 I/O
│   │
│   └─ L2  framework 引擎  ── crates/fkst-framework(依赖 L1;新增 mlua·notify·tokio·ulid·base64·nix)
│       ├ L2a 基座      path_resolver · raise
│       │   概念:事实寻址 + at-most-once 意图   读← env 三个 root(只算路径)
│       │   写→ raise 经 stdout 发 RAISED(不落盘)
│       ├ L2b 调度引擎  supervise/{graph_scan,source_runner,event_fanout,consumer,spawner,raised}
│       │   概念:事件循环 source→fanout→route→spawn→raise   crate tokio(mpsc)·notify
│       │   读← <PKG>/departments/*/main.lua · <PKG>/raisers/*.lua · <HOST> 同名(建图)· inotify 监听 · 子进程 stdout
│       │   写→ <RT>/logs/framework-child/<dept>(部门进程日志);spawn `fkst-framework run <lua>`(setsid)
│ ═════ 以下需 Lua(mlua);以上是纯 Rust 骨架 ═════
│       ├ L2c Lua 桥 + SDK  mlua_init · sdk_basic · sdk_codex · sdk_fs · sdk_git · sdk_log
│       │   概念:substrate API —— Lua 能调用的有界原语集(trusted base 边界)
│       │   crate mlua·nix(flock/killpg)·ulid   外部程序 codex exec·git·/bin/sh
│       │   读← Lua 指定文件 · git · permit 池
│       │   写→ <RT>/worktrees/<prefix>-<ULID>/ + refs/heads/<candidate>  ← setup_worktree
│       │       <RT>/codex-permits/{0..N}(flock)  ← sdk_codex
│       │       <RT>/locks/*(with_lock fcntl)     ← sdk_git
│       │       <RT>/logs/<codex-log>             ← sdk_codex
│       │       任意 file.write 落点(经 runtime_path 解析)← sdk_fs ; stderr ← sdk_log
│       └ L2d 生命周期    known_good · update · host_conformance · self_test
│           概念:自演化接管 —— 新 framework 经 conformance + 健康观察自我替换
│           外部程序 git(ref CAS)·bash(conformance)
│           读← refs/known-good · conformance 结果   写→ refs/known-good(CAS)· $prefix install(swap)
│
└─ L3  Lua 层(注入的 package;本库不含;所有文件 I/O 都经 L2c SDK)
    概念:substrate 惯用法 + SDLC 业务语义;business 概念的唯一合法栖息地,严禁下沉到 L2/L1/L0
    读/写:经 SDK 访问 <RT>/{mailbox,pipeline,evolve-requests} + git;落点由 runtime_path(kind) 解析
```

### 2.3 抽掉 Lua,引擎还剩什么

`mlua` 只被 framework 的 `mlua_init / sdk_* / raise / graph_scan / self_test` 用。分界线以上(L0 + L1 + L2a + L2b)是纯 Rust 骨架。抽掉 Lua 剩下的是**一个被监督、能 spawn 子进程、inotify 监听、cron 发拍、mpsc 扇出、管 git ref/worktree/fcntl 锁/known-good 的进程引擎骨架**——但它没有图、没有 handler、不会起 codex,因为图定义、handler、`spawn_codex` 调用都在 Lua(package)层。**Lua(经 mlua)是把"引擎骨架"变成"会做事的系统"的那层**:骨架定义能怎么运转,package 定义运转成什么。

### 2.4 每层资源矩阵

| 层 | 引入概念 | crate | OS 原语 | 外部程序 | 事实源 | 频率 |
|---|---|---|---|---|---|---|
| L0 supervisor | 进程存活 | tokio nix | spawn/waitpid/signal | framework | 无 | 十年级 |
| L1 common | 名词类型化 | serde thiserror | 系统时钟 | 无 | 无 | 年级 |
| L2a 基座 | 事实寻址 | base64 | — | — | fs | 月级 |
| L2b 引擎 | 事件循环 | tokio notify | setsid | framework run | fs · stdout 管道 | 月级 |
| L2c SDK | substrate API | **mlua** nix ulid | flock · killpg | codex · git · sh | fs · git · 锁 | 月级 |
| L2d 生命周期 | 自演化接管 | nix | flock | git · bash | git refs · fs | 月级 |
| L3 package | substrate 惯用法 + SDLC 语义 | (经 SDK) | — | codex·git(间接) | (经 SDK) | 小时级 |

---

## 3. 运行态数据流(单次事件,机制层)

```
external trigger            framework dispatcher              department / codex
cron/file_watch/raise ─▶  graph_scan(建图,启动一次)
                          source_runner(cron/file_watch 触发 event)
                          event_fanout(Vec<mpsc::Sender>)
                          consumer(按 type first-match 路由)
                          spawner(setsid 起部门进程)
                                    │
                                    ▼
                          departments/<dept>/main.lua :: pipeline(event)
                            · spawn_codex_sync / await_all / with_lock
                            · git commit(部门档案)  · raise(可选)
                                    │
                                    ▼
                          stdout: `RAISED: <base64-json>` → dispatcher 重注入
```

退出物只两种:`git commit`(档案)与可选 `raise`(寄信)。pipeline 在协程内跑到完成即清理。

**事件流转三种传输**(按跨不跨进程/时间):
1. 同进程同调度周期 → 内存 `tokio::sync::mpsc` channel(不碰文件系统)。
2. pipeline → dispatcher 回交 → stdout `RAISED:` 管道(进程边界,ephemeral,父死即丢)。
3. 跨 pipeline / 进程 / 重启 → 文件系统(inbox/mailbox 文件)+ git commit,由 file_watch / cron scanner 重新发现重放(durable)。

文件系统不是事件总线,是**持久化兼恢复底座**:要活过重启的事件必须先变成事实(文件/commit),scanner 再转回事件。

---

## 4. SDK surface(暴露给 Lua 的固定接口)

| 分组 | 函数 |
|---|---|
| 声明 | `pipeline` · `source` |
| 事件 | `raise` |
| codex | `spawn_codex_sync` · `spawn_codex`(handle 由 `await_all` join) |
| 外部命令 | `exec_sync` |
| 协程协调 | `await_all` |
| 跨 pipeline 协调 | `with_lock`(fcntl,进程死自动释放) |
| 事实派生(只读 git) | `git_log_count` · `git_log_grep` · `count_worktrees` · `list_orphan_worktrees` |
| worktree | `setup_worktree`(自动加 ULID 后缀 + branch) |
| 文件 | `file`(read/write/exists) |
| 日志 | `log.info` · `log.warn` · `log.error`(→ stderr) |
| 时间 | `now` |

framework 内部 surface(mlua context、协程 scheduler、inotify、subprocess lifecycle、信号、supervisor 协议)对 Lua 不可见、不可 hook。加 SDK 函数 = 扩 trusted base,走演化通路 + 补 conformance。

---

## 5. 事件 / 队列机制(机制层,非具体部门)

部门间不直接通信,经 framework 具名队列连接,契约来自每部门 `M.spec`:

```lua
M.spec = {
    consumes = { "<queue>" },   -- 0..N
    produces = { "<queue>" },   -- 0..N
    fanout   = { "<queue>" },   -- 0..N;每个须同时被本部门 consume 或 produce
    timeout  = "<duration>",    -- stall window(无输出多久判卡死),非总时长上限
}
```

- 路由 first-match,按 graph 顺序;启动 lint 检测多 match。
- fanout = 显式 `Vec<mpsc::Sender>`,禁 `broadcast`;慢消费者 `try_send` 失败只 drop 该订阅者。
- 启动 schema validation:未声明队列被引用 / `capacity==0` / 缺 lua / bad timeout / 孤立队列 → refuse-to-start。
- source 只在 `raisers/*.lua` 静态声明;内置 `cron`(`Ns|Nm|Nh`)/ `file_watch`。

---

## 6. RuntimeKind 运行时目录

`crates/fkst-common/src/runtime_layout.rs` 的 `RuntimeKind` 定义 7 类,挂在 `FKST_RUNTIME_ROOT` 下:

```
<RT>/pipeline/         pipeline artifact
<RT>/mailbox/          部门间寄信(threads)
<RT>/evolve-requests/  {inbox,running,done,failed} 触发面
<RT>/worktrees/        candidate worktree(本机)
<RT>/codex-permits/    fcntl permit 池(本机)
<RT>/locks/            with_lock fcntl 锁(本机)
<RT>/logs/             codex/部门日志(本机)
```

`mailbox` / `pipeline` / `evolve-requests` 的**内容由 Lua(package)产生**,经 SDK `file.write` / git 写出;引擎只:① `RuntimeKind` 枚举保留命名空间;② `runtime_path(kind,…)` 解析路径;③ `source_runner` file_watch 读 `evolve-requests/inbox` 转事件。引擎对这三类零 `fs::write` 内容。`FKST_RUNTIME_ROOT` 由 host 配置注入,引擎不硬编码;`runtime_path` 拒绝路径穿越。

---

## 7. 并发模型

- 单 Lua state + 协程:同一时刻一个 coroutine 跑 Lua,handler 间天然无 race。
- 协程切换只在 `spawn_codex_sync / await_all / with_lock`。
- 每个 codex 调用 = 独立 OS 进程,OS 调度真并发。
- codex 全局并发由 fcntl permit 池强制(`codex-permits/`),拿不到阻塞(可被 timeout 取消),进程死自动释放。
- 跨 pipeline 协调只有 `with_lock`;计数用 `count_worktrees / git_log_count` 自约束;无 semaphore/mutex/rate_limiter SDK。
- stall window 监督:codex 持续产出跑到自然完成,只在 stall window 内零输出才 `SIGKILL -pgid`。

---

## 8. 自演化接管(known-good)

- `fkst-framework known-good promote` 检测 integration branch 领先 `refs/known-good` + conformance / self-test / review evidence 过 → 健康观察 → CAS 推进 `refs/known-good`;健康失败写 rollback witness 并 checkout 回 known-good。
- `refs/known-good` 是 git ref(非 branch),由 `known_good.rs` 原子维护,表示"当前在跑的 framework binary"。
- 生效延迟分层:package prompt/tunables(commit 即时)< 部门 lua(file watch 秒级)< framework Rust(worktree + cargo build + swap 分钟级)< SPEC/conformance(共识后)< supervisor(共识 + auto-swap + 健康观察)。

---

## 9. 跨文档定位

- 当前 invariant 权威定义 → `SPEC.md`(Tier II)
- 引擎治理 / 哲学不动点 → `CLAUDE.md`
- 本文 → 引擎结构地图:分层、依赖、资源、I/O、SDK、事件机制、runtime 目录、并发、known-good

⟦AI:FKST⟧

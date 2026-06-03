# CLAUDE.md

## 工作语言

本仓库的源文件内部一律使用英文。`.rs`、`.lua`、`.sh`、`.py`、`.ts` 等源码里的注释、docstring、log、error、panic 文本、模板字符串和标识符都必须保持英文。fkst-substrate 是 trusted base，不是面向最终用户的 UI；源码、日志与错误文本需要和 Rust/Lua 生态、命令行工具、LLM 语料保持一致。

源文件之外的对外产物使用中文，包括对话回复、文档、issue/PR/comment、变更说明、TODO 段落和治理文本。代码标识符、路径、crate 名、第三方 API、命令名、协议名、测试断言和引用的原文可以保留英文。

不要用中英混杂凑句子。中文表达不顺时改写中文句子，不把动词切成英文。用户输入里有英文术语时，回复仍用中文，术语原样保留。

## 架构哲学

fkst-substrate 是稳定发布的受监督事件 / SDK / 进程衬底。它提供 supervisor、framework、common 类型、Lua SDK、事件调度、runtime layout、worktree / lock / codex 进程契约；它不包含业务 Lua package，不包含具体部门拓扑，不包含某个 host 的研发策略。

核心不是写应用，而是提供应用可运行、可观察、可组合的衬底。framework 提供受限、可审计、可组合的运行环境；业务行为由独立 package root 或 host root 注入的 `departments/`、`raisers/` 和 Lua helper 组成。引擎不能因为某个 host 的业务方便，把业务概念下沉到 Rust。

self-hosting 只表示 host/package 可以在此 runtime 上编排 SDLC 工作流；这不是 engine 内建职责，也不是把某个 host 的运行内容写进引擎库的理由。具体部门、判断策略、审查策略、组织拓扑和发布策略属于 package、host 或外部发布系统。

agent 是 system call，不是合作伙伴。一次 `codex exec` 是一次 OS 子进程调用，输入来自 prompt/stdin/context/worktree，输出落到 stdout/stderr/log/exit_code；实例没有身份、记忆、连续性，也不直接互相通信。多 agent 协作只能由 Lua Department 通过事件、文件、git、lock 和 `await_all` 组织。

trusted base 回答“系统是什么”，由 `SPEC.md`、conformance 和小内核锁定；package/host 行为回答“当前能处理什么”，由真实事件、失败、commit、worktree、日志和验证结果推动外部工作流。accepted release state 是外部 release pipeline 的事实：build → test → `--self-test` → conformance → 签名 artifact → deploy → canary / 回退策略。engine 不拥有 runtime accepted-state/回退；发布安全是外部策略。

## 三层稳定性

Tier I 是 `crates/fkst-supervisor`。它是进程根，只负责定位 framework binary、spawn `fkst-framework supervise`、继承 stdout/stderr、等待退出、处理信号和 reap 子进程。它不依赖 `fkst-common` 或 `fkst-framework` Rust 类型，不做业务。目标是极小、可独立审计；`≤150 LOC` 是当前 supervisor 规模门槛，偏离必须触发 audit。

Tier II 是 `SPEC.md` 与 conformance。它定义引擎身份不动点和边界测试。Tier II 需要保持小、稳定、可审计；它不是 package 策略文档，也不能写入某个 host 的临时路线。

Tier III 是 `crates/fkst-framework`、`crates/fkst-common` 和由独立 package/host 注入的 Lua graph。Rust framework 与 common 是引擎代码，Lua package 是外部行为层。Tier III 可以变化，但发布进入 accepted state 的事实由外部 release pipeline 产生。

bootstrap seed 可以有小 LOC 口径；当前生产态 Tier III 总 LOC 不是硬 gate。硬约束是 trusted base 可审计：一名 senior 工程师必须能在合理时间内独立读懂 supervisor、Tier II anchor、SDK surface、进程边界和 runtime fact boundary。

## 三级公司

结构只有三级：Company / Department / Person。

Company 是 supervisor + framework + composed graph。它接收 source 事件、根据静态图建立 queue 和 consumer、按 queue 将事件送入 Department、spawn 一次 `fkst-framework run <lua>` 执行 pipeline，解析 `RAISED:` 后再投递事件。Company 不写业务。

Department 是 package root 或 host root 中的 `departments/<dept>/main.lua`。它暴露 `M.spec` 和 `pipeline(event)`；`M.spec` 声明 `consumes`、`produces`、`fanout`、`timeout`。Department 无 lifecycle hook，无共享内存，同一个 `pipeline` 跑两次就是两次独立调用。

Person 是一次 `codex exec` 子进程。它通过 `spawn_codex_sync` 或 `spawn_codex` 启动，做完一件事即退出。Person 之间禁止直接通信；并发 Person 只能由 Department 用 `await_all` join。

不能加层。不要引入团队、科室、工作组、agent 会议、共享 context、持久 agent 记忆、第二 dispatcher 或旁路 coordinator。任何新概念必须先能映射回这三级，否则不是引擎模型的一部分。

## 状态与持久化

跨 pipeline 的 durable 真相只允许来自 `git refs/commits/branches`、外部源和明确的 host filesystem fact。engine 不替它们持久，不存任何“为活过崩溃”的东西。内存是 cache，进程死后归零；事件队列、raise buffer 和 in-flight pipeline 都是瞬时过程。framework 进程独立一次性，崩溃等价从 0 重来；不为故障恢复/持久化做任何设计。恢复只能由 cron/file_watch scanner 从 git、明确 host filesystem fact 或外部源重新推导，不恢复内存或 in-flight 工作。没有 record 即不存在；进程内“觉得发生了”不是事实。

framework 不写持久状态文件。`RuntimeLayout` 把 `FKST_RUNTIME_ROOT` 下的路径限制为 `worktrees`、`codex-permits`、`locks`、`logs`。这些是引擎一次性 scratch 落点；package 不在 `<RT>` 下放 inbox、完成态或业务 schema。package root / source tree 在运行期只读，只承载代码、fixture 和 asset；运行产生物只能落 RuntimeLayout scratch、明确 host fact boundary、git commit 或外部源，不得写回 package 代码树。

禁止 SQLite、KV store、通用 event log、`.current`、`.heartbeat`、`.next-sha`、进程内计数器或状态文件承担事实源职责。派生状态从事实源重读：计数用 `git_log_count` 或 `count_worktrees`，去重用 `git_log_grep`，互斥用 `with_lock` 或 codex permit pool。

失败是一等输出。pipeline 失败可以产生日志、失败 commit、失败 socket 文件或无 commit，但不能停在隐形状态。日志是 supervisor / framework / dept 三层过程可追溯记录，可 grep、可调试；它不是事实源、不是 reconciliation 输入、不是 accepted state。dept 的 `log.*` 走 stderr，由 supervise 捕获进 `<RT>/logs/framework-child` 下的 framework-child 具名 log；codex log 落 `FKST_RUNTIME_LOG_DIR` 或平台默认日志目录。可观察性来自 git、filesystem、fcntl lock witness 和落盘日志，不来自 dashboard 或外部 telemetry。

## 并发模型

单次 `fkst-framework run` 使用一个 Lua state。Lua handler 内没有共享内存并发；跨 pipeline 的真实并发来自 OS 进程。

`spawn_codex_sync`、`spawn_codex`、`await_all`、`exec_sync`、`with_lock`、`setup_worktree` 等 SDK 调用把阻塞、进程、文件锁或 worktree 操作显式暴露。不要在 framework 内引入按 Department 命名的 scheduler、业务 semaphore、retry policy 或 rate limiter。

Codex 全局上限由 `<RT>/codex-permits/permit-*` 的 fcntl permit 池强制。拿不到 permit 就阻塞在文件锁上；进程退出或崩溃时 fd 关闭，permit 自动释放。这个机制是引擎层唯一 codex 并发权威。

## Pipeline / 事件 / 路由

事件流是 `source -> fanout -> route -> spawn -> RAISED`。`cron` 和 `file_watch` source 由 `raisers/*.lua` 静态声明；Department 的 `M.spec` 静态声明 consumes/produces/fanout/timeout。启动时 graph scan 一次性求值 package root 与 host root 中固定目录，构造 Config。

路由按 queue 和消费者集合发生。普通 queue 只能有一个 active consumer；fanout queue 必须由相关 Department 的 `M.spec.fanout` 显式声明。重复声明幂等，未声明多消费者或同队列反馈会在启动 validation 阶段拒绝。

`raise(queue, payload)` 是 best-effort、at-most-once、derived-only。它只在进程内 buffer，在 pipeline 退出前向 stdout 打一行 `RAISED: <base64-url-encoded JSON>`。supervise 从 stdout 末尾向前解析，malformed 只 warn，不 crash。

Source 只能来自 `cron` 与 `file_watch`。复杂日历、时区、退避、业务轮询、HTTP ingress 都不进入 source kind；用现有 source + Lua + 文件落盘组合。新增 source kind 是 trusted base 扩张，必须有真实 evidence、设计闭包和 conformance。

启动图固定来自 `FKST_PACKAGE_ROOT` 或 `--package-root` 指向的 package root，再加 host root。合法输入是 `departments/`、`raisers/` 和可被 Lua `require` 的 package 文件。`package.lua`、package manifest、root list、dependency/order/override DSL、`FKST_STDLIB_ROOT`、`FKST_RUNTIME_PACKAGE_ROOT`、`FKST_GRAPH_ROOTS` 都不是合法 surface。

## 边界

Lua SDK surface 固定为：

`pipeline / source / raise / spawn_codex_sync / spawn_codex / exec_sync / await_all / with_lock / git_log_count / git_log_grep / count_worktrees / list_orphan_worktrees / setup_worktree / file / log.{info,warn,error} / now`

其中 `pipeline` 与 `source` 是 graph/package 侧约定，Rust 注册的运行时 primitive 是 `raise`、codex、exec、await、lock、git/worktree、file、log、now。新增 SDK 函数就是扩 trusted base，必须走 Tier III 测试和 conformance，不能顺手加入。

framework 看得见的概念只有 `event`、`source`、`queue`、`pipeline`、`coroutine/Lua state`、`worktree`、`subprocess`、`git ref`、`filesystem`、`file lock`、`time`。framework 看不见业务轮次、业务关卡、业务阶段、判断策略、审查策略、退避策略、重试策略、修复尝试、审计会话或任何具体业务部门名。

SDK 命名可以直接写 `codex`。不要做通用 LLM provider 抽象；别的模型或 CLI 可以由 package/host 用 `exec_sync` 组合。

## Fanout / 队列

队列物理实现是显式 `Vec<mpsc::Sender<Event>>`。禁止换成 `tokio::sync::broadcast`；broadcast 的 lag 语义和本引擎的 per-consumer drop-on-full 语义不一致。

Producer 对 queue 调用 `try_send`。某个慢消费者 `Full` 或 `Closed` 只 drop 该订阅者并 warn，不阻塞其它订阅者。未知 queue warn 后 drop。

启动 validation 强制检查 queue capacity > 0、引用的 queue 必须存在、Department lua 文件必须存在、timeout 必须是 `s/m/h` 结尾、孤立 queue 拒绝、多消费者必须 fanout、同 Department consume+produce 同一 queue 必须 fanout。

## 子进程契约

supervisor spawn framework 时使用独立 process group，但收到 `SIGINT` 或 `SIGTERM` 时只退出自己，不向 event runtime 发送信号。restart 不杀 in-flight framework/codex；在飞工作可以自然完成并留下事实，后续事件若丢失应由 package/host scanner 从事实源恢复。

Department execution 由 supervise spawn `fkst-framework run <lua> --package-root <path> --event <json>`。每个 framework child 有独立 process group、具名 log、stdout/stderr 捕获和 no-output stall window。stall window 是无输出卡死检测，不是总时长上限；有持续输出就等自然退出。stall 时发送 `SIGKILL` 到整个 process group，退出码映射为 `124`。

Codex 调用固定为 `codex exec --dangerously-bypass-approvals-and-sandbox [-C worktree] [--context context] -`。prompt 写入 stdin；stdin EOF 是调用边界。stdout、stderr、exit_code、cmd、done time、stall window 必须写入 codex log。`spawn_codex` 返回的 handle 只能由同一 pipeline 的 `await_all` 消费，不能跨 pipeline 复用。

## 单 repo 单实例

一个 host git repo 对应一个 supervisor、一个 framework binary、一组 package+host composed graph 和一个 `FKST_RUNTIME_ROOT`。多 repo 或多业务是多次部署，不是在同一 framework 进程里跑多套主链路。

不做 web dashboard。完整可观察面是 git、runtime filesystem、locks、logs 和 host 自己选择的外部界面。dashboard 会把事实源变成第二系统，违反 trusted base 最小化。

## Git 与发布边界

`setup_worktree` 会创建 candidate branch。所有 git SDK 命令使用 `git -C <HOST> ...`，不依赖 framework launcher cwd。branch 前缀和 from separator 是 HostFact，来自 `FKST_CANDIDATE_PREFIX` / `FKST_CANDIDATE_FROM_SEP` 或 host `fkst.env`，缺失时 fail-closed。具体 integration branch、candidate topology、runtime hidden refs、push/pull 策略属于 package/host，不是 substrate 固定事实。

engine 无 runtime accepted-state/回退；发布安全是外部策略。

## 编码行为纪律

源文件内部英文。错误分类要窄，避免 `general error`。日志和 commit/body/event payload 要可 grep，写清触发来源、行为类型、等价语义、后续复用和失败归属。AI 生成的对外文本末尾保留 `⟦AI:FKST⟧`。

不写历史叙述，不写版本阶段标签，不写“曾经如何”。文档描述当前态；历史留给 git。不要留下 deprecated shim、compat layer、`.old`、`.bak`、`_legacy` 或“后续删除”的并行实现。改契约就改完整，旧形态从当前态删除。

命名即本体论。`Company`、`Department`、`Person` 是结构约束，不是比喻。类型与函数名应反映引擎事实，而不是 host 业务愿望。Rust framework 中出现业务名、具体部门名或共识策略名，通常就是边界泄漏。

## 已拒绝方向

拒绝 workflow engine 化、actor 平台化、LLM provider 抽象、package manifest DSL、YAML/JSON 配置语言驱动行为图、多 package-root override graph、runtime dynamic handler registration、SQLite/KV/event-log 状态层、web dashboard、持久 agent 记忆、agent 直接通信、framework 内业务 retry/cooldown/gate/round。

拒绝用文档警告替代接口收窄。接口如果很容易被误用，说明设计还没完成；应缩小 surface，把不变量放进代码和 conformance。

## 设计完备性判据

一个引擎改动只有在同时满足这些条件时才算完整：它不扩大 trusted base 或者有 evidence 支撑扩张；它保持三层稳定性和三级公司边界；它的事实落点只在 git/filesystem/fcntl；日志只作 observability scratch；它不引入业务概念；它的失败可分类、可 grep、可恢复；它的 runtime 产物能映射到 `RuntimeLayout` 或明确的 git ref；它有适用测试或 conformance；它不需要 dashboard、内存状态或人工记忆才能解释系统发生了什么。

跨文档定位以本仓库为准：`README.md` 说明当前验证命令，`SPEC.md` 定义身份锚点，`docs/architecture.md` 说明引擎架构。任何引用都必须指向 fkst-substrate 自己的文档，不把外部 host 仓库当成当前事实。

⟦AI:FKST⟧

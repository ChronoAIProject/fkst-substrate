# CLAUDE.md — fkst-substrate(引擎 / universal SDLC substrate)

本仓库是 fkst 的**引擎**:trusted base(Tier I supervisor + Tier III framework + common crate)。它不含任何业务 Lua 包——部门 / raiser / stdlib / tunables 由独立的 Lua package 经 `FKST_PACKAGE_ROOT` 注入。本文是引擎域治理,只写引擎与 trusted base 的不变量。

## 存在意义(L0)
fkst 是**质量强制衬底**:承认"LLM 生成代码默认不可信",所有进主线的产物必经独立检查(共识 / review / 自动验证的适用组合)。引擎的职责是**给任意 host project 提供自动研发衬底**——不写应用,写"应用生长的环境"。引擎是 universal substrate;任何具体部门拓扑、路径、sentinel、分支前缀都必须可由 host / package 配置注入,不能硬编码成单一 host 的事实。

## 工作语言(L0,优先于一切)
- **源文件内部一切英文**(`.rs` / `.lua` / `.sh` 等):注释、docstring、`log`/`error`/`panic` 字符串、标识符、代码内构造的模板字符串。理由:substrate 无 end-user UI,人读 `git log` / `journalctl` / source。
- **源文件之外的对外产物中文**:对话、commit subject+body、PR、issue、`docs/*.md`。
- 例外(保留英文):标识符 / 路径、第三方 API/crate/命令名、git diff/patch 原文、引用的英文文档原话、测试断言。
- 禁止:源文件内中文自然语言;中英混杂凑通顺。

## 架构哲学(L1)
- **不写程序,写程序生长的环境**(substrate / harness programming):产物是衬底,不是应用。违反则退化成"又一个 workflow engine"。
- **自托管闭环**:衬底能在自己之内演化自己。不能 self-host 的 harness 是装饰不是基础设施。
- **agent 是 system call,不是合作伙伴**:codex 无记忆 / 身份 / 连续性,做一次决策就死;实例间不直接通信。
- **程序是数据驱动的临时不动点**:当前 `framework Rust + package graph` 过 conformance 的集成态由 evidence + 改动 + 验证迭代收敛,非"写出来"的 artifact。trusted base 是身份不动点(SPEC + supervisor 锁定)。
- **存在即可记录**:无独立于 `git/fs/flock` 记录的"运行时事实"。进程内觉得 X 发生了是 cache 不是事实。
- **commit 是命名加承诺**:每个跨 pipeline 事实(commit/raise/lock/worktree)隐含五槽——触发来源 / 行为类型 / 等价语义 / 可否复用 / 失败痕迹归属,要可 grep。
- **失败分到最窄 socket**:detector / 失败分到具体类别(timeout / permit-exhaust / conformance-fail / lock-contention),不打 general error。
- **Trusted base 越小越好,但不为零**:fix point 定义"系统是什么",可演化部分定义"能变成什么"。比照编译器:GCC 演化,C 规范是 fix point。
- **可审计性是硬约束**:任何设计须让 senior 工程师一周内独立 audit 整个 trusted base。效率 / 灵活性 / 性能都让位于可审计性。
- **Surface area 就是负债**:每加一个 SDK 函数 / source 类型 / Tier II invariant 都是永久成本。增加前三问:能组合表达?能 derive?能在 Lua 写?
- **接口增长需要 evidence**:新 SDK / source 类型 / invariant 必须证明消除了 ≥N 个真实 unhandled case。美感 / 一致性 / "更通用"都不是理由。

## 三层稳定性(强制)(L2)
- **Tier I `supervisor` 源码(Rust)**:`crates/fkst-supervisor`,≤150 LOC(目标值,偏离触发 audit);默认 immutable;改动经深度共识 + auto-swap + 健康观察。spawn framework 子进程、转发信号、记录 exit;不做业务。
- **Tier II `SPEC.md` + conformance**:fix-point,定义"系统是什么";改动经深度共识 + review + conformance 验证。
- **Tier III `framework`(Rust)+ 注入的 package graph**:`crates/fkst-framework` auto-evolved;package root 承载部门 / raiser / stdlib / tunables。
- 信任梯度只能经共识向上流:Tier I 治理 II,II 治理 III;向上改动唯一授权是深度共识(3/3 solver + meta-judge + 多角度独立 review)。

## 三级公司(强制)(L2)
- **Level 1** = supervisor 进程 + framework + composed graph 的路由;收事件 → 按 type 查路由 → 转给部门;不做业务。
- **Level 2** = composed graph 中 `departments/<X>/main.lua`;暴露 `pipeline(event)`;无 lifecycle hook;无状态纯函数。
- **Level 3** = 一次 codex CLI 子进程;做完一件事就走;实例间禁止直接通信。
- 不变量:不能加层;Level 2 间无共享内存;Level 3 间无直接通信;Level 1 不做业务;部门无状态,共享只通过 git + filesystem。

## 状态与持久化(强制)(L2)
- 跨 pipeline 唯一持久通道:`git refs/commits/branches`、文件系统、fcntl 文件锁。
- **framework 不写持久状态文件**:进程内一切是 cache,进程死归零;跨 pipeline 通信只通过 git/fs/flock。
- 派生状态从事实源派生:计数用 `count_worktrees / git_log_count`;dedup 用 `git_log_grep`;历史用 `git log`。
- 已否决:SQLite event log、通用 KV(`state.get/set/seen`)、`.current/.next-sha/.heartbeat` 状态文件、in-flight session continuation。
- runtime 产物经 `RuntimeKind`(`pipeline / mailbox / evolve-requests / worktrees / codex-permits / locks / logs`)挂在 `FKST_RUNTIME_ROOT` 下;root 由 host 配置注入,引擎不硬编码。

## 并发模型(强制)(L2)
- 单 Lua state + 协程:同一时刻只有一个 coroutine 跑 Lua,handler 间天然无 race。
- 协程切换只在显式 yield:`spawn_codex_sync / await_all / with_lock`。
- 每个 codex 调用 = 独立 OS 进程,OS 调度真并发;framework 不"调度"codex。
- codex 全局上限用 fcntl 文件 permit 池(`codex-permits/`),进程死自动释放;不靠内存计数。
- 不引入 semaphore / mutex / rate_limiter 跨 pipeline SDK;只有 `with_lock`。

## Pipeline / 事件 / 路由(强制)(L2)
- 事件流:`external (cron/file_watch/raise) → dispatcher → 按 type first-match 部门 → pipeline → 可选 raise → 信回 dispatcher`。
- 路由 first-match,按 graph 的 depts 顺序;启动 lint 检测潜在多 match 并 warn;未处理 event 记 warn + 丢弃。
- 启动图固定来自 installed package root + host root 的 `departments/` + `raisers/`;package root 由 `FKST_PACKAGE_ROOT` 或 `--package-root` 注入;**禁止** `package.lua` / manifest / root-list DSL。
- Source 只在 `raisers/<X>.lua` 顶层声明,启动一次性确定;禁止 runtime 动态注册。当前内置 source:`cron / file_watch`;`cron` interval 只支持 `Ns|Nm|Nh`。
- pipeline 退出物只两种:`git commit`(部门档案)与可选 `raise`(寄信);其它都是临时副作用。
- **raise 语义**:`raise(queue, payload)` 进程内 buffer,退出时 stdout 发一行 `RAISED: <base64-json>`,父进程解析重注入。best-effort / at-most-once / derived-only。durable 意图走文件系统(写 inbox)+ file_watch,**不靠 raise**。

## 边界(强制)(L2)
- 暴露给 Lua 的 SDK surface 固定:`pipeline / source / raise / spawn_codex_sync / spawn_codex / exec_sync / await_all / with_lock / git_log_count / git_log_grep / count_worktrees / list_orphan_worktrees / setup_worktree / file / log.{info,warn,error} / now`。
- 加 SDK 函数 = 扩 trusted base = 走演化通路,需 conformance 测试覆盖;不在主线随手加。
- framework **看不见**业务概念:`round / gate / phase / cooldown / retry_policy / consensus / heal_attempt`。这些在 Lua 层组合,永不下沉到 Rust framework。
- framework 内部 surface(mlua context、协程 scheduler、inotify 包装、subprocess lifecycle、信号、supervisor 协议)对 Lua 不可见、不可 hook。
- SDK 命名带 `codex`,不假装通用 LLM provider 抽象;想用别的 LLM 用 `exec_sync` 跑那个 CLI。

## Fanout / 队列(强制)(L2)
- 队列扇出 = 显式 `Vec<mpsc::Sender>`;**禁止 `tokio::sync::broadcast`**(滞后语义与 per-consumer drop on full 不兼容)。
- 慢消费者:`try_send` 失败只对该订阅者 drop + warn,不影响其它。
- 启动时 schema validation:未声明队列被引用、`capacity==0`、缺 lua 文件、bad timeout、孤立队列 → refuse-to-start。
- fanout 来自 Department `M.spec.fanout`;声明者必须在同一 `M.spec` 的 `consumes`/`produces` 引用该 queue。

## 子进程契约(强制)(L2)
- spawn 用 `setsid`(新 process group);timeout/kill 时 `SIGKILL -pgid` 收 codex 子孙,避免孤儿。
- supervisor restart 不杀 in-flight framework/codex:framework 独立进程组单次跑完即退;restart 只停自己。
- framework 向 supervisor 输出 raised events 用 stdout 单行 `RAISED: <base64-url-JSON>`;多行最后赢;malformed 当空不 crash;从末尾向前扫避免 log 内 `RAISED:` 误判。
- worktree 隔离:framework 自动加 ULID 后缀,业务给 prefix。
- spawn 监督是 liveness/stall window 非总时长上限:codex 持续产出就跑到自然完成,**stall(无输出)才 kill**。dept `M.spec.timeout` 语义是 stall window。

## 单 repo 单实例(强制)(L2)
- 一个 git repo = 一个 supervisor = 一个 framework binary = 一组 graph;不在同一进程跑多套。
- 多 repo / 多业务 = 多次部署,各自独立 supervisor;跨 repo 通信走 git push/pull。
- **web dashboard / UI 永不做**:`git log + worktrees + journalctl` 是完整可观察面。

## Codex 调用契约(强制)(L3)
- 非交互入口始终 `codex exec`,禁止裸 `codex`(进 TUI,无人值守 hang)。
- stdin 必须有确定 EOF;argv prompt 仅 ≤4KB,更大用 stdin 喂(`-` 占位 + 文件重定向)。
- 权限 `--dangerously-bypass-approvals-and-sandbox`;`-C <worktree>` 显式工作目录;`--add-dir <repo>` 让 codex 读 repo。
- codex 无固定 wall-clock 超时,改 liveness 监督:持续产出跑到完成,停滞(stall window 内零输出)才 `SIGKILL`。
- prompt + log 双 file:log 必填路径,stdout+stderr+`EXIT=`+`DONE_AT=` 全进同一文件;禁止 `> /dev/null` / 纯 stdout。
- 必须输出终止 marker(`IMPLEMENT_DONE:` / `VERIFY_DONE:` 等);用 marker 路由,不靠 exit code 单独判断。
- codex 只自治 own branch 的 git/gh 事实;禁止 `push --force` / `merge` / `rebase` / `reset --hard` / 推 integration branch / `pr merge` / 跨 branch 操作;禁止装新依赖、disable 测试换 CI 绿、用 `sleep` 凑测试节奏。
- AI 生成对外内容末尾带 sentinel `⟦AI:FKST⟧`。
- **Prompt purity**:codex 任务 prompt 给抽象准则,不列举具体反例(喂答案 → 只能 pattern-match;给原则 → 才会真思考)。

## 编码行为纪律(无人值守)(L3)
- 不确定**不 ask user**:把假设 / tradeoff 写进 commit body / log,自主选最优解推进,演化与 conformance 兜底。
- 最简优先:最小可解代码,零投机;200 行能压到 50 就重写。
- 手术刀守 scope(只动任务需要的)但不守旧代码:scope 内该删的 dead code 直接 `git rm`,该重构就重构,不留 compat shim / `.bak` / `_legacy`。
- 目标驱动:任务转可验证目标;行为变更必须留回归测试;禁 disable / `[skip]` 测试换 CI 绿,禁 `sleep` 凑测试节奏。
- 核心语义强类型:影响路由 / 控制流 / 状态判定的语义用具名 typed 表达;禁止塞无结构 `String` blob / 通用 bag / `general` 兜底。
- 命名表达职责与意图,禁含糊词(`mgr`/`util`/`data`/`handle`/`tmp`)。

## 删除 / 卫生(L3)
- 无遗留 / 无历史兼容 / 无迁移路径:转新设计删干净旧的,不留 compat shim;已 push 的破坏性改动 commit body 标 `BREAKING CHANGE:`。
- 删除优先:废弃文件 `git rm`,不创建 `.deprecated/.bak/.old`;历史在 `git log`。
- 无 amnesic GC:批量清理留显式 ledger 索引(删了什么 + 为什么 + 原文 sha)。
- 不写历史叙述,只写当前态;保留反面示例(`❌`)。
- 代码 / config 不硬编码版本号或阶段标签(`Stage X / vN / round-N`);round 来自 payload,版本来自 git tag。

## 已拒绝的设计方向(永久)(L4)
YAML/DSL 规则语言 · Python/.NET 实现 · SQLite/通用 KV 状态 · Pipeline ABC/WorktreeMgr/GateRunner 业务抽象入 framework · 多 project 共存一进程 · 嵌套 pipeline/sub-pipeline · agent-to-agent 直接通信 · in-flight session continuation · 状态文件 · semaphore/mutex 跨 pipeline SDK · 部门 `concurrency` 声明 · 框架级复杂 cron · web dashboard · runtime 动态注册 source · `package.lua`/manifest/root-list graph · 多 LLM provider 抽象 · fix point 为零。

## 反模式(禁止)(L4)
- ❌ framework Rust 里出现业务名词(`round/gate/cooldown`)→ 业务概念漏入,演化频率不匹配
- ❌ 加 SDK 函数不走演化通路 / 不补 conformance → trusted base 静默膨胀
- ❌ 用文件存"系统当前在做什么"(`.heartbeat/.in-flight`)→ 与 git refs 双轨
- ❌ codex 实例间直接通信 → 状态散在多进程内存
- ❌ 用 `tokio::sync::broadcast` 替 `Vec<mpsc::Sender>` → 滞后语义错配
- ❌ spawn 不带 setsid + pgid kill → timeout 时孤儿 codex 吃资源
- ❌ 用固定 wall-clock timeout 砍 codex → 砍掉正在产出的 working codex;只在 stall 时 kill
- ❌ 部门里维护 module-level Lua 变量做跨调用状态 → 部门必须无状态

## 跨文档定位(L5)
- 当前 invariant 权威定义 → `SPEC.md`(Tier II)
- 详细引擎架构(分层 / 依赖 / I/O / SDK / runtime 目录)→ `docs/architecture.md`
- 哲学不动点 → 本文件

⟦AI:FKST⟧

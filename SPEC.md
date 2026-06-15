# fkst SPEC

⟦AI:FKST⟧

本文档定义 fkst-substrate 的 Tier II 身份锚点。它只回答“系统是什么”，不记录“系统现在在做什么”。

## 身份边界

- fkst-substrate 是稳定发布的受监督事件 / SDK / 进程衬底。
- Tier I 是 process-root supervisor 源码；它只定位 framework binary、启动 `fkst-framework supervise`、继承 stdout/stderr、等待退出、处理进程级 signal 与 reap 子进程。
- Tier II 是身份锚点：`SPEC.md`、conformance 入口以及不可覆盖 gate 的承诺。
- Tier III 是 `crates/fkst-framework`、`crates/fkst-common` 与由 installed package roots input set / host root 注入的 Lua graph。Rust framework 与 common 是引擎代码，Lua package 是外部行为层。
- installed package roots input set 由可重复 `--package-root`、`FKST_PACKAGE_ROOTS` 或旧单值 `FKST_PACKAGE_ROOT` 定位，承载 package-owned 标准 `departments/`、`raisers/`、`fkst/`、`scripts/`；host root 承载 host-owned 扩展、业务代码和 `fkst.env`。`FKST_PACKAGE_ROOTS` 使用平台 path list 分隔符，不使用逗号。
- engine operation knob 由 source-owned typed registry 声明；解析顺序是 process env、host `fkst.env`、Operational 默认。HostFact 缺失 fail closed。registry 不读取 `tunables/*.txt`，不提供 set/write/dynamic registration/YAML/DSL/manifest/plugin。
- package identity 是 canonical package-root 的 basename。package id、department name、raiser name 与 queue 段名都必须匹配 `[A-Za-z0-9_-]+`；`.` 只作为 `pkg.queue` 跨包限定符。两个 package root 的 basename 相同 fail closed；独立 host root 存在时，package basename `host` fail closed，因为 `host` 是固定 host namespace。
- queue 是包内命名空间。多 graph-root 组合时，裸 queue 名按 owner namespace 归一化为 `<pkg>.<queue>` 或 `host.<queue>`；跨包消费必须显式写 `pkg.queue`。折叠单包（`package-root == host-root` 且只有一个 graph root）保持 LegacyFlat 字节等价：裸名仍输出裸名，同包限定名只作为幂等别名解析回裸名。
- composed graph 中的 child `run` 接收完整 package roots input set 作为 namespace catalog；`--owner-namespace` 选择 owner root，并由 owner root 计算 Lua `require` roots。`--package-root` 不是跨包 `require` 授权。
- 不存在 package manifest、依赖、order、override 或跨包 require 语义；`FKST_STDLIB_ROOT`、`FKST_RUNTIME_PACKAGE_ROOT`、`FKST_GRAPH_ROOTS` 不是合法 contract。一个 supervisor 仍只组合出一张 composed graph。
- 当前 fkst-substrate 仓库把 Tier II 身份锚点物化在根目录 `SPEC.md`；Rust `fkst-framework conformance` 是 engine host-conformance。
- 根目录 SPEC/conformance 只是当前 fkst-substrate 物化，不是所有 host project 的路径本体论。
- Tier II 身份不得依赖仓库内提交的派生 hash 摘要。

## 发布事实

- accepted release state 是外部 release pipeline 的事实，不是 engine runtime 内部事实。
- 推荐外部发布链路是 build → test → `--self-test` → conformance → 签名 artifact → deploy → canary / 回退策略。
- engine 不拥有 runtime accepted-state，不维护 runtime promotion，不内建发布安全决策。
- self-hosting 只表示 host/package 可以在此 runtime 上编排 SDLC 工作流；它不是 engine 内建职责。
- 具体 branch topology、artifact registry、签名策略、部署、canary 与回退策略属于 host 或外部发布系统。

## 公司结构

- Level 1 是 process-root supervisor 进程、Tier III event runtime、framework runner、启动期验证过的固定 behavior graph。
- Level 2 是 composed graph 中的 `departments/<dept>/main.lua`，暴露 `pipeline(event)`。
- DepartmentLocalModule 是同一 composed graph 中 `departments/<dept>/<helper>.lua` 同部门 helper 文件；它只允许被同一个 `departments/<dept>/` 内生产 Lua 加载，不构成新组织层级或跨部门共享库。
- Level 3 是一次 codex CLI 子进程。
- Level 1 不承载业务概念；Level 2 无持久状态；Level 3 实例之间不直接通信。
- Tier I process-root 不得拥有 graph scan、source semantics、queue fanout、department dispatch、per-event framework spawn、RAISED parsing、domain reconcile。

## 事实源

- 跨 pipeline 稳定事实只能来自 `git refs/commits/branches`、host-configured filesystem boundary、外部源（GitHub issue / host repo 文件）。
- runtime layout 是 host-local scratch；事件队列、raise buffer、locks、logs 不构成实体业务真相，也不跨机同步。`marks` 只承载 `once` success marker，`cache` 只承载 `cache_get` / `cache_set` / `cache_expire` 的 best-effort scratch KV 与 opt-in `exec_sync` read coalescing success entries，不承载业务 schema、accepted-state 或 rollback state。可靠在途 delivery 状态落在 `FKST_DURABLE_ROOT` 的 redb delivery store；它只承载 delivery lease / retry / DLQ 账本，不承载实体业务真相。恢复由 cron / file_watch scanner 从 durable 事实 re-derive 并重新 enqueue；`<RT>` 被清空时 cache miss 返回 nil，不影响 redb delivery store。host main repo HEAD/index 不承载 `<RT>/worktrees`、`<RT>/codex-permits`、`<RT>/locks`、`<RT>/logs`、`<RT>/marks`、`<RT>/cache` runtime layout pathspec。
- 内存、coroutine local table、subprocess handle、prompt 记忆、agent 判断都只是 cache。
- framework 不写持久状态文件。
- `SPEC.md` 禁止记录 runtime facts；包括 active branch、当前 round、队列深度、正在运行的 pid、临时 worktree 列表、最近失败次数。

## Gate

- `conformance` 是不可覆盖 gate。
- Tier II 改动必须有深度共识、独立 review、conformance 通过。
- 单个 codex 实例不能凭自身判断扩张 Tier II invariant 或绕过 gate。

## Conformance

- 当前 fkst-substrate 的 engine host-conformance 入口是 Rust CLI `fkst-framework conformance --project-root <path> [--package-root <path> ...]`。
- 当前 conformance check 是 `runtime-layout`、`project-layout`、`locale-catalogs`、`graph-scan`、`department-non-empty`、`schema-validation`。
- conformance 读取仓库文件与 Lua graph 来执行上述 check。
- conformance 不得调度工作、重试 pipeline、调用 GitHub、写隐藏状态、维护队列或承担 workflow engine 职责。

## Supervise process safety

- The governing practice for this problem class is process supervision with explicit ownership, non-blocking async executors, and bounded concurrency with backpressure. A new `supervise` process rule that deviates from those practices requires evidence that the existing practice does not apply to the engine boundary.
- This invariant applies to the engine-owned Department child boundary in `fkst-framework supervise`: admission, process ownership, pipe capture, exit observation, reaping, and delivery completion. It does not add a host workflow scheduler, Department-specific retry policy, dashboard, durable business inbox, or any new Lua SDK surface.
- Each `fkst-framework run` child started by `fkst-framework supervise` has exactly one engine owner responsible for observing exit and reaping the process. Child exit observation must feed delivery completion; a delivery must not stay leased or unacked because no owner waited on the child.
- `supervise` must not run blocking `wait` or blocking pipe I/O on the core async runtime thread. Child stdout/stderr capture and process exit observation must be arranged so cron ticks, file-watch ticks, lease renewal, retry, dead-letter maintenance, and reliable wake handling keep making progress while many Department children are running.
- Department child spawn must be bounded by engine-owned concurrency/backpressure before reliable delivery lease or equivalent dispatch ownership is consumed. Durable dispatch admission is bounded by finite operational knobs: `FKST_MAX_IN_FLIGHT_PER_DEPT` caps steady-state concurrent durable children per Department, and `FKST_DURABLE_ADMISSION_BURST_PER_DEPT` caps new durable children admitted per Department dispatch pass. When capacity is unavailable, due deliveries remain due or queued for a later dispatch pass rather than being leased into an unobservable backlog.
- `M.spec.stall_window` remains a reliable delivery lease and renewal window. It is not a child no-output timeout, not a process kill deadline, and not permission to let unbounded child spawn load starve runtime timers.
- Acceptance for this invariant is a local engine change that documents and tests: no zombie accumulation under concurrent Department child exits, no core-runtime starvation from child `wait` or pipe I/O, bounded Department spawn admission before lease consumption, and delivery ack/retry decisions tied to the single observed child exit. It is not a new SDK surface, source kind, durable business fact, dashboard, retry policy, or host workflow concept.

## SDK surface

- 固定 Lua SDK surface 锚点是 `fixed-lua-sdk-surface`；允许 surface 是 `pipeline`、`source`、`raise`、`spawn_codex_sync`、`spawn_codex`、`fkst.codex_runs`、`exec_sync`、`await_all`、`with_lock`、`once`、`cache_set`、`cache_get`、`cache_expire`、`graph_json`、`t`、`git_log_count`、`git_log_grep`、`count_worktrees`、`list_orphan_worktrees`、`setup_worktree`、`file`、`json.decode`、`log.info`、`log.warn`、`log.error`、`now`。
- `t(key[, vars])` is the fixed key-catalog localization primitive. It reads owner-root `locales/<locale>.lua` and `locales/en.lua`, resolves `<locale>` from `FKST_OUTPUT_LANG`, falls back to `en` for missing locale or key with a structured warning, and interpolates `{name}` placeholders from scalar vars. Catalog files are flat Lua tables with stable string keys and literal UTF-8 string values. `en` is the reference locale; conformance requires every non-`en` catalog to cover all `en` keys, rejects decode-helper-hidden literals in `locales/`, and rejects machine protocol tokens in catalog keys or values. Locale catalogs are the sanctioned exception to the source-files-English rule for prose literals; machine tokens remain code and must not be catalog content.
- `graph_json() -> string` 是显式授权的只读 introspection surface；只有当前 Department 的 `M.spec.graph_json = true` 时可调用。它按当前 fixed package roots input set 与 host root 重新扫描并验证 composed graph，返回稳定 JSON 字符串，schema 为 `fkst.graph.v1`。输出只包含 topology fact：raiser / queue / department nodes、raiser→queue / queue→department / department→queue edges，以及 department 的 `consumes`、`produces`、`ephemeral`、`stall_window` 和 materialized `retry` metadata；node `id` 与 edge endpoint 使用 `kind:canonical_name` 形态以区分同名 raiser / queue / department；不输出 `lua` path、`owner_root`、queue capacity 或 runtime state。排序必须确定性。
- `with_lock(name, fn)`、`once(key, fn)`、`cache_set(key, value[, ttl_seconds])`、`cache_get(key)` 与 `cache_expire(key)` 共用 runtime key 合约：key / name 必须是非空相对 filesystem path，`/` 表示目录；每个 segment 非空、最长 255 bytes、匹配 `[A-Za-z0-9._-]+`，且不是全点 segment（如 `.` / `..` / `...`）；禁止 leading / trailing `/`、`//`、反斜杠、NUL 与绝对路径。校验后的 key 保持为 `<RT>/{locks,marks,cache}/<key>/` 目录路径，engine 在该目录下写 reserved leaf file（`=lock` / `=mark` / `=value`）；`=` 不在合法 key segment 字符集内，因此不会与有效 key 冲突。
- `with_lock(name, fn)` 在 `<RT>/locks/<name>/=lock` 上持有 exclusive flock；`once(key, fn)` 在 `<RT>/locks/once/<key>/=lock` 上持有 exclusive flock 后检查 `<RT>/marks/<key>/=mark`；`locks/once/` 是 once 内部锁的保留子目录，不属于 `with_lock` 用户锁命名空间。marker 已存在时返回 `false` 且不调用 `fn`，不存在时调用 `fn`，成功后写入 marker 并返回 `true`，失败时传播错误且不写 marker。
- `cache_set(key, value[, ttl_seconds])`、`cache_get(key)` 与 `cache_expire(key)` 读写 `<RT>/cache/<key>/=value`。`cache_set` 原子覆盖写入 byte-exact string value 与可选 expiry metadata；`ttl_seconds` 缺省或 nil 表示不过期，正数表示按 wall-clock deadline 过期。`cache_get` 命中时返回 string，缺失、过期、malformed 或 unreadable 时返回 nil，过期文件会 best-effort lazy evict；`cache_expire` 显式删除 key，缺失视为成功。cache 是 host-local best-effort scratch，不是 durable state；调用者需要 read-compare-write 原子性时必须外层使用 `with_lock`。
- `exec_sync({ cmd = "...", read_coalesce = { key = "...", ttl_seconds = n } })` is opt-in external-read coalescing. Omitting `read_coalesce` is force-fresh current behavior. The key uses the runtime key contract; `ttl_seconds` must be positive finite and is clamped to at most 300 seconds. The engine fingerprints the caller key, resolved `/bin/sh -c` command/argv, exact execution cwd, sorted effective environment, and timeout. Coalescing is only allowed for stdin-less commands; if a future command path carries stdin bytes or inherits stdin, the engine bypasses read coalescing and runs fresh. A fresh success entry (`exit_code == 0`) returns cached `{stdout, stderr, exit_code, timed_out?, error_class?}` before rate-pool acquisition; on miss the process takes a per-fingerprint `<RT>/locks` flock, rechecks, then acquires any matching rate-pool token and runs the command. Non-zero exits are not cached. This primitive is only for caller-declared read-safe commands; the engine never infers read safety from command text.
- Department 默认以可靠方式消费队列；`M.spec.ephemeral = {"queue"}` 可将本 Department 对指定 consumed queue 的订阅降级为非可靠。`M.spec.retry = false` 只表示失败不重试；`M.spec.retry = { ... }` 可覆盖 `max_attempts`、`base`、`cap` 的任意子集，缺失字段从全局默认补齐。`M.spec.stall_window` 是可靠投递 lease 与续租窗口，不是 framework child 无输出 kill deadline。可靠订阅启动必须有 `FKST_DURABLE_ROOT`，缺失 fail-closed。可靠 source event 必须带 `SourceRef{kind,reference}`；cron 由 raiser 名派生，file_watch 由绝对路径派生，Department `RAISED` 进入可靠 queue 时继承上游 source_ref，缺失则 publish fail-closed 且上游 delivery 不 ack。可靠 consumer 由 Fanout wake + 定时 tick 调用 redb store `lease`，spawn framework 后仅在 exit 0 且 RAISED publish 成功时 `ack`；非零退出、codex timeout、spawn error 或 RAISED publish 失败调用 `retry`，到 max attempts 写 redb dead 表并 best-effort publish `dead_letter`。当前 delivery 来自 `dead_letter` 时抑制再次发送 `dead_letter`。该机制不是新 source kind，不提供 exactly-once；语义是 at-least-once-until-ack，`Fanout::send` 在可靠路径只作进程内唤醒。
- `spawn_codex_sync` 与 `spawn_codex` 接受 `timeout` opt，默认 3600 秒，作为 codex 子进程整体 wall-clock cap；stdout/stderr 输出只被捕获，不延长 timeout。`spawn_codex` handle 只能由 `await_all` join；单 handle 等待使用 `await_all({handle})`；first-result fanout 与 sleep timer 不是固定 Lua SDK surface。
- `fkst.codex_runs()` is a read-only bounded observability surface over engine codex run records. It returns running and recent codex runs with `role`, `started_at`, `status` (`running` / `done` / `failed`), bounded `output_tail`, and optional `exit_code`; it does not expose runtime paths or unbounded stdout/stderr.
- 边界资源必须静态枚举并经 adapter mediation 访问。当前 engine registry 锚点是 `fkst-framework boundary-resources` 与 `crates/fkst-framework/src/boundary_resource.rs`，覆盖 `codex.process`、`shell.process`、`git.process`、`runtime.filesystem` 与 `wall-clock`。可分类的 adapter failure 使用 `error_class`，值域为 `quota-exhausted`、`auth-degraded`、`provider-unavailable`、`provider-throttle`；package 不应从 stderr 文本反推边界状态。
- `fkst-framework test` 额外注册 test-mode-only `fkst.test` table。除断言与 `run_department` 外，`fkst.test.mock_command(pattern, result)` 与 `fkst.test.command_calls()` 可劫持 `exec_sync`、codex SDK 与 git SDK 的外部命令调用；匹配基于渲染命令行的前缀或子串，mock 按注册顺序一次性消费，未 mock 的外部命令 fail closed 且不启动真实进程。Mocked `exec_sync` bypasses read coalescing so command call accounting remains deterministic. production `run`、`supervise`、`--self-test` 与 conformance 不注册该 mock state。`setup_worktree` 在 test mode 也经同一 git mock runner，但不模拟 worktree filesystem 副作用。`fkst-framework test --report-json <path>` 写入 authoritative machine-readable 测试结果与清单，schema 为 `fkst.test.report.v1`，条目字段是 `owner_namespace`、`file`、`name`、`status` 和失败时的 `error`；`owner_namespace`、`file`、`name` 三元组是身份，不提供拼接 `id`。报告由 Rust 侧枚举的 `test_*` key 构造，Lua `print` 不能伪造条目。stdout 的 `PASS` / `FAIL` / summary 行只作为 legacy human / compatibility surface，不是 machine-authoritative。
- `json` 是 decode-only：`json.decode` 暴露 engine 自身 JSON wire format 的解析（event 进、`raise` 出都是 JSON），Lua 值经 `raise` 出引擎，故不提供 `json.encode`；`json` table 锁定为只含 `decode`，新增 encode 或其它 key 必须另走 evidence + conformance。`raise` 不推断 bare Lua empty table 的数组 / 对象意图：裸 `{}` 序列化为 JSON object `{}`；由 `json.decode("[]")` 构造的 array-tagged empty table 经 `raise` 保持为 JSON array `[]`。需要可能为空的数组字段时，package 必须显式用 `json.decode("[]")` 形成 array-tagged table；engine 不提供 `json.array`、不提供 schema / field-name 推断，也不启用全局 empty-table-as-array 开关。
- 新增 SDK 函数必须经 evidence、深度共识与 conformance 覆盖，不能由单个 codex 实例直接扩张。

## 改动范围

- Tier II 身份锚点保持小而可审计。
- 新 Tier II invariant 必须由真实 evidence 支撑，并经深度共识授权。
- 可由 Tier III 组合表达的能力不得下沉为 Tier II invariant。
- Tier I 范围是 `crates/fkst-supervisor/`。
- Tier II 范围是 `SPEC.md` 与 conformance assets。
- Tier III 范围是 `crates/fkst-framework/`、`crates/fkst-common/`、package root 或 host root 中的 `departments/<dept>/main.lua`、`departments/<dept>/<helper>.lua`、`raisers/<dept>.lua`、`departments/<dept>/<prompt>.txt`、`fkst/`、`scripts/`。
- business Lua 范围是 package root 或 host root 中的 `departments/<dept>/main.lua`、`departments/<dept>/<helper>.lua`、`raisers/<dept>.lua`、`fkst/`、`scripts/`。

⟦AI:FKST⟧

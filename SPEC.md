# fkst SPEC

⟦AI:FKST⟧

本文档定义 fkst 的 Tier II 身份锚点。它只回答"系统是什么",不记录"系统现在在做什么"。

> 抽取快照说明:本文从 ChronoAIProject/fkst 原样带入作身份锚点。其中引用的 `conformance/run_all.sh`、conformance gate 集尚未迁入 fkst-substrate(见 README deferred 清单);相关条目描述的是 substrate 的目标 invariant,不代表本仓库当前已物化对应文件。

## 身份边界

- Tier I 是 process-root supervisor 源码,默认不可变;只允许定位 accepted runtime artifact、启动 / 观察 Tier III event runtime、记录 spawn/exit、转发 process-level signal。
- Tier II 是身份锚点:SPEC、conformance 入口,以及不可覆盖 gate 的承诺。
- Tier III 是 framework Rust(event runtime + one-shot runner) 与固定 behavior graph,由 installed package root 与 host root 组合物化;两者共同形成单一 Company、单一 dispatcher、单一 queue set。
- installed package root 由 `FKST_PACKAGE_ROOT` 或 `--package-root` 定位,承载 package-owned 标准 `departments/`、`raisers/`、`fkst/`、`scripts/` 与随 runtime 发布的 gate 资产;host root 承载 host-owned 扩展与业务代码。
- engine operation knob 由 source-owned typed registry 声明;解析顺序是 process env、host `fkst.env`、Operational 默认。HostFact 缺失 fail closed。registry 不读取 `tunables/*.txt`,不提供 set/write/dynamic registration/YAML/DSL/manifest/plugin。
- integration branch promotion policy 不读取 package default;其 ladder 是 explicit CLI、process env、host `fkst.env`,缺失时 fail closed。
- `fkst.package_asset` 是 package-root immutable asset reader,不得读取 host root relative path;host operation config 读取归属 Rust registry。
- layout collision 只拒绝会改变 behavior graph 或 package Lua module identity 的同名 `departments/*`、`raisers/*`、`fkst/*`;host `scripts/*` 与 `conformance/*` 同名普通文件不构成 package identity collision。
- 不存在第二 package/root 语义;`FKST_STDLIB_ROOT`、`FKST_RUNTIME_PACKAGE_ROOT`、`FKST_GRAPH_ROOTS` 不是合法 contract。
- 当前 fkst 仓库把 Tier II 身份锚点物化在根目录 `SPEC.md` 与 `conformance/run_all.sh`。
- 根目录 SPEC/conformance 只是当前 fkst dogfood 物化,不是所有 host project 的路径本体论。
- 未来 host policy 可以命名等价 SPEC 与 conformance 入口,但不得覆盖身份 gate。
- Tier II 身份不得依赖仓库内提交的派生 hash 摘要。

## 公司结构

- Level 1 是 process-root supervisor 进程、Tier III event runtime、framework runner、启动期验证过的固定 behavior graph。
- Level 2 是 composed graph 中的 `departments/<dept>/main.lua`,暴露 `pipeline(event)`。
- DepartmentLocalModule 是同一 composed graph 中 `departments/<dept>/<helper>.lua` 同部门 helper 文件;它只允许被同一个 `departments/<dept>/` 内生产 Lua 加载,不构成新组织层级或跨部门共享库。
- Level 3 是一次 codex CLI 子进程。
- Level 1 不承载业务概念;Level 2 无持久状态;Level 3 实例之间不直接通信。
- Tier I process-root 不得拥有 graph scan、source semantics、queue fanout、department dispatch、per-event framework spawn、RAISED parsing、domain reconcile。

## 事实源

- 跨 pipeline 稳定事实只能来自 `git refs/commits/branches`、host-configured filesystem boundary（包括由 host-configured hidden git ref 物化出的 worktree）、fcntl 文件锁。
- 持久 runtime facts 通过 host-configured hidden runtime ref projection 同步;host main repo HEAD/index 不承载 `.fkst/runtime/**`、`.fkst-mailbox/**`、`.fkst-pipeline/**`、`.evolve-requests/**`、`.worktrees/**`、`.codex-permits/**` runtime layout pathspec。
- 内存、coroutine local table、subprocess handle、prompt 记忆、agent 判断都只是 cache。
- framework 不写持久状态文件。
- `SPEC.md` 禁止记录 runtime facts;锚点 `runtime-facts-prohibited`;包括 active branch、当前 round、队列深度、正在运行的 pid、临时 worktree 列表、最近失败次数。

## Gate

- `approach_consensus`、`diff_consensus`、`independent_review`、`conformance` 是不可覆盖 gate。
- Tier II 改动必须有深度共识、独立 review、conformance 通过。
- 候选级授权 evidence 由 review evidence gate 绑定 candidate changed paths、artifact blob、approach consensus、diff consensus、independent review 与 protected witness。
- conformance 覆盖 review evidence gate 的机制、反例和调用方 wiring;静态无候选 payload 的 `run_all.sh` 不承担 promote 授权判定。
- 物理 GPG 签名可以是 host deployment policy,但不是 Tier II 合法性的必要来源。
- 单个 codex 实例不能凭自身判断扩张 Tier II invariant 或绕过 gate。

## Conformance

- 当前 fkst 仓库的 conformance 入口是 `conformance/run_all.sh`。
- 首批 invariant 分组是 Tier II identity、source-language-identity、三级公司、事实源、SDK surface、部门本地模块边界、演化白名单、CI wiring、Tier I boundary。
- `source-language-identity` 要求 `crates/`、`conformance/`、`departments/`、`raisers/`、`scripts/`、`tests/` 下后缀为 `.rs`、`.lua`、`.sh`、`.py`、`.ts` 的 managed source files 不含中文自然语言文本。
- `conformance/source_language_identity.sh` 是该 invariant 的 Tier II gate。
- `host tree pollution` 要求 host main repo HEAD/index 不跟踪 runtime layout pathspec;durable runtime facts 只能通过 host-configured hidden runtime ref projection 进入 runtime ledger。
- `conformance/host_tracked_runtime_artifacts.sh` 是该 invariant 的 Tier II gate。
- conformance 可以读取仓库文件、检查可执行位、检查治理文档锚点。
- conformance 必须强制 `crates/fkst-supervisor/src` 只有 `main.rs`、Tier I LOC ≤ 150、且不得 import 或提及 event runtime / department / raiser surface。
- conformance 必须提供 changed-file 文件集白名单 guard,对 Tier I、Tier II、Tier III、detector、business Lua 演化范围 fail closed。
- conformance 不得调度工作、重试 pipeline、调用 GitHub、写隐藏状态、维护队列或承担 workflow engine 职责。

## SDK surface

- 固定 Lua SDK surface 锚点是 `fixed-lua-sdk-surface`;允许 surface 是 `pipeline`、`source`、`raise`、`spawn_codex_sync`、`spawn_codex`、`exec_sync`、`await_all`、`with_lock`、`git_log_count`、`git_log_grep`、`count_worktrees`、`list_orphan_worktrees`、`setup_worktree`、`file`、`log.info`、`log.warn`、`log.error`、`now`。
- `spawn_codex` handle 只能由 `await_all` join;单 handle 等待使用 `await_all({handle})`;first-result fanout 与 sleep timer 不是固定 Lua SDK surface。
- 新增 SDK 函数必须经 evidence、深度共识与 conformance 覆盖,不能由单个 codex 实例直接扩张。

## 演化范围

- Tier II 身份锚点保持小而可审计。
- 新 Tier II invariant 必须由真实 evidence 支撑,并经深度共识授权。
- 可由 Tier III 组合表达的能力不得下沉为 Tier II invariant。
- 演化白名单分组锚点 `evolution-whitelist-groups` 必须覆盖 Tier I、Tier II、Tier III、detector、business Lua 路径。
- Tier I 白名单是 `crates/fkst-supervisor/`。
- Tier II 白名单是 `SPEC.md`、`conformance/*.sh`。
- Tier III 白名单是 `crates/fkst-framework/`、package root 或 host root 中的 `departments/<dept>/main.lua`、`departments/<dept>/<helper>.lua`、`raisers/<dept>.lua`、`departments/<dept>/<prompt>.txt`、`fkst/`、`scripts/`。
- detector 白名单是 package root 或 host root 中的 `departments/<dept>/<prompt>.txt`、`gates/`、`audit_rules/`,并精确包含 `scripts/check_source_english.sh` 与 `scripts/check_source_english_test.sh` 两个 source-English detector 例外。
- business Lua 白名单是 package root 或 host root 中的 `departments/<dept>/main.lua`、`departments/<dept>/<helper>.lua`、`raisers/<dept>.lua`、`fkst/`、`scripts/`,但 `scripts/` 排除 `scripts/check_source_english.sh` 与 `scripts/check_source_english_test.sh` 两个 detector 例外。

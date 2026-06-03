# fkst SPEC

⟦AI:FKST⟧

本文档定义 fkst-substrate 的 Tier II 身份锚点。它只回答“系统是什么”，不记录“系统现在在做什么”。

## 身份边界

- fkst-substrate 是稳定发布的受监督事件 / SDK / 进程衬底。
- Tier I 是 process-root supervisor 源码；它只定位 framework binary、启动 `fkst-framework supervise`、继承 stdout/stderr、等待退出、处理进程级 signal 与 reap 子进程。
- Tier II 是身份锚点：`SPEC.md`、conformance 入口以及不可覆盖 gate 的承诺。
- Tier III 是 `crates/fkst-framework`、`crates/fkst-common` 与由 installed package root / host root 注入的 Lua graph。Rust framework 与 common 是引擎代码，Lua package 是外部行为层。
- installed package root 由 `FKST_PACKAGE_ROOT` 或 `--package-root` 定位，承载 package-owned 标准 `departments/`、`raisers/`、`fkst/`、`scripts/`；host root 承载 host-owned 扩展、业务代码和 `fkst.env`。
- engine operation knob 由 source-owned typed registry 声明；解析顺序是 process env、host `fkst.env`、Operational 默认。HostFact 缺失 fail closed。registry 不读取 `tunables/*.txt`，不提供 set/write/dynamic registration/YAML/DSL/manifest/plugin。
- `fkst.package_asset` 是 package-root immutable asset reader，不得读取 host root relative path；host operation config 读取归属 Rust registry。
- layout collision 只拒绝会改变 behavior graph 或 package Lua module identity 的同名 `departments/*`、`raisers/*`、`fkst/*`；host `scripts/*` 与 `conformance/*` 同名普通文件不构成 package identity collision。
- 不存在第二 package/root 语义；`FKST_STDLIB_ROOT`、`FKST_RUNTIME_PACKAGE_ROOT`、`FKST_GRAPH_ROOTS` 不是合法 contract。
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

- 跨 pipeline 稳定事实只能来自 `git refs/commits/branches`、host-configured filesystem boundary、fcntl 文件锁。
- 持久 runtime facts 通过 host-configured hidden runtime projection 同步；host main repo HEAD/index 不承载 `.fkst/runtime/**`、`.fkst-mailbox/**`、`.fkst-pipeline/**`、`.worktrees/**`、`.codex-permits/**` runtime layout pathspec。
- 内存、coroutine local table、subprocess handle、prompt 记忆、agent 判断都只是 cache。
- framework 不写持久状态文件。
- `SPEC.md` 禁止记录 runtime facts；包括 active branch、当前 round、队列深度、正在运行的 pid、临时 worktree 列表、最近失败次数。

## Gate

- `approach_consensus`、`diff_consensus`、`independent_review`、`conformance` 是不可覆盖 gate。
- Tier II 改动必须有深度共识、独立 review、conformance 通过。
- 候选级授权 evidence 由 review evidence gate 绑定 candidate changed paths、artifact blob、approach consensus、diff consensus、independent review 与 protected witness。
- conformance 覆盖 review evidence gate 的机制、反例和调用方 wiring；静态无候选 payload 的入口不承担发布授权判定。
- 物理 GPG 签名可以是 host deployment policy，但不是 Tier II 合法性的必要来源。
- 单个 codex 实例不能凭自身判断扩张 Tier II invariant 或绕过 gate。

## Conformance

- 当前 fkst-substrate 的 engine host-conformance 入口是 Rust CLI `fkst-framework conformance --project-root <path>`。
- 首批 invariant 分组是 Tier II identity、source-language-identity、三级公司、事实源、SDK surface、CI wiring、Tier I boundary。
- `source-language-identity` 要求 `crates/`、`conformance/`、`departments/`、`raisers/`、`scripts/`、`tests/` 下后缀为 `.rs`、`.lua`、`.sh`、`.py`、`.ts` 的 managed source files 不含中文自然语言文本。
- `host tree pollution` 要求 host main repo HEAD/index 不跟踪 runtime layout pathspec。
- conformance 可以读取仓库文件、检查可执行位、检查治理文档锚点。
- conformance 必须强制 `crates/fkst-supervisor/src` 只有 `main.rs`、Tier I LOC ≤ 150、且不得 import 或提及 event runtime / department / raiser surface。
- conformance 不得调度工作、重试 pipeline、调用 GitHub、写隐藏状态、维护队列或承担 workflow engine 职责。

## SDK surface

- 固定 Lua SDK surface 锚点是 `fixed-lua-sdk-surface`；允许 surface 是 `pipeline`、`source`、`raise`、`spawn_codex_sync`、`spawn_codex`、`exec_sync`、`await_all`、`with_lock`、`git_log_count`、`git_log_grep`、`count_worktrees`、`list_orphan_worktrees`、`setup_worktree`、`file`、`log.info`、`log.warn`、`log.error`、`now`。
- `spawn_codex` handle 只能由 `await_all` join；单 handle 等待使用 `await_all({handle})`；first-result fanout 与 sleep timer 不是固定 Lua SDK surface。
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

# fkst-substrate

fkst-substrate 是稳定发布的受监督事件 / SDK / 进程衬底：Tier I supervisor + Tier III framework + common。它不包含业务 Lua 包；业务行为由外部 package root 或 host root 注入。

开发分支：dev。

验证命令：

```sh
cargo build --workspace
cargo test --workspace -- --test-threads=1
```

## 配置机制

引擎操作配置由 `crates/fkst-framework/src/config_registry.rs` 中的静态 typed registry 声明。读取优先级固定为 process env → host `fkst.env` → operational 默认；HostFact 缺失 fail-closed。registry 只读，没有 set/apply/watch、YAML/DSL/manifest/plugin 或 per-key `tunables/*.txt` 兼容层。

5 个 knob:

- Operational: `FKST_QUEUE_CAPACITY` 默认 `16`
- Operational: `FKST_DEPARTMENT_DEFAULT_TIMEOUT` 默认 `30s`
- Operational: `FKST_CODEX_PERMIT_SLOTS` 默认 `20`
- HostFact: `FKST_CANDIDATE_PREFIX` 必填
- HostFact: `FKST_CANDIDATE_FROM_SEP` 必填

只读自省:

```sh
target/debug/fkst-framework config \
  --project-root "$PWD/examples/minimal-package" \
  --package-root "$PWD/examples/minimal-package"
```

## 独立运行

本仓库内置一个最小 package-root fixture：`examples/minimal-package`。它声明一个 cron source `tick` 和一个 log-only department `logger`。cron source 产生 `tick` queue 事件；`logger` 消费 `tick`，并通过 `log.info` 写结构化过程日志。

`FKST_RUNTIME_ROOT` 仍是引擎 scratch 配置，用于 worktree、codex permit、lock 与 log 等运行时落点；这个 fixture 的 Lua 不读取它，也不把 `<RT>` 当 package 状态目录。fixture 只展示 package-root 独立加载、graph validation，以及 source 事件 dispatch 到 `pipeline(event)`。

下列命令证明的范围如下：

- `conformance`：minimal-package 的单 source / 单 department 图通过 validation。
- `run logger`：单个 logger pipeline 消费 `tick`，并向 stderr 写结构化 `event received` 日志行。

```sh
cargo build --workspace
repo="$PWD"
tmp_host="$(mktemp -d)"
cp -R examples/minimal-package/. "$tmp_host/"
target/debug/fkst-framework conformance \
  --project-root "$tmp_host" \
  --package-root "$tmp_host"
(
  cd "$tmp_host" &&
  "$repo/target/debug/fkst-framework" run \
    "$tmp_host/departments/logger/main.lua" \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --event '{"type":"tick","payload":{}}'
)
```

logger 的 stderr 应包含结构化日志行，例如 `LEVEL=info MSG=event received: tick`。

## 发布边界

fkst-substrate 的 accepted release state 来自外部 release pipeline，而不是 engine runtime 内部状态。推荐外部链路是 build → test → `--self-test` → conformance → 签名 artifact → deploy → canary / 回退策略。engine 无 runtime accepted-state/回退；发布安全是外部策略。

host/package 可以在此 runtime 上编排 SDLC 工作流，但这属于外部行为层，不是 engine 内建职责。

engine 队列是瞬时的；durable 真相属于 git commit、明确的 host filesystem fact 或外部源，不在 engine 内部维护。

## 文档

- `CLAUDE.md`（= `AGENTS.md` 软链）：引擎治理与哲学不动点。
- `SPEC.md`：Tier II 身份锚点。
- `docs/architecture.md`：详细引擎架构（分层 / 依赖 / I/O / SDK / 事件机制 / runtime 目录 / 并发）。

来源：从 ChronoAIProject/fkst clean-init 抽取（历史留在该 public repo）。

本仓库作为稳定发布衬底消费前，应以 `cargo build --workspace`、`cargo test --workspace -- --test-threads=1`、`--self-test` 与 `conformance` 结果为准。

⟦AI:FKST⟧

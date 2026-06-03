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

本仓库内置一个通用最小 package：`examples/minimal-package`。它声明 cron source `tick`、producer（consume `tick`、produce+fanout `work`、写 witness）与两个 fanout consumers（consume `work`、各写 witness），用来作为 `--package-root` 的可加载实例。

下列命令证明的范围如下：

- `--self-test`：引擎自检通过。
- `conformance`：minimal-package 图与 fanout 契约通过 validation。
- `run producer`：单个 producer pipeline 独立加载并运行，发出 `RAISED: work` 并写 producer witness。

```sh
cargo build --workspace
FKST_RUNTIME_ROOT=/tmp/min-rt-$$ target/debug/fkst-framework --self-test
FKST_RUNTIME_ROOT=/tmp/min-rt-$$ target/debug/fkst-framework conformance \
  --project-root "$PWD/examples/minimal-package" \
  --package-root "$PWD/examples/minimal-package"
FKST_RUNTIME_ROOT=/tmp/min-rt-$$ target/debug/fkst-framework run \
  "$PWD/examples/minimal-package/departments/producer/main.lua" \
  --project-root "$PWD/examples/minimal-package" \
  --package-root "$PWD/examples/minimal-package" \
  --event '{"type":"tick","payload":{}}'
```

最后一条输出应包含 `RAISED: work`。

## 发布边界

fkst-substrate 的 accepted release state 来自外部 release pipeline，而不是 engine runtime 内部状态。推荐外部链路是 build → test → `--self-test` → conformance → 签名 artifact → deploy → canary / 回退策略。engine 无 runtime accepted-state/回退；发布安全是外部策略。

host/package 可以在此 runtime 上编排 SDLC 工作流，但这属于外部行为层，不是 engine 内建职责。

engine 队列是瞬时的；durable intent 属于 package/外部可观测事实，不在 engine 内部维护。

## 文档

- `CLAUDE.md`（= `AGENTS.md` 软链）：引擎治理与哲学不动点。
- `SPEC.md`：Tier II 身份锚点。
- `docs/architecture.md`：详细引擎架构（分层 / 依赖 / I/O / SDK / 事件机制 / runtime 目录 / 并发）。

来源：从 ChronoAIProject/fkst clean-init 抽取（历史留在该 public repo）。

本仓库作为稳定发布衬底消费前，应以 `cargo build --workspace`、`cargo test --workspace -- --test-threads=1`、`--self-test` 与 `conformance` 结果为准。

⟦AI:FKST⟧

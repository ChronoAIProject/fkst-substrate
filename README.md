# fkst-substrate

fkst-substrate = universal SDLC substrate 引擎（Tier I supervisor + Tier III framework + common）；不含业务 Lua 包（那是 fkst Lua package，另库）。

开发分支：dev。

验证命令：

```sh
cargo build --workspace
cargo test --workspace
```

独立运行：

本仓库内置一个通用最小 package：`examples/minimal-package`。它包含 cron source `tick`、producer、两个 fanout consumers，以及 runtime witness 文件写入，用于证明 source → route → produce → fanout → raise → file 的最小闭环。

```sh
cargo build --workspace
target/debug/fkst-framework --self-test
FKST_RUNTIME_ROOT=/tmp/min-rt-$$ target/debug/fkst-framework conformance \
  --project-root /Users/auric/fkst-substrate/examples/minimal-package \
  --package-root /Users/auric/fkst-substrate/examples/minimal-package
FKST_RUNTIME_ROOT=/tmp/min-rt-$$ target/debug/fkst-framework run \
  /Users/auric/fkst-substrate/examples/minimal-package/departments/producer/main.lua \
  --package-root /Users/auric/fkst-substrate/examples/minimal-package \
  --event '{"type":"tick","payload":{}}'
```

治理与架构：

- `CLAUDE.md`（= `AGENTS.md` 软链）：引擎治理与哲学不动点。
- `SPEC.md`：Tier II 身份锚点（invariant 权威定义）。
- `docs/architecture.md`：详细引擎架构（分层 / 依赖 / I/O / SDK / 事件机制 / runtime 目录 / 并发 / known-good）。

来源：从 ChronoAIProject/fkst clean-init 抽取（历史留在该 public repo）。

本仓库仍在抽取过程中，尚不可作为发布版消费。验证面当前只承诺 `cargo build/test --workspace` 绿。

已知 contract gap（公开消费前必读）：

- **`update` 自更新子命令已删除**：旧实现只服务旧单仓自更新契约，未被 known-good/swap 非测试代码依赖；fkst-substrate 不提供 `update` 命令。
- **`SPEC.md` 引用的 `conformance/run_all.sh` 尚未迁入本仓库**：SPEC 作为身份锚点先行带过，conformance gate 集的迁移/泛化为 deferred（见下）。

Deferred（尚未迁入）清单：

- conformance gate 迁移/泛化
- install/release 脚本
- dogfood 自演化闭环

⟦AI:FKST⟧

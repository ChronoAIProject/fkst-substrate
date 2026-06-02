# fkst-substrate

fkst-substrate = universal SDLC substrate 引擎（Tier I supervisor + Tier III framework + common）；不含业务 Lua 包（那是 fkst Lua package，另库）。

开发分支：dev。

验证命令：

```sh
cargo build --workspace
cargo test --workspace
```

来源：从 ChronoAIProject/fkst clean-init 抽取（历史留在该 public repo）。

本仓库仍在抽取过程中，尚不可作为发布版消费。验证面当前只承诺 `cargo build/test --workspace` 绿。

已知 contract gap（公开消费前必读）：

- **`update` 自更新子命令尚不可用**：`crates/fkst-framework/src/update.rs` 的发布/安装契约仍是旧单仓形态——默认更新源指向 `ChronoAIProject/fkst`，并假设安装 payload 含 `lib/fkst/current/share/fkst/VERSION` 等捆绑 Lua 包布局。fkst-substrate 不带该 payload，故不要对本仓库执行 `update`，直至重新打包。
- **`SPEC.md` 引用的 `conformance/run_all.sh` 尚未迁入本仓库**：SPEC 作为身份锚点先行带过，conformance gate 集的迁移/泛化为 deferred（见下）。

Deferred（尚未迁入）清单：

- examples/ 示例包（证明任意 package root 可加载）
- conformance gate 迁移/泛化
- install/release 脚本 + `update.rs` 发布/安装契约改写（去 share/fkst 捆绑假设、更新源指向本仓）
- dogfood 自演化闭环

⟦AI:FKST⟧

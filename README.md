# fkst-substrate

fkst-substrate = universal SDLC substrate 引擎（Tier I supervisor + Tier III framework + common）；不含业务 Lua 包（那是 fkst Lua package，另库）。

开发分支：dev。

验证命令：

```sh
cargo build --workspace
cargo test --workspace
```

来源：从 ChronoAIProject/fkst clean-init 抽取（历史留在该 public repo）。

本仓库仍在抽取过程中，尚不可作为发布版消费。

Deferred（尚未迁入）清单：

- examples/ 示例包
- conformance gate 迁移/泛化
- install/release 脚本
- update.rs 中 share/fkst payload 假设修正
- dogfood 自演化闭环

⟦AI:FKST⟧

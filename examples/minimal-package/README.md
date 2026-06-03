# minimal-package 最小 fixture

`examples/minimal-package` 是一个最小 package-root fixture。它只包含一个 cron source 和一个 log-only department：

- `raisers/tick.lua` 每 5 秒产生 `tick`。
- `departments/logger/main.lua` 消费 `tick`，在 `pipeline(event)` 中写一行结构化日志。

这个 fixture 用来证明 `--package-root` 可以独立加载、通过 graph validation，并且 source 事件可以 dispatch 到 `pipeline(event)`。它不演示业务扫描、派生工作、完成事实或持久化策略。

运行方式：

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

`run logger` 的 stderr 应包含结构化日志行，例如 `LEVEL=info MSG=event received: tick`。

⟦AI:FKST⟧

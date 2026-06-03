# minimal-package 引擎事件 fixture

`examples/minimal-package` 是一个 package-root fixture，用来展示引擎事件从 source 到 Department 再到 `raise` 派发的真实 surface：

- `raisers/tick.lua` 每 1 秒产生 `tick`。
- `departments/producer/main.lua` 消费 `tick`，调用 `raise("example_event", payload)`。
- `departments/consumer/main.lua` 消费 `example_event`，只读并打印完整标准事件。

这个 fixture 用来证明 `--package-root` 可以独立加载、通过 graph validation，并且真实 supervise 可以完成 `cron source -> producer -> raise -> dispatch -> consumer`。它不演示业务扫描、派生工作、完成事实或持久化策略。

Department 收到的标准事件结构是 `Event{queue,payload,ts}`。`raise` 自身只输出 queue 和 payload；runtime 在重新派发时生成 `ts`。

`run --event` 是单 pipeline 注入，示例里的事件不会获得 runtime 生成的 `ts`；consumer 示例使用 `ts=0` 只是占位。只有真实 supervise 派发时，runtime 才会补上 numeric `ts`。

producer 的 `RAISED:` 解码后形状如下，注意这里还没有 `ts`：

```json
[{"queue":"example_event","payload":{"from":"producer","note":"opaque example payload","source_queue":"tick","source_raiser":"tick"}}]
```

真实 supervise 派发给 consumer 的标准事件形状如下，`ts` 是 runtime 生成的数字，实际值会变：

```json
{"queue":"example_event","payload":{"from":"producer","note":"opaque example payload","source_queue":"tick","source_raiser":"tick"},"ts":1234567890}
```

单点查看命令不会经过路由，只是向单个 pipeline 注入事件：

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
    "$tmp_host/departments/producer/main.lua" \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --event '{"queue":"tick","payload":{"raiser":"tick"}}'
)
(
  cd "$tmp_host" &&
  "$repo/target/debug/fkst-framework" run \
    "$tmp_host/departments/consumer/main.lua" \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --event '{"queue":"example_event","payload":{"from":"producer","note":"opaque example payload","source_queue":"tick","source_raiser":"tick"},"ts":0}'
)
```

`run producer` 的 stdout 应包含 `RAISED:`，解码后 queue 是 `example_event`。`run consumer` 的 stderr 应包含 `consumer received Event{queue=example_event`。这两个命令都是单 pipeline 注入，不经过 supervise 路由。

真实路由由 supervise 完成，运行后用 `Ctrl-C` 停止：

```sh
FKST_RUNTIME_ROOT="$tmp_host/.fkst/runtime" \
  "$repo/target/debug/fkst-framework" supervise \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --framework-bin "$repo/target/debug/fkst-framework"
```

consumer 的完整事件日志会落在 `<RT>/logs/framework-child/` 下；本 fixture 的集成测试覆盖了真实 producer -> consumer 路由。

⟦AI:FKST⟧

# minimal-package 引擎事件 fixture

`examples/minimal-package` 是一个 package-root fixture，用来展示 source、Department 和 `raise` 的引擎 surface：

- `raisers/tick.lua` 每 1 秒产生 `tick`。
- `departments/producer/main.lua` 消费 `tick`，调用 `raise("example_event", payload)`。
- `departments/consumer/main.lua` 消费 `example_event`，只读并打印完整标准事件。
- `tests/sanity_test.lua` 演示 test-mode Lua 单测文件返回 `{ test_* = fn }` 表。

这个 fixture 用来证明 `--package-root` 可以独立加载、通过 graph validation、两个 Department 可以被直接触发执行，并且 producer 的真实 `RAISED:` payload 可被 consumer 作为标准事件消费。它不演示业务扫描、派生工作、完成事实或持久化策略。

Department 收到的标准事件结构是 `Event{queue,payload,ts}`，其中 `ts` 是 Unix 毫秒。`raise` 自身只输出 queue 和 payload；runtime 在重新派发时生成 `ts`。

`run --event` 是单 pipeline 注入，不经过 supervise 路由。示例里的事件不会获得 runtime 生成的 `ts`；consumer 示例里的 numeric `ts` 是注入的标准事件值。真实 dispatch 由 runtime 生成 `ts`。

producer 的 `RAISED:` 解码后形状如下，注意这里还没有 `ts`：

```json
[{"queue":"example_event","payload":{"from":"producer","note":"opaque example payload","source_queue":"tick","source_raiser":"tick"}}]
```

真实 dispatch 派发给 consumer 的标准事件形状如下，`ts` 是 runtime 生成的 Unix 毫秒，实际值会变：

```json
{"queue":"example_event","payload":{"from":"producer","note":"opaque example payload","source_queue":"tick","source_raiser":"tick"},"ts":1717420800000}
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
target/debug/fkst-framework test \
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

Lua 单元测试由 `fkst-framework test` 执行。runner 只扫描 `departments/*/*_test.lua` 和 `tests/*_test.lua`，不全树递归，也不扫描 `raisers/` 或 `fkst/`。`fkst.test` 只在 test-mode 注册，断言包含 `eq`、`is_true`、`raises`、`is_nil`；`run_department(path, event[, opts])` 可用 fresh Lua state 运行一个 department entrypoint 并返回 captured raises。它不是 production SDK surface，也不是 mock / fixture 框架。

可以手动运行 supervise 观察真实路由，运行后用 `Ctrl-C` 停止；它不是 example 测试依赖：

```sh
FKST_RUNTIME_ROOT="$tmp_host/.fkst/runtime" \
  "$repo/target/debug/fkst-framework" supervise \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --framework-bin "$repo/target/debug/fkst-framework"
```

consumer 的完整事件日志会落在 `<RT>/logs/framework-child/` 下。真实 routing / dispatch 由 framework 自身的 supervise / consumer 测试覆盖；本 fixture 的测试只覆盖 graph validation、直接触发 pipeline 行为和 producer -> consumer 契约对接。

⟦AI:FKST⟧

# minimal-package reconciliation 示例

这是一个最小但完整的 reconciliation/control-loop 示例。它有两个 source：

- `raisers/tick.lua` 每 5 秒产生 `reconcile_tick`。
- `raisers/requests.lua` 监听 host root 相对的 `requests/*.md`，产生 `request_changed`。

两个 source 都只触发 `scanner` 做同一件事：全量扫描 host repo 里的 `requests/*.md`，再检查 `state/done/<id>.txt` 是否存在。没有完成事实的请求会被 `raise("work", ...)` 放回当前内存队列。`worker` 消费 `work`，用 `with_lock("worker-" .. id, ...)` 抢一次性租约，在锁内重查 done 文件；仍未完成时才写 `state/done/<id>.txt`。

`crash = redo`。事件队列是瞬时的，framework 进程崩溃或重启会丢掉在飞事件；这不是恢复问题。下一次 cron tick，或 file_watch 启动扫描/文件变更，会重新扫描 `requests/` 与 `state/done/`。只要没看到 done 文件，scanner 就会重新 enqueue。`with_lock` 只是进程内租约，进程死后文件描述符关闭，锁自动释放。

durable 真相分两档。本示例为了本地演示，把完成事实写成 host repo 普通文件 `state/done/<id>.txt`，这个目录是运行产生物。真正跨机或长期保留的完成事实应升级为 git commit 或外部源，例如 GitHub issue 状态；scanner 仍从这些 durable 源重新推导未完成工作。

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
  FKST_RUNTIME_ROOT="$tmp_host/.fkst/runtime" "$repo/target/debug/fkst-framework" run \
    "$tmp_host/departments/scanner/main.lua" \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --event '{"type":"reconcile_tick","payload":{}}'
)
(
  cd "$tmp_host" &&
  FKST_RUNTIME_ROOT="$tmp_host/.fkst/runtime" "$repo/target/debug/fkst-framework" run \
    "$tmp_host/departments/worker/main.lua" \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --event '{"type":"work","payload":{"id":"req-001","request_path":"requests/req-001.md"}}'
)
```

scanner 输出应包含一行 `RAISED:` 前缀的 base64 编码事件，解码后 queue 为 `work`。worker 运行后会产生 `state/done/req-001.txt`；再次运行 scanner 时不会再为 `req-001` raise work。

⟦AI:FKST⟧

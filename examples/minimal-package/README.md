# minimal-package log-only 示例

这是一个最小 log-only 示例。它有两个 source：

- `raisers/tick.lua` 每 5 秒产生 `reconcile_tick`。
- `raisers/requests.lua` 监听 host root 相对的 `requests/*.md`，产生 `request_changed`。

两个 source 都只触发 `scanner` 做同一件事：全量扫描 host repo 里的 `requests/*.md`，并把每个请求通过 `raise("work", ...)` 放回当前内存队列。`worker` 消费 `work`，校验 payload id，读取请求摘要，并通过 `log.info` 写结构化过程日志。

`crash = redo`。事件队列是瞬时的，framework 进程崩溃或重启会丢掉在飞事件；这不是恢复问题。下一次 cron tick，或 file_watch 启动扫描/文件变更，会重新扫描 `requests/` 并再次 enqueue。这个示例接受每 tick 重新 raise，不证明幂等。

日志是过程可追溯 scratch，不是事实源、reconciliation 输入或 accepted state。真实 package 的幂等 done 事实应来自 git commit、外部源或明确的 host filesystem fact；scanner 再从这些 durable 源重新推导未完成工作。

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
    "$tmp_host/departments/scanner/main.lua" \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --event '{"type":"reconcile_tick","payload":{}}'
)
(
  cd "$tmp_host" &&
  "$repo/target/debug/fkst-framework" run \
    "$tmp_host/departments/worker/main.lua" \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --event '{"type":"work","payload":{"id":"req-001","request_path":"requests/req-001.md"}}'
)
```

scanner 输出应包含一行 `RAISED:` 前缀的 base64 编码事件，解码后 queue 为 `work`。worker 的 stderr 应包含结构化日志行，例如 `LEVEL=info MSG=work completed: req-001`。再次运行 scanner 仍会为 `req-001` raise work。

⟦AI:FKST⟧

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

本仓库内置一个 reconciliation/control-loop 最小 package：`examples/minimal-package`。它声明 cron source `reconcile_tick`、file_watch source `request_changed`、`scanner` 与 `worker`。cron 和文件变更都会触发 scanner 全量扫描 host repo 里的 `requests/*.md`，只为 `state/done/<id>.txt` 不存在的请求 raise `work`；worker 抢一次性 `with_lock` 租约，重查 done 文件后写入 host repo 普通文件作为完成事实。

`FKST_RUNTIME_ROOT` 仍是引擎 scratch 配置，用于 worktree、codex permit、lock 与 log 等运行时落点；这个示例 Lua 不读取它，也不把 `<RT>` 当 package 状态目录。示例运行产生的 `state/done/` 是 host repo 文件，用来演示本地 durable 事实。

下列命令证明的范围如下：

- `conformance`：minimal-package 的 scanner/worker 图通过 validation。
- `run scanner`：单个 scanner pipeline 扫描请求并为未完成请求输出一行 `RAISED:` 前缀的 base64 编码事件，解码后 queue 为 `work`。
- `run worker`：单个 worker pipeline 消费 work，并写入 `state/done/req-001.txt`。

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

scanner 输出应包含一行 `RAISED:` 前缀的 base64 编码事件，解码后 queue 为 `work`。worker 运行后，`$tmp_host/state/done/req-001.txt` 存在；再次运行 scanner 时不会再为 `req-001` raise work。

## 发布边界

fkst-substrate 的 accepted release state 来自外部 release pipeline，而不是 engine runtime 内部状态。推荐外部链路是 build → test → `--self-test` → conformance → 签名 artifact → deploy → canary / 回退策略。engine 无 runtime accepted-state/回退；发布安全是外部策略。

host/package 可以在此 runtime 上编排 SDLC 工作流，但这属于外部行为层，不是 engine 内建职责。

engine 队列是瞬时的；durable 真相属于 git commit、host repo 文件或外部源，不在 engine 内部维护。崩溃后由 cron/file_watch 触发 scanner 重新扫描 durable 源并推导未完成工作。

## 文档

- `CLAUDE.md`（= `AGENTS.md` 软链）：引擎治理与哲学不动点。
- `SPEC.md`：Tier II 身份锚点。
- `docs/architecture.md`：详细引擎架构（分层 / 依赖 / I/O / SDK / 事件机制 / runtime 目录 / 并发）。

来源：从 ChronoAIProject/fkst clean-init 抽取（历史留在该 public repo）。

本仓库作为稳定发布衬底消费前，应以 `cargo build --workspace`、`cargo test --workspace -- --test-threads=1`、`--self-test` 与 `conformance` 结果为准。

⟦AI:FKST⟧

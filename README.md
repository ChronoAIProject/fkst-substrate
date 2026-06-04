# fkst-substrate

fkst-substrate 是稳定发布的受监督事件 / SDK / 进程衬底：Tier I supervisor + Tier III framework + common。它不包含业务 Lua 包；业务行为由外部 package root 或 host root 注入。

开发分支：dev。

验证命令：

```sh
cargo build --workspace
cargo test --workspace -- --test-threads=1
target/debug/fkst-framework test \
  --project-root "$PWD/examples/minimal-package" \
  --package-root "$PWD/examples/minimal-package"
```

这些 gate 现已收敛进 `scripts/verify.sh`，本地与 CI（`.github/workflows/ci.yml`，单 job、stable Rust、push 到 `dev` 与 PR 到 `dev` 触发）都通过它执行，避免本地与 CI 验证漂移。

## 配置机制

引擎操作配置由 `crates/fkst-framework/src/config_registry.rs` 中的静态 typed registry 声明。读取优先级固定为 process env → host `fkst.env` → operational 默认；HostFact 缺失 fail-closed。registry 只读，没有 set/apply/watch、YAML/DSL/manifest/plugin 或 per-key `tunables/*.txt` 兼容层。

5 个 knob:

- Operational: `FKST_QUEUE_CAPACITY` 默认 `16`
- Operational: `FKST_DEPARTMENT_DEFAULT_STALL_WINDOW` 默认 `30s`
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

本仓库内置一个 package-root fixture：`examples/minimal-package`。它声明一个 cron source `tick`、一个 producer department 和一个 consumer department。cron source 产生 `tick` queue 事件；producer 消费 `tick` 后 `raise("example_event", payload)`；consumer 消费 `example_event`，只读并打印完整标准事件。

`FKST_RUNTIME_ROOT` 仍是引擎 scratch 配置，用于 worktree、codex permit、lock 与 log 等运行时落点；这个 fixture 的 Lua 不读取它，也不把 `<RT>` 当 package 状态目录。fixture 只展示 package-root 独立加载、graph validation、两个 Department 的直接触发 pipeline 行为，以及 producer 真实 `RAISED:` 输出可被 consumer 作为标准事件消费的契约。

下列命令证明的范围如下：

- `conformance`：minimal-package 的单 source / 双 department / 双 queue 图通过 validation。
- `test`：扫描 `departments/*/*_test.lua` 和 `tests/*_test.lua`，执行返回表里的 `test_*` 函数。
- `run producer`：单个 producer pipeline 消费注入的 `tick` 事件，并在 stdout 输出 `RAISED:`。
- `run consumer`：单个 consumer pipeline 消费注入的标准事件，并向 stderr 打印 `Event{queue,payload,ts}`。
- `producer -> consumer` 契约测试：直接把 producer 的真实 payload 放进 consumer 标准事件，不经过 supervise dispatcher 路由。

Department 收到的标准事件结构是 `Event{queue,payload,ts}`，其中 `ts` 是 Unix 毫秒。producer 的 `RAISED:` 解码后是 queue + payload，还没有 `ts`：

```json
[{"queue":"example_event","payload":{"from":"producer","note":"opaque example payload","source_queue":"tick","source_raiser":"tick"}}]
```

`run --event` 是单 pipeline 注入，不经过 supervise 路由。示例里的事件不会获得 runtime 生成的 `ts`；consumer 示例里的 numeric `ts` 是注入的标准事件值。真实 dispatch 由 runtime 生成 `ts`。

真实 dispatch 派发给 consumer 的标准事件会包含 runtime 生成的 Unix 毫秒 `ts`，实际值会变：

```json
{"queue":"example_event","payload":{"from":"producer","note":"opaque example payload","source_queue":"tick","source_raiser":"tick"},"ts":1717420800000}
```

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

上面两个 `run` 命令是单 pipeline 注入，不经过路由。可以手动运行 supervise 观察真实 producer -> consumer 路由，运行后用 `Ctrl-C` 停止；它不是 example 测试依赖：

```sh
FKST_RUNTIME_ROOT="$tmp_host/.fkst/runtime" \
  "$repo/target/debug/fkst-framework" supervise \
    --project-root "$tmp_host" \
    --package-root "$tmp_host" \
    --framework-bin "$repo/target/debug/fkst-framework"
```

consumer 的完整事件日志会落在 `<RT>/logs/framework-child/` 下。真实 routing / dispatch 由 framework 自身的 supervise / consumer 测试覆盖；minimal-package 测试不重复启动 supervise。

Lua 单元测试由 `fkst-framework test` 执行。runner 只发现 package root 和 host root 下的 `departments/*/*_test.lua` 与 `tests/*_test.lua`，不全树递归，也不扫描 `raisers/` 或 `fkst/`。测试文件应 `return { test_name = function() ... end }`；runner 按文件路径和 `test_*` key 排序，失败后继续执行后续测试，最后输出通过 / 失败汇总。

`fkst.test` 只在 `test` 子命令的 Lua state 中注册，不属于 production Lua SDK surface；`run` 与 `supervise` 模式不可依赖它。当前断言只有 `eq(actual, expected[, msg])`、`is_true(value[, msg])`、`raises(fn[, msg])` 和 `is_nil(value[, msg])`。test-mode 还提供 `run_department(path, event[, opts])`，用 fresh Lua state、production SDK 和独立 raise buffer 执行一个 department entrypoint，返回 `{ exit_code = int, raises = { { queue = string, payload = table }, ... } }`；相对路径按 package root 解析，`opts.cwd`、`opts.env`、`opts.path_prepend` 只作用于该次执行并随后恢复。这是最小单测工具，不提供 fixture、mock、hook 或测试框架 DSL；除非有意验证真实 CLI 路径，Lua 单测不应调用 `spawn_codex_sync`。

production Lua SDK 包含 `once(key, fn) -> boolean`。它是 best-effort per-key de-bounce scratch marker，不是 durable state。`key` 必须是非空字符串；framework 对 key bytes 做 hex 编码，在 `<RT>/locks/once-<hex>` 上获取 exclusive flock，再检查 `<RT>/marks/<hex>`。marker 已存在时返回 `false` 且不调用 `fn`；marker 不存在时调用 `fn`，成功后写入 marker 并返回 `true`；`fn` 失败时错误原样传播且不写 marker，后续调用会重试。

`once` 的可观察性来自 engine log 和 runtime scratch：skip / run 决策会写入可 grep 的 `once decision=... key=...` 结构化日志；marker 内容只提供 `key` 与 `marked_at` 的人工可读提示，不参与判重；LIVE lock holder 可用 `lsof <RT>/locks/once-<hex>` 查看。

## 安装与更新

`scripts/install.sh` 是 operator 便利脚本，和 `scripts/verify.sh` 同级，不是 engine surface：它只生成本机 operator 配置,不改 SPEC、conformance、supervisor 或任何二进制默认值。更新走独立的 `fkst-update` 二进制(见下)。

`scripts/install.sh`：

```sh
scripts/install.sh
```

它 `cargo build --release --workspace`，把 `fkst-supervisor` 与 `fkst-framework` 装进 `$FKST_HOME/bin`（默认 `~/fkst/bin`），创建 package root（默认 `~/fkst-packages`），并生成启动器 `$FKST_HOME/bin/fkst-run`，由它设置 `FKST_PACKAGE_ROOT` / `FKST_RUNTIME_ROOT` 并 `exec fkst-supervisor`。引擎二进制本身**没有默认 package root**（缺 `FKST_PACKAGE_ROOT` / `--package-root` 时 fail-closed）；`~/fkst-packages` 这个默认值只活在生成的启动器里，是 operator config。路径可用 env 覆盖：

```sh
FKST_HOME=/opt/fkst FKST_PACKAGE_ROOT=/srv/fkst-packages scripts/install.sh
```

装好后启动（启动器 `cd` 到 package root，PKG == HOST 单 root）：

```sh
~/fkst/bin/fkst-run
```

更新走 `fkst-update`（独立二进制，`crates/fkst-update`，由 `install.sh` 一并装入 `$FKST_HOME/bin`）：从 GitHub Release 下载外部 release pipeline 产出的 `fkst-<target>.tar.gz` 与 `SHA256SUMS`、校验 SHA-256 后**逐二进制原子替换**（每个 rename 在 bin dir 文件系统上原子）`$FKST_HOME/bin` 里的 `fkst-supervisor`、`fkst-framework`，不重建、不联系源码。它只做 verify+swap，**不**实现 known-good / accepted-state / rollback / 进程重启（这些仍是外部策略，见「发布边界」）。

```sh
fkst-update                 # 装到 latest release
fkst-update --tag v0.1.0    # 装到指定 tag
fkst-update --bin-dir /opt/fkst/bin
```

`fkst-update` 消费的 artifact 由 `.github/workflows/release.yml` 在打 `v*` tag 时产出（构建两 target 的 `fkst-<target>.tar.gz` + 聚合 `SHA256SUMS` 发 GitHub Release）。**没有发布过 release 时 `fkst-update` 无 artifact 可拉**（报 `no-matching-release`）；先 `git tag v0.1.0 && git push origin v0.1.0` 触发发布。

持有源码 checkout、想直接从 dev 跟踪而不走 release 的话，更新就是一行 `git pull --ff-only && scripts/verify.sh && scripts/install.sh`（先过 gate 再重装），不需要单独的更新脚本。

自动更新是把 `fkst-update` 交给 operator 的调度器，引擎不拥有调度。macOS 下用一个 LaunchAgent（落 `~/Library/LaunchAgents/com.fkst.update.plist`，属于本机 ops，不进本仓库），把下面示例里的 `/Users/you/...` 换成你的实际路径：

```xml
<plist version="1.0"><dict>
  <key>Label</key><string>com.fkst.update</string>
  <key>ProgramArguments</key>
  <array><string>/Users/you/fkst/bin/fkst-update</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/usr/bin:/bin</string></dict>
  <key>StartCalendarInterval</key>
  <dict><key>Hour</key><integer>4</integer><key>Minute</key><integer>0</integer></dict>
  <key>StandardOutPath</key><string>/tmp/fkst-update.log</string>
  <key>StandardErrorPath</key><string>/tmp/fkst-update.log</string>
</dict></plist>
```

`StartCalendarInterval` 必须同时给 `Hour` 和 `Minute`，否则 launchd 会在该小时内每分钟触发；`fkst-update` 只用到 `/usr/bin` 下的 `curl`、`tar`、`shasum`。`launchctl load ~/Library/LaunchAgents/com.fkst.update.plist` 后每天 04:00 跑一次。注意 `fkst-update` 只换二进制不重启——新版本要等 supervisor 重启才生效（由你的 launchd 服务或手动重启负责）。

operator 的 package/host root（默认 `~/fkst-packages`）需自备：它应是一个 git repo（git-based SDK 用 `git -C <HOST>`），并在 `fkst.env` 里提供必填 HostFact（如 `FKST_CANDIDATE_PREFIX`、`FKST_CANDIDATE_FROM_SEP`），否则相关 department 行为 fail-closed。安装器只建目录，不替你注入业务配置。

## 发布边界

fkst-substrate 的 accepted release state 来自外部 release pipeline，而不是 engine runtime 内部状态。推荐外部链路是 build → test → `--self-test` → conformance → 签名 artifact → deploy → canary / 回退策略。engine 无 runtime accepted-state/回退；发布安全是外部策略。

host/package 可以在此 runtime 上编排 SDLC 工作流，但这属于外部行为层，不是 engine 内建职责。

engine 队列是瞬时的；durable 真相属于 git commit、明确的 host filesystem fact 或外部源，不在 engine 内部维护。

## 文档

- `CLAUDE.md`（= `AGENTS.md` 软链）：引擎治理与哲学不动点。
- `SPEC.md`：Tier II 身份锚点。
- `docs/architecture.md`：详细引擎架构（分层 / 依赖 / I/O / SDK / 事件机制 / runtime 目录 / 并发）。

本仓库作为稳定发布衬底消费前，应以 `cargo build --workspace`、`cargo test --workspace -- --test-threads=1`、`--self-test` 与 `conformance` 结果为准。

⟦AI:FKST⟧

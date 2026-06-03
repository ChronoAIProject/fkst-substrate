# codex-package 引擎 SDK fixture

`examples/codex-package` 是一个 package-root fixture，用来展示现有 Lua SDK 里的 `spawn_codex_sync`：

- `raisers/requests.lua` 通过 `file_watch` 观察 `requests/*.md`，产生 `codex_request`。
- `departments/codex_demo/main.lua` 消费 `codex_request`，调用 `spawn_codex_sync({ prompt, stall_window })`，再 `raise("codex_result", payload)`。
- `departments/codex_demo/prompt.lua` 只包含纯字符串逻辑，不调用 SDK。
- `departments/codex_demo/codex_demo_test.lua` 用 Lua 内置 `assert` 覆盖纯逻辑。

这个 fixture 只演示引擎 surface：file source、Department、现有 codex SDK、`raise` 和可由 `run` 执行的 Lua 断言测试。它不包含业务部门、审查策略、重试策略、完成事实或 host 发布逻辑。

## require 路径

framework 给 package root 注入的 Lua 搜索路径是 `?.lua`、`?/init.lua`、`?/main.lua`。因此同目录的纯逻辑模块使用：

```lua
local prompt = require("departments.codex_demo.prompt")
```

`prompt.lua` 顶层不依赖 SDK，所以 graph validation 加载 Department 顶层代码时不会触发 codex。

## codex SDK

`spawn_codex_sync` 已是引擎 Lua SDK 的固定 surface。示例只使用已有函数，不新增 CLI、SDK、SPEC 或 Lua test runner。

调用形状：

```lua
local result = spawn_codex_sync({
  prompt = "Summarize: input",
  stall_window = 300,
})
```

返回表包含：

```lua
{
  stdout = "...",
  stderr = "...",
  exit_code = 0,
  log_path = "..."
}
```

运行真实 codex 示例时，`PATH` 上必须存在 `codex`。测试不会依赖真实 CLI；测试把 fake `codex` 放到 `PATH` 前面，fake 读取 stdin 后输出固定 marker。

## 单点运行

这些命令都是单 pipeline 注入，不经过 supervise 路由，也不会自动消费 `requests/req-001.md`：

```sh
cargo build --workspace
repo="$PWD"
tmp_host="$(mktemp -d)"
cp -R examples/codex-package/. "$tmp_host/"
target/debug/fkst-framework conformance \
  --project-root "$tmp_host" \
  --package-root "$tmp_host"
```

Lua 单元测试用内置 `assert` 加现有 `run` 执行；没有独立 Lua test SDK：

```sh
"$repo/target/debug/fkst-framework" run \
  "$tmp_host/departments/codex_demo/codex_demo_test.lua" \
  --project-root "$tmp_host" \
  --package-root "$tmp_host" \
  --event '{}'
```

如果要手动验证 codex 调用路径，可以放一个 fake `codex` 到 `PATH` 前面：

```sh
fake_bin="$(mktemp -d)"
cat > "$fake_bin/codex" <<'SH'
#!/bin/sh
cat >/dev/null
echo FAKE_CODEX_OK
exit 0
SH
chmod 755 "$fake_bin/codex"

PATH="$fake_bin:$PATH" \
FKST_RUNTIME_ROOT="$tmp_host/.fkst/runtime" \
FKST_CODEX_PERMIT_SLOTS=4 \
"$repo/target/debug/fkst-framework" run \
  "$tmp_host/departments/codex_demo/main.lua" \
  --project-root "$tmp_host" \
  --package-root "$tmp_host" \
  --event '{"queue":"codex_request","payload":{"input":"hello"}}'
```

stdout 应包含 `RAISED:`，解码后 queue 是 `codex_result`，payload 里 `summary` 包含 `FAKE_CODEX_OK`，`exit_code` 是 `0`。stderr 应包含 `codex exit_code=0 log=`。

也可以把 fake 换成真实 `codex`，前提是 `PATH` 上已有 `codex`。真实运行会写 codex log 到 runtime log 目录或平台默认日志目录。

⟦AI:FKST⟧

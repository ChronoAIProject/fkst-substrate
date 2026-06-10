# 可靠投递重构：嵌入式持久 delivery（redb）架构定稿

> 状态：已接入 phase-2 cutover。取代 PR#5 的 scratch retry（`retry_state.rs`/`retry_sweep.rs`/`<RT>/{retry,dead}`）。

## 动机

PR#5 在"不持久的 scratch 底座上手搓持久队列保证"，一路打地鼠（净化碰撞静默吞活儿、marker 无界、channel 缓冲丢失、无 jitter 风暴、健康长任务租约双跑、wall-clock 跳、dead_letter 静默丢）。改用**成熟纯 Rust 嵌入 ACID KV（redb）承载 delivery 状态**，fkst 只写薄策略层——基础（持久化/崩溃恢复/事务）不手搓。

## 基座：redb

- 纯 Rust、嵌入、单文件、ACID、MVCC、B-tree、typed KV、支持范围扫描。非 SQL、无服务、无 daemon。
- fkst 不碰 WAL/崩溃恢复；只在 redb 之上写 **5 个动作的策略层**：`enqueue / lease / ack / retry / dead`。
- 关键契合：`commit 先于内存唤醒`、`ack 原子删/移`、`按 not_before 范围扫到期` 全在**单个 redb 写事务**里完成。

## 作用域：混合

- `DeliveryRouter` 按 graph subscription 分流：**可靠订阅 → durable delivery（redb）**；**`M.spec.ephemeral` opt-out 订阅 → 现有内存 Fanout 降级为纯进程内唤醒**（不再承载事实）。
- 改动半径可控；非可靠路径维持现有 at-most-once；不把所有 raise 强行 durable。

## 物理模型：per-(queue, dept) delivery 流

- graph validation 把 `queue -> consumers` 展开；入队时对每个目标 `(queue, dept)` 生成一条**独立 delivery**。
- fanout queue = N 条独立 delivery（各自 ack/retry/dead）；单消费者 = 1 条。
- redb 落地：delivery 记录按 `delivery_id` 主键；按 `(state, not_before)` 或 `(dept, not_before)` 建二级索引表供"到期/可投"范围扫描。`(queue,dept)` 作为记录字段，不需要每对一个物理文件（与 redb 单文件多表契合）。

## 存什么：轻量触发器 + 消费时回源 derive

```
DeliveryEnvelope {
  delivery_id, queue, dept,
  source_kind, source_ref,        // 可回源引用（path / git ref / external id / raiser name），非业务真相
  observed_at, attempt,
  lease_generation, lease_until,  // fencing
  not_before,                     // 退避+jitter 的 eligible 时间
  last_error_excerpt,
}
```
- 队列只存"某 dept 需重新观察某事实"的触发器；**消费时 Department 回源 derive 当前真相**（git/外部源/明确 host fact）。回源失败 = 本次 delivery 失败 → retry/backoff → 超限 DLQ。
- 例外：cron tick 这类无外部实体的触发器可带小 payload（它本就是触发事实）。
- 当前实现允许 bounded payload 作为过渡：可靠 delivery 的 payload 超过引擎上限时 fail-closed，不把无界业务对象落进 redb。完整"trigger-only + consume-time source lookup"是目标契约；拆除现有 handler 对 payload 的依赖需要下游 package 配合，属于后续跨包迁移。
- 不把完整业务对象/审查态/accepted state 塞进队列。

## durable 落点：一等 `FKST_DURABLE_ROOT`

- 新增 `FKST_DURABLE_ROOT` + `DurableLayout`，**不属于 `<RT>`**：`<DURABLE>/delivery.redb`（+ 必要的 meta）。
- `FKST_RUNTIME_ROOT` 继续只放 `worktrees/codex-permits/locks/logs` 等 scratch；`marks/retry/dead` 不再属于 RuntimeLayout。
- 语义：operator 管理、可备份、**可靠投递启用时缺失 fail-closed**；损坏时 fail-closed + 明确错误；恢复 = durable delivery + 外部源重建，不从 logs 重建。

## 状态机（仅 5 动作，别再长成扫描器+marker 海）

`enqueue`（可靠 producer：redb 写事务 commit 成功才唤醒内存 dispatcher）→ `lease`（dispatcher 取 due 记录、写 lease_generation/lease_until）→ `ack`（成功：事务内删 delivery）→ `retry`（失败：attempt+1，`not_before = now + min(cap, base·2^attempt) + bounded_jitter(delivery_id, attempt)`，达上限转 `dead`）→ `dead`（写 dead 表 + 可选 raise `dead_letter` 通知）。

## 崩溃 / 正确性

- **durable commit 先于内存唤醒** → 可靠路径堵死 channel 缓冲丢失（唤醒丢了由 poll/扫 durable 补上）。Fanout 只承载 ephemeral 真事件；可靠订阅使用独立 per-dept wake 信号通知 dispatcher 查 store，wake 失败不影响 publish 成功。
- **运行中 child handle 是进程内权威**；lease 只防"进程重启后过早重投"。ack 必须匹配 `lease_generation`（fencing）→ 旧 lease 完成不误 ack。
- 进程内到期调度用 monotonic（`Instant`/`tokio::time`）；持久 `not_before` 用 wall-clock 仅表"不早于"，时钟回拨致延后可接受，**不接受活任务双 ack**（fencing 兜底）。
- 语义：**at-least-once-until-ack**（非 exactly-once）；Department 回源 + 幂等承受重复。

## 极端清单覆盖

| 问题 | 新架构 |
|---|---|
| 净化碰撞静默吞 | structured `delivery_id`；dedup 仅可选 coalesce 提示，冲突记录不静默丢 |
| marker 无界 | 删 success marker；ack 即删 delivery；dead 表只保留 compact tombstone（id/queue/dept/source/time/attempt/error excerpt），不保留 payload |
| `<RT>` 清/重启风暴 | delivery 在 `FKST_DURABLE_ROOT`，清 `<RT>` 不影响；重启续 lease 过期/ready |
| 无 jitter herd | retry `not_before` 加 bounded jitter；dispatcher 每轮限批取 due |
| 健康长任务双跑 | 运行 handle 权威 + lease fencing；只跨重启重投 |
| wall-clock 跳 | monotonic 调度；wall-clock 只"不早于"；fencing 防双 ack |
| dead_letter 静默丢 | DLQ 是 durable 表；写 dead 成功才 ack 原 delivery；通知丢账本还在 |
| channel 缓冲丢失 | commit 先于唤醒；reliable wake 仅唤醒非事实；Fanout 只承载 ephemeral 真事件 |
| 资源随并发涨 | per-dept 有界 in-flight + 现有 codex permits；delivery 表按 ack 收敛 |

## PR#5 处置

- **删**：`retry_state.rs`、`retry_sweep.rs`（retry/dead 文件、sweeper 重投、文件 due 扫描）、`RuntimeKind::{Retry,Dead}`、净化 key/flock skip。`RuntimeKind::Marks` 仍保留给 SDK `once`。
- **留/迁**：`RetryDecl`(max_attempts/base/cap) 配置形状、`dead_letter` 概念、backoff 计算（迁入 delivery 层 + **加 jitter**）、`error_excerpt`、`spawn_and_report`/stall/RAISED 解析。
- **重定义**：`retry=false` = "不重试 → 失败直接 ack/drop"（不再表示"不可靠投递"）。可靠/非可靠由 `M.spec.ephemeral` 决定。

## doctrine 文档要点

引擎可持有 **durable 在途 delivery 状态**（仅用于 at-least-once / lease / retry / DLQ）；它**不是**实体业务真相、不是 accepted release state、不是 rollback state。外部源 + git + 明确 host filesystem fact 仍是实体内容真相。队列记录是**触发器 + 投递账本**，消费时 Department 回源 derive。`FKST_RUNTIME_ROOT` 是 scratch；`FKST_DURABLE_ROOT` 是 operator 管理的一等持久边界。

## 实现落点

1. redb API 使用 `delivery_by_id`、`ready_by_dept_due`、`leased_by_dept_until`、`dead_by_id` 和 `meta` 表，所有状态迁移在写事务内完成。
2. `DeliveryRecord` 持有 `delivery_id`、`queue`、`dept`、小 payload、`source_ref`、`cron_payload`、观测时间、attempt、lease generation、lease_until、not_before 和 last error excerpt。
3. `DurableLayout` 只从 `FKST_DURABLE_ROOT` 解析 `<DURABLE>/delivery.redb`；有可靠订阅时缺失 fail-closed，纯 ephemeral host 不创建 durable store。
4. 可靠默认启用，`M.spec.ephemeral = {"queue"}` 对本 Department 的指定 consumed queue opt-out。
5. cron source_ref 来自 raiser name，file_watch source_ref 来自绝对路径；RAISED 进入可靠 queue 时缺 source_ref fail-closed。
6. dispatcher 由 reliable wake signal + 1s tick 驱动，调用 store lease 处理 due 和过期 lease；Fanout 只服务 ephemeral 事件，不再有文件 sweeper。

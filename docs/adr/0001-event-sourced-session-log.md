# ADR 0001:事件溯源会话日志(Event-sourced session log)

- **状态:** Accepted
- **日期:** 2026-08-14
- **关联:** Phase 0(dsh-alignment);架构 §5.5 / §12 决策表

## 背景(Context)

Phase 0 之前,gasket 只在**一轮成功结束后**把 `AgentMessage` 整批追加到 `messages.jsonl`(`session.append(new_msgs)`)。这带来三个问题:

1. **崩溃 / 失败 / 取消的轮次不留痕**——一轮跑到一半(已经执行了若干工具、已经流出了部分助手消息),如果随后崩溃、报错或被 Ctrl-C 中止,这些**已经发生的副作用**一行都不会落盘;下次恢复时上下文"回滚"到本轮开始之前,仿佛什么都没发生过。
2. **token 预算只在内存里**——压缩用的 `ContextBudget.last_input_tokens` 由调用方在 `on_event` 里喂入,重启即丢失,退化成按消息条数兜底压缩。
3. **真相源分散**——内存 transcript 与磁盘 JSONL 两份,前端又往 localStorage 双写,任何一处不一致都难以裁定。

**需求**:无论一轮以何种方式结束(Completed / Aborted / Error / 进程崩溃),其**已经发生的副作用**必须能从磁盘派生。

## 决策(Decision)

采用**追加写事件日志**(`events.jsonl`,每行一条 `SessionEvent`)作为唯一真相源。一轮的每个可观测事实,在其发生时经注入的同步 `persist` 回调落盘:

| 事件 | 何时落盘 | 落点 |
|---|---|---|
| `TurnStart` / `User` | host 在轮次开始时写入,框定本轮 | `gasket-host/src/lib.rs`(`Host::run_turn`) |
| `Assistant { message, usage }` | 组装完成后、**该消息内任何工具执行之前** | `gasket-core/src/agent_loop.rs`(`run_agent_loop` 内 `persist_event`) |
| `ToolResult` | 每个结果定稿后(含成功 / 工具错误 / hook 拒绝 / 参数错误 / 超限丢弃) | `agent_loop.rs`(`record_tool_result`) |
| `TurnEnd { reason }` | **总是**落盘——成功、失败、中止皆然 | `gasket-host/src/lib.rs`(`run_turn` 尾部) |

**派生而非携带**:历史**从不**由调用方持有,每个轮次由 `derive_messages(log)` 现算(`session_event.rs`)。压缩预算从日志尾部恢复(最后一条 `Assistant` 事件的 `usage`,`run_turn` 内),重启后 token 感知压缩天然存活。

**崩溃安全不变量**:`Assistant` 事件先于其中任何工具落盘——进程在工具执行中途崩溃,日志留下的是诚实的"助手提问了、工具未应答"尾巴,而非幻影。

## 与 dsh 参考设计的三点有意分歧

| # | 分歧 | 理由 |
|---|---|---|
| 1 | **无 merge-extensible ignorable 事件** | dsh 支持多写者按"可忽略事件类型"合并。gasket 是**单写者**:每个会话一个 Host / 一个 loop(由 `turn_in_flight` 槽强制串行)。单写者没有合并,合并机制是死重量,删之。 |
| 2 | **持久化取消原因(D2)** | `TurnEndReason::Aborted { cause: Option<CancelCause> }`(含 `User` / `Parent` / `Hook`)落盘,中止原因跨重启可读。dsh 把取消当裸信号;gasket 记录"谁取消的",恢复后的日志自带解释力。 |
| 3 | **无 request / header 事件** | dsh 记录每次 LLM 请求的元数据(头、请求信封)。gasket **不**持久化请求信封——只存模型可见面(`User` / `Assistant` / `ToolResult`)+ 轮次框定 + usage。请求可从消息列表 + 配置重建;持久化它只会让日志膨胀、对恢复无价值。 |

## 迁移策略(D1)

旧 `messages.jsonl` 在首次打开时**迁移一次**(`SessionManager::open_or_migrate`):每条 `AgentMessage` 经 `SessionEvent::from_message` 包裹 → 整批写 `events.jsonl.tmp` → `sync_all` → `rename`(POSIX 原子)→ 成功后才 `delete_legacy` 删旧文件。

- 迁移前崩溃:只有 `.tmp` 存在,`has_events` 仍为 false,下次重新从完整无损的旧文件迁移(幂等;陈旧 `.tmp` 被整体替换)。
- rename 后、delete 前 崩溃:`events.jsonl` 已完整,`has_events` 短路命中它,残留旧文件无害(原地保留,不值得再加一遍清理)。
- **旧文件迁移成功后不保留**(D1)——不可逆,README / usage 已明示。

## 未知事件类型 → fail-closed

`load_events` 以 `scan_jsonl(…, fail_closed_on_data = true)` 读取。一条**完整**但 `type` 标签不匹配任何已知 `SessionEvent` 变体的行,是 `serde_json::error::Category::Data` 错误——**直接让加载失败并带上文件 + 行号**,而非被当成 torn tail 悄悄抹掉。

理由:字节截断的写只能产生 `Syntax` / `Eof` 错误;因此末行的 `Data` 错误只可能是**版本错位**(更新的 gasket 写了本读者不认识的行)。把它当作 torn tail 修复会**静默销毁数据**。中间行的损坏同样是真实损坏(位腐、外部编辑),同样报错带行号。

(对比:旧 `messages.jsonl` 的恢复策略**冻结不变**——`fail_closed_on_data` 关闭,任何不可解析的末行都按 torn tail 自愈。)

## 结果(Consequences)

**正面**

- 崩溃 / 失败 / 取消的轮次,其已发生副作用全部可派生(回归测试 `mid_turn_failure_preserves_side_effect` 为证)。
- 压缩预算从日志恢复,token 感知压缩跨重启存活,不退化为条数。
- 日志是唯一真相源;REST `GET /api/sessions/{key}/messages` 现读盘 `derive_messages`,为前端提供后端真相端点(D3)。

**负面 / 取舍**

- `persist: None` 路径(裸 `agent_loop` 与既有测试)必须**逐字节不变**——由既有 14 个 loop 测试守护。
- 每事件一次 `O_APPEND` 单次 `write_all` 的 `line\n`,无 per-event fsync(崩溃窗口 = OS 页缓存,与原 JSONL 同级);如需更强可后加 `GASKET_FSYNC=1`。
- 迁移后磁盘格式不可逆(旧文件已删)——迁移测试 + README/usage 明示覆盖。

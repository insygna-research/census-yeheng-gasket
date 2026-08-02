# Gateway 会话管理设计

> 状态:草案 · 日期 2026-08-01 · 阶段 1

## 1. 目标

让 Web/桌面前端能**发现后端磁盘上的会话**(CLI 创建的、其他设备创建的、或 localStorage 丢失后的),并切换到它们。当前前端 chats 只存 localStorage,与后端 `~/.gasket/sessions/` 完全脱节。

## 2. 现状(为什么缺口真实)

| 组件 | 状态 |
|---|---|
| 前端侧边栏 UI | ✅ 已完整(create/rename/delete/select,`App.vue`) |
| 前端 chatStore | ✅ 已完整(`chatStore.ts`),但只读写 localStorage |
| 前端 WS 连接 | ✅ `useIMWebSocket` watch chatId → 连 `/ws?user_id=<chatId>`,重连自动 resume |
| 后端 `SessionManager.list()` | ✅ 已实现(列 id/mtime/msg_count) |
| 后端 `resume_or_adopt()` | ✅ 已实现(WS 连接时已调用) |
| **后端 REST 会话列表** | ❌ 不存在 |
| **前端从后端拉取会话** | ❌ 不存在 |

**缺口精准定位:** 不是"前端缺侧边栏",是"后端会话列表没有 REST 暴露 + 前端没有从后端同步的逻辑"。

## 3. 范围(YAGNI 边界)

| 在范围内 | 不在范围内 |
|---|---|
| `GET /api/sessions` 列出后端所有会话 | 会话重命名(前端已有,后端不存名字) |
| 前端能发现并切换到后端会话 | 会话删除(后端不删磁盘文件) |
| 新建会话时用合法 session_id 格式 | 会话搜索/标签/分组 |
| 前端 localStorage 与后端会话合并显示 | 跨设备实时同步 |

## 4. 设计

### 4.1 新增 REST 路由

```
GET /api/sessions
  → 200 [{ "id": "uuid", "msg_count": 42, "mtime": 1722470400000 }, ...]
  → 500 { "error": "..." }
```

返回 `SessionManager::list()` 的结果(`SessionInfo` 序列化)。按 mtime 降序(最新在前)。

**不新增 POST/DELETE:** 会话创建靠 WS 连接(首次消息落盘自动创建),删除靠前端 localStorage(后端 append-only 不删)。REST 只暴露已有的 `list()`。

### 4.2 前端:会话同步

在 `App.vue` 或 `chatStore` 初始化时(`onMounted`),拉取 `GET /api/sessions`,把后端有、前端 localStorage 没有的会话**合并**进 chatStore:

```typescript
// chatStore 新增 syncFromBackend()
async syncFromBackend() {
  const res = await fetch(`${baseUrl()}/api/sessions`);
  const sessions = await res.json();  // [{id, msg_count, mtime}]
  for (const s of sessions) {
    if (!this.chats.find(c => c.id === s.id)) {
      // 后端有、前端没有 → 合并进来(消息懒加载:切换时 WS resume 读回)
      this.chats.push({
        id: s.id,
        name: `Session (${s.msg_count} msgs)`,
        messages: [],  // 空,切换时 WS resume 会恢复
        ...
      });
    }
  }
}
```

**消息懒加载:** 合并的会话 messages 为空。用户点击切换时,WS 重连触发 `resume_or_adopt`,后端把磁盘历史读回。前端不需要单独拉消息的 REST。

### 4.3 新建会话 session_id 格式

当前前端 `chat_<timestamp>_<random>` 含下划线,通过 `is_valid_session_id`(允许 `_`)。**不改**——格式合法、可做 session_id。但后端 CLI 用 UUID v4。两种格式并存,不影响功能(session_id 只做路径组件 + JSONL 文件名)。

## 5. 不碰

- WS 协议不变(重连已 resume)。
- 磁盘 JSONL 格式不变。
- `SessionManager` API 不变(只复用 `list()`)。
- CLI 不变。
- 前端侧边栏组件结构不变(只加初始化同步逻辑)。

## 6. 验收

1. `GET /api/sessions` 返回后端磁盘上所有会话。
2. 前端首次加载,localStorage 没有但磁盘有的会话出现在侧边栏。
3. 点击合并进来的会话 → WS 重连 → 历史消息恢复(resume)。
4. `cargo check + test` 全绿。
5. 新增 REST handler 单测。

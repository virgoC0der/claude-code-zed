# Zed 活跃文件感知（Active-File Awareness via SQLite Watcher）— 设计文档

> 日期：2026-06-09
> 状态：设计待评审
> 关联：`docs/protocol.md` §3.2（`getOpenEditors`）、§3.3（`selection_changed`）、§9（session-aware routing）

## 1. 背景与目标

### 目标
让 Claude Code 在不需要用户主动操作的情况下，**准实时**感知到「当前正在编辑的活跃文件」——对标 JetBrains 插件已实现的能力。

具体而言：当用户在 Zed 中切换 / 打开文件时，已连接的 Claude Code 会话通过 MCP 工具 `getOpenEditors` / `getCurrentSelection` 能看到该会话对应工作区的活跃文件，无需用户按任何快捷键。

### 用户已确认的范围与取舍（brainstorming 阶段）
- **体验**：真·自动实时（非「按需快照」、非「顺带上报」）。
- **感知范围**：仅当前活跃文件（`items.active = 1` 对应的 editor），不要求枚举所有 tab。
- **窗口匹配判据**：用 **Claude Code 会话的 cwd** 去匹配 Zed 打开的 worktree（而非靠 macOS 窗口焦点 / 标题匹配）。
- **实现形态**：sidecar 内置 watcher 模式（非独立子进程）。
- **已接受的代价**：
  1. 准实时（秒级延迟，非毫秒级）——源于 Zed 写盘节流，非进程内事件。
  2. 依赖 Zed 私有 SQLite 表结构（`editors` / `items` / `workspaces`）——靠版本探测 + 优雅降级兜底。

## 2. 现状分析（已验证）

### Sidecar 端：接收与 serve 管道已完整存在
- `getOpenEditors` MCP 工具已实现（`src/mcp/tools.rs:117`），返回 `{uri, isActive, isPinned, isPreview, isDirty?, languageId?}`。
- `getCurrentSelection` / `getLatestSelection` 已实现（同文件）。
- `selection_changed` 通知机制已实现（`src/ipc/server.rs:444`），含工作区感知路由。
- IPC server 已能接收并处理 `IpcFrame::OpenEditors`（`server.rs:394`）和 `IpcFrame::Selection`（`server.rs:216`），写入 `EditorState`。

### 缺口：触发侧
当前**唯一**会向 sidecar 发帧的，是用户主动按 `cmd-ctrl-c` 触发的 task，且只发 `at_mention`。没有任何机制在「切换 / 打开文件」时自动上报当前文件。

### 根本约束
Zed 的 `zed_extension_api`（≤0.7，当前 Zed 1.5.4）既不暴露编辑器 primary selection，也不提供「活跃文件变化」事件 hook 给扩展。因此自动感知**无法**通过 Zed 扩展实现，只能旁路。

## 3. 关键发现：Zed 本地状态可读且实时（已实测）

### 存储位置
```
~/Library/Application Support/Zed/db/<channel>/db.sqlite
```
其中 `<channel>` 为 `0-stable`（stable 通道）、可能为 `0-preview` / `0-dev` 等。`db.sqlite-wal` 是 WAL 文件，真正的实时写入落在这里（实测 mtime 与当前时间差 ~10s 内，证明 Zed 持续写盘而非仅退出时持久化）。

### 相关表结构（实测，Zed 1.5.4）
```sql
-- workspaces：每个曾打开的工作区一行
workspace_id INTEGER PRIMARY KEY,
paths        TEXT,        -- worktree 绝对路径
timestamp    TEXT,        -- DEFAULT CURRENT_TIMESTAMP，状态写入时更新（UTC）
session_id   TEXT,        -- 当前 Zed 进程会话 ID；窗口关闭后清空
window_id    INTEGER,     -- 同一真实窗口的多个 worktree 共享此值
...

-- items：每个 pane 内的 item（编辑器 tab 等）
item_id      INTEGER,
workspace_id INTEGER,
pane_id      INTEGER,
kind         TEXT,        -- 'Editor' 等
active       INTEGER,     -- 1 = 该 workspace 内的活跃 item
...

-- editors：editor item 的详情
item_id      INTEGER,
workspace_id INTEGER,
path         BLOB,        -- 文件绝对路径
...
```

### 判据链（全部从盘上硬读，非推测；已在当前数据上验证唯一命中）

```sql
-- 1. 当前会话 ID = 还开着的窗口的标志
SELECT session_id FROM workspaces
WHERE session_id <> '' ORDER BY timestamp DESC LIMIT 1;

-- 2. 用 Claude 会话的 cwd 匹配「当前会话开着的 worktree」，取活跃文件
SELECT e.path
FROM workspaces w
JOIN items   i ON i.workspace_id = w.workspace_id AND i.active = 1 AND i.kind = 'Editor'
JOIN editors e ON e.item_id = i.item_id AND e.workspace_id = w.workspace_id
WHERE w.session_id = :current_session
  AND (:cwd = w.paths OR :cwd LIKE w.paths || '/%')   -- cwd 等于或在 worktree 之下
ORDER BY length(w.paths) DESC                         -- 嵌套 worktree：最长前缀（最具体）优先
LIMIT 1;
```

**为什么 cwd 匹配是正确判据**（验证结论）：
- `session_id = 当前会话` 精确切出「本次 Zed 启动还开着的窗口」，排除已关闭的历史记录（关闭时 Zed 清空 `session_id`）。
- 用 cwd 匹配规避了「焦点歧义」：SQLite **没有**「哪个窗口此刻被 OS 聚焦」这个信息（纯内存 UI 状态，不落盘）。但每个 Claude 会话有确定的 cwd，按 cwd 各自匹配，天然支持多窗口多会话、互不干扰。
- `length(paths) DESC` 处理嵌套 worktree（如 `/a` 与 `/a/inner` 同时打开）：更具体的路径赢。
- 实测三个不同 cwd（`claude-code-zed`、`workspace-prod-af` 子目录、`prod-af-agent` 子目录）全部唯一命中正确 worktree 与活跃文件；同一窗口内的 `feed-worker` 不会被 `claude-code-zed` 的 cwd 误匹配。

### 与现有 session-routing 同源
sidecar 已经知道每个连接的 Claude 会话的 `workspace_root`（从 `clientInfo.cwd` 或 `--workspace` 解析并 canonicalize，见协议 §9「Workspace identification」）。本方案直接复用这个 cwd 概念——不引入任何新的焦点 / 标题依赖。

## 4. 架构设计

### 数据流
```
用户在 Zed 切换 / 打开文件
   ↓ Zed 将状态写入 db.sqlite + WAL（实测秒级）
Watcher（sidecar 内置后台任务）
   ↓ notify crate 监听 db.sqlite-wal 文件事件 → 去抖（debounce）
   ↓ 对每个已连接的 Claude 会话：
   ↓   用其 canonical cwd 执行 §3 判据查询 → 得到活跃文件路径
   ↓   与上次该会话已上报的活跃文件比对（dedup）
   ↓   若变化：构造 selection（空选区 / 仅 filePath）+ open_editors 单条
   ↓   写入 EditorState，并对该会话推送 selection_changed（复用现有路由）
   ↓
Claude 的 getOpenEditors / getCurrentSelection 反映正确的活跃文件
```

### 组件与层级归属（遵循 `.harness/project.md` layer_order）

新增一个模块，归属在 `transport` 层之上、`app` 层之下的位置。考虑到它读 SQLite（I/O）且需要访问 registry（已连接会话）与 EditorState，定位为一个独立子模块：

- **`src/zed_watch/mod.rs`** — 新模块，watcher 的公共入口与配置。
- **`src/zed_watch/db_path.rs`** — 解析 Zed db 路径：定位 `~/Library/Application Support/Zed/db/<channel>/db.sqlite`，处理多通道（优先 `0-stable`，可被 CLI flag / env 覆盖）。纯路径逻辑，少量 I/O（目录探测）。
- **`src/zed_watch/schema_probe.rs`** — 启动时校验 `workspaces`（`session_id`/`paths`/`timestamp`/`window_id` 列）、`items`（`active`/`kind` 列）、`editors`（`path` 列）存在。不符合预期则返回探测失败，watcher 静默禁用。
- **`src/zed_watch/query.rs`** — 封装 §3 判据查询：输入 canonical cwd → 输出 `Option<PathBuf>`（活跃文件）。只读连接（`mode=ro`）。
- **`src/zed_watch/watcher.rs`** — 后台 tokio 任务：`notify` 监听 WAL 文件 + 去抖 + 对每个会话 dedup + 调用现有 EditorState/notification 通路。

> **层级与依赖**：`zed_watch` 依赖 `protocol`（帧/通知类型）、`mcp::state::EditorState`、`transport::registry`（枚举会话与其 cwd）、`transport::router`（推 selection_changed）。它被 `app` 层 wiring 启动。SQLite 是新的 I/O 依赖，集中在 `query.rs` / `db_path.rs` / `schema_probe.rs`，绝不出现在 `protocol/` 或 `mcp/`。

### 关键设计决策

1. **SQLite 访问方式**：使用 `rusqlite`，以 **只读** 模式打开（`OpenFlags::SQLITE_OPEN_READ_ONLY`）。WAL 模式下只读连接不阻塞 Zed 的写入。每次查询用短连接或缓存连接（待实现细化）。
   > 需评估 `rusqlite` 是否引入 bundled SQLite（避免与系统库版本冲突）；倾向 `features = ["bundled"]`。

2. **触发机制**：`notify` crate 监听 `db.sqlite-wal` 的变更事件。WAL 频繁写，故必须**去抖**（如 300–500ms，与现有 `selection_changed` 的 300ms 去抖风格一致）。去抖窗口结束后做一次查询批处理。
   > 兜底：若文件监听不可用，退化为低频轮询（如每 2s），由配置控制。

3. **per-session dedup**：watcher 为每个会话记忆「上次上报的活跃文件路径」。仅当变化时才写 EditorState + 推通知，避免每次 WAL 写都触发无谓通知。

4. **会话 cwd 来源**：复用 `transport::registry` 中每个 client 的 canonical `workspace_root`。**已实测验证（2026-06-09）**：项目既有的 `peer-cwd-discovery` 机制（`transport/cwd_resolver.rs`，用 `libproc` 读 Claude CLI 进程的真实 cwd）即使在 **LaunchAgent + `--workspace $HOME`** 部署下，也能把每个会话精确解析到具体项目目录（`workspace_source="peer-cwd-libproc"`），**不会退化成 `$HOME`**。日志样本：

   ```
   client a1a364b0 workspace=Some("/Users/sx.chen/Code/personal/claude-code-zed") workspace_source="peer-cwd-libproc"
   ```

   优先级（沿用协议 §9 + peer-cwd-discovery）：`x-claude-code-workspace` header → `clientInfo.cwd` → **peer-cwd-libproc（实测主力来源）** → `--workspace` 默认值。
   **降级策略（保守兜底）**：仅当某会话的 `workspace_root` 恰好等于 `--workspace` 默认值（即所有更精确来源都失败，理论边界）时，watcher 对该会话不推送活跃文件（记 DEBUG），避免在 `$HOME` 这类过宽根上误报。实测此分支极少触发。

5. **EditorState 写入形态**：活跃文件以 `open_editors` 单条形式写入（`{uri: file://<path>, isActive: true, ...}`），同时构造一个空选区的 `selection_changed`（`isEmpty: true`，无 text）推给对应会话，使 `getCurrentSelection` 与 `getOpenEditors` 一致。
   > 待确认：是否需要同时填 `selection`（光标位置）。MVP 仅文件路径，选区留空。

### 版本探测 + 优雅降级（核心健壮性要求）
- 启动时 `schema_probe` 校验表 / 列存在。不符 → watcher 静默禁用，记一条 WARN（说明 Zed 版本可能不兼容），**主 sidecar 与现有 at-mention 功能完全不受影响**。
- db 文件不存在（Zed 未安装 / 非 macOS）→ watcher 不启动，记 INFO。
- 运行期查询失败（如 Zed 升级中重建 db）→ 记 WARN，跳过本次，下次 WAL 事件重试，不 panic、不退出。

### CLI / 配置
- 新增 flag：`--watch-zed-db`（默认行为待定：建议**默认开启**，因为这是核心价值；提供 `--no-watch-zed-db` 关闭）。
- 新增 flag：`--zed-db-path <PATH>`（覆盖自动探测，便于测试与非标准安装）。
- 去抖 / 轮询间隔暂用代码内常量，不暴露为 flag（YAGNI）。

## 5. 错误处理与日志（遵循 harness 约定）
- 模块边界用 `thiserror` 定义 typed error（如 `ZedWatchError`）。
- `anyhow::Result` 仅在 `app` 层 wiring / `main.rs` 边界。
- 日志用 `tracing`：watcher 启停、探测结果、每次推送（DEBUG）、降级 / 失败（WARN）。无 `println!`/`eprintln!`。
- 不引入 `unsafe`（harness 仅允许 `cwd_resolver.rs` 的既有豁免）。

## 6. 测试策略
- **单元测试**（inline `#[cfg(test)]`）：
  - `query.rs`：用临时 SQLite db（在测试里建表 + 灌入 fixture 行），验证 cwd 匹配的各分支——精确命中、子目录命中、嵌套最长前缀、已关会话（session_id 空）被排除、同窗口多 worktree 不误匹配、无匹配返回 None。
  - `schema_probe.rs`：缺列 / 缺表 / 完整 三种 fixture。
  - `db_path.rs`：通道选择、覆盖路径。
- **集成测试**（`tests/`）：
  - 构造临时 db.sqlite + 模拟一个已连接会话（registry 注入 cwd）→ 修改 db 中 active 行 → 触发查询 → 断言 EditorState 的 open_editors 更新且 selection_changed 被路由到正确会话。
  - dedup：同一活跃文件不重复推送。
  - 降级：schema 不符时 watcher 不影响 at-mention 既有路径。
- 不依赖真实 Zed 进程（用 fixture db），保证 CI 在 macOS runner 上可跑（现有 CI 即 macOS）。

## 7. 明确的非目标（YAGNI）
- 不枚举所有打开的 tab（仅活跃文件）——用户已确认范围。
- 不做 macOS 窗口焦点 / 标题匹配——cwd 匹配已足够。
- 不支持 Linux 的 Zed db 路径探测（本迭代 macOS only；路径模块预留扩展点）。
- 不上报选区文本 / 光标位置（MVP 仅文件路径，空选区）。
- 不触碰 Zed 扩展 API。

## 8. 风险与待确认项（实现前需解决）
1. ~~**`clientInfo.cwd` 精度**~~ — **已解决（2026-06-09 实测）**。既有 `peer-cwd-discovery`（`libproc`）在 LaunchAgent + `$HOME` 部署下仍能精确解析每个会话 cwd，不退化成 `$HOME`。见 §4 决策 4。本功能的端到端链路已用真实运行数据验证通过。
2. **`rusqlite` bundled 依赖体积**：评估对 sidecar 二进制大小 / 编译时间的影响。
3. **WAL 读一致性**：只读连接读 WAL 中未 checkpoint 的数据是否完整可见——`rusqlite` 默认行为需验证。
4. **多通道并存**：用户若同时装 stable + preview，db 路径探测需选对（默认 `0-stable`，flag 覆盖）。
5. **`peer-cwd-discovery` 的活跃性**：peer-cwd 是在 WebSocket 连接建立时解析一次并缓存在 registry。若用户在 Claude 会话存活期间 `cd` 到别的目录，registry 里的 cwd 不会更新。对本功能影响很小（Claude 会话的 cwd 通常等于启动目录、稳定不变），但需在实现时确认是否要在 watcher 侧补一次 cwd 重解析，或接受「以连接时 cwd 为准」。倾向后者（YAGNI）。

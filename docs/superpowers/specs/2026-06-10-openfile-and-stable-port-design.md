# openFile 工具 + 稳定端口自动连接 — 设计文档

> 日期：2026-06-10
> 状态：设计已获用户批准（"可以先做 1、2"）
> 关联：`docs/protocol.md` §3.2 / §5、JetBrains 插件能力调研（本会话）

## 1. 背景与目标

JetBrains / VSCode 官方插件能力调研（实证：反编译 JetBrains 插件 0.1.14-beta jar + grep Claude CLI v2.1.170 二进制）确认了两个高价值、低工程量的缺口：

1. **`openFile` MCP 工具**（CLI 二进制引用 8 处，调用最频繁的可选工具）——Claude 能把用户的编辑器跳转到指定文件:行，"带你看代码"。
2. **稳定端口 + 终端自动连接**——`--port` 固定 WebSocket 端口后，配合 `CLAUDE_CODE_SSE_PORT` 环境变量，在 Zed 内置终端运行 `claude` 免敲 `/ide` 自动连接（CLI 发现逻辑 §5 第 4/5 步实证支持）。

## 2. Ground truth（实证，非推测）

### openFile 的上游契约（VSCode extension.js v2.1.76，verbatim 提取）

入参 schema：
```
filePath: string（必填）
preview: boolean = false
startText: string?   — 在文件内容中定位选区起点的文本模式
endText: string?     — 选区终点文本模式（在 startText 之后查找）
selectToEndOfLine: boolean = false
makeFrontmost: boolean = true
```

行为：
- 相对路径 → 基于第一个 workspace folder 解析为绝对路径。
- `startText` 缺省时：响应为 JSON 文本 `{success:true, filePath, fileUrl, message:"Opened file: <p>"}`。
- `startText` 命中：定位选中并居中显示，响应为**纯文本消息**（如 `Opened file and selected text "<t>"`）。
- `startText` 未命中：`Opened file, but text "<t>" not found`（文件仍打开）。

### Zed 外部入口（实测 Zed 1.5.4 CLI）

- `zed -e <path>:<line>:<col>` — 在已有窗口打开文件并定位到行列（1-indexed）。
- **CLI 无法设置选区**，只能定位光标；也无法后台打开（必聚焦）。

### CLI 自动连接（protocol.md §5，实证自 CLI 二进制）

- 第 4 步：`CLAUDE_CODE_SSE_PORT=N` 已设且恰有一个有效 lock 端口为 N → 直接使用。
- 第 5 步：该环境变量已设 → **触发 auto-connect**（无需 `/ide`）。
- lock 有效性仍要求 workspaceFolders 前缀匹配 CLI cwd —— LaunchAgent 的 `$HOME` workspace 满足 `~/` 下任意项目。

## 3. 设计决策

### 功能 1：openFile

**D1 — 层级合规：`McpResponse::OpenFile` 延迟执行变体。**
harness 规定 `mcp/` 禁止 I/O，而 openFile 需 spawn `zed` 进程。解法：纯函数 `dispatch` 只做参数校验，对 openFile 返回新枚举变体 `McpResponse::OpenFile { id, args }`；由本就做 I/O 的 transport 层（`ws.rs::dispatch_text`，async）执行 spawn 并组装 JSON-RPC 响应。`mcp` 保持纯净、可测。

**D2 — 新模块 `zed_cli/`（层级位置：mcp 与 transport 之间）。**
职责：startText → (line, col) 文本定位（读文件,1-indexed,字节列）、`zed -e` argv 构造、tokio::process spawn（5s 超时）、响应消息构造。二进制名可注入（测试用 fake script）。仅依赖 `protocol`。

**D3 — 能力差异如实降级（不撒谎）。**
- Zed CLI 不能选区 → startText 命中时消息为 `Opened file and positioned at "<t>"`（不说 selected）；`endText`/`selectToEndOfLine` 接受但忽略（参与不了定位）。
- 不能后台打开 → `makeFrontmost:false` 接受但行为仍是聚焦打开。
- `preview` 忽略。
- 差异在 README 与 protocol.md 中明示。

**D4 — 错误形状。**
- 文件不存在：`{success:false, message:"File not found: <p>"}`（不 spawn）。
- spawn 失败/超时/非零退出：`{success:false, message:"Failed to launch zed: <err>"}`。
- 相对路径基准：`EditorState.workspace_folders()` 首项，缺省退回 daemon `--workspace`。

**D5 — 规格联动更新。**
openFile 从 out-of-scope 移入 in-scope：`docs/protocol.md` §3.2 表行改 YES、`openspec/specs/mcp/spec.md` 的 "out-of-scope tools are not advertised" 需求更新、`mcp/tools.rs` 与 `mcp/server.rs` 中 forbidden 列表测试同步（tools_list 变 5 个工具）。`.harness/project.md` layer_order 插入 `zed_cli/`。

### 功能 2：--port 稳定端口

**D6 — `--port <N>`（Option<u16>），指定即 `bind_fixed`，失败 fail-fast。**
端口被占时不回退随机端口——显式意图就显式失败（launchd KeepAlive + ThrottleInterval 会重试并在日志暴露）。未指定时行为不变（`bind_random(16)`）。

**D7 — plist 模板加 `--port 52840`（带注释）。**
LaunchAgent 用户开箱获得稳定端口。52840 为任意选定的不常用端口；冲突时日志清晰可见，用户可改。

**D8 — 自动连接配置写入 README，两种方式：**
- 全局：shell rc `export CLAUDE_CODE_SSE_PORT=52840`（任意终端的 `claude` 都自动连）。
- 按项目：`.zed/settings.json` → `{"terminal": {"env": {"CLAUDE_CODE_SSE_PORT": "52840"}}}`（仅 Zed 内置终端）。

## 4. 测试策略

- `protocol`：OpenFileArgs serde 缺省值/roundtrip 单测。
- `mcp`：dispatch 对 openFile 返回 OpenFile 变体（纯,无 I/O 即可测）；tools/list 含 5 工具；参数缺失/非法 → -32602。
- `zed_cli`：文本定位（多行/未命中/空文件）、argv 构造、用 fake 脚本（写参数到临时文件后 exit 0）验证 spawn 与响应消息;超时与非零退出分支。
- `transport`：集成测试——真实 WS 连接发 `tools/call openFile`,fake zed 脚本捕获 argv,断言响应形状与 argv 正确。
- `--port`：bind_fixed 成功/冲突测试;CLI 解析测试;lock 文件名等于固定端口的集成断言。

## 5. 非目标（YAGNI）

- openDiff / close_tab / closeAllDiffTabs（独立立项,需 spike）。
- getDiagnostics（外部拿不到 Zed LSP 状态,诚实不可行）。
- 选区模拟（osascript 键击注入等脆弱方案）。
- Windows / Linux 的 zed CLI 路径差异处理（沿用 PATH 查找）。

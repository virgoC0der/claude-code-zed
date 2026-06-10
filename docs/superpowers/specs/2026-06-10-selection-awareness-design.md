# 选区感知 v2（Selection Awareness）— 设计文档

> 日期：2026-06-10
> 状态：设计已获用户批准（定标实验后确认推进）
> 前置：active-file watcher（PR #3，已合并）。本设计将 watcher 推送的空选区升级为真实光标/选区。

## 1. 目标

Claude 通过 `getCurrentSelection` / `selection_changed` 自动感知用户在 Zed 中的**光标行**与**选中范围及文本**——无需任何按键，对齐 JetBrains/VSCode 插件的 `selection_changed` 语义。

## 2. Ground truth（全部实证）

- **`editor_selections` 表实时持久化选区**：`(item_id, editor_id, workspace_id, start INTEGER, end INTEGER)`。`start == end` 为光标，`start != end` 为选区。
- **偏移量语义 = UTF-8 字节偏移**。定标实验：用 openFile 把光标放到第 258 行 → 落盘偏移 9120 → 按字节还原恰为 258 行（字符语义还原为 261 行，排除）。
- **`editors.path` 与 `editors.contents` 是 BLOB 列**：SQL 文本字面量 `=` 比较永远不命中；必须按字节读出（Rust 侧 `Vec<u8>`）或 `CAST(... AS TEXT)`。v1 的 query 已按 `Vec<u8>` 读 path，本迭代保持并写入回归测试。
- **`editors.contents` 持久化脏缓冲区文本**（实测 597 行中 7 行非空）：未保存的编辑以此为文本基准，避免对照磁盘旧内容算错行号。
- **wire 语义**（protocol.md §3.3）：`selection_changed` 的 Position 是 **0-indexed**，`character` 按 VSCode 语义为 UTF-16 code unit 偏移。

## 3. 设计决策

**D1 — query 返回结构升级。** `active_file_for_cwd` 的返回从 `Option<PathBuf>` 升级为 `Option<ActiveEditor>`：

```rust
pub struct ActiveEditor {
    pub path: PathBuf,
    /// UTF-8 字节偏移；选区行缺失时为 None（保持 v1 空选区行为）。
    pub selection: Option<(u64, u64)>,
    /// editors.contents 非空时的脏缓冲区文本（选区换算的文本基准优先用它）。
    pub unsaved_contents: Option<String>,
}
```

SQL 在现有 join 上 LEFT JOIN `editor_selections`（`s.editor_id = i.item_id AND s.workspace_id = i.workspace_id`）并加选 `e.contents`。

**D2 — 偏移→Position 换算（watcher 侧，纯函数）。**
文本基准：`unsaved_contents` 优先，否则读磁盘文件。换算：`line0 = 基准[..off] 的换行计数`（0-indexed）；`character = 行首到 off 的 UTF-16 code unit 数`。选中文本 = 基准字节区间 `[start, end)` 的 UTF-8 lossy 解码，同时填进 `text` 字段——Claude 直接拿到选中内容。偏移越界（基准与落盘不同步的窗口期）→ 整体降级为 v1 空选区，不 panic、不报错。

**D3 — dedup 键升级。** `PushState.last` 从 `HashMap<ClientId, PathBuf>` 改为 `HashMap<ClientId, StoredSelection>`：光标移动/选区变化即触发推送（仍受 400ms 去抖约束），完全相同则跳过。

**D4 — EditorState/通知形状不变。** 仍复用 `apply_selection` + `selection_changed` 直推；只是 StoredSelection 从空选区换成真数据。`getOpenEditors` 不受影响。

## 4. 测试策略

- `query.rs`：fixture 增加 editor_selections 行——有选区/无选区（LEFT JOIN 仍命中文件）/有 contents；**BLOB path 回归测试**（文本字面量过滤必须查空，按字节读必须命中）。
- `watcher.rs`：偏移换算纯函数单测——多行、UTF-8 多字节字符（中文/→）、UTF-16 列、越界降级、脏缓冲区基准优先于磁盘。
- 集成（tests/zed_watch.rs）：落盘 DB 带选区 → refresh → `selection_changed` 携带正确 0-indexed 行列与选中文本。

## 5. 非目标

- 多选区（multi-cursor）：只取主选区（表里 PRIMARY KEY (item_id) 天然单行）。
- 列的 grapheme 精确性：按 UTF-16 code unit（VSCode 同款），不做字素簇修正。
- 实时性提升：仍受 Zed 落盘节流（秒级）。

---
sidebar_position: 6
---

# egui 原生界面重构方案

本文档描述将 NyaTerm 从当前的 Tauri WebView + React UI 迁移到 Rust 原生 egui UI 的建议框架。目标不是一次性删除现有前端，而是在保留当前可运行应用的前提下，逐步抽离核心能力、建立原生 UI 壳、迁移终端工作区和功能面板。

## 背景与目标

当前 NyaTerm 是 Tauri 2 桌面应用：React / TypeScript 前端位于 `src/`，Rust 后端位于 `src-tauri/src/`，两者通过 Tauri command 和事件通信。该方案开发效率高，但终端类应用对输入延迟、大输出吞吐、滚动缓冲区和窗口布局有较高性能要求，WebView/xterm.js 路径会引入额外开销。

重构目标：

- 保留现有 Rust 后端的 SSH、SFTP、PTY、Telnet、Serial、Recording、Cloud Sync、AI 和 redb 存储能力。
- 用 egui / eframe 替换 WebView UI shell。
- 终端控件优先复用 `egui_term`、`alacritty_terminal` 等现有 Rust 生态能力，不默认自研完整 xterm.js 替代品。
- 将字符串式 Tauri command/event 逐步改造为 Rust typed API 和 typed event bus。
- 迁移终端工作区、标签页、分屏、设置、连接管理和文件传输界面。
- 在迁移期保持现有 Tauri UI 可运行，降低一次性替换风险。

## 目标分层

推荐最终分层如下：

```text
crates/
  nyaterm-core/        # Tauri/egui 无关的核心领域层
  nyaterm-app/         # runtime、manager 组装、事件总线和应用服务
  nyaterm-terminal/    # 终端适配层，优先封装 egui_term/alacritty_terminal 等现成库
  nyaterm-ui-egui/     # egui/eframe 原生 UI
src-tauri/             # 迁移期保留 Tauri shell 与 command bridge
src/                   # 迁移期保留 React UI，最终删除或归档
```

约束：

- `nyaterm-core` 不依赖 Tauri、egui 或 React。
- `nyaterm-ui-egui` 不直接访问存储实现，必须通过 typed app service。
- 敏感配置仍使用现有加密和 storage helper。
- 迁移期间 Tauri command 层继续存在，但内部逐步委托给 core facade。

## Core facade

当前分支已经开始落地统一 facade：`src-tauri/src/app_core.rs` 中的 `NyatermCore` 负责集中持有 backend managers，并为 Tauri command 与未来 egui UI 提供共享 typed API。迁移目标仍是继续把 `src-tauri/src/lib.rs` 中的直接 manager 组装逐步收敛到 facade：

```rust
pub struct NyatermCore {
    pub sessions: Arc<SessionManager>,
    pub tunnels: Arc<TunnelManager>,
    pub recording: Arc<RecordingManager>,
    pub pending_auth: Arc<PendingAuthManager>,
    pub host_key_verify: Arc<HostKeyVerifyManager>,
    pub quick_commands: Arc<QuickCommandsStore>,
    pub cloud_sync: Arc<CloudSyncManager>,
    pub agent_approval: Arc<AgentApprovalManager>,
}
```

当前已接入的 session typed API：

- `list_sessions`
- `write_to_session`
- `resize_session`
- `attach_session`
- `start_zmodem_upload` / `cancel_zmodem_upload`
- `close_session`

下一批需要补齐的 typed app services：

- session 创建 API：`create_ssh_session`、`create_local_session`、Telnet、Serial 等；
- settings API：`get_app_settings`、`save_app_settings`，并保持 `AppContext` / `ChildAppProvider` 与 Rust 默认值一致；
- saved connections / groups API：`get_saved_connections`、`save_connection`、分组增删改；
- quick commands API；
- SFTP / transfer API；
- cloud sync / backup API；
- AI history / audit / agent approval API。

Tauri commands 保持对外名称不变，但内部逐步调用这些 typed API。这样现有 React UI 与未来 egui UI 可以共用同一后端服务。

## Typed event bus

当前分支已经新增 `src-tauri/src/app_event.rs`，用 `AppEvent` / `AppEventBus` 承载 typed backend events；迁移期继续保留现有字符串式 Tauri events，确保 React UI 可运行。当前后端向前端发送的大量字符串事件包括：

- `terminal-output-{id}`
- `cwd-changed-{id}`
- `session-closed-{id}`
- `sessions-changed`
- `transfer-event`
- `otp-request`
- `cloud-sync-status-changed`
- `cloud-sync-conflict`

egui 版本应使用 typed event：

```rust
pub enum AppEvent {
    TerminalOutput { session_id: String, bytes: Vec<u8> },
    CwdChanged { session_id: String, cwd: String },
    SessionClosed { session_id: String },
    SessionsChanged,
    Transfer(TransferEvent),
    OtpRequest(OtpRequest),
    SshAuthRequest(SshAuthRequest),
    HostKeyVerifyRequest(HostKeyVerifyRequest),
    CloudSyncStatusChanged(CloudSyncStatus),
    CloudSyncHistoryChanged(CloudSyncHistory),
    CloudSyncConflict(CloudConflict),
}
```

推荐事件流：

```text
session / sftp / ai task
        │
        ▼
AppEventBus(tokio::broadcast)
        │
        ▼
egui update loop drain events
        │
        ▼
WorkspaceState / PanelState / TerminalModel
```

当前 typed event bus 已覆盖 terminal output、CWD changed、session closed、sessions changed、transfer、OTP、SSH auth、host-key verify、cloud-sync status/history/conflict 等事件。迁移期间必须继续双发：同时发 Tauri event 和 `AppEventBus` event，直到 React UI 下线。

## egui App shell

egui UI 应围绕一个主 app struct 组织：

```rust
pub struct NyatermEguiApp {
    core: Arc<NyatermCore>,
    events: AppEventReceiver,
    workspace: WorkspaceState,
    panels: PanelState,
    settings: AppSettings,
}
```

`eframe::App::update` 建议流程：

1. drain backend events；
2. 更新 terminal model / workspace / panels；
3. 渲染 top bar；
4. 渲染 activity bar；
5. 渲染 side panels；
6. 渲染 terminal workspace；
7. 渲染 bottom panels；
8. 渲染 dialogs / modals。

建议目录：

```text
nyaterm-ui-egui/src/
  app.rs
  actions.rs
  workspace.rs
  terminal_view.rs
  panels/
    saved_connections.rs
    file_explorer.rs
    quick_commands.rs
    command_history.rs
    ai_assistant.rs
  dialogs/
    settings.rs
    new_session.rs
    otp.rs
    host_key_verify.rs
```

## 终端控件策略

终端是迁移风险最高的部分。当前 `XTerminal.tsx` 依赖 xterm.js，并集成了搜索、链接、命令建议、关键词高亮、ZMODEM、文件拖放、AI marker、重连恢复和性能保护。

迁移原则应调整为：**优先集成成熟 Rust egui terminal 库，只有在现有库无法满足 NyaTerm 需求时，才补充适配层或局部 fork**。不要一开始自研完整终端模拟器。

### 首选方案：egui_term 适配层

`egui_term` 是基于 egui 的终端 widget，底层使用 `alacritty_terminal` 作为终端后端。它已经覆盖 PTY 内容渲染、多实例、基础键盘输入、自定义键鼠绑定、resize、scrolling、focus、selection、字体/颜色主题和 hyperlink 处理等能力。

建议新增的 `nyaterm-terminal` 不再作为完整终端模拟器，而是作为 NyaTerm 对第三方终端库的稳定适配层：

```text
nyaterm-terminal
  EguiTermAdapter       # 封装 egui_term widget 生命周期
  TerminalIoBridge      # 对接 SessionManager 的 read/write/resize
  TerminalThemeMapper   # 将 NyaTerm theme 映射到 egui_term/alacritty 颜色
  TerminalKeyBindings   # 注入 NyaTerm 快捷键、复制/粘贴和自定义动作
  TerminalLinkAdapter   # 复用或扩展 hyperlink/action-link 检测
  TerminalCapabilityMap # 记录上游已支持能力和 NyaTerm 仍需补齐的差距
```

首个 egui terminal spike 应验证：

- 多 session / 多 tab 下能创建多个 `egui_term` 实例；
- local PTY、SSH、Telnet、Serial 的输出都能进入同一 terminal widget；
- 输入、paste、resize 能通过现有 `SessionManager` 写回后端；
- 主题、字体大小、selection、scrollback、hyperlink 行为满足基础使用；
- 大输出场景下 CPU、内存和帧率明显优于或不差于 xterm.js/WebView 路径。

### 备选方案

如果 `egui_term` 的公开 API 不足以接入 NyaTerm 的 session/event 模型，优先顺序为：

1. 通过 adapter 层扩展，不 fork；
2. 向上游提交 PR；
3. 在仓库内临时 patch/fork `egui_term`，但保留 upstream sync 说明；
4. 直接封装 `alacritty_terminal` terminal model 并自绘 egui widget；
5. 最后才考虑基于 `vte` 自研完整 terminal model。

### 能力差距清单

迁移前必须建立 capability matrix，对比 xterm.js 现有能力、`egui_term` 已支持能力和 NyaTerm 需要自补的能力：

| 能力                                        | 迁移策略                                               |
| ------------------------------------------- | ------------------------------------------------------ |
| 基础 ANSI/VT、scrollback、selection、resize | 优先复用 `egui_term` / `alacritty_terminal`            |
| 多实例 tabs/splits                          | 复用 `egui_term` 多实例能力，workspace 自己管理布局    |
| 字体和主题                                  | 编写 `TerminalThemeMapper`                             |
| 链接识别                                    | 先复用 hyperlink，再补 action-link                     |
| 命令建议和 credential autofill              | 保留 NyaTerm overlay 逻辑，叠加在 terminal widget 外层 |
| ZMODEM、AI marker、command capture          | 通过 `TerminalIoBridge` 观察输入输出流补齐             |
| 文件拖放、右键菜单、快捷键                  | egui 外层处理后转发给 adapter                          |
| IME、宽字符、alternate screen、性能边界     | spike 阶段重点压测，不满足时评估 upstream patch        |

## Workspace 迁移

当前前端有两套工作区模型：

- `workspaceTabs.ts`：可持久化的逻辑 tab / pane tree；
- `tabWindows.ts`：运行时 terminal window 布局。

egui 版本仍应保留这两个概念，只是迁移为 Rust struct：

```rust
pub struct WorkspaceState {
    pub tabs: Vec<Tab>,
    pub active_tab_id: Option<TabId>,
    pub windows: TerminalWindowNode,
}

pub enum PaneNode {
    Leaf(SessionPane),
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}
```

必须保持 `ui.open_tabs` 的 JSON 兼容，避免用户升级后丢失工作区布局。

## Action registry

建议参考 rerun 的 action-driven UI 思路，建立统一 action registry：

```rust
pub struct Action {
    pub id: ActionId,
    pub label: String,
    pub shortcut: Option<Shortcut>,
    pub enabled: fn(&ActionContext) -> bool,
    pub run: fn(&mut ActionContext),
}
```

适合注册为 action 的操作：

- 新建会话；
- 关闭会话；
- 横向 / 纵向分屏；
- 打开设置；
- 切换侧栏面板；
- 执行快捷命令；
- 打开命令面板。

这样键盘快捷键、菜单、command palette 和按钮可以复用同一套行为定义。

## 迁移阶段

### Phase 0：facade 与 event bus 基础设施（已完成第一批）

- 已新增 `NyatermCore` facade，并开始把 session command 委托到 facade。
- 已新增 `AppEvent` / `AppEventBus`。
- 已对 terminal、session、transfer、auth、host-key verify、cloud-sync 等事件建立 typed event。
- 继续要求后端事件双发到 Tauri event 和 typed event bus。
- 保持当前 React/Tauri UI 完全可用。

### Phase 1：补齐 typed app services

- 将 settings、saved connections、quick commands、SFTP、cloud sync、backup、AI history/audit 和 agent approval 继续收敛为 typed services。
- 为 session 创建链路补齐 local / SSH / Telnet / Serial typed API。
- 把迁移后的 Tauri commands 保持为薄 bridge，避免 React UI 与 egui UI 分叉业务逻辑。
- 对敏感配置继续复用现有 crypto/storage helpers。

### Phase 2：egui / eframe binary spike

- 新增 eframe binary。
- 启动 Tokio runtime。
- 初始化 `NyatermCore` 与 `AppEventBus` receiver。
- 渲染空布局：top bar、activity bar、terminal area、status bar。
- 验证 local terminal session 的 create/list/write/resize/close 闭环。

### Phase 3：terminal adapter spike

- 基于 `egui_term` / `alacritty_terminal` 实现 `nyaterm-terminal` 适配层。
- 验证多 session / 多 tab / split panes 下的 terminal widget 生命周期。
- 支持输入、paste、resize、scrollback、selection、copy/paste、theme/font mapping 和 hyperlink 基础能力。
- 针对大输出、宽字符、IME、alternate screen 和高频 resize 做性能与兼容性压测。

### Phase 4：workspace / panel 迁移

- 将 `workspaceTabs.ts` 与 `tabWindows.ts` 概念迁移为 Rust `WorkspaceState`，保持 `ui.open_tabs` JSON 兼容。
- 迁移 saved connections、quick commands、command history、SFTP file explorer、transfer panel 和 settings dialog。
- 迁移 OTP / SSH auth / host key verify dialogs、ZMODEM、file drop、AI assistant、cloud sync、backup / import、updater 等高级交互。
- 评估多窗口或 modal viewport 替代方案。

### Phase 5：默认切换与 WebView 清理

- egui binary 成为默认桌面产物。
- release pipeline 切换到 Rust native build。
- React/Vite/Tailwind/xterm.js 依赖删除或归档。
- Tauri WebView runtime 清理。
- 更新开发文档和迁移说明。

## 首个里程碑

建议第一个可验证里程碑为：

> 启动 egui 原生窗口，创建 local terminal session，看到 shell 输出，能够输入命令，并能随窗口大小变化 resize PTY。

该里程碑需要完成：

1. `NyatermCore` facade；
2. `AppEventBus`；
3. eframe app shell；
4. `egui_term`/`alacritty_terminal` 集成 spike；
5. `EguiTermAdapter` 基础渲染和输入；
6. local session create/write/resize/list/close API。

完成这个闭环后，再迁移 SSH、SFTP、设置和 AI 等复杂功能会更安全。

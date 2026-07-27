# OcHub CLI 设计文档

> 状态：**设计提案，待评审**
>
> 文档版本：**1.0**
>
> 目标版本：**OcHub CLI 0.1+**
>
> 基线版本：**OcHub 0.2.7**
>
> 最后更新：**2026-07-27**

## 1. 摘要

OcHub 可以在不依赖 GUI 的情况下提供完整的业务配置和运维能力。现有项目已经具备 SQLite 数据层、各应用配置 Codec、Provider/MCP/Skill/Usage/Backup 服务、协议转换网关和本地控制 API，CLI 不需要重新实现这些能力。

本设计选择以下产品和架构方向：

1. 新增面向用户的 `ochcli` 命令行程序。
2. 将仍位于 GPUI View 或 HTTP Handler 中的业务流程提取为统一的 Application/Use-case Facade。
3. GUI、CLI、HTTP API 和 daemon 只负责输入输出适配，共享同一套用例、校验、事务和错误模型。
4. 短命令默认优先连接当前 OcHub owner；没有 owner 时可直接加载 core 执行。
5. Relay Gateway、自动同步、定时备份等常驻能力由 daemon 承担。
6. 任意时刻只允许一个 owner 修改数据库和外部工具配置文件，避免 GUI、CLI、daemon 竞争写入。
7. 人类输出与机器输出同时作为一等能力，提供稳定 JSON Schema、退出码、脱敏和非交互行为。
8. 最终支持声明式 `plan/apply`，但不会用隐式删除或静默覆盖换取表面的“自动化”。

CLI 的目标是覆盖全部业务能力，不是复制 GUI 的窗口、拖拽、托盘等表现层行为。主题、托盘、启动方式等持久化设置仍可由 CLI 管理；打开窗口、复制到剪贴板等纯桌面交互不属于 headless 等价范围。

## 2. 背景与现状

### 2.1 当前 Workspace

| Crate | 当前职责 | CLI 设计中的职责 |
|---|---|---|
| `ochub-core` | 领域模型、SQLite、配置读写、业务服务、Gateway | 增加 Application Facade、Mutation Planner、Runtime Coordinator |
| `ochub-server` | 本地 HTTP 控制 API、独立 headless server | 调用 Facade；保留兼容 API；承载 daemon runtime |
| `ochub-convert` | Messages、Chat、Responses 协议转换 | 保持独立，由 Gateway 使用 |
| `ochub-app` | GPUI 桌面界面、系统托盘、主题和桌面交互 | 改为 Facade 的 UI Adapter，不再持有独占业务流程 |
| `ochcli` | 尚不存在 | 新增；命令解析、交互提示、格式化、IPC Client |

### 2.2 当前数据与配置

| 数据 | 默认位置 | 说明 |
|---|---|---|
| 主数据库 | `~/.ochub/ochub.db` | SQLite，WAL 模式 |
| 设备设置 | `~/.ochub/settings.json` | 不随数据库云同步 |
| 数据目录覆盖 | `~/.ochub/app_paths.json` | 固定从默认 OcHub 根目录发现 |
| 用户应用 Manifest | `~/.ochub/apps/*.toml` | 动态应用描述 |
| 主题 | `~/.ochub/themes/*.ochub-theme.json` | 当前主要由 GUI 管理 |
| Skills | `~/.ochub/skills/` 或 `~/.agents/skills/` | 取决于存储设置 |
| cc-switch 迁移源 | `~/.cc-switch/` | OcHub 只读，不应写回 |

第三方工具配置继续位于各自目录，例如：

- Claude Code：`~/.claude/settings.json` 和 `~/.claude.json`
- Codex：`~/.codex/config.toml` 和 `~/.codex/auth.json`
- Grok Build：`~/.grok/config.toml`
- OpenCode：`~/.config/opencode/opencode.json`
- OpenClaw：`~/.openclaw/openclaw.json`
- Hermes：`~/.hermes/config.yaml`

自定义配置目录优先于默认目录。所有路径均应通过 `ochub-core::paths` 和应用 Adapter 解析，CLI 不自行拼接路径。

### 2.3 当前内置应用

| App ID | 显示名 | Provider 模式 | 主要能力 |
|---|---|---|---|
| `claude` | Claude Code | Switch | Provider、MCP、Plugin、Session、Usage |
| `claude-desktop` | Claude Desktop | Switch | 官方和第三方连接 |
| `codex` | Codex | Switch | Provider、OAuth、History、Session、Usage |
| `grokbuild` | Grok Build | Switch | Provider、Session、Usage |
| `opencode` | OpenCode | Additive | Provider、MCP、Skill、Session、Usage |
| `openclaw` | OpenClaw | Additive | Provider、Agent、Tools、Session |
| `hermes` | Hermes | Additive | Provider、MCP、Skill、Memory、Session |

Switch 模式的活动配置聚焦一个当前 Provider；Additive 模式允许多个由 OcHub 管理的 Provider 同时存在于工具配置中。

公共协议和持久化统一使用规范 App ID `grokbuild`；`grok-build`、`grok_build` 和 `grok` 只作为输入兼容别名，并在解析后归一化。

### 2.4 已发现的架构缺口

现有 HTTP API 覆盖面较高，但不能直接视为完整 CLI Backend：

1. Provider 漂移预览与 `preserve/discard/abort` 决策仍由 GUI 流程主导。
2. Provider 跨应用复制和转换仍包含 View 层逻辑。
3. 主题持久化、cc-switch 手动迁移、部分定价目录操作没有完整控制 API。
4. 用户 Manifest 已使用 `AppId`，但 Provider、GUI 和多数 HTTP Handler 仍依赖封闭的 `AppType`。
5. SQLite WAL 不能保护数据库之外的 Claude/Codex 等配置文件。
6. 当前 control API 使用 loopback、permissive CORS，且没有专用控制鉴权。
7. 独立 server 在全新数据库上会自动导入 cc-switch；普通 CLI 命令不应有这种隐式副作用。
8. Deep Link 和部分兼容代码仍有历史应用分支，需要在 CLI Schema 稳定前清理。
9. 数据库中的历史 profile 兼容数据尚未形成完整产品能力，本设计不将其包装成可用功能。

## 3. 目标、非目标与原则

### 3.1 目标

- 在没有图形环境、窗口系统和桌面 Session 的机器上完成全部 OcHub 业务配置。
- 覆盖 Provider、Gateway、MCP、Skills、Sessions、Usage、Auth、Backup、Sync、Migration 和应用高级能力。
- GUI 和 CLI 对相同输入产生语义等价的数据库记录与外部配置文件。
- 支持终端交互、Shell Script、CI、容器和远程 SSH 场景。
- 支持稳定、可解析、默认脱敏的 JSON/JSONL 输出。
- 支持 dry-run、diff、显式冲突策略和可恢复写入。
- 支持 macOS、Windows 和 Linux，平台不支持的能力返回明确错误。
- 为动态应用 Manifest 提供 Schema 驱动的通用 CLI。
- 保持当前数据库和工具配置兼容，不要求用户重新导入。

### 3.2 非目标

- 不在首个版本提供远程多租户管理服务。
- 不把控制端口暴露到局域网或公网。
- 不在 CLI 中复刻 GPUI 页面布局、托盘菜单或系统对话框。
- 不承诺所有第三方工具在所有操作系统上具备相同能力。
- 不把云同步升级为记录级多人协作或自动冲突合并。
- 不把 Usage 成本估算描述为实际账单。
- 不在功能尚未贯通前公开 profile 命令。
- 不允许用户 Manifest 绕过统一的安全、路径和写入策略。

### 3.3 设计原则

1. **单一业务实现**：业务行为属于 Facade/Core，不属于 CLI、GUI 或 Handler。
2. **显式优于隐式**：不静默迁移、不静默删资源、不静默丢弃外部编辑。
3. **安全默认值**：默认脱敏、默认不覆盖冲突、默认只监听本机。
4. **可自动化**：稳定输出、退出码、非交互模式和幂等行为。
5. **可恢复**：变更前有计划，关键写入有快照或回滚记录。
6. **能力驱动**：命令是否可用由 App Capability 决定，不散落硬编码应用列表。
7. **渐进兼容**：现有 GUI、HTTP API、DB 和配置文件继续可用。

### 3.4 Headless 等价边界

“完整 CLI”按行为结果定义，不按 GUI 控件数量定义：

| GUI 能力类型 | Headless 要求 | 示例 |
|---|---|---|
| 业务配置 | 必须完全等价 | Provider、MCP、Station、Backup |
| 查询与运维 | 必须完全等价 | Usage、Health、Session 扫描 |
| 持久化桌面设置 | 必须可读写 | Theme、托盘、启动行为 |
| 桌面 convenience | 返回信息即可 | 打开目录改为输出路径 |
| 纯视觉交互 | 不适用 | 窗口尺寸、拖拽、截图 |
| 常驻行为 | 由 daemon 等价实现 | Gateway、自动同步、健康检查 |

如果 GUI 操作会修改数据库、settings 或第三方工具文件，它就必须有 CLI/Fascade 等价用例；如果 GUI 只负责展示或调用系统外壳，则不要求在无桌面环境中模拟窗口行为。

## 4. 用户场景

### 4.1 本地无 GUI

用户通过 SSH 登录开发机，导入 Provider、切换 Codex 配置、同步 MCP 和启动 Relay Gateway，全程不启动 OcHub 桌面窗口。

### 4.2 自动化初始化

用户在新机器上执行：

```bash
ochcli plan -f ochub.yaml
ochcli apply -f ochub.yaml
```

配置应用目录、Providers、MCP、Skills 和 Gateway。Secret 从环境变量或文件读取，不进入仓库。

### 4.3 与 GUI 并存

GUI 已运行时，用户在终端切换 Provider。CLI 自动连接 GUI 所属的 runtime owner，由同一进程执行写入，GUI 随后刷新状态。

### 4.4 常驻网关

用户不安装 GUI，只安装 CLI/daemon，通过用户级系统服务让 Gateway、健康检查、自动备份和同步持续运行。

### 4.5 故障恢复

用户先用 `--dry-run` 查看恢复影响，再恢复数据库快照。如果进程在修改外部配置时中断，下次启动会报告未完成操作并提供恢复。

## 5. 产品形态

### 5.1 二进制

| 二进制 | 定位 | 生命周期 |
|---|---|---|
| `ochcli` | 用户命令入口 | 通常短生命周期 |
| `ochubd` | 本地 runtime owner、Gateway 和后台任务 | 常驻，可作为用户级服务 |
| `ochub` | 现有 GPUI 应用 | 常驻，可成为 runtime owner |
| `ochub-server` | 现有兼容入口 | 兼容期保留，内部逐步复用 daemon runtime |

初期允许 `ochcli daemon run` 前台启动与 `ochubd` 相同的 runtime，方便开发、容器和进程管理器使用。正式安装包提供 `ochubd`。

### 5.2 Runtime 模式

`ochcli` 支持三种执行路径：

| 模式 | 触发条件 | 行为 |
|---|---|---|
| Owner RPC | 发现 GUI 或 daemon owner | 通过本地 IPC 执行 |
| Direct | 没有 owner，命令不要求常驻 | 获取独占 mutation lock 后直接调用 Facade |
| Foreground runtime | `gateway serve`、`daemon run` | 当前进程持有 owner lock 并持续运行 |

自动选择顺序：

1. 尝试连接 runtime endpoint。
2. 验证 protocol version、进程身份和数据目录。
3. 连接成功则走 RPC。
4. endpoint 不存在或确认失效时，尝试 Direct。
5. owner lock 已存在但 RPC 不可用时立即报错，不绕过锁写入。

`--direct` 只禁止主动使用 RPC，不允许绕过正在运行的 owner。`--socket` 用于诊断或测试，不作为普通用户的日常参数。

### 5.3 常驻能力

以下功能要求 owner 持续运行：

- Relay Gateway。
- Channel 健康检查。
- 自动备份。
- WebDAV/S3 自动同步。
- 定价目录后台刷新。
- OAuth 登录轮询，除非 CLI 在前台持续等待。

一次性配置、导入、查询和手动同步不要求 daemon。

## 6. 总体架构

```text
┌────────────────────── Adapters ──────────────────────┐
│                                                      │
│  GPUI App       ochcli       Control API       IPC │
│      │              │               │             │  │
└──────┼──────────────┼───────────────┼─────────────┼──┘
       └──────────────┴───────────────┴─────────────┘
                              │
                  Application / Use-case Facade
                              │
             ┌────────────────┼─────────────────┐
             │                │                 │
       Query Services   Mutation Planner   Runtime Tasks
             │                │                 │
             └────────────────┼─────────────────┘
                              │
                         Domain/Core
             ┌────────────────┼─────────────────┐
             │                │                 │
          SQLite       App Adapters       Gateway/Network
                          │
                External tool configs
```

### 6.1 新增模块

建议在 `ochub-core` 中新增：

```text
crates/core/src/application/
├── mod.rs
├── dto.rs
├── error.rs
├── apps.rs
├── providers.rs
├── gateway.rs
├── mcp.rs
├── skills.rs
├── sessions.rs
├── usage.rs
├── auth.rs
├── backup.rs
├── sync.rs
├── migration.rs
├── settings.rs
├── themes.rs
└── operations.rs

crates/core/src/runtime/
├── mod.rs
├── coordinator.rs
├── lock.rs
├── journal.rs
└── endpoint.rs
```

新增 CLI crate：

```text
crates/cli/
├── Cargo.toml
└── src/
    ├── main.rs
    ├── command.rs
    ├── client.rs
    ├── interactive.rs
    ├── output.rs
    ├── secret_input.rs
    └── commands/
```

依赖策略：

- 命令解析采用 `clap` derive。
- completion/manpage 分别使用 `clap_complete` 和 `clap_mangen`。
- CLI 复用 workspace 的 `serde`、`tokio`、`tracing` 和 `ochub-core`。
- `ochcli` 不依赖 `gpui`、`ochub-app` 或窗口系统。
- IPC Client 与 Server 的协议类型放在非 UI crate 中，避免循环依赖。
- 引入新的终端表格或交互依赖前检查二进制体积、Windows 行为和维护状态。

### 6.2 Application Facade

Facade 提供面向用例而非面向表的 API。例如：

```rust
pub trait ProviderUseCases {
    fn list(&self, request: ListProviders) -> AppResult<Vec<ProviderSummary>>;
    fn plan_switch(&self, request: PlanProviderSwitch) -> AppResult<MutationPlan>;
    fn switch(&self, request: SwitchProvider) -> AppResult<OperationResult>;
    fn copy(&self, request: CopyProvider) -> AppResult<ProviderDetails>;
}
```

具体命名可在实现时按 Rust 约定调整，但必须满足：

- DTO 不依赖 GPUI、Axum Extractor 或终端类型。
- 输入中包含所有冲突、安全和交互决策。
- 用例不会自行弹窗或读取 stdin。
- 返回值可以稳定序列化。
- GUI、HTTP、IPC 和 CLI 使用相同的用例。
- DAO 不直接暴露给 Adapter。

### 6.3 App Registry 与 Capability

CLI 的公共接口统一使用 `AppId`，不使用封闭的 `AppType` 作为协议字段。

建议引入：

```rust
pub trait AppAdapter {
    fn descriptor(&self) -> &AppDescriptor;
    fn capabilities(&self) -> CapabilitySet;
    fn inspect_live(&self, context: &AppContext) -> AppResult<LiveState>;
    fn plan_write(&self, request: AppWriteRequest) -> AppResult<FileMutationPlan>;
    fn validate_provider(&self, value: &Value) -> AppResult<ValidationReport>;
}
```

Registry 负责将内置 Rust Adapter 和用户 Manifest Adapter 统一为：

- `AppDescriptor`
- `CapabilitySet`
- `FormSchema`
- `ProviderCodec`
- `LiveConfigAdapter`
- `McpAdapter`
- 可选 Hook

`AppType` 可以继续作为内置应用内部的穷举类型，但不得成为新 CLI/IPC Schema 的限制。

### 6.4 Capability 示例

```text
provider.read
provider.write
provider.switch
provider.additive
provider.copy
mcp.import
mcp.sync
skill.sync
session.scan
session.resume
usage.scan
gateway.apply
auth.oauth
memory.manage
theme.manage
```

当应用不具备能力时，命令返回 `CAPABILITY_UNSUPPORTED`，而不是静默跳过。

### 6.5 内置 App Adapter 责任

CLI 不硬编码以下字段，但各内置 Adapter 和 Schema 必须完整表达它们：

| 应用 | Provider Schema 与专属行为 |
|---|---|
| Claude Code | Base URL、Bearer/x-api-key 选择、API Key、默认模型、Sonnet/Opus/Haiku/Fable 角色模型、可选 1M、API format、User-Agent、完整 URL |
| Claude Desktop | Anthropic-native endpoint/auth、官方连接、第三方连接、安全角色与模型路由；Linux 上返回平台不支持 |
| Codex | Provider ID/name、Base URL、API-only/OAuth-only/组合鉴权、API Key、模型、reasoning、Responses wire API、response storage、query params、headers、remote compaction、preserve OAuth、unified history |
| Grok Build | Profile、upstream model、display name、Base URL、inline key 或 env key、Responses/Chat/Messages backend、context window |
| OpenCode | Provider ID、AI SDK package、name、Base URL、API Key、额外 options/headers、模型列表 |
| OpenClaw | Protocol、Base URL、API Key、models、headers，以及默认模型、Agent、环境和工具权限 |
| Hermes | API mode、Base URL、API Key、models、rate limit，以及 MEMORY/USER memory |

每个 Adapter 还必须定义：

- live config 路径和格式。
- managed 字段边界。
- 导入、规范化、验证和稳定序列化。
- Secret 字段集合。
- Provider 模式和可用动作。
- Gateway 客户端 Dialect。
- MCP/Skill/Session/Usage Capability。
- 平台限制。

### 6.6 用户 Manifest Adapter

用户 Manifest 当前可以描述应用元数据、配置文件、表单、映射和 Hook。CLI 完整支持要求 Manifest 至少能声明：

- switch/additive Provider 模式。
- JSON、YAML、TOML 和 env 映射。
- text、secret、select、toggle、key-value、model-grid 等字段。
- required、pattern、enum、长度和数值范围校验。
- 默认值、Preset 和字段可见条件。
- live read/write 的 managed 范围。
- Hook 的阶段、输入、权限和超时。
- Capability 与不支持原因。

`app schema` 输出的机器 Schema 是 Manifest 和内置 Adapter 的统一投影。CLI 不能为内置应用生成一套字段协议、再为用户应用生成另一套不兼容协议。

## 7. 进程所有权、IPC 与并发

### 7.1 Runtime 目录

Runtime 发现文件固定放在默认 OcHub 根目录下，不随数据库目录覆盖移动：

```text
~/.ochub/runtime/
├── owner.lock
├── mutation.lock
├── ochub.sock             # Unix
├── owner.json
├── control.token          # TCP fallback
└── operations/
```

Windows 使用：

```text
\\.\pipe\ochub-<user-id>
```

`owner.json` 只存非敏感元数据：

```json
{
  "protocolVersion": 1,
  "pid": 12345,
  "kind": "gui",
  "startedAt": "2026-07-27T10:00:00Z",
  "dataDir": "/Users/example/.ochub",
  "endpoint": "unix:~/.ochub/runtime/ochub.sock"
}
```

### 7.2 Owner 规则

- GUI、`ochubd` 和前台 runtime 启动时竞争 `owner.lock`。
- 获得 owner lock 的进程负责 IPC、后台任务和所有 mutation。
- 没获得 owner lock 的 GUI 可以连接已有 owner，或明确报告冲突；不建立第二个 Gateway。
- Direct CLI 在命令期间持有 `mutation.lock`。
- owner 执行 mutation 时同样持有 `mutation.lock`，保证 Direct 和 owner 路径一致。
- 只读查询尽量不获取 mutation lock，但 DB restore、data-dir 切换等操作获取全局独占锁。

### 7.3 为什么 WAL 不够

SQLite WAL 只能协调 SQLite Connection，不能协调：

- `~/.claude/settings.json`
- `~/.codex/config.toml`
- `~/.config/opencode/opencode.json`
- Skills 目录
- 数据库文件替换与恢复
- 云同步覆盖本地快照

因此所有业务写入必须进入 Runtime Coordinator。

### 7.4 IPC

首选本地双向流：

- macOS/Linux：Unix Domain Socket。
- Windows：Named Pipe。
- 无法使用上述传输时：随机 loopback 端口 + 0600 权限 token。

IPC 使用版本化 JSON Lines Frame：

```json
{"type":"request","protocolVersion":1,"requestId":"01...","operation":"provider.switch","params":{}}
{"type":"event","requestId":"01...","sequence":1,"event":"progress","data":{"stage":"write_live"}}
{"type":"response","requestId":"01...","ok":true,"data":{},"warnings":[]}
```

要求：

- 每个 Request 有唯一 ID。
- 支持进度事件和长任务取消。
- Client 断开不自动取消已进入不可中断提交阶段的 mutation。
- 协议握手返回 daemon 版本、Schema 版本和数据目录。
- 主版本不兼容时拒绝执行；次版本能力通过 capability negotiation 判断。
- IPC 消息、日志和 tracing 字段默认脱敏。

现有 HTTP Control API 在兼容期继续存在，但 CLI 不以无鉴权 HTTP 作为首选控制通道。

兼容入口继续默认监听 `127.0.0.1:8787`，兼容期沿用现有 `MS_PORT` 覆盖方式；新 IPC 与 Gateway 的 `127.0.0.1:4180` 数据面是三个不同职责的 endpoint，不得复用端口或混用鉴权。新配置不再增加 `MS_*` 命名，后续使用 `OCHUB_*` 并保留兼容别名。

## 8. 变更计划、事务与恢复

### 8.1 Mutation Plan

所有会修改 DB 或文件的核心用例应能生成统一计划：

```json
{
  "operationId": "01...",
  "summary": "Switch Codex provider to team-openai",
  "risk": "medium",
  "changes": [
    {
      "target": "database",
      "action": "update",
      "resource": "provider-current:codex",
      "redactedDiff": {}
    },
    {
      "target": "file",
      "action": "update",
      "path": "~/.codex/config.toml",
      "beforeHash": "sha256:...",
      "afterPreview": "***"
    }
  ],
  "conflicts": [],
  "requiresConfirmation": false
}
```

`--dry-run` 只生成和显示计划，不执行 mutation。

### 8.2 外部编辑漂移

Provider 切换和任何 managed live config 写入都必须支持：

```text
abort       检测到漂移立即退出，默认值
preserve    合并可保留的外部修改
discard     使用 OcHub 计划覆盖 managed 范围
```

CLI 参数：

```bash
ochcli provider switch <id> --app codex --on-drift abort
```

交互终端可以在 `abort` 后提示用户重新执行，但 core 不负责提问。非交互模式绝不自动将 `abort` 提升为 `preserve` 或 `discard`。

### 8.3 Operation Journal

建议新增 operation journal，用于跨 DB 和文件的可恢复操作：

```text
planned -> prepared -> db_committed -> files_committed -> completed
                                \-> rollback_pending -> rolled_back
                                \-> recovery_required
```

Journal 至少记录：

- operation ID 和类型。
- 输入摘要，Secret 脱敏。
- 目标文件及写前 hash。
- 临时备份位置。
- 数据库 transaction 状态。
- 当前阶段和错误。
- 可重试/可回滚标志。

启动时发现 `recovery_required`：

- 只读命令继续执行并输出 warning。
- 新 mutation 默认拒绝执行。
- 用户运行 `ochcli operation inspect|recover|rollback <id>`。

### 8.4 原子性边界

- 单纯 DB 写入使用 SQLite transaction。
- 单文件写入使用现有 temp + rename 原子写。
- 多文件写入先全部生成 temp，再依次替换。
- DB + 文件无法获得真正分布式事务，因此依赖 journal、写前备份和补偿。
- Snapshot restore、SQL import、data-dir 切换必须独占 runtime，并暂停 Gateway 和后台同步。
- 批量操作默认逐资源提交并报告部分失败；需要 all-or-nothing 的命令必须显式声明。

## 9. CLI 通用规范

### 9.1 基本形式

```text
ochcli [GLOBAL OPTIONS] <RESOURCE> <ACTION> [ARGUMENTS]
```

统一使用：

- 小写命令。
- kebab-case 参数和 App ID。
- Resource ID 作为位置参数。
- 应用通过 `--app <app-id>` 指定。
- 布尔变更使用明确的 `enable/disable`，不使用难以审计的 toggle。

### 9.2 全局参数

| 参数 | 行为 |
|---|---|
| `--output human|json|jsonl` | 输出格式；默认 `human` |
| `--json` | `--output json` 简写 |
| `--quiet` | 只输出必要结果 |
| `--no-color` | 禁止 ANSI 色彩 |
| `--non-interactive` | 禁止读取 stdin 进行选择或确认 |
| `--yes` | 确认命令已经声明的危险操作 |
| `--dry-run` | 生成计划但不写入 |
| `--timeout <duration>` | 网络或长操作超时 |
| `--offline` | 禁止主动发起网络请求 |
| `--lang <language>` | 覆盖本次命令的人类消息语言 |
| `--show-secrets` | 在受支持命令中显式显示 Secret |
| `--data-dir <path>` | 本次进程使用指定数据目录，不持久化 |
| `--direct` | 不主动连接 owner；owner 存在时仍拒绝竞争写入 |
| `--socket <path>` | 指定 IPC Endpoint，主要用于诊断和测试 |
| `-v/--verbose` | 增加诊断日志 |
| `--trace-id <id>` | 为自动化系统传入关联 ID |

环境变量：

```text
OCHUB_OUTPUT
OCHUB_DATA_DIR
OCHUB_SOCKET
OCHUB_NO_COLOR
OCHUB_LANG
OCHUB_TEST_HOME       # 仅测试和隔离环境
RUST_LOG
```

优先级：

```text
CLI 参数 > 环境变量 > settings/app_paths > 默认值
```

`--help`、`--version` 和 shell completion 不初始化数据库、不启动后台同步，也不访问网络。

如果已发现的 owner 使用的数据目录与 `--data-dir` 不同，CLI 不向该 owner 发送命令，也不在其仍持锁时竞争写入。持久化切换数据目录只能使用 `data-dir set`。

人类消息默认跟随设备 language setting；JSON 字段、枚举、error code 和命令名始终使用稳定英文标识。

### 9.3 Duration 与时间

- 输入接受 `500ms`、`30s`、`15m`、`24h`。
- JSON 时间统一为 RFC 3339 UTC。
- 人类输出可显示本地时区，并在表头或详情中注明。
- 统计日期参数接受 `YYYY-MM-DD`，按显式 `--timezone` 或设备时区解释。

### 9.4 ID 与名称

- 脚本应使用稳定 ID。
- 人类模式允许通过唯一名称解析。
- 名称不存在返回 `NOT_FOUND`。
- 名称匹配多个资源返回 `AMBIGUOUS_REFERENCE`，列出候选但不选择。
- 新增资源可通过 `--id` 指定；未指定时由 core 生成。

### 9.5 动态字段输入

Provider 和用户 Manifest 表单支持三种输入：

```bash
ochcli provider add --app codex --from provider.yaml
ochcli provider add --app codex --set name=Team --set base_url=https://example.com
ochcli provider edit <id> --app codex --patch patch.json
```

规则：

- `--from` 为完整资源。
- `--patch` 使用 JSON Merge Patch 语义。
- `--set path=value` 适合简单标量；复杂数组和对象应使用文件。
- 字段路径和类型由 App Schema 校验。
- 未知字段默认报错；兼容导入可显式使用 `--allow-unknown`。
- Secret 不鼓励通过 `--set` 传入。

Schema 查询：

```bash
ochcli app schema codex --resource provider --output json
```

### 9.6 Secret 输入

支持通用、可重复的 Secret 字段参数：

```text
--secret <field>=stdin
--secret <field>=env:<ENV_NAME>
--secret <field>=file:<PATH>
```

`field` 使用 App Schema 中的规范字段路径；同一字段不能同时由普通输入和 `--secret` 提供。

声明式文件使用引用：

```yaml
apiKey:
  fromEnv: ANTHROPIC_API_KEY
```

要求：

- Secret 默认显示为 `******`，可以附带后四位或 hash 指纹。
- 默认 JSON 输出同样脱敏。
- Secret 不写入 shell completion、错误上下文、operation journal 和日志。
- `--show-secrets` 是逐次显式授权，不保存到 settings。
- 非 TTY 输出 Secret 时发出高风险 warning；配合 `--non-interactive` 时还需 `--yes`。
- 读取 Secret 文件时拒绝目录、设备文件和超出合理上限的内容。

### 9.7 输出

stdout：

- 成功结果。
- `--output json|jsonl` 的结构化数据。

stderr：

- 进度。
- warning。
- 交互提示。
- 诊断日志。

非 TTY 环境默认关闭动画和颜色。

JSON Envelope：

```json
{
  "schemaVersion": "1",
  "ok": true,
  "data": {},
  "warnings": [],
  "meta": {
    "requestId": "01...",
    "source": "direct"
  }
}
```

错误 Envelope：

```json
{
  "schemaVersion": "1",
  "ok": false,
  "error": {
    "code": "CONFIG_DRIFT",
    "message": "Codex live config changed outside OcHub.",
    "retryable": false,
    "details": {
      "path": "~/.codex/config.toml"
    }
  },
  "warnings": [],
  "meta": {
    "requestId": "01..."
  }
}
```

JSON 字段名、枚举值和 error code 使用英文稳定标识；`message` 可本地化。

### 9.8 退出码

| Code | 类别 | 示例 |
|---:|---|---|
| 0 | 成功 | 查询或变更完成 |
| 1 | 未分类内部错误 | 未预期 invariant 失败 |
| 2 | 参数或校验错误 | 缺少参数、Schema 不合法 |
| 3 | 资源不存在 | Provider ID 不存在 |
| 4 | 冲突 | 配置漂移、owner 冲突、版本冲突 |
| 5 | 权限或安全拒绝 | 文件权限、路径策略、鉴权失败 |
| 6 | 网络或上游失败 | Provider、WebDAV、S3 不可达 |
| 7 | 外部依赖缺失 | Node、npx、目标 CLI 未安装 |
| 8 | 部分成功 | Batch 中部分资源失败 |
| 9 | 用户取消 | OAuth 或交互操作取消 |
| 10 | Runtime 不可用 | daemon 无响应且不能 Direct |

错误码枚举属于公共兼容协议，不能随意重命名。

## 10. 命令设计

以下命令树是目标完整面。首个可用版本按实施阶段逐步开放，但已开放命令必须遵守本设计的兼容约束。

### 10.1 系统与诊断

```text
ochcli version
ochcli status
ochcli doctor
ochcli paths
ochcli runtime portable
ochcli runtime lightweight status
ochcli runtime lightweight enter
ochcli runtime lightweight exit
ochcli desktop autostart status
ochcli desktop autostart enable
ochcli desktop autostart disable
ochcli completion <bash|zsh|fish|powershell|elvish>
ochcli man
ochcli operation list
ochcli operation inspect <id>
ochcli operation recover <id>
ochcli operation rollback <id>
```

`doctor` 检查：

- 数据库可打开和 Schema 版本。
- runtime owner/IPC 状态。
- 配置目录读写权限。
- 外部工具配置路径。
- Node/npx 和被管理工具 CLI。
- Gateway 端口冲突。
- 未完成 operation。
- Manifest 加载错误。
- WebDAV/S3 配置完整性，但默认不发网络请求；`--network` 才执行连通性测试。

`runtime lightweight` 映射当前 owner 进程内的 lightweight flag，不持久化；Direct 模式没有可切换的常驻进程时返回 capability unsupported。`runtime portable` 只报告当前安装通道。`desktop autostart` 管理现有 GUI 自动启动，和 `daemon install/start` 是不同能力。

### 10.2 应用与插件

```text
ochcli app list
ochcli app show <app-id>
ochcli app enable <app-id>
ochcli app disable <app-id>
ochcli app status <app-id>
ochcli app path get <app-id>
ochcli app path set <app-id> <path>
ochcli app path reset <app-id>
ochcli app schema <app-id> --resource <provider|mcp|settings>

ochcli plugin list
ochcli plugin show <app-id>
ochcli plugin validate <file>
ochcli plugin install <file>
ochcli plugin reload
ochcli plugin errors
ochcli plugin remove <app-id>
```

规则：

- `app disable` 不删除 Provider 或工具现有配置。
- 修改应用配置目录前展示受影响文件。
- 相对路径在非交互模式下拒绝。
- Plugin 安装先验证 Manifest、路径和 Hook 权限。
- Plugin 删除不默认删除该 App ID 对应的数据库记录；使用 `--purge-data` 才进入危险流程。

### 10.3 Settings

```text
ochcli settings list
ochcli settings get <path>
ochcli settings set <path> <value>
ochcli settings unset <path>
ochcli settings export
ochcli settings import <file>
```

覆盖范围包括：

- 语言和主题模式。
- 托盘、最小化、后台驻留。
- 启动时运行、静默启动、自动更新检查。
- Codex auth/history 选项。
- 应用启用状态和当前 Provider。
- Skill 存储位置与同步方式。
- 自动备份周期和保留数量。
- WebDAV/S3 设备设置。
- Preferred terminal。

单字段设置同样经过 typed validation，不直接修改 JSON。

### 10.4 Provider

```text
ochcli provider list --app <app-id>
ochcli provider show <id> --app <app-id>
ochcli provider current --app <app-id>
ochcli provider add --app <app-id> [--from <file>] [--set ...]
ochcli provider edit <id> --app <app-id> [--patch <file>] [--set ...]
ochcli provider delete <id> --app <app-id>
ochcli provider sort --app <app-id> <id>...
ochcli provider copy <id> --from-app <id> --to-app <id>
ochcli provider export <id> --app <app-id>
ochcli provider seed-official --app <app-id>
ochcli provider import-live --app <app-id>
ochcli provider sync-live [--app <app-id>|--all]
ochcli provider preview <id> --app <app-id>
ochcli provider switch <id> --app <app-id>
ochcli provider add-to-live <id> --app <app-id>
ochcli provider remove-from-live <id> --app <app-id>
ochcli provider test <id> --app <app-id>
ochcli provider speed-test <id> --app <app-id>
ochcli provider models <id> --app <app-id>
ochcli provider balance <id> --app <app-id>
ochcli provider quota <id> --app <app-id>
ochcli provider usage-script run <id> --app <app-id>
ochcli provider usage-script test --app <app-id> --from <file>
ochcli provider terminal <id> --app <app-id>

ochcli provider endpoint list --app <app-id>
ochcli provider endpoint add <url> --app <app-id>
ochcli provider endpoint remove <url> --app <app-id>

ochcli config common get --app <app-id>
ochcli config common set --app <app-id> --from <file>
ochcli config common extract --app <app-id>
ochcli config common apply --app <app-id> [--provider <id>...]
```

语义：

- `delete` 删除 OcHub 数据；如果资源仍在 live config，默认拒绝并提示先 `remove-from-live`，或使用显式组合参数。
- `remove-from-live` 不删除 OcHub Provider。
- Switch 应用使用 `switch`；Additive 应用使用 `add-to-live/remove-from-live`。
- `copy` 调用 core 的跨 App 转换器，不能由 CLI 拼字段。
- `seed-official` 幂等创建缺失的内置官方 Provider，不覆盖同 ID 的用户修改。
- `sync-live` 根据数据库当前状态重建 managed live 范围，仍需执行漂移计划。
- `preview` 返回 managed diff、漂移和 Secret 脱敏后的目标文件。
- `test/speed-test/models/balance/quota` 是网络操作，受 `--offline` 和 `--timeout` 控制。

### 10.5 Managed Auth

```text
ochcli auth copilot status
ochcli auth copilot login
ochcli auth copilot poll <flow-id>
ochcli auth copilot account list
ochcli auth copilot account set-default <id>
ochcli auth copilot account remove <id>
ochcli auth copilot token [--account <id>]
ochcli auth copilot models [--account <id>]
ochcli auth copilot usage [--account <id>]

ochcli auth codex status
ochcli auth codex login
ochcli auth codex poll <flow-id>
ochcli auth codex logout [--account <id>]
ochcli auth codex account list
ochcli auth codex account set-default <id>
ochcli auth codex account remove <id>
ochcli auth codex models
ochcli auth codex quota

ochcli auth binding list
ochcli auth binding set --app <app-id> --provider <id> --account <id>
ochcli auth binding remove --app <app-id> --provider <id>

ochcli quota subscription <provider-id> --app <app-id>
ochcli quota coding-plan <provider-id> --app <app-id>

ochcli claude-desktop status
ochcli claude-desktop ensure-official
ochcli claude-desktop import-from-claude
```

在无浏览器环境下，登录命令输出 verification URL、user code 和 flow ID。`--open-browser` 只在平台支持时尝试打开浏览器，失败不会破坏 device flow。

`auth copilot token` 是 Secret 输出命令，必须遵守 `--show-secrets`、TTY 和 `--yes` 规则；普通 status/account 命令不返回 token。

### 10.6 Gateway、Station 与高级资源

面向大多数用户的抽象：

```text
ochcli gateway status
ochcli gateway config show
ochcli gateway config set [--port <port>] [--require-key <bool>]
ochcli gateway start
ochcli gateway stop
ochcli gateway restart
ochcli gateway serve
ochcli gateway health
ochcli gateway models
ochcli gateway supported-apps
ochcli gateway connection-info [--app <app-id>]
ochcli gateway probe-dialect --url <url>

ochcli station list
ochcli station show <id>
ochcli station add --from <file>
ochcli station edit <id> --patch <file>
ochcli station delete <id>
ochcli station enable <id>
ochcli station disable <id>
ochcli station probe <id>
ochcli station models <id>
ochcli station select <id> --app <app-id>
ochcli station apply <id> --app <app-id>
ochcli station disconnect --app <app-id>
ochcli station connection-info <id> --app <app-id>
```

完整高级能力：

```text
ochcli gateway channel list|show|add|edit|delete|enable|disable|probe
ochcli gateway channel import-provider <provider-id> --app <app-id>
ochcli gateway route list|show|add|edit|delete|enable|disable
ochcli gateway route rule list|add|edit|delete|sort
ochcli gateway key list|show|create|revoke|bind
```

Gateway 配置覆盖：

- 监听地址和端口。
- autostart。
- 本地 key 要求。
- health interval。
- Channel URL、Key、Dialect、Path override。
- 模型 matcher、override、priority、weight。
- Station/Endpoint 分组。
- Route allowed channels、默认模型和模型规则。
- Reasoning auto/passthrough/disabled。
- Reasoning low/medium/high/max budget。
- 客户端 key 与 route 绑定。

安全约束：

- 默认只绑定 loopback。
- 非 loopback 绑定首版拒绝；未来如开放必须有独立安全设计。
- `gateway start` 在没有 owner 时启动 daemon，或给出明确的安装/前台运行指令。
- `gateway serve` 前台阻塞，适合容器和进程管理器。

#### 10.6.1 Gateway 数据面契约

CLI 负责配置和运维，但不能改变既有数据面兼容目标。daemon/foreground Gateway 必须继续提供：

```text
POST /v1/messages
POST /v1/messages/count_tokens
POST /v1/chat/completions
POST /v1/responses
GET  /v1/models
GET  /models
GET  /health
WS   /v1/responses
```

运行时要求：

- 支持 Anthropic Messages、OpenAI Chat Completions、OpenAI Responses。
- 支持 streaming/non-streaming、SSE 聚合和协议间流转换。
- 保留 Tool、图片/data URL、Usage、Thinking 和 Thinking Signature 语义。
- 路由时在同一显式 priority 内优先 native Dialect。
- 根据 priority、weight、health 和 matcher 选择 Channel。
- 上游失败时按策略重试和 failover，不能把已向客户端提交的流重新播放。
- 健康排除后仍允许按配置回退，避免全部节点被永久隔离。
- 模型 alias、upstream override、hard channel/dialect binding 必须可预测。
- Codex remote compaction 只路由到兼容 Responses 的上游。
- 本地 key 可以绑定客户端标识和 Route，用于 Usage 归因。
- 错误响应维持调用方 Dialect，不把内部 Channel Secret 或完整上游错误泄露给客户端。

`gateway status --json` 至少返回监听地址、运行状态、启动时间、活动 Route、Channel 健康摘要、当前连接数和配置 revision，不返回 Channel Key。

### 10.7 MCP

```text
ochcli mcp list
ochcli mcp show <id>
ochcli mcp add --from <file>
ochcli mcp edit <id> --patch <file>
ochcli mcp delete <id>
ochcli mcp validate [<id>|--from <file>]
ochcli mcp import --app <app-id>
ochcli mcp enable <id> --app <app-id>
ochcli mcp disable <id> --app <app-id>
ochcli mcp sync <id> [--app <app-id>...]
ochcli mcp sync-all

ochcli claude mcp status
ochcli claude mcp config show
ochcli claude mcp server upsert <id> --from <file>
ochcli claude mcp server delete <id>
ochcli claude mcp path validate
ochcli claude mcp onboarding status
ochcli claude mcp onboarding skip
ochcli claude mcp onboarding clear
```

当前完整写入目标为 Claude、Codex、Grok Build、OpenCode 和 Hermes。Claude Desktop 与 OpenClaw 遇到 MCP sync 应返回 capability unsupported。

### 10.8 Skills

```text
ochcli skill list
ochcli skill show <id>
ochcli skill search <query>
ochcli skill discover <repo>
ochcli skill install <source>
ochcli skill uninstall <id>
ochcli skill check <id>
ochcli skill check-all
ochcli skill update <id>
ochcli skill update-all
ochcli skill enable <id> --app <app-id>
ochcli skill disable <id> --app <app-id>

ochcli skill repo list
ochcli skill repo add <url>
ochcli skill repo update <id>
ochcli skill repo remove <id>
ochcli skill repo catalog <id>
```

要求：

- 在执行前检查 Node/npx。
- `--offline` 禁止 search/discover/install/update 的网络步骤。
- 删除或覆盖 Skill 前检查所有目标应用。
- symlink/copy 行为由 settings 中的 sync method 决定。
- 目录边界和 symlink 目标必须验证，禁止路径穿越。

### 10.9 Sessions 与外部工具

```text
ochcli session list [--app <app-id>] [--query <text>]
ochcli session show <id> --app <app-id>
ochcli session delete <id> --app <app-id>
ochcli session delete-batch --from <file>
ochcli session resume <id> --app <app-id>
ochcli session scan [--app <app-id>...]

ochcli tool versions
ochcli tool probe <tool>
ochcli tool install <tool>
ochcli tool update <tool>
ochcli tool terminal <tool>
```

规则：

- 删除前验证 Session 源路径位于目标应用允许的根目录内。
- `delete-batch` 默认打印计划并要求确认；非交互必须带 `--yes`。
- `resume` 依赖平台终端能力；不支持时返回 `PLATFORM_UNSUPPORTED`。
- Claude Desktop 不作为 Session 源。

### 10.10 Usage 与 Pricing

```text
ochcli usage summary [--from <date>] [--to <date>]
ochcli usage sources
ochcli usage by-app
ochcli usage trend --interval <day|week|month>
ochcli usage providers
ochcli usage models
ochcli usage logs
ochcli usage show <request-id>
ochcli usage sync [--app <app-id>...]
ochcli usage limits

ochcli pricing status
ochcli pricing refresh
ochcli pricing list
ochcli pricing missing
ochcli pricing override list
ochcli pricing override set --model <id> --from <file>
ochcli pricing override remove --model <id>
ochcli pricing backfill
```

要求：

- `usage sync` 明确显示扫描的应用和文件范围。
- 统计命令不因定价缺失而失败，返回 missing pricing warning。
- `pricing refresh` 是显式网络操作。
- 一次性 CLI 查询不自动触发后台定价更新。
- 人类输出明确标记成本为估算。

### 10.11 Backup、导入导出与云同步

```text
ochcli backup list
ochcli backup create [--name <name>]
ochcli backup rename <id> <name>
ochcli backup restore <id>
ochcli backup delete <id>
ochcli backup export-sql <file>
ochcli backup import-sql <file>
ochcli backup policy show
ochcli backup policy set --interval <duration> --retain <count>

ochcli sync webdav status
ochcli sync webdav configure --from <file>
ochcli sync webdav test
ochcli sync webdav upload
ochcli sync webdav download
ochcli sync webdav remote-info

ochcli sync s3 status
ochcli sync s3 configure --from <file>
ochcli sync s3 test
ochcli sync s3 upload
ochcli sync s3 download
ochcli sync s3 remote-info

ochcli data-dir show
ochcli data-dir set <path>
ochcli data-dir reset
```

规则：

- Restore、Import SQL、Remote Download 默认先创建本地安全快照。
- Download 输出本地和远端 manifest/hash，不宣称自动合并。
- 上传和下载使用 last-writer-wins 时必须明确展示方向。
- Snapshot 的范围必须在输出中说明：数据库和 OcHub 管理的 Skills，不等价于所有第三方 live config。
- 更换 data-dir 需要停止 Gateway 和后台任务，并在重启后生效。

### 10.12 cc-switch 迁移

```text
ochcli migrate ccswitch detect
ochcli migrate ccswitch plan
ochcli migrate ccswitch import
```

要求：

- `detect` 和 `plan` 只读。
- `import` 显式执行，已有 DB 时先创建快照。
- OcHub 永不写入 `~/.cc-switch`。
- 重复导入沿用稳定 ID 和既有覆盖语义，并在计划中展示。
- 新 CLI/daemon 第一次启动不自动导入。
- 现有 `ochub-server` 的自动导入行为在兼容窗口中保留并给出 deprecation warning，之后由显式开关控制。

### 10.13 应用高级命令

```text
ochcli env scan
ochcli env clean <conflict-id>
ochcli env restore <backup-id>

ochcli claude plugin status
ochcli claude plugin show
ochcli claude plugin apply --from <file>
ochcli claude plugin restore

ochcli codex history status
ochcli codex history migrate
ochcli codex history restore

ochcli opencode omo status
ochcli opencode omo current
ochcli opencode omo local-file
ochcli opencode omo disable
ochcli opencode omo-slim status
ochcli opencode omo-slim current
ochcli opencode omo-slim local-file
ochcli opencode omo-slim disable

ochcli openclaw health
ochcli openclaw model default get|set
ochcli openclaw models
ochcli openclaw agent-defaults get|set
ochcli openclaw env get|set
ochcli openclaw tools get|set

ochcli hermes models get|set
ochcli hermes memory status
ochcli hermes memory limits
ochcli hermes memory read <memory|user>
ochcli hermes memory write <memory|user> --from <file>
ochcli hermes memory enable|disable
```

“打开目录”“打开 Web UI”“复制连接信息”等桌面 convenience 命令可以在有桌面环境时作为可选命令提供，但不属于 headless 核心验收项。对应信息必须始终能通过 stdout 获得。

### 10.14 Theme

```text
ochcli theme list
ochcli theme show <id>
ochcli theme validate <file>
ochcli theme import <file>
ochcli theme export <id>
ochcli theme duplicate <id>
ochcli theme delete <id>
ochcli theme set <id>
ochcli theme mode <system|light|dark>
```

为了实现这些命令，需要把主题文件解析、校验、4.5:1 对比度检查和持久化从 `ochub-app` 移到非 GPUI core 模块。GPUI 仅保留渲染和系统外观监听。

### 10.15 Deep Link 与 Update

```text
ochcli deeplink parse <uri>
ochcli deeplink import <uri>

ochcli update status
ochcli update check
ochcli update install
```

`deeplink import` 必须先经过与普通 add/import 相同的 Schema、安全和 Secret 检查。稳定 CLI 前要清理已移除应用的历史分支并补齐 Grok Build。

Update 能力按平台保持现有差异：

- macOS DMG、Windows Installer、Linux AppImage 可完整更新。
- deb 和 portable 等不支持自替换的包只提供检查和升级提示。
- daemon 更新需要协调停止 Gateway、替换和重启。

### 10.16 Daemon

```text
ochcli daemon run
ochcli daemon status
ochcli daemon install
ochcli daemon start
ochcli daemon stop
ochcli daemon restart
ochcli daemon logs
ochcli daemon uninstall
```

用户级服务实现：

| 平台 | 方案 |
|---|---|
| macOS | `launchd` LaunchAgent |
| Linux | `systemd --user`，无 systemd 时提供前台模式 |
| Windows | 当前用户级 Scheduled Task/Startup；避免首版要求管理员权限 |

`uninstall` 只删除服务注册，不删除数据库、配置、日志和备份。

## 11. 声明式配置

### 11.1 文件结构

目标格式：

```yaml
apiVersion: ochub.io/v1alpha1
kind: OcHubConfig
metadata:
  name: workstation
spec:
  settings:
    language: zh-CN
    preferredTerminal: ghostty
    skillStorageLocation: unified

  apps:
    - id: claude
      enabled: true
    - id: codex
      enabled: true

  providers:
    - id: team-claude
      app: claude
      state: present
      config:
        name: Team Claude
        baseUrl: https://api.example.com
        apiKey:
          fromEnv: TEAM_CLAUDE_KEY
        defaultModel: claude-sonnet-4-5
      live:
        state: active
        onDrift: abort

  mcpServers:
    - id: filesystem
      state: present
      spec:
        type: stdio
        command: npx
        args:
          - -y
          - "@modelcontextprotocol/server-filesystem"
          - /workspace
      apps:
        claude: enabled
        codex: enabled

  gateway:
    enabled: true
    listen:
      host: 127.0.0.1
      port: 4180
    requireKey: true
```

### 11.2 `plan` 与 `apply`

```bash
ochcli config validate -f ochub.yaml
ochcli plan -f ochub.yaml
ochcli apply -f ochub.yaml
```

语义：

- 文件中声明的资源由该配置文件管理。
- 未声明资源默认保持不变。
- `state: absent` 才表示删除。
- `--prune` 可删除该 manager 上次管理但本次消失的资源，必须配合 plan 和确认。
- plan 输出 create/update/delete/noop/conflict。
- apply 在执行前重新检查 plan 基于的 resource version 和文件 hash。
- Secret 引用在 plan 阶段只检查引用是否可解析，不显示值。
- 网络探测默认不属于 apply；可使用 `--verify`。

### 11.3 资源所有权

建议新增 managed resource 元数据：

```text
resource_kind
resource_id
manager
source_path
last_applied_hash
last_applied_at
```

`manager` 示例：

```text
cli:file:/absolute/path/ochub.yaml
gui
manual-cli
import:ccswitch
```

同一资源被另一个 manager 修改时，apply 返回 conflict，不直接夺取所有权。用户可通过 `--adopt` 显式接管。

### 11.4 版本策略

- 初始格式使用 `v1alpha1`。
- Alpha 阶段允许提供自动迁移工具，但不能静默改变文件。
- 稳定 `v1` 后，字段只做向后兼容增加。
- `ochcli config migrate --to v1 <file>` 输出新文件，不覆盖原文件。

## 12. 功能对照矩阵

| 领域 | 当前 Core | 当前 HTTP | 当前 GUI | 目标 CLI | 需要重构 |
|---|---:|---:|---:|---:|---|
| App enable/path | 是 | 是 | 是 | 是 | AppId 统一 |
| Provider CRUD | 是 | 是 | 是 | 是 | Facade 封装 |
| Provider switch | 是 | 是 | 是 | 是 | 统一 drift 输入 |
| Drift preview/resolution | 部分 | 不完整 | 是 | 是 | 从 View 提取 |
| Provider copy/convert | 部分 | 否 | 是 | 是 | 从 View 提取 |
| Custom endpoint | 是 | 是 | 是 | 是 | DTO 稳定化 |
| Model/speed/balance/quota | 是 | 是 | 是 | 是 | 统一网络错误 |
| Copilot/Codex OAuth | 是 | 是 | 是 | 是 | 前台/daemon flow |
| Gateway runtime | 是 | 是 | 是 | 是 | daemon owner |
| Station | 是 | 是 | 是 | 是 | Facade 封装 |
| Channel/route/key | 是 | 是 | GUI 隐藏部分 | 是 | 高级命令 |
| MCP | 是 | 是 | 是 | 是 | Capability 化 |
| Skills | 是 | 是 | 是 | 是 | 依赖探测 |
| Sessions | 是 | 是 | 是 | 是 | 平台返回统一 |
| Usage | 是 | 是 | 是 | 是 | Query DTO |
| Pricing catalog refresh | 是 | 不完整 | 是 | 是 | 用例入口 |
| Backup/SQL | 是 | 是 | 是 | 是 | Journal/锁 |
| WebDAV/S3 | 是 | 是 | 是 | 是 | Secret 输入 |
| cc-switch 手动迁移 | 是 | 不完整 | 是 | 是 | 显式用例 |
| Env/OMO/Codex history | 是 | 是 | 是 | 是 | 命令适配 |
| OpenClaw/Hermes 高级项 | 是 | 是 | 是 | 是 | Schema/Facade |
| Theme | GUI 模块 | 否 | 是 | 是 | 移入 core |
| User Manifest | 部分 | 部分 | 部分 | 是 | AppId/Capability 贯通 |
| Deep Link | 是 | 是 | 是 | 是 | 清理历史分支 |
| Update | 是 | 是 | 是 | 是 | daemon 协调 |
| 托盘/窗口 | 不适用 | 不适用 | 是 | 不适用 | 仅设置可管理 |

## 13. 安全设计

### 13.1 本地控制面

- IPC Endpoint 目录权限为当前用户专用。
- Unix Socket 验证 peer UID；Named Pipe 限制当前用户 SID。
- TCP fallback 使用随机 bearer token，token 文件权限为 0600。
- Control API 不默认监听非 loopback。
- permissive CORS 不用于新的 privileged IPC。
- `owner.json` 不包含 token、Key 或 OAuth 信息。

### 13.2 文件与路径

- 写入前 canonicalize 可解析的父目录。
- 防止 `..`、symlink 跳转和路径穿越。
- 用户显式配置的外部目录必须记录为受信任路径。
- Session 删除只能发生在对应应用允许的 Session 根目录内。
- Backup extract 检查 Zip Slip、绝对路径和软链接。
- 导入文件设置大小、条目数和递归深度上限。

### 13.3 Secret

- 日志、错误、diff、journal、telemetry 和 crash report 全部脱敏。
- 新建敏感文件使用最小权限。
- 不把 Secret 作为命令行参数推荐路径。
- OAuth refresh token 和云同步凭据沿用现有安全存储策略；若未来接入 OS Keychain，需要独立迁移设计。
- `provider export` 默认省略 Secret，只输出引用占位。

### 13.4 Hook 与 Script

用户 Manifest Hook 和 Provider Usage Script 具备执行风险：

- 显示来源、hash 和所需权限。
- 首次执行未知 Hook 时要求信任；非交互需要显式 `--allow-hooks`。
- 设置超时、输出上限和工作目录。
- 环境变量使用 allowlist，默认不继承全部 OcHub Secret。
- rquickjs Script 限制可访问能力。
- Hook 失败不能留下未记录的半完成 mutation。

### 13.5 破坏性操作

以下操作需要 plan 和显式确认：

- Provider/MCP/Skill/Session 删除。
- Snapshot restore/delete。
- SQL import。
- Cloud download 覆盖本地。
- `--prune`。
- data-dir 切换。
- Plugin purge。
- `on-drift=discard`。
- 环境变量清理。

`--yes` 只确认当前命令已经列出的目标；不能确认运行期间新增的未知目标。

## 14. 错误模型

公共错误至少包含：

```text
INVALID_ARGUMENT
VALIDATION_FAILED
NOT_FOUND
AMBIGUOUS_REFERENCE
ALREADY_EXISTS
CAPABILITY_UNSUPPORTED
PLATFORM_UNSUPPORTED
CONFIG_DRIFT
RESOURCE_CONFLICT
OWNER_CONFLICT
RUNTIME_UNAVAILABLE
PROTOCOL_INCOMPATIBLE
DEPENDENCY_MISSING
PERMISSION_DENIED
PATH_UNSAFE
NETWORK_UNAVAILABLE
UPSTREAM_REJECTED
AUTH_REQUIRED
AUTH_EXPIRED
RATE_LIMITED
PARTIAL_FAILURE
RECOVERY_REQUIRED
INTERNAL
```

错误要求：

- `code` 稳定。
- `message` 对人友好。
- `details` 提供脚本所需结构化上下文。
- `retryable` 明确。
- 可执行建议使用 `hints` 数组，而不是把命令埋在长文本中。
- Secret 永不进入 details。

## 15. 日志、进度与可观测性

### 15.1 日志

- CLI 默认只显示 warning 和 error。
- `-v/-vv` 增加 operation 和 debug 信息。
- daemon 写滚动日志，限制总大小和保留天数。
- 每次命令具有 request ID/operation ID。
- 日志字段使用结构化 tracing。
- 默认不记录完整 Provider Payload、Header、Prompt 或 Session 内容。

### 15.2 进度

长任务阶段示例：

```text
validate -> snapshot -> download -> verify -> apply -> finalize
```

TTY 使用单行或多阶段进度；JSONL 输出事件：

```json
{"type":"progress","operationId":"01...","stage":"download","current":2,"total":5}
```

### 15.3 审计

本地 audit 记录可选，建议至少记录：

- 时间。
- operation 类型。
- actor：GUI、CLI、HTTP 或 daemon task。
- 资源 ID。
- 成功/失败。
- 变更摘要。

不记录 Secret 值和完整配置正文。

## 16. 性能与网络行为

- `--help` 和 completion 不加载 GPUI、不打开 DB、不访问网络。
- 普通本地 list/get 不触发 Provider 探测、定价更新或云同步。
- 网络操作必须由命令语义或 `--verify/--network` 明确触发。
- Direct 模式只初始化当前命令所需服务。
- daemon 缓存定价目录、Usage 聚合和健康状态。
- 大列表支持 `--limit`、`--cursor` 和服务端过滤。
- Session/Usage 扫描支持进度、取消和增量结果。
- JSONL 用于大量日志和 Session 输出，避免在内存中构造超大数组。

建议性能目标：

| 场景 | 目标 |
|---|---|
| `--help` / `--version` | 不初始化业务 runtime |
| daemon 本地 RPC 查询 | p95 小于 100 ms，不含扫描和网络 |
| Direct 简单 DB 查询 | p95 小于 300 ms |
| Ctrl-C 响应 | 可中断阶段 1 秒内开始取消 |

这些目标在首个实现阶段通过基准测试校准，不作为绕过正确性的理由。

## 17. 跨平台行为

### 17.1 macOS

- 完整 Provider、Gateway、MCP、Skill、Usage、Backup。
- Claude Desktop 第三方配置。
- `launchd` daemon。
- Terminal resume 和 app bundle autostart。
- 后续 GPUI 改动遵循固定 `/tmp/OCHUB-QA.app` 验收流程。

### 17.2 Windows

- Named Pipe。
- 路径和原子替换行为使用 Windows 语义。
- Claude Desktop 第三方配置。
- 用户级 Scheduled Task/Startup daemon。
- PowerShell completion。

### 17.3 Linux

- Unix Domain Socket。
- `systemd --user` 或 foreground daemon。
- Claude Desktop 原生第三方直配标记为 unsupported。
- AppImage 可自更新；deb 提供检查和包管理器提示。
- 无桌面环境时不尝试 open browser/folder。

### 17.4 容器

- 推荐 `ochcli gateway serve` 或 `ochcli daemon run` 前台模式。
- 通过 volume 挂载数据目录和需要管理的工具目录。
- 不自动安装系统服务。
- Secret 通过环境变量、Secret volume 或 stdin。
- 收到 SIGTERM 后停止接收新请求、等待有限时间并完成 journal 状态写入。

## 18. 兼容与迁移

### 18.1 数据兼容

- 复用当前 SQLite Schema 和 migration 机制。
- CLI 不创建第二份数据库。
- 当前 Provider、MCP、Skill、Gateway、Usage 和 Backup ID 保持不变。
- settings 和 app_paths 保持当前格式，由 typed API 修改。

### 18.2 HTTP API

- 现有 `/api/*` 在兼容窗口保留。
- Handler 逐步改为调用 Application Facade。
- 对外响应可以保持旧 DTO；新 IPC 使用版本化 DTO。
- 在未增加安全方案前，不扩展为远程管理 API。

### 18.3 GUI

- 第一阶段只替换数据/操作调用，不改变 UI。
- GUI 和 CLI 共享 mutation plan、validation 和 error code。
- Owner 收到 CLI mutation 后发布内部 change event，GUI 刷新相关 View。
- GUI 未运行时，CLI 行为不依赖 GPUI。

### 18.4 用户插件

迁移顺序：

1. CLI/IPC 公共协议全部使用 `AppId`。
2. Registry 提供统一 Capability。
3. 内置 `AppType` 通过 Adapter 暴露。
4. Manifest Codec 接入相同 Facade。
5. GUI 可见应用列表和 Provider 页面停止过滤到 `AppType`。
6. 插件命令达到与声明 Capability 一致的可用性后，才宣称完整支持。

### 18.5 不兼容行为修正

以下修正应在 Release Note 中明确：

- 新 CLI/daemon 不自动导入 cc-switch。
- 漂移默认策略为 abort。
- Secret 默认在所有输出中脱敏。
- Runtime owner 存在时禁止第二进程直接写入。
- 不再把历史 Gemini/Profiles 兼容数据表现为当前可配置应用功能。

## 19. 测试策略

### 19.1 Unit

- clap 命令解析。
- App Schema 字段解析。
- Secret redaction。
- error code 到 exit code。
- human/JSON/JSONL formatter。
- capability 判断。
- drift policy。
- operation state machine。
- IPC frame 编解码和版本协商。

### 19.2 Contract

- 所有 JSON DTO 使用 golden snapshot。
- 已发布字段不可无迁移删除或改名。
- GUI Adapter、HTTP Handler、IPC Handler 对相同 Facade Result 的映射。
- 每个 error code 有稳定 exit code。

### 19.3 Integration

使用临时目录和 `OCHUB_TEST_HOME`：

- 初始化全新数据库。
- 每个内置应用的 Provider add/edit/switch/remove。
- JSON/TOML/YAML live config round-trip。
- 外部编辑漂移三种策略。
- MCP import/sync。
- Skill copy/symlink。
- Session 安全删除。
- Backup create/restore。
- WebDAV/S3 fixture server。
- cc-switch plan/import。
- Theme import/validate。

测试不得访问真实用户 HOME。

### 19.4 GUI/CLI Parity

每个业务用例建立 parity fixture：

1. 准备相同 DB 与第三方配置。
2. 分别通过 GUI Adapter 和 CLI Adapter 构造同一 Facade Request。
3. 执行后比较 DB 语义快照。
4. 比较外部配置的规范化 AST，而不是只比较格式空白。
5. 验证 warning、conflict 和 backup 行为相同。

涉及 GPUI 的实际改动验收继续执行：

```bash
just qa-app
```

并使用固定 `/tmp/OCHUB-QA.app` 进行 AX 树和截图验收。CLI-only 改动不要求构建 GPUI 包，但需要 `just ci`。

### 19.5 并发

- GUI owner 与 20 个并发只读 CLI。
- GUI owner 与并发 mutation CLI。
- Direct CLI 竞争 mutation lock。
- owner crash 后 stale endpoint 恢复。
- DB restore 时 Gateway 请求。
- 自动同步与手动 Provider switch。
- operation 中途 SIGTERM/kill 后恢复。

### 19.6 安全

- Secret 不出现在 stdout/stderr/log/journal。
- symlink/path traversal。
- 恶意 Backup archive。
- 超大/深层 JSON/YAML/TOML。
- IPC 非当前用户连接。
- token 文件权限。
- Hook 超时和输出炸弹。
- Session 删除越界。

### 19.7 跨平台 CI

至少覆盖：

- macOS latest。
- Windows latest。
- Ubuntu latest。
- Rust pinned toolchain。
- 无 Node 与有 Node 两种 Skill 环境。

## 20. 验收标准

CLI 达到“业务全功能”需要满足：

1. 七个内置应用的已支持 Provider 能力均可无 GUI 管理。
2. GUI 和 CLI 使用同一 Facade，没有 View/Handler 私有写入流程。
3. Provider 漂移不会被默认覆盖。
4. Gateway 可在 daemon 或前台模式持续运行。
5. MCP、Skills、Sessions、Usage、Pricing、Backup、Sync、Migration 都有机器可读命令。
6. 用户 Manifest 声明的能力可以通过 AppId 和 Schema 驱动命令使用。
7. Secret 在默认 human/JSON/log/journal 中均脱敏。
8. GUI、CLI、daemon 并发不会导致 DB 或第三方配置丢失。
9. 危险操作可 dry-run，并具有明确确认和恢复路径。
10. macOS、Windows、Linux 对不支持能力返回结构化错误。
11. JSON Schema、错误码和退出码有 contract tests。
12. 安装包包含 CLI；无 GUI 安装路径包含 daemon 和服务管理文档。

## 21. 实施阶段

### Phase 0：基线与契约

- 确认本设计。
- 建立逐功能 parity 清单。
- 为现有 live config 生成 golden fixtures。
- 固定公共 App ID、Capability 和 error code。
- 清理 Deep Link/文档中的历史应用分支。

完成条件：每项 GUI 能力都有 core/use-case/CLI 目标映射。

### Phase 1：Application Facade

- 新建 application DTO/error。
- Provider CRUD/switch/preview/copy 进入 Facade。
- Settings/App Registry 进入 Facade。
- HTTP Handler 和 GUI 试点调用 Facade。
- 统一 AppId/Capability 边界。

完成条件：Provider 主流程不再依赖 View 私有逻辑。

### Phase 2：CLI 基础与只读能力

- 新建 `ochcli` crate 和 `ochcli`。
- 全局参数、human/JSON/JSONL、退出码。
- `version/status/doctor/paths`。
- App、Settings、Provider、Gateway、MCP、Usage 只读命令。
- completion 和 manpage。

完成条件：脚本可以无 GUI 安全盘点全部 OcHub 状态。

### Phase 3：安全 Mutation

- Runtime lock。
- Mutation Plan 和 dry-run。
- Provider/App/Settings mutation。
- Secret stdin/env/file。
- Operation Journal 和恢复命令。
- GUI/CLI parity tests。

完成条件：CLI 可安全完成 Provider 全流程，不与 GUI 竞争写入。

### Phase 4：Daemon 与 Gateway

- owner discovery。
- UDS/Named Pipe IPC。
- `ochubd` 和 service management。
- Gateway 生命周期、Station、Channel、Route、Key。
- health、autostart 和 graceful shutdown。

完成条件：纯 headless 主机可长期运行 Gateway。

### Phase 5：全领域覆盖

- MCP 和 Skills mutation。
- Sessions 和 Tools。
- Usage/Price。
- Auth/Quota。
- Backup/WebDAV/S3。
- cc-switch migration。
- Env、OMO、Codex history、OpenClaw、Hermes。

完成条件：除主题和动态插件收尾外，内置应用达到业务 parity。

### Phase 6：动态插件与主题

- Manifest AppAdapter 完整贯通。
- Plugin validate/install/reload/errors。
- Theme 逻辑移入 core。
- CLI Schema 驱动输入。

完成条件：公开宣称“全部应用配置均可由 CLI 管理”。

### Phase 7：声明式配置与发布

- `config validate`、`plan`、`apply`。
- managed resource ownership。
- prune/adopt。
- 跨平台安装包。
- 文档、示例、迁移指南和兼容说明。

完成条件：CI 和服务器场景可以重复、幂等部署。

## 22. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| CLI 直接复刻 GUI 逻辑 | 长期行为分叉 | 先做 Facade，再开放 mutation |
| GUI/CLI 同时写配置 | 配置丢失或损坏 | owner + mutation lock + journal |
| HTTP control 面无鉴权 | 本机恶意网页调用 | IPC 优先；Control API 收紧 CORS/鉴权 |
| 动态 App 仍被 AppType 限制 | 插件名义支持、实际不可用 | AppId/Capability 作为公共协议 |
| 自动迁移产生惊讶 | 首次命令修改用户数据 | 新 CLI 显式 migrate |
| Secret 经 argv/JSON 泄露 | 高安全风险 | stdin/env/file、默认脱敏 |
| 跨 DB/文件无法原子提交 | 半完成变更 | plan、备份、journal、补偿 |
| daemon 跨平台差异 | 安装和生命周期不一致 | 用户级服务 + 前台 fallback |
| 云同步覆盖新数据 | 数据丢失 | manifest/hash、方向确认、安全快照 |
| 命令树过大 | 可发现性差 | capability-aware help、examples、completion |
| User Hook 执行任意代码 | 本机安全风险 | 信任、权限、超时、环境 allowlist |

## 23. 需要确认的产品决策

本设计给出推荐默认值，实施前只需确认以下产品选择：

1. 面向用户的名称采用 `ochcli`，常驻进程采用 `ochubd`。
2. 新 CLI/daemon 首次启动不自动导入 cc-switch。
3. Provider 漂移的非交互默认值为 `abort`。
4. Gateway 首版只允许 loopback。
5. 声明式配置放在基础命令稳定后实现，不阻塞首个 CLI 版本。
6. 用户插件只有在 AppId/Capability 全链路完成后才标记为 CLI fully supported。
7. profile 不进入本轮 CLI 范围，直到形成完整领域模型和产品语义。

## 24. 示例

### 24.1 盘点状态

```bash
ochcli doctor
ochcli app list
ochcli provider list --app codex
ochcli gateway status
```

### 24.2 从 stdin 安全添加 Provider

```bash
printf '%s' "$TEAM_CODEX_KEY" |
  ochcli provider add \
    --app codex \
    --from provider.yaml \
    --secret apiKey=stdin
```

### 24.3 预览并切换

```bash
ochcli provider preview team-codex --app codex
ochcli provider switch team-codex --app codex --on-drift abort
```

如果存在外部修改：

```text
Error [CONFIG_DRIFT]: Codex live config changed outside OcHub.

Path: ~/.codex/config.toml
Hint: inspect with `ochcli provider preview team-codex --app codex`
Hint: rerun with `--on-drift preserve` after reviewing the diff
```

### 24.4 Headless Gateway

```bash
ochcli daemon install
ochcli daemon start
ochcli station add --from station.yaml
ochcli station apply team-relay --app claude
ochcli gateway status --json
```

### 24.5 自动化查询

```bash
ochcli provider list --app claude --json |
  jq -r '.data[] | select(.current == true) | .id'
```

### 24.6 云端恢复前检查

```bash
ochcli sync s3 remote-info
ochcli sync s3 download --dry-run
ochcli sync s3 download --yes
```

### 24.7 隔离测试环境

```bash
OCHUB_TEST_HOME="$(mktemp -d)" ochcli status
```

生产文档不应推荐将 `OCHUB_TEST_HOME` 用作长期多 profile 方案。

## 25. 最终结论

OcHub CLI 的技术可行性已经成立，核心工作是统一业务边界和安全地协调进程，而不是重新实现现有功能。按照本设计推进后：

- GUI 是交互丰富的 Adapter。
- CLI 是可脚本化的 Adapter。
- HTTP API 是兼容和集成 Adapter。
- daemon 是常驻 runtime owner。
- `ochub-core` 的 Application Facade 是唯一业务事实来源。

只有达到这一结构，才能可靠地承诺用户：同一个 OcHub 数据目录和同一套工具配置，无论通过 GUI 还是 CLI 操作，都具有一致、可恢复、可审计的结果。

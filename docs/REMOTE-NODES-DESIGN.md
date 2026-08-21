# OcHub Remote Nodes / Fleet 设计方案

> 状态：**Phase 0 / Phase 1 / 现有桌面业务页面的 Phase 2 已实现；可靠性增强与 Fleet 按本文边界演进**
>
> 目标：让 OcHub 桌面版通过 SSH 安全控制只安装了 `ochcli` 的无桌面环境
>
> 最后更新：**2026-08-02**

本文是架构、协议与安全边界设计。面向用户的安装、WSL 配置、SSH Host Key 核对、
添加节点、切换 Provider 和故障排查步骤，请阅读
[远程节点使用指南](https://docs.ochub.org/zh/guides/remote-nodes)。

当前实现已覆盖版本化 SSH stdio 协议、稳定节点身份、远端策略、系统 OpenSSH
连接与 Host Key 固定、daemon owner 转发、Remote Nodes 桌面工作区、operation
journal、跨 SSH 会话幂等恢复和 `remote-desktop` 审计。现有适用于无桌面节点的
Provider、MCP、Skills、Usage/Pricing、Sessions/全文索引、Network、Settings、
Sync/Backup、Tools/高级维护、Update、Gateway/Station 页面均由同一
`WorkspaceBackend` 在本机和远端执行。后续的 operation recover/rollback、长任务事件、显式文件传输、
Gateway Tunnel 和 Fleet 批量编排仍按本文的 typed method 与逐节点 journal 边界演进，
不通过通用 Shell 或危险降级提前开放。

## 1. 摘要

OcHub Remote Nodes 将无桌面开发机、服务器和容器主机纳入桌面版的统一管理。
用户通过 SSH 建立连接后，可以在桌面版中查看和修改远端的 Provider、MCP、
Skills、Gateway、Usage 和 Sessions 等状态，而不需要在远端安装或运行桌面环境。

本设计的核心决策如下：

1. SSH 负责传输加密、主机认证和用户认证。
2. 不在远端开放新的 HTTP/TCP 管理端口。
3. 远端运行 `ochcli remote serve --stdio`，在 SSH stdin/stdout 上承载版本化协议。
4. 远端数据库、配置文件和 Gateway 始终由远端本机的 runtime owner 执行和维护。
5. 桌面端只消费脱敏 DTO 并提交操作，不下载数据库进行离线修改。
6. 写操作采用 plan/confirm/apply 流程，并使用 revision、idempotency key 和
   operation journal 防止误写、重复写和断线导致的未知状态。
7. 第一阶段聚焦单节点的高价值闭环；多节点一致性和批量变更作为 Fleet 阶段演进。

## 2. 产品定义

### 2.1 名称

- 单节点远程控制：**OcHub Remote Nodes**
- 多节点编排和一致性管理：**OcHub Fleet**
- 远端协议入口：`ochcli remote serve --stdio`

### 2.2 用户价值

Remote Nodes 解决以下场景：

- 用户在本地运行 OcHub 桌面版，在远端 Linux 开发机运行 Claude Code、Codex、
  OpenCode 等工具。
- 远端只有 SSH 和 `ochcli`，没有桌面环境。
- 用户希望从同一个桌面界面切换远端 Provider、同步 MCP/Skills、查看用量、
  管理 Gateway，并诊断配置漂移。
- 团队希望查看多台机器的配置一致性，并在确认计划后批量应用变更。

### 2.3 核心体验

桌面版提供一个始终可见的操作目标选择器：

```text
[ This Mac ▾ ]   Providers / Codex
```

切换远端后：

```text
[ prod-gpu-01 · ubuntu@10.0.0.8 ▾ ]   Providers / Codex
```

选中远端节点后，现有功能页面的作用域随之切换。远端身份必须持续显示，
所有写操作的确认界面都必须再次显示目标主机，防止用户忘记当前操作对象。

## 3. 目标与非目标

### 3.1 目标

- 只安装 `ochcli` 的远端环境可以被桌面版发现和控制。
- 复用用户已有的 OpenSSH 配置、SSH Agent、ProxyJump 和硬件密钥。
- 不扩大远端网络暴露面。
- 本地和远端使用相同的 Application Facade、校验、错误模型和业务语义。
- 支持断线重连、版本协商、能力发现和操作进度。
- 远端写操作可预览、可审计、可检测冲突、可恢复。
- 逐步让现有桌面页面同时支持 Local 和 Remote backend。

### 3.2 非目标

- 不实现通用远程终端或任意 Shell 执行。
- 不把 SSH 替换成 OcHub 自有的认证和加密协议。
- 不将远端 SQLite 下载到本地后修改再上传。
- 不在首版提供中心化云控制面或远程多租户服务。
- 不在首版保证所有 CLI 命令都能通过桌面远程执行。
- 不默认保存 SSH 密码、私钥内容或已有 Provider Secret。
- 不在离线状态下排队执行 mutation。
- 不把本地和远端 Gateway 数据面隐式混在一起。

## 4. 总体架构

```text
┌───────────────────────────────────────────┐
│ OcHub Desktop                             │
│                                           │
│ App UI → WorkspaceBackend → RemoteClient  │
└───────────────────┬───────────────────────┘
                    │
                    │ system OpenSSH
                    │ versioned JSONL over stdin/stdout
                    ▼
┌───────────────────────────────────────────┐
│ Remote host                               │
│                                           │
│ ochcli remote serve --stdio               │
│              │                            │
│              │ local UDS / named pipe     │
│              ▼                            │
│       ochcli daemon run / runtime owner    │
│              │                            │
│              ▼                            │
│       Application Facade                  │
│       ├── ~/.ochub SQLite                 │
│       ├── live tool configuration         │
│       ├── Gateway                         │
│       └── operation journal               │
└───────────────────────────────────────────┘
```

### 4.1 控制面

控制面使用一条 SSH 会话：

```sh
ssh -T <ssh-alias> -- ochcli remote serve --stdio
```

桌面端使用子进程参数启动系统 OpenSSH，不通过 Shell 拼接完整命令。远端程序只从
stdin 读取协议帧并向 stdout 写协议帧；诊断日志只能写入 stderr。

### 4.2 远端 runtime

`remote serve` 启动后执行：

1. 探测远端 active owner。
2. 如果 owner 存在，通过本地 IPC 转发请求。
3. 如果 owner 不存在，按策略启动 `ochcli daemon run`，再连接本地 IPC。
4. 如果远端平台无法提供持久 daemon，可进入明确标识的 ephemeral 模式。
5. SSH 断开时终止控制桥，但不终止已经独立运行的 daemon 和 Gateway。

远端仍然遵守一个 owner、一个 mutation writer 的既有约束。Remote Bridge 不直接
绕过 owner 修改数据库或第三方工具配置。

### 4.3 可选数据面隧道

控制面稳定后，可以增加显式的“打开 Gateway 隧道”功能：

```text
127.0.0.1:<local-port>
        │ SSH local forwarding
        ▼
remote 127.0.0.1:<gateway-port>
```

该隧道是独立、可见、可停止的数据面能力。它不能随着控制连接自动开启，也不能改变
Gateway 默认只监听远端 loopback 的安全策略。

## 5. SSH 集成

### 5.1 使用系统 OpenSSH

首版优先调用系统 `ssh`，不内置新的 SSH 协议栈，原因包括：

- 自动兼容 `~/.ssh/config`。
- 复用 SSH Agent、系统 Keychain 和硬件密钥。
- 支持 ProxyJump、企业 Bastion 和 ControlMaster。
- 沿用用户已有的主机算法、密钥和合规策略。
- 减少 OcHub 直接处理私钥和密码的范围。

桌面端需要提供 OpenSSH 可用性诊断，并在平台缺少 `ssh` 时给出明确安装说明。

### 5.2 主机认证

- 优先使用用户现有 `known_hosts`。
- 未知主机必须展示 Host Key 类型和 SHA256 指纹。
- 用户明确确认后才能加入 OcHub 管理的 known-hosts 文件。
- 默认不使用静默 `StrictHostKeyChecking=no`。
- Host Key 变化时立即阻断连接，不能自动覆盖。
- UI 应提示用户通过云控制台、运维系统或其他可信渠道核对首次指纹。

OcHub 自有记录建议保存在：

```text
~/.ochub/ssh/known_hosts
```

文件权限在 Unix 上为 `0600`。

### 5.3 用户认证

首版支持：

- SSH Agent
- `~/.ssh/config` 中的 IdentityFile
- 系统 Keychain / 硬件密钥
- ProxyJump

首版不保存：

- SSH 密码
- 私钥内容
- 私钥 passphrase

密码认证如需支持，应通过受控的 SSH_ASKPASS 集成单独设计。

### 5.4 受限控制密钥

标准 SSH 登录的实际权限等同于远端 Unix 用户。若需要真正的 capability 限制，
可以提供可选的专用控制密钥部署模式，在 `authorized_keys` 中使用 forced command：

```text
restrict,command="/usr/local/bin/ochcli remote serve --stdio" ssh-ed25519 ...
```

这种密钥只能进入 OcHub Remote 协议，不能获得通用 Shell。远端 policy 才能在此模式下
形成真实的 read-only/read-write 权限边界；普通 SSH 用户仍可绕过 UI 直接运行本机命令。

## 6. 远程协议

### 6.1 协议位置

建议新增独立 workspace crate：

```text
crates/protocol/
├── src/frame.rs
├── src/handshake.rs
├── src/capability.rs
├── src/operation.rs
├── src/error.rs
└── src/lib.rs
```

桌面端、CLI Remote Bridge 和本地 runtime IPC 可以共享 DTO，但远程协议版本与现有
本地 IPC 版本独立演进。

### 6.2 Frame

协议使用 UTF-8 JSON Lines，一行一个 Frame：

```json
{"type":"hello","protocolMin":2,"protocolMax":2,"clientVersion":"0.5.0","locale":"zh-CN"}
{"type":"helloAck","protocolVersion":2,"serverVersion":"0.5.0","node":{"id":"...","hostname":"prod-gpu-01","os":"linux","arch":"x86_64"},"capabilities":["status.read","provider.read","provider.write","provider.network"]}
{"type":"request","protocolVersion":2,"requestId":"...","method":"provider.switch.plan","params":{"app":"codex","providerId":"team"}}
{"type":"response","requestId":"...","ok":true,"data":{},"warnings":[]}
{"type":"event","requestId":"...","event":"progress","stage":"write-live-config","current":2,"total":3}
{"type":"cancel","requestId":"..."}
{"type":"ping","timestamp":"..."}
{"type":"pong","timestamp":"..."}
```

### 6.3 协议能力

- 协议版本范围协商
- 服务端版本和 Schema 版本
- 节点 ID、hostname、OS、架构和当前用户
- capability discovery
- request ID / trace ID
- mutation idempotency key
- resource revision / file hash
- progress event
- cache invalidation event
- cancellation
- heartbeat
- 明确的 retryable error
- 最大 Frame 大小
- 请求超时和并发限制

### 6.4 不使用任意 argv 作为长期契约

现有本地 daemon IPC 可以执行结构化 argv，这适合快速原型，但不应成为正式远程契约。
正式协议应映射到类型化 Application 用例，例如：

```text
status.read
doctor.run
app.list
provider.list
provider.get
provider.switch.plan
provider.switch.apply
gateway.status
gateway.start
gateway.stop
operation.list
operation.inspect
```

原因包括：

- CLI 参数会随版本演进。
- argv 容易携带 Secret 并进入进程信息或诊断上下文。
- 类型化操作更容易做 capability、审计和兼容性检查。
- UI 不应该依赖 human CLI 输出。

原型阶段可以在 Remote Bridge 内部保留受限的 `cli.execute`，但必须有服务端 allowlist，
且不允许 Shell、daemon uninstall、任意路径写入或未经确认的破坏性命令。

## 7. 节点身份和连接存储

### 7.1 远端节点身份

每台远端主机生成稳定的非秘密节点 ID：

```text
~/.ochub/node.json
```

示例：

```json
{
  "schemaVersion": 1,
  "nodeId": "4b518b86-...",
  "createdAt": "2026-07-30T00:00:00Z"
}
```

节点 ID 用于处理 hostname、地址和 SSH alias 变化；真正的主机身份认证仍由 SSH Host Key
完成，节点 ID 本身不是认证凭据。

### 7.2 桌面连接记录

建议单独保存：

```text
~/.ochub/remote-hosts.json
```

记录可以包含：

```json
{
  "schemaVersion": 2,
  "hosts": [
    {
      "id": "local-connection-id",
      "sshAlias": "prod-gpu-01",
      "remoteNodeId": "4b518b86-...",
      "hostKeyFingerprint": "SHA256:...",
      "ochcliPath": "ochcli",
      "tags": ["production", "gpu"],
      "lastSeenAt": "2026-07-30T00:00:00Z"
    }
  ]
}
```

不保存密码、私钥内容或 Provider Secret。该文件是设备本地连接信息，默认不进入
OcHub 云同步、远端数据库或普通数据库备份。

连接记录也不保存用户自定义的节点名称。界面名称来自 OCH 协议握手中的
`node.hostname`；在尚未完成握手或节点离线时，暂时使用 `sshAlias` 作为连接标识。
从 schema 1 升级时会删除原有的 `label` 字段，并将记录重写为 schema 2。

## 8. 写操作与一致性

### 8.1 两阶段操作

所有高价值远程 mutation 使用：

```text
plan → user confirmation → apply
```

Plan 至少包含：

- node ID 和显示主机名
- operation type
- operation ID / plan ID
- 当前 resource revision
- 数据库变更摘要
- live config 文件路径
- 脱敏 diff
- drift 和 ownership conflict
- 是否包含删除
- 是否要求 daemon/Gateway 重启
- plan 失效条件

Apply 请求包含：

```json
{
  "planId": "...",
  "expectedRevision": "...",
  "idempotencyKey": "..."
}
```

远端在执行前重新计算 revision。发生变化时返回 conflict，不使用旧计划继续写入。

### 8.2 Operation Journal

远端沿用本机 operation journal：

```text
planned → prepared → db_committed → files_committed → completed
```

SSH 断线不等于 mutation 失败。重连后桌面端必须通过 operation ID 查询最终状态，
不能直接重复执行。只有 operation 明确标记 retryable，或者相同 idempotency key
确认安全时，才允许重试。

### 8.3 并发

- 每台远端主机仍然只有一个 runtime owner。
- mutation 通过远端 mutation lock 串行化。
- 只读请求可以在后续版本并发执行。
- 桌面同一节点默认限制 mutation 并发为 1。
- Fleet 批量执行可以跨节点并发，但单节点内部仍串行。

### 8.4 离线行为

- 可以展示最后一次成功获取的脱敏缓存。
- 缓存必须显示获取时间和“离线/可能过期”状态。
- 不允许在离线时创建待发送 mutation。
- 重新连接后先刷新 revision，再允许操作。

## 9. 安全约束

### 9.1 Secret

- 响应默认脱敏。
- 远端已有 Provider Key 不返回明文。
- Provider drift 仅返回字段路径；`live` / `incoming` 值在 SSH 出站前统一掩码，
  真实值只用于远端 revision 计算和 apply 前复核。
- 新 Secret 通过加密 SSH 会话中的协议参数传输。
- Secret 不进入 argv、日志、error details、operation journal 或桌面缓存。
- `showSecrets` 远程能力首版禁用。
- Secret 字段在内存中应尽快释放，后续可以引入 secrecy/zeroize。

### 9.2 路径

- Remote Bridge 不接受任意文件读取/写入方法。
- 文件类操作使用 Application 层已知资源和路径策略。
- 上传 declarative manifest 时传内容和来源名称，不允许桌面指定任意远端目标路径。
- 下载备份和日志需要独立的大小限制、类型限制和确认设计。

### 9.3 操作策略

服务端 capability 至少区分：

```text
read
write
gateway.lifecycle
daemon.lifecycle
backup.restore
update.install
```

以下能力首版默认不开放或要求额外确认：

- runtime shutdown
- daemon uninstall
- database restore/import SQL
- update install
- data-dir change
- Secret 导出
- 大批量 session 删除

### 9.4 审计

远端 journal 记录：

- operation ID
- actor 类型：`remote-desktop`
- 桌面设备 ID（非秘密）
- SSH 远端可见用户信息
- node ID
- operation type
- plan hash
- 开始和完成时间
- 成功、失败、部分失败

审计数据不记录 Secret 或完整敏感配置。

## 10. 桌面架构改造

### 10.1 当前约束

当前 `AppRoot` 和多个 View 直接持有 `Arc<AppState>`，并直接调用数据库或 services。
这种方式只能操作本机状态，不能透明切换到远端。

### 10.2 WorkspaceBackend

建议增加统一的目标 backend：

```rust
enum WorkspaceBackend {
    Local(LocalBackend),
    Remote(RemoteBackend),
}
```

Backend 对 UI 暴露 Application DTO，而不是数据库对象：

```rust
trait ProviderBackend {
    fn list_providers(&self, app: AppId) -> BoxFuture<'static, Result<Vec<ProviderListItem>>>;
    fn get_provider(&self, app: AppId, id: String)
        -> BoxFuture<'static, Result<ProviderDetails>>;
    fn plan_switch(&self, request: ProviderSwitchRequest)
        -> BoxFuture<'static, Result<ProviderSwitchPlan>>;
    fn apply_switch(&self, request: ApplyPlannedOperation)
        -> BoxFuture<'static, Result<OperationOutcome>>;
}
```

Local backend 调用进程内 Application Facade；Remote backend 发送远程协议请求。
同一个 View 只依赖 DTO 和 Backend，不判断数据来自 SQLite 还是 SSH。

### 10.3 渐进迁移

不要求一次重写全部页面。推荐顺序：

1. Remote Nodes 页面和连接管理。
2. Status / Doctor。
3. Apps 和 Providers。
4. Gateway。
5. MCP 和 Skills。
6. Usage 和 Sessions。
7. Settings、Backup、Sync 和 Update 等高风险页面。

## 11. 桌面信息架构

### 11.1 节点选择器

节点选择器放在侧边栏顶部，并始终可见。它是单一工作区下拉框，不把“添加节点”
伪装成一个工作区选项：

- This Mac
- 已固定的远端节点

添加和管理只发生在“远程节点”页面。连接作用域只允许通过侧边栏左上角的工作区
选择器切换；节点列表不再提供第二套会话开关，避免同一节点出现两种切换语义。

不同节点可以使用稳定但克制的 scope 色彩。远端 mutation 对话框必须同时显示图标、
OCH 上报的 hostname、`user@host` 和操作摘要。工作区下拉框只显示 OCH hostname，
不再拼接或维护一份本地节点名称。

### 11.2 节点管理页

每个节点显示：

- 在线、连接中、离线、Host Key 错误、版本不兼容
- SSH endpoint
- hostname / node ID / OS / arch
- `ochcli` 版本
- daemon owner 类型和 PID
- Gateway 状态
- registered/enabled apps
- 未完成 operation
- 上次成功连接时间
- 标签

节点管理页采用 SSH 连接列表：实时在线探测、`ochcli` 版本和平台信息在同一行可见；
列表只负责管理、诊断、安装和更新，当前工作区由左上角选择器统一表达。

“添加”使用模态流程，默认解析本机 `~/.ssh/config` 及其 `Include` 文件中的具体
`Host` 别名，展示解析后的 user、HostName、port 和 IdentityFile 名称。左下角保留
“手动添加”；无论来源如何，保存前都必须进入 Host Key 指纹确认。

操作包括：

- Connect / Disconnect
- Test connection
- Open node
- Edit tags
- Verify Host Key
- Start/install daemon
- View diagnostics
- Remove local connection record

删除连接记录不删除远端数据、daemon 或配置。

## 12. Fleet 演进

Fleet 在单节点 Remote Nodes 稳定后提供：

- 机器标签和机器组
- 版本和 capability 矩阵
- Provider/MCP/Skill 一致性扫描
- declarative manifest 的多节点 plan
- aggregate diff
- 用户一次确认后按并发上限应用
- 每个节点独立 journal
- partial failure 汇总
- 对失败节点使用原 idempotency key 安全重试
- 漂移和离线节点提醒

Fleet 不尝试制造跨机器分布式事务。某个节点失败时，不自动回滚其他已经成功的节点；
UI 显示每个节点的最终状态，并提供明确的重试或单节点回滚入口。

## 13. 代码落点

### 13.1 Workspace

```text
Cargo.toml
crates/protocol/
```

### 13.2 CLI

```text
crates/cli/src/command.rs
crates/cli/src/run.rs
crates/cli/src/remote.rs
crates/cli/src/runtime_client.rs
crates/cli/src/daemon.rs
```

新增命令建议：

```sh
ochcli remote probe
ochcli remote serve --stdio
ochcli remote policy show
ochcli remote policy validate
```

### 13.3 Core

```text
crates/core/src/application/
crates/core/src/runtime/
crates/core/src/remote_policy.rs
crates/core/src/node_identity.rs
```

需要补齐：

- 类型化远程 DTO
- resource revision
- plan/apply 契约
- idempotency 记录
- remote actor audit
- capability 计算

### 13.4 Desktop

```text
crates/app/src/remote/
├── mod.rs
├── ssh.rs
├── client.rs
├── connection.rs
├── store.rs
└── backend.rs

crates/app/src/remote_view.rs
crates/app/src/app_ui.rs
```

## 14. 分阶段实现

### Phase 0：协议与可行性原型（已完成）

- 新增协议 crate。
- 实现 hello/helloAck、request/response、ping/pong。
- 实现 `ochcli remote probe`。
- 实现 `ochcli remote serve --stdio`。
- 通过系统 SSH 完成 status 和 doctor 请求。
- 验证 daemon owner 转发和无 daemon 启动路径。

完成条件：桌面开发工具或测试客户端能通过一条 SSH 会话稳定读取远端状态。

### Phase 1：Remote Nodes MVP（已完成）

- 添加、测试、删除节点。
- 侧边栏工作区下拉选择。
- SSH Config 自动发现弹窗和手动添加入口。
- Host Key 固定。
- 带在线探测、版本和平台信息的节点连接列表与状态页。
- Apps 列表。
- Provider 列表、详情、当前值。
- Provider switch plan/apply。
- Gateway 状态、启动和停止。
- 断线重连、超时和版本错误。
- 操作后刷新远端状态。

完成条件：用户可以在桌面版连接无头主机，安全预览并切换远端 Codex/Claude
Provider，且远端 Gateway 能由 daemon 持续运行。

### Phase 2：完整远程工作区（现有无桌面业务页面已完成）

目标不是在 Remote Nodes 管理页继续堆叠快捷操作，而是让工作区选择器决定正常 GUI
页面背后的 Backend。除纯桌面外观、窗口、托盘等控制端能力外，远端只安装一个
`ochcli` 时应具备与本机相同的管理面。

迁移范围与完成状态：

1. Provider 全功能：CRUD、排序、复制、原生配置导入/同步、公共配置、端点、测速、
   模型、余额/额度、Auth 和 Secret 写入。
2. App Settings 和桌面现有 Gateway/Station 全功能，包括应用连接信息。
3. MCP、Skills。
4. Usage、Pricing、Sessions 和 Session Index。
5. Tools、环境冲突、应用专项高级配置。
6. Settings、Network、Sync、Backup、Migration 和远端 Update。
7. operation list/inspect 与 mutation journal。

当前开发状态：

- Provider 已覆盖 CRUD、排序、复制、原生配置同步、公共配置、端点、测速、模型、
  余额/额度和 Secret 写入。
- Provider 编辑使用 JSON Merge Patch，未修改的 Secret 不需要从远端读回。
- 直接 mutation 要求 idempotency key；Secret 写入跟随 `allowWrite`。
- 工作区下拉驱动正常 GUI，而不是远程节点页里的另一套快捷操作。
- MCP 列表、添加/编辑、删除、应用开关、导入与同步已通过同一 Backend，并使用
  Merge Patch 保留未回传的 Secret。
- Skills 列表、市场搜索、仓库发现、安装/卸载/更新、应用开关和仓库管理已通过
  `WorkspaceBackend`；远端技能目录不会错误地在控制端 Finder 中打开。
- Usage 摘要、趋势、Provider/模型统计、请求日志/详情、来源同步、定价覆盖与默认倍率
  已通过 `WorkspaceBackend`；`ochcli` 新增此前 GUI 独有的定价默认值读写。
- Sessions 列表、完整消息、元数据/全文搜索、索引构建/维护/删除与会话删除已通过
  `WorkspaceBackend`；远程作用域不会读取控制端会话目录或索引。
- Gateway Station 列表、新建、Merge Patch 编辑、启停、删除、端点探测、模型发现和
  应用绑定、Provider 导入、连接信息已通过 `WorkspaceBackend`；删除操作在 core 层
  拒绝仍被应用引用的 Station。
- Network proxy、通用 Settings/App Settings、数据目录、cc-switch migration、
  WebDAV/S3 Sync、备份/恢复、SQL 导入导出全部在所选节点执行；远端路径不会在控制端
  展开。
- Tools 的 CLI 生命周期、环境冲突、Claude/Codex/OpenClaw/Hermes 高级维护以及 About
  页的更新检查/安装已使用受限 typed action；桌面登录项、托盘、主题和打开本地文件夹
  保持控制端语义。
- 高风险恢复、数据导入和更新安装分别要求 `backup.restore` / `data.import` /
  `update.install` capability；新 Secret 写入跟随 `allowWrite`。
- 协议协商范围为 v1–v2，新增方法依赖 capability；旧 `ochcli` 可以继续建立连接，
  但完整工作区和受管更新需要新版桌面端与远端 `ochcli`。

完成条件：所有适用于无桌面环境的 GUI 管理能力在本机与远端拥有一致体验；页面通过
capability 表达平台或版本差异，而不是退回独立的远程快捷面板。

### Phase 2.1：单文件安装与远程一键更新（已完成）

- 发布包只要求一个 `ochcli`；同一文件同时提供命令、SSH Bridge 和持久 Owner。
- `ochcli node install` 建立用户级 versions/current 布局、稳定命令入口和用户服务；
  没有 systemd 的 WSL 使用按需后台 Owner。
- 新增 `headless.json`，按目标平台记录下载地址、字节数、SHA-256 和 payload minisign
  签名；未签名发布仍可展示版本，但不能一键安装。
- `node.update.read` 默认可用；`node.update.install` 和 `node.update.relay` 默认授权，
  可通过 `allowUpdateInstall = false` 显式禁用。
- 桌面端先读取节点版本和平台；自动策略优先在控制端下载后经 SSH 中继，以便持续展示
  下载与上传字节进度并支持取消。远端直连下载保留为显式手动策略。
- 即使节点连更新清单也无法获取，桌面端仍可自行获取清单并进入中继路径。
- 中继接收端重新校验 Node ID、平台、长度、哈希与签名，不把控制端当作信任根。
- 激活通过稳定 `current` 链接完成；Owner 重启后必须报告目标版本，否则自动回滚。
- 更新 UI 明确区分控制端下载、SSH 上传、远端安装和重新连接；下载/上传阶段可以取消，
  完整 payload 通过长度、哈希和签名校验前不会进入激活流程。
- 受管安装在安装前后及回滚后执行边界内 GC：只保留 state 引用的 active/previous 两个
  版本，并清除中断遗留的 `.tmp`、`.current-*` 与 `.ochcli-*` 原子写入产物；GC 不跟随
  符号链接，也不扫描 OcHub 托管目录之外的版本文件。安装、回滚与 GC 共用排他文件锁，
  避免两个 SSH 更新会话同时清理彼此的暂存文件。
- SSH 配置若仍指向旧 bootstrap 文件，远程入口会在读协议前移交给受管 `current`，
  避免桥接协议与 Owner 版本分裂。
- SSH 可达但远端缺少/无法执行 `ochcli`，或版本早于当前协议时，桌面将失败分类为稳定的
  本地化故障；详情弹窗保留退出码与原始 stderr，连接行不再展示整段底层错误。
- 对缺少、过旧、无执行权限或架构错误的 CLI，桌面可先用固定只读脚本识别 Unix 平台，
  在控制端下载并验签 raw `ochcli`，再通过 Host Key 已确认的 SSH stdin 上传。固定安装脚本
  只校验长度/哈希、执行 `node install` 并验证稳定入口，不接收任意用户命令、不使用 sudo。
- 初次安装后必须以稳定绝对路径再次完成远程协议握手并报告清单版本，随后才保存 Node ID、
  `lastSeenAt` 与连接路径；因此远端非交互 PATH 不包含 `~/.local/bin` 也可继续控制。
- 首次 bootstrap 只以 SSH 登录权限为授权边界；节点运行后的一般更新仍要求远端显式广告
  `node.update.install` / `node.update.relay` capability，二者不能混为一个绕过 policy 的入口。

完成条件：WSL、Linux 开发机或无桌面 macOS 只需安装一个 `ochcli`，随后可在桌面连接
列表中查看版本并安全完成一键更新。

### Phase 2.2：可靠性与数据面增强（后续演进）

- operation recover/rollback。
- 长任务 progress/cancel 与主动 cache invalidation event。
- 明确的远端文件上传/下载，不把控制端路径误当作远端路径。
- 可选、显式、可停止的 Gateway SSH Tunnel。
- 若未来新增低层 Channel/Route/Rule/Key 桌面页面，同步实现其远端 Backend；现有
  `ochcli gateway channel/route/key` 命令本身不通过通用 Shell 暴露。

### Phase 3：Fleet（后续演进）

- 标签和节点组。
- 多节点状态与版本矩阵。
- 一致性和漂移扫描。
- aggregate plan。
- 有并发上限的批量 apply。
- partial failure、重试和审计。

完成条件：团队可以安全管理一组无头 OcHub 节点，而不是逐台登录执行命令。

## 15. 测试策略

### 15.1 协议

- Frame golden tests
- 未知字段向前兼容
- 不兼容协议版本
- 超大 Frame
- 非 UTF-8 和无效 JSON
- request ID 关联
- cancel 和 heartbeat
- Secret 不出现在序列化快照

### 15.2 SSH

- 已知 Host Key
- 未知 Host Key
- Host Key 变化
- Agent 认证
- IdentityFile
- ProxyJump
- `ochcli` 不在非交互 PATH
- SSH 断线和重连
- stdout 协议与 stderr 日志隔离

### 15.3 Runtime

- 远端 daemon 已运行
- 没有 daemon 时按需启动
- 单个 `ochcli` 安装后同时提供命令入口与 daemon owner
- 直连更新与桌面 SSH 中继更新
- payload 长度、SHA-256、签名和目标平台校验
- 原子激活、Owner 版本健康检查和失败回滚
- stale owner endpoint
- 并发 mutation
- SSH 在 mutation 中途断开
- 使用 operation ID 查询最终状态
- 重复 idempotency key

### 15.4 UI

- 本机/远端作用域始终可见
- Host Key 确认文案
- plan diff 包含目标节点
- 离线缓存标记
- 版本不兼容提示
- 远端操作后刷新状态和文案

GPUI 改动后的验收遵守仓库 `AGENTS.md`：使用 `just qa-app` 构建固定
`/tmp/OCHUB-QA.app`，再通过 computer-use 读取 AX 树、操作并截图，验收后退出应用并保留包壳。

## 16. 风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| GUI 直接依赖 `AppState` | 页面不能切换远端 | 渐进引入 WorkspaceBackend 和 DTO |
| 远端 CLI/桌面版本不同 | 请求语义不一致 | 协议协商、capability、类型化方法 |
| Host Key 被静默接受 | 中间人攻击 | 首次明确确认，变化立即阻断 |
| Secret 经 argv 或日志泄露 | 凭据泄露 | stdin 协议、默认脱敏、日志检查 |
| SSH 中途断开 | 操作状态不明 | operation ID、journal、幂等查询 |
| 批量操作部分失败 | 节点状态分叉 | 每节点独立结果，不伪装全局事务 |
| 任意远程命令接口 | 扩大攻击面 | 类型化 allowlist，不提供通用 Shell |
| 远端无 daemon | 长期任务随连接退出 | 按需启动 daemon，明确 ephemeral 状态 |
| 非交互 SSH PATH 不含 ochcli | 无法探测 | 保存经过验证的绝对路径并提供诊断 |
| 节点无法访问下载站 | 无法更新 | 控制端验证后经现有 SSH 信任路径中继，节点再次验证 |
| 更新后 Owner 无法启动 | 节点失联 | 保留 previous 版本、健康检查失败自动回滚 |

## 17. 首版验收标准

1. 远端只安装 `ochcli` 时可以完成连接和能力探测。
2. 不需要在远端开放管理端口。
3. 未确认的 Host Key 不会被接受。
4. 桌面可以读取远端 Status、Apps、Providers 和 Gateway 状态。
5. Provider 切换先显示远端生成的脱敏计划和目标节点。
6. Apply 经过 revision 和 idempotency 检查。
7. SSH 断线后可通过 operation ID 确认最终结果。
8. Secret 不出现在 argv、日志、journal 和桌面缓存。
9. 远端 mutation 始终经过该主机的 owner 和 mutation lock。
10. 断开桌面连接后，远端 daemon/Gateway 可以继续运行。
11. 桌面始终明确显示当前作用域是本机还是哪一台远端节点。
12. 版本或 capability 不兼容时拒绝危险降级并给出升级建议。
13. 远端只安装一个 `ochcli` 即可运行持久 Owner，并且命令与 Owner 不会版本分裂。
14. 更新可在远端直连和桌面中继之间选择，两条路径都在节点侧验证签名。
15. 更新重启健康检查失败时恢复 previous 版本。

## 18. 结论

OcHub 已经具备实现 Remote Nodes 所需的主要业务基础：共享 core、Application Facade、
本地 daemon IPC、结构化输出、owner/mutation lock、plan/apply 和 operation journal。
新增工作的重点不是重新实现管理能力，而是：

1. 增加 SSH stdio 上的版本化远程协议。
2. 把 Remote Bridge 安全地接到远端 runtime owner。
3. 将桌面 View 从直接依赖本地 `AppState` 渐进迁移到 Local/Remote Backend。
4. 以 plan、revision、idempotency 和审计约束远程 mutation。

单节点 MVP 已打通“连接节点 → 查看状态 → 预览并切换 Provider → 管理 Gateway”的
基础闭环。当前优先级是完成正常 GUI 页面的 Local/Remote Backend 迁移；只有单节点
达到适用于无桌面环境的功能对等后，才进入多节点 Fleet。

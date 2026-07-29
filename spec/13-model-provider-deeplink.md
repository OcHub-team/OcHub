# 模型供应商 URL 一键导入

## 1. 结论

可以做，并且适合沿用 OcHub 已有的 `ochub://v1/import` Deep Link 协议。

但现有 `resource=provider` 表示“给某个 AI 工具导入一条直连连接”，必须保留兼容；模型供应商页面管理的是 Gateway Station，由一条路由和若干上游接口组成，因此新增：

```text
ochub://v1/import?resource=model-provider&...
```

用户点击后只打开导入预览，不直接写数据库。用户看过供应商名称、API 地址、协议、模型和将要发生的改动后，主动点击“导入模型供应商”才落库。

API Key 可以直接写进 JSON 载荷，再和整份配置一起做 Base64URL 编码。这样链接可以真正做到点击、确认、导入，不需要用户再次复制 Key。

Base64URL 只是编码，不是加密。任何拿到链接的人都能还原 API Key，因此含 Key 的链接本身必须按密钥管理，不应发到群聊、工单或公开网页。

## 2. 当前基础与缺口

仓库已经具备：

- `crates/core/src/deeplink`：解析 `ochub://v1/import`，支持直连 Provider、MCP 和 Skill。
- `Application::parse_deeplink` / `import_deeplink`：CLI 可预览和导入。
- `GatewayChannel`、`GatewayRoute`：模型供应商的持久化模型。
- `GatewayView`：模型供应商编辑、接口检测、模型拉取与本地连接导入。

尚缺：

- `resource=model-provider` 的独立 Schema 和导入服务。
- 桌面应用注册 `ochub` URL Scheme。
- 冷启动、应用已运行时接收 URL 并转到模型供应商页面的入口。
- 导入确认界面。
- Gateway Station 的事务化导入和来源标识。

因此这不是从零开始，但不能直接复用当前 `resource=provider` 的落库逻辑。

## 3. 用户体验

### 3.1 标准流程

```text
供应商网站 / README 中点击“导入 OcHub”
                    │
                    ▼
              唤起 OcHub
                    │
                    ▼
┌──────────────────────────────────────────────┐
│ 导入模型供应商                               │
│                                              │
│  Aster API                                   │
│  aster.example                               │
│                                              │
│  API 地址                                    │
│  https://api.aster.example                   │
│  Messages · Responses · Chat Completions     │
│                                              │
│  模型                                        │
│  claude-* · gpt-* · 另 12 个                 │
│                                              │
│  思考强度                                    │
│  原样传递                                  ▾ │
│                                              │
│  API Key                                     │
│  [ sk-•••••••••••••••••••••••••••• ] [显示] │
│                                              │
│  此链接将新增 1 个模型供应商，不会自动应用到  │
│  Claude、Codex 或其他工具。                  │
│                                              │
│                    取消   导入模型供应商      │
└──────────────────────────────────────────────┘
```

确认后进入“模型供应商”页面，新条目在列表中短暂高亮，并显示：

```text
已导入“Aster API”    测试连接    应用到工具…
```

第一版不在导入后自动改写任何 AI 工具配置。导入供应商和“应用到工具”是两个权限感受完全不同的动作，应分开完成。

### 3.2 API Key

供应商后台生成的个人专属链接可以在编码后的 `payload` 中包含 API Key。预览页只显示掩码，用户点击确认后保存到 OcHub。

公开文档中的通用链接仍可省略 API Key；这种情况下预览页显示输入框，让用户补填后再导入。

### 3.3 应用未安装（后续）

公开传播时建议外面再包一层 HTTPS 落地页：

```text
https://<official-domain>/import/<template-id>
```

落地页尝试打开 `ochub://...`，同时显示“下载 OcHub”和“复制手动配置”。自定义 Scheme 仍是应用实际接收的协议，HTTPS 页面只负责安装兜底，不承载长期密钥。

### 3.4 重复导入（后续）

模板带稳定的 `source.id` 和 `source.revision`。OcHub 已存在同来源配置时，预览页显示差异并提供：

- 更新现有配置：保留本机 API Key，以及用户后来修改的启停状态。
- 另存一份：创建新的模型供应商。
- 取消。

不按显示名称自动覆盖，也不静默更新。

## 4. URL 协议

### 4.1 第一版载荷

第一版只支持内嵌 payload：

```text
ochub://v1/import?resource=model-provider&payload=<base64url-json>
```

约束：

- `payload` 使用无 padding 的 Base64URL，解码后上限 64 KiB。
- 未知字段忽略，未知必需能力通过 `requires` 拒绝，从而允许向前兼容。

远程 Manifest、摘要校验和安装兜底页不属于第一版。

### 4.2 Manifest Schema

```json
{
  "schema": "io.ochub.model-provider/v1",
  "source": {
    "id": "com.aster/default",
    "revision": "2026-07-01",
    "website": "https://aster.example"
  },
  "name": "Aster API",
  "apiKey": "sk-user-secret",
  "dialects": ["messages", "responses", "chat"],
  "models": ["claude-*", "gpt-*"],
  "websocketEnabled": true,
  "endpoints": [
    {
      "baseUrl": "https://api.aster.example"
    },
    {
      "baseUrl": "https://backup.aster.example"
    }
  ],
  "defaultModel": "claude-sonnet-4-5",
  "modelRules": [
    {
      "model": "fast",
      "upstreamModel": "claude-haiku-4-5",
      "dialect": "messages"
    }
  ],
  "applyTo": [
    { "app": "codex", "preferredModel": "gpt-5.4" },
    { "app": "claude", "preferredModel": "claude-sonnet-4-5" },
    { "app": "opencode" }
  ],
  "enabled": true,
  "requires": []
}
```

字段映射：

| Manifest | OcHub |
|---|---|
| `name` | `GatewayRoute.name` 和各 `GatewayChannel.name` |
| `source.website` | `GatewayRoute.website_url` |
| `apiKey` | 各 `GatewayChannel.api_key` |
| `dialects[]` | 供应商支持的接口；与每个 endpoint 展开为 `GatewayChannel` |
| `models[]` | 供应商模型列表；复制到所有派生 Channel |
| `websocketEnabled` | `GatewayRoute.websocket_enabled`；声明原生 Responses WebSocket 能力 |
| `endpoints[]` | 等价的故障切换 URL；数组顺序即尝试顺序，每个 URL 生成一个 `endpoint_id` |
| `defaultModel` | `GatewayRoute.default_model` |
| `modelRules[]` | `GatewayRoute.model_rules` |
| `applyTo[]` | 用户确认后，为每个目标应用创建并切换到 OcHub 本地 Gateway Provider |
| `reasoning` | `GatewayRoute.reasoning` |
| `enabled` | Route 和 Channel 的初始状态 |

`dialects` 只接受 OcHub 当前的 `messages`、`responses`、`chat`。`websocketEnabled` 默认为 `false`，设为 `true` 时必须同时包含 `responses`，并表示所有故障切换地址都原生支持 Responses WebSocket。Endpoint 只接受 `baseUrl`，不能单独声明接口、模型或 WebSocket 能力；如果两个地址能力不同，它们应建成两个模型供应商。Manifest 不允许指定数据库 ID、创建时间、本地监听端口或 OcHub 本地访问密钥。

`applyTo` 支持 `claude`、`claude-desktop`、`codex`、`grokbuild`、`opencode`、`openclaw` 和 `hermes`。每个应用最多出现一次，可通过 `preferredModel` 指定该工具的默认模型；供应商 `models` 非空时，默认模型必须匹配其中一个精确名称或通配规则。Deep Link 只负责在预览页预选目标；桌面端必须等用户确认后才写入工具配置。上游 API Key 不会写入工具配置，目标工具只接收 OcHub 本地 Gateway 地址和独立生成的本地密钥。

### 4.3 思考强度

思考强度默认原样传递：

- `reasoning` 缺失：使用 `passthrough`。
- `reasoning.mode = "passthrough"`：尽可能保持客户端传入的思考参数，不做档位或预算转换。
- `reasoning.mode = "auto"`：明确启用强度与 token 预算之间的自动映射。
- `reasoning.mode = "disabled"`：明确移除思考参数。

只有 `auto` 模式需要预算配置：

```json
{
  "reasoning": {
    "mode": "auto",
    "lowBudget": 4096,
    "mediumBudget": 10000,
    "highBudget": 16000,
    "maxBudget": 32000
  }
}
```

Deep Link 省略 `reasoning` 时，导入预览页默认选中“原样传递”。用户可以在确认导入前改成“自动映射”或“关闭思考”；修改后的选择随模型供应商一同保存。

这也应成为 OcHub 新建模型供应商的统一默认值，而不只是 Deep Link 的特殊规则。实现时需要把 `GatewayReasoningMode` 和 `GatewayReasoningConfig` 的默认模式从当前的 `Auto` 调整为 `Passthrough`；已有明确保存为 `auto` 的配置保持不变。

### 4.4 Key 编码

`apiKey` 是 Manifest 的可选字段。个人专属链接将整份 JSON（包括 `apiKey`）编码到 `payload`：

```text
json utf-8 → base64url no-padding → URL query payload
```

例如原始 JSON：

```json
{
  "schema": "io.ochub.model-provider/v1",
  "name": "Aster API",
  "apiKey": "sk-user-secret",
  "dialects": ["messages"],
  "endpoints": [
    {
      "baseUrl": "https://api.aster.example"
    }
  ]
}
```

生成：

```text
ochub://v1/import?resource=model-provider&payload=<上述 JSON 的 Base64URL>
```

不把 `apiKey` 单独放在明文 Query 参数中。OcHub 解码后只在 Key 输入框中以掩码展示；如果载荷没有 Key，则允许用户现场填写。

含 Key 的 payload 仍可逆，不应被称为加密链接。供应商后台应提示用户：

> 此链接包含你的 API Key。请勿分享；如链接泄露，请立即在供应商后台轮换密钥。

## 5. 校验与安全

导入分为三层，不允许“解析成功即写入”：

1. Parse：只做 URL 和 JSON 结构解析，不联网、不落库。
2. Decode + Validate：解码 payload、校验字段，生成脱敏预览。
3. Commit：用户确认后在单个数据库事务中写入 Route 和 Channels。

必须执行：

- API 地址只允许 HTTP(S)；公网 Manifest 中的 API 地址默认要求 HTTPS。
- Manifest 下载禁止访问 loopback、私网、link-local 和云元数据地址，防止 SSRF。
- API Key 在日志、错误、遥测、AX 辅助功能文案和通知中始终脱敏。
- 应用不得记录原始 Deep Link 或完整 `payload`；错误只记录载荷摘要和随机追踪 ID。
- 不从 Manifest 接受额外请求头中的 `Authorization`、`Cookie`、`Host` 等敏感头；第一版不开放 `extra_headers`。
- 不执行 Manifest 中的代码、脚本或 Shell 命令。
- 名称、URL 数量、模型数量和字符串长度有明确上限。
- 导入前不测试上游；连接测试是预览页或导入完成后的显式动作，避免点击链接就向第三方发请求。
- `enabled=true` 仅表示这个 Station 可参与路由，不表示自动应用到任何工具。

建议限制：

| 项目 | 上限 |
|---|---:|
| endpoint | 8 |
| 供应商 dialect | 3 |
| 供应商模型 | 500 |
| model rule | 100 |
| 单个字符串 | 4 KiB |
| 解码后 payload | 64 KiB |

## 6. 应用内架构

### 6.1 Core

新增独立模块，避免继续扩张面向各工具直连配置的 `DeepLinkImportRequest`：

```text
crates/core/src/deeplink/model_provider.rs
```

核心类型建议为：

```rust
pub struct ModelProviderImportManifest { /* wire schema */ }
pub struct ResolvedModelProviderImport { /* validated + redacted preview */ }
pub enum ModelProviderImportConflict { None, Existing { /* diff */ } }
pub struct ModelProviderImportResult { /* route/channel ids */ }
```

Application API 分成：

```rust
resolve_model_provider_deeplink(uri) -> ResolvedModelProviderImport
commit_model_provider_import(resolved, api_key, conflict_action)
    -> ModelProviderImportResult
```

不要让 UI 自己循环调用 `upsert_gateway_channel`。Commit 服务必须开启 SQLite 事务，一次写完 Route、Channels 和来源信息，任意一步失败全部回滚。

为了可靠处理更新，在 Station/Route 上持久化：

```text
import_source_id
import_source_revision
import_manifest_hash
```

本机 API Key 不参与 Manifest hash。

### 6.2 GPUI

新增全局 `PendingDeepLink` 队列：

- 冷启动：初始化数据库和主窗口后消费。
- 热启动：将 URL 转发给已运行实例，激活主窗口后消费。
- 同时到达多个链接：顺序排队，一次只显示一个确认页。
- 窗口关闭但驻留托盘：重新显示主窗口。

确认页建议作为模型供应商页面顶部的专用导入面板，而不是系统原生小弹窗。它需要显示多个 endpoint、协议、模型摘要、冲突差异、Key 输入与校验错误，普通确认弹窗空间不足。

导入面板的状态机：

```text
Resolving → Preview → Claiming → Committing → Done
              └──────────────→ Error（保留用户输入，可重试）
```

关闭面板或点击取消不会写入任何数据。

### 6.3 系统注册

macOS：

- 正式包 `packaging/macos/Info.plist` 增加 `CFBundleURLTypes` / `CFBundleURLSchemes = ochub`。
- QA 包 `scripts/qa/Info.plist` 同步注册，继续使用固定包 `/tmp/OCHUB-QA.app` 和 Bundle ID `io.ochub.debug.qa`。
- 验收冷启动和已运行两种 `open "ochub://..."` 路径。

Windows：

- Installer 注册 `URL:OcHub Protocol` 与 `ochub` Scheme。
- 第二次启动把 URL 转发给现有进程，而不是创建第二个数据库写入者。

Linux：

- `.desktop` 声明 `MimeType=x-scheme-handler/ochub`，`Exec=ochub %u`。
- 无桌面环境时仍可用 `ochcli deeplink parse/import`。

## 7. 建议分期

### Phase 1：内嵌配置导入

- 注册系统 URL Scheme。
- 支持内嵌 `payload`。
- 新增预览确认面板。
- 支持从编码后的 `payload` 读取 API Key，也支持在 OcHub 内补填。
- 思考强度默认原样传递；允许在预览页选择，或由 payload 明确配置。
- 事务化创建 Station。
- 导入后不自动应用到工具。

这是最小可交付版本，也足以让供应商在文档或控制台放“导入 OcHub”按钮。

### Phase 2：分发与更新

- HTTPS Manifest 与摘要校验。
- 来源签名和更新差异。
- HTTPS 安装兜底页。

### Phase 3：生态能力

- OcHub 内“复制导入链接”，默认只导出无密钥模板。
- `.ochub-provider.json` 文件导入，与 URL 共用同一 Schema。
- 官方模板目录和签名信任标记。

## 8. 验收标准

- 点击 URL 后，冷启动与热启动都只出现一个 OcHub 窗口。
- 未确认前数据库零变更。
- 预览完整展示名称、来源、API 地址、协议、模型摘要、思考强度和是否含凭据。
- payload 未提供 `reasoning` 时，预览和最终落库均为 `passthrough`。
- payload 明确提供 `auto`、`passthrough` 或 `disabled` 时按配置预选，用户仍可在确认前修改。
- 取消后零变更；确认后 Route 与全部 Channels 原子落库。
- API Key 不以独立明文参数出现；应用日志、通知和 AX 树中不出现完整 API Key。
- 应用日志不记录原始 Deep Link 或 Base64URL payload。
- 同来源重复导入不会静默覆盖。
- 恶意超大载荷、未知 dialect、重复 dialect、空 endpoint 均被拒绝。
- 导入完成后不改写 Claude、Codex 等工具配置。
- `ochcli deeplink parse` 输出默认脱敏；`--dry-run` 不写入凭据。
- GPUI 改动后用 `just qa-app` 生成并覆盖 `/tmp/OCHUB-QA.app`，通过 AX 树与截图验收，最后退出应用并保留 QA 包。

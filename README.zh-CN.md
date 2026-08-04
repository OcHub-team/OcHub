<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/ochub-wordmark-light.png">
    <img src="docs/assets/ochub-wordmark-dark.png" alt="OcHub" width="620">
  </picture>
</p>

<p align="center">
  <a href="README.md">English</a> · 简体中文
</p>

<p align="center">
  <strong>一个原生控制中心，统一管理你的 AI 编程工具。</strong>
</p>

<p align="center">
  连接模型供应商、切换模型、共享 MCP 服务器与 Skills，<br>
  并通过一个本地网关路由所有受支持的客户端。
</p>

<p align="center">
  <a href="https://github.com/OcHub-team/OcHub/releases/latest"><strong>下载 OcHub</strong></a>
  ·
  <a href="#为什么选择-ochub">为什么选择 OcHub</a>
  ·
  <a href="#安装">安装</a>
  ·
  <a href="#从源码构建">从源码构建</a>
</p>

<p align="center">
  <a href="https://github.com/OcHub-team/OcHub/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/OcHub-team/OcHub/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/OcHub-team/OcHub/releases/latest"><img alt="最新版本" src="https://img.shields.io/github/v/release/OcHub-team/OcHub?display_name=tag&sort=semver"></a>
  <a href="LICENSE"><img alt="GPL-3.0-or-later" src="https://img.shields.io/github/license/OcHub-team/OcHub"></a>
  <img alt="macOS、Windows、Linux" src="https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-31b8b5">
</p>

---

## 为什么选择 OcHub

AI 编程工具功能强大，但各自的连接、MCP 服务器、Skills、会话和用量数据
分散在不同位置。修改 API 端点，或在多个客户端之间共享同一套配置，
很快就会变成反复编辑配置文件。

OcHub 将这些工作集中到一个快速的原生桌面应用中。你可以将它作为简单的
连接切换器，也可以把本地网关放在工具与上游 API 之间，集中完成路由、
兼容转换、故障转移和用量统计。

OcHub 目前支持管理 **Claude Code、Claude Desktop、Codex、Cherry Studio、
Grok Build、Kimi Code、OpenCode、OpenClaw 和 Hermes**。

## 所有能力，一处管理

| | 能力 | 你能获得什么 |
| --- | --- | --- |
| 🔌 | **连接管理** | 发现、导入、编辑、测试和切换直连 API，无需手动修改客户端配置 |
| ↗️ | **模型供应商** | 为多个客户端提供统一的本地端点，集中控制上游与路由 |
| 🔀 | **智能路由** | 映射模型名称和推理等级，重试兼容接口，切换备用供应商并监控健康状态 |
| 🧩 | **MCP 与 Skills** | 集中管理可复用的 MCP 服务器和 Skills，再分发给需要的工具 |
| 📊 | **会话与用量** | 浏览本地 CLI 会话，查看 Token、缓存、延迟、请求量和预估成本 |
| 🔄 | **同步与备份** | 通过快照保护配置，并使用受支持的远程存储同步 OcHub 数据 |
| 🖥️ | **远程节点** | 通过 SSH 安全切换 WSL、开发机和无桌面服务器上已有的 Provider |
| ⚡ | **Codex 原生模式** | 在 macOS 上从 OcHub 启动 Codex，通过原生模型选择器选择兼容模型和上游所支持的 Fast 或 Ultra |
| 🌙 | **Kimi Code 配置档** | 管理 `~/.kimi-code/config.toml` 中的原生供应商、模型别名、限制、能力、请求头与凭据映射 |
| 🍒 | **Cherry Studio 导入** | 在 OcHub 保存可复用连接，再打开 Cherry Studio 的公开 Deep Link 并在应用内确认导入 |

## 两种连接方式

### 直接连接

选择一个工具，添加并测试 API 连接，然后完成切换。OcHub 会写入客户端的
原生配置，因此即使关闭 OcHub，工具也能继续正常工作。

**适合：**简单修改端点、使用工具的官方登录，或为不同工具独立配置连接。

### 模型供应商模式

将受支持的客户端指向 OcHub 的本机回环网关。请求到达上游之前，本地网关可以在
受支持的 API 协议之间转换，并应用模型别名、推理等级映射、接口重试、
健康检查和故障转移。

**适合：**在多个工具间共享上游、统一模型名称、转换 API 格式、集中统计用量，
或搭建更可靠的多供应商方案。

### Codex 原生 Fast 与 Ultra 启动器

在 macOS 的 Codex 应用页面点击**启动 Codex**，OcHub 会启动一个独立的 Codex 桌面实例，
并为兼容的 GPT-5.6 模型解锁应用原生模型选择器中的 Fast 与 Ultra。OcHub 只通过绑定到
本机回环地址的调试连接在内存中拦截 renderer 脚本，不修改或重新签名已安装的 Codex App。

这项功能让原生控件可选，并确保 Codex 请求保留所选的 Fast service tier；模型供应商仍须
真正支持对应的服务档位与推理强度。每次需要解锁时都应从 OcHub 启动 Codex。支持的模型、
安全边界与排障方法见
[Codex 进阶指南](https://docs.ochub.org/zh/codex/advanced#使用原生-fast-与-ultra-启动-codex)。

## 为桌面而生

- **原生界面**由 GPUI 驱动，不使用浏览器外壳或 WebView
- **本地优先存储**，数据位于 `~/.ochub/`
- **跨平台发布**，支持 macOS、Windows 和 Linux
- **应用内更新**，支持 DMG、Windows 安装包和 AppImage
- **开源软件**，采用 GPL-3.0-or-later 许可证

> [!IMPORTANT]
> OcHub 仍处于积极的预发布开发阶段。尝试早期版本前，请备份重要的工具配置。
> OcHub 只会修改你明确选择管理的应用程序实时配置。

## 安装

### Homebrew

Homebrew 会自动选择正确的 Apple Silicon 或 Intel 版本：

```sh
brew install --cask ochub-team/tap/ochub
```

OcHub 使用 Apple Developer ID 签名，但尚未启用公证。如果 macOS 阻止首次启动，
请打开**系统设置 → 隐私与安全性**，选择**仍要打开**。

### 直接下载

前往 **[GitHub Releases 页面](https://github.com/OcHub-team/OcHub/releases/latest)**
下载适用于你所在平台的最新版本。

| 平台 | 可用安装包 |
| --- | --- |
| macOS | Apple Silicon 和 Intel `.dmg`；无桌面 CLI `.tar.gz` |
| Windows 10/11 x64 | NSIS 安装程序、便携 GUI `.zip` 和无桌面 CLI `.zip` |
| Linux x64 | AppImage、Debian `.deb` 和无桌面 CLI `.tar.gz` |

发布内容包含 `SHA256SUMS` 和 GitHub 构件证明。打包、签名验证和发布详情请参阅
[发布指南](packaging/README.md)。

无桌面压缩包内只包含一个可自管理的 `ochcli`。如需从 OcHub Desktop 控制 WSL 或开发机，
请阅读[远程节点使用指南](https://docs.ochub.org/zh/guides/remote-nodes)。

## 从源码构建

OcHub 是一个使用 GPUI 和 axum 构建的 Rust 工作区。仓库已锁定 Rust 工具链，
rustup 会自动选择正确的版本。

```sh
git clone https://github.com/OcHub-team/OcHub.git
cd OcHub
cargo run -p ochub-app
```

平台要求：

- **macOS：**Xcode 或 Xcode Command Line Tools
- **Windows：**Visual Studio 2022 Build Tools 和 Windows SDK
- **Debian/Ubuntu：**`./scripts/ci/install-linux-deps.sh`

常用开发命令：

```sh
just check
just test
just ci
just qa-app       # macOS：构建用于验收测试的 /tmp/OCHUB-QA.app
```

工作区主要由以下组件组成：

| Crate | 职责 |
| --- | --- |
| `ochub-core` | 领域模型、SQLite 存储、客户端配置、同步、MCP、Skills、会话、用量和认证 |
| `ochub-convert` | 在受支持的 API 协议之间转换请求与响应 |
| `ochub-app` | 原生 GPUI 桌面应用 |
| `ochcli` | 无界面的命令行工具 |

## 许可证

OcHub 采用 [GNU 通用公共许可证 v3.0 或更高版本](LICENSE)。

OcHub 的灵感来源包括
[`cc-switch`](https://github.com/farion1231/cc-switch) 和
[`new-api`](https://github.com/QuantumNous/new-api)。

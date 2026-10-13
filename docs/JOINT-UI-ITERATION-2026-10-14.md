# OcHub / OcHerdr / ochub-ui 联合迭代

这轮把重复的输入行为和快捷键基础设施收回共享库，并让 OcHub 总览解释模型路由的配置过程。应用继续持有业务命令、配置草稿、终端会话和网络任务，共享库负责控件行为。

## 共同依赖

| 项目 | 共享 UI | GPUI / Zed | Rust |
|---|---|---|---|
| OcHub | ochub-ui 0.8.0 | OcHub-team/zed `66519810e58c849d2acf93e1930086390a9bcb22` | 1.98.1 |
| OcHerdr | ochub-ui 0.8.0 | 同上 | 1.98.1 |
| ochub-ui | — | 同上 | 1.98.1 |

应用锁定同一份 UI Git 提交 `04567c1bb54a65257577a8f5c21875144914b137`。共同 GPUI 提交在独立的 `ochub/shared-gpui` 分支上合并了已有 Display P3 支持和 macOS 26+ 模糊材质修复，没有删除任一应用依赖的渲染接口。未发布的本地提交通过 `just use-local-ui ../ochub-ui` 转为带校验和的精确源码快照，Cargo.lock 仍保留真实的 Git 来源，不读取任意未提交的共享库工作区。

## 路由的含义

总览中的路由是当前配置关系；近期请求是另一份已记录的数据。两者不能通过供应商显示名称拼成历史轨迹。

```mermaid
flowchart LR
    Client[客户端模型与请求协议] --> Policy[调用方专属策略或路由默认策略]
    Policy --> Match[首条匹配规则或未命中默认模型]
    Match --> Filter[路由允许列表、规则绑定、接口模型范围]
    Filter --> Candidates[候选上游与最终发送模型]
    Candidates --> Runtime[运行时健康状态、权重抽选、配额与重试]
```

前四步可以根据快照解释。最后一步依赖运行时状态，总览不发送请求，也不伪造实际选择结果。

- 调用方没有专属策略时沿用路由规则；明确配置了空策略时全部透传，不继承路由默认模型。
- 第一条匹配规则生效。命中的透传规则会抑制“未匹配时”的默认模型。
- 先用映射后的模型筛选接口；发送时的模型按路由映射、接口覆盖、客户端原值依次确定。
- 候选按优先级和协议偏好分组，组内由权重抽选。界面说明分组次序，不声称展示某次随机结果。
- 停用、允许列表、模型范围、绑定接口和绑定协议都有明确的排除原因。
- API 凭据不进入总览投影。读取调用方策略的 SQL 不选择密钥列。
- 旧节点缺少策略详情时不进行推演，保留原有只读总览并提示升级。

总览的路由 UI 和核心投影分别拆入 `overview_view/routing.rs`、`application/overview/routing.rs`。转发器与推演器共享 `gateway/routing_policy.rs` 中的纯规则，避免两套默认模型语义。

## 共享输入与快捷键

OcHerdr 不再维护整份 TextInput 副本。共享输入框负责 Unicode 选区、撤销、查找、输入法组合态、macOS 转置及纯文本粘贴。绑定动作和终端宿主的原始按键分发都遵守输入法保护。

`Shortcut` 让命令绑定、匹配和显示标签使用同一份平台定义。`KeyReleaseTracker` 按实际按键配对释放事件，避免快捷键误吞另一按键的松开事件。`ShortcutReveal` 统一按住修饰键时的提示动画，并尊重减少动态效果设置。

OcHerdr 的快捷键参考可搜索，使用共享对话框的焦点管理。文本框获取焦点后保留系统编辑语义；终端获取焦点后保留 Ctrl+C、Ctrl+W、Ctrl+L 等终端控制序列。

## 验证记录

| 范围 | 结果 |
|---|---|
| OcHub 工作区 | 1,233 通过，0 失败，2 忽略；其中包含 11 项总览核心测试 |
| 共同 GPUI 版本上的 OcHub 应用 | 136 通过；全工作区 Clippy 通过，仅保留已有 `gateway/server.rs` 的 `result_large_err` 提示 |
| ochub-ui 0.8.0 | 116 通过；Clippy `-D warnings`、Rustdoc `-D warnings`、WASM gallery 构建通过 |
| OcHerdr 工作区 | 512 通过，0 失败，3 忽略；严格 Clippy 通过 |
| 用户文档 | 三语 42 组页面、117 文件类型检查、127 文件格式检查通过；构建 130 页、索引 126 页，校验 324 条 WASM 路由 |

主仓库使用 `just qa-app` 构建固定 `/tmp/OCHUB-QA.app`，Bundle ID 保持 `io.ochub.debug.qa`。OcHerdr 使用自身 `just qa-app` 构建 `target/qa/OcHerdr.app`，并通过签名验证。OcHerdr 的终端依赖升级由并行工作提供，本轮验收使用与其锁定值及 SHA-256 一致的 GhosttyKit 产物。

原生验收使用独立 QA 数据和 cua-driver 的 AX → 操作 → 刷新 → 截图流程。没有向配置的示例上游发送请求，也没有操作正式 OcHub / OcHerdr 进程。验收结束后两款 QA 应用均已退出，验收包恢复为源码中的默认启动设置并保留；临时启动别名已移除。

- OcHub：960×640、1280×848 下检查输入示例、调用方策略切换、候选与排除原因、滚动和布局；再次构建后确认三阶段模型、候选优先级和排除原因出现在原生 AX 树中。
- OcHub 原有交互：通知中心开关、环境下拉重复开关、代理草稿离开检查与放弃后导航。
- OcHerdr：离线状态打开快捷键参考、中文过滤、全选替换、按词删除、撤销、Escape 关闭；通知详情的点击隔离和键盘隔离由实际 GPUI 元素树回归测试覆盖。
- 新增通知回归最初失败于 Ctrl+B 穿透，修复后通过；关闭通知的 Escape 松开事件也被保留在通知层。
- 快捷键筛选前后的输入框边界完全一致，并有 GPUI 布局回归保护。无会话和无标签状态保留终端焦点，使应用快捷键仍可使用。

原生证据：

- [960 宽路由预览](assets/joint-ui-2026-10-14/ochub-route-960.png)
- [调用方策略与候选](assets/joint-ui-2026-10-14/ochub-caller-policy-1280.png)
- [上游排除原因](assets/joint-ui-2026-10-14/ochub-route-exclusions-1280.png)
- [路由 AX 文本](assets/joint-ui-2026-10-14/ochub-routing-ax.txt)
- [OcHerdr 快捷键参考](assets/joint-ui-2026-10-14/ocherdr-shortcuts-960.png)、[中文筛选](assets/joint-ui-2026-10-14/ocherdr-shortcut-search-960.png)、[快捷键 AX 文本](assets/joint-ui-2026-10-14/ocherdr-shortcuts-ax.txt)
- [OcHerdr 无会话时按快捷键打开帮助](assets/joint-ui-2026-10-14/ocherdr-empty-state-shortcut.png)
- [通知中心](assets/joint-ui-2026-10-14/ochub-notifications-960.png)、[草稿离开提示](assets/joint-ui-2026-10-14/ochub-dirty-navigation-960.png)

Windows / Linux 原生渲染与 CI 尚未在本机执行。共享库保留了对应平台的 CI 配置。前一轮交互修复的分析见 [交互审查](INTERACTION-REVIEW-2026-10-14.md)。用户操作文档维护在相邻 `ochub-docs` 仓库。

## 本地提交与复现

- 共享库：`04567c1bb54a65257577a8f5c21875144914b137`，包含 `b32d5a9` 的输入与快捷键提取及 `6684141` 的密码编辑修复。
- 共同 GPUI：`66519810e58c849d2acf93e1930086390a9bcb22`，位于相邻 Zed 仓库的 `ochub/shared-gpui` 分支。
- OcHerdr 验收快照：`ff168691335e2cc6eb60da6aa1223138e6c0351c`，位于 `joint/ui-iteration-20261014`。该快照包含当前工作树原有的协议/终端升级及本轮 UI 联调；当前分支、索引和工作树均保留，未替换并行工作。
- 文档：`dcd5391`，已更新实际 `ochub-docs` 仓库的三语总览、工具链要求，并调整检查顺序以先生成当前路由清单。
- 新增本地提交未签名：现有 1Password Git 签名器调用失败，本轮仅对单次提交关闭签名，没有改全局 Git 配置。先前签名提交仍保留。
- 按要求不推送、不部署。对外发布前应先推送 GPUI，再推送 UI，最后推送应用和文档提交。

本地复现命令及可选 GPUI 本地缓存入口见 [LOCAL-UI-DEVELOPMENT.md](LOCAL-UI-DEVELOPMENT.md)。

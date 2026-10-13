# Console implementation · 2026-10-13

2026-10-14 更新：总览、导航与通知中心已经合入主工作树，共享库和三语文档已同步到实际相邻仓库。解除沙箱限制后的主仓库完整测试为 1,233 通过、0 失败、2 忽略；固定验收包构建通过。原生验收已核对通知中心、环境下拉和代理表单的离开保护。

后续的模型路由预览、OcHerdr 联调、共同依赖及最终验证见 [联合迭代记录](JOINT-UI-ITERATION-2026-10-14.md)。以下保留早期构建诊断与补丁来源，历史沙箱失败不代表当前状态。

## 主仓库构建修复 · 2026-10-14

直接在主仓库运行 `just qa-app` 复现了 9 个错误：`ochub_ui::navigation` 不存在，以及 `NotificationHost` 的通知中心方法不存在。原因是应用代码已经调用新版 API，依赖仍固定在旧 commit。先前临时工作区的编译结果不能代替主仓库验收。

现在 `Cargo.toml` 与 `Cargo.lock` 已同步固定到上述 UI 签名提交。新增 `just use-local-ui <本地仓库>`，通过 Git archive 读取固定 commit，建立带校验文件的 Cargo 来源快照，不读取未提交代码，也不改变锁文件中的 Git 身份。当前配置来自 `/tmp/ochub-console-ui-commit`，之后主仓库直接执行普通 Cargo / just 命令即可。

`cargo metadata --locked --offline` 已确认实际源码和锁文件都属于 `d04d3845d8fbd491592a3b149b7db289bc5b051a`。本地配置保留、重复设置稳定性、缺失 commit 时不修改现有配置三项检查通过。复现方式见 [本地 UI 联调](LOCAL-UI-DEVELOPMENT.md)。后文 2026-10-13 的临时工作区记录保留为历史验证。

主仓库直接验证结果：

| 检查 | 2026-10-14 结果 |
| --- | --- |
| `just qa-app` | 退出码 0；固定包 `/tmp/OCHUB-QA.app` 已更新 |
| 包内二进制 | 与主仓库 `target/qa/ochub` 逐字节一致 |
| 包内 assets | 与 `crates/app/assets` 全部一致 |
| Bundle ID | 仍为 `io.ochub.debug.qa` |
| `cargo check --workspace --all-targets --locked --offline` | 通过，无编译错误 |
| `cargo fmt --all -- --check`、`git diff --check` | 通过 |
| 原生 AX / 截图验收 | 已使用 cua-driver 核对通知、环境下拉与代理修改离开保护；后续路由验收见联合迭代记录 |

首次直接构建修复了全部 Rust 错误，但重复编译未变化图标时 `actool` 因系统服务不可达退出。QA 打包脚本现会在图标源文件一致时复用固定包中已经编译的图标，同时核对图标构建脚本；新生成的包记录该脚本用于后续失效检查。源码或脚本变化时重新编译，也可设置 `OCHUB_QA_REBUILD_ICON=1` 强制编译。正式发布流程不使用此缓存。随后在主仓库重跑 `just qa-app` 成功，日志为 `/tmp/ochub-main-qa-final.log`；全 workspace 检查日志为 `/tmp/ochub-main-check-final.log`。

## 已实现

- `crates/core/src/application/overview.rs` 提供 `Application::overview_snapshot`。网关、配置路由、应用连接、用量、趋势、供应商分组和近期记录分别返回数据或错误；读取不探测接口、不扫描客户端、不修复选择、不补写费用。
- `ochcli overview --from <UTC秒> --to <UTC秒>` 和 `overview.read` 共用该投影。远端需要 `overview-v1`，以及 ProviderRead、GatewayRead、UsageRead；不支持或离线时不回退到本机。
- `crates/app/src/overview_view.rs` 提供 24 小时 / 7 天 / 30 天、路由与上游搜索、配置路径检查、应用和草稿概况、趋势及数值表、近期记录与供应商概况。只展示已存储事实；配置关联不等同于历史请求轨迹，网关监听不等同于模型可用，导入会话不显示伪造的请求耗时。
- 默认入口改为总览。220px 侧栏使用共享导航，保留环境切换、应用直达和可折叠列表；通知、主题、设置和关于在底部。环境切换保留总览，旧回包不能覆盖新的环境或时间范围。
- 共享 `ochub-ui` 增加排版与尺寸 tokens、导航和表格组件，统一控件密度。通知采用短预览与本次启动的通知中心，提供完整详情、复制、未读、合并、关闭和键盘操作；悬停或聚焦暂停倒计时。
- 评审修复 toast 点击穿透和浅色辅助文字对比度；总览选中行的辅助文字同样使用 `subtext`。
- 三语用户文档在独立文档仓库的临时副本完成，包含新总览指南、CLI、远端要求和通知行为。主仓库这里只保存开发记录及补丁，没有重建文档站。

设计稿：[交互原型](design/ochub-console-2026-10-13/index.html)、[设计与数据契约](design/ochub-console-2026-10-13/DESIGN.md)。原型中的六页、全局搜索、密度切换和请求详情用于设计比较，不能全部视作已上线功能。生产首期实现上述总览、共享控件和导航；已有页面继续使用各自的完整业务界面。

## 历史验证结果（受限环境）

联合工作区位于 `/tmp/ochub-console-validation`，共享库副本位于 `/tmp/ochub-console-ui`。仅临时工作区增加指向该副本的 Cargo patch；主仓库没有引入 `/tmp` 或本机绝对路径依赖。共享库测试副本使用主仓库的固定 GPUI fork；这份测试配置不在交付补丁内。

| 检查 | 结果 |
| --- | --- |
| 联合工作区 `cargo check --workspace --all-targets --offline` | 通过 |
| 联合工作区 `cargo clippy --workspace --all-targets --offline` | 通过；剩余原有 `gateway/server.rs:62` 的 `result_large_err` 警告 |
| 主仓库 `cargo fmt --all -- --check`、`git diff --check` | 通过 |
| 共享库 `cargo test --lib --offline` | 104 通过，0 失败；含 toast 点击穿透、悬停、焦点、长详情及尺寸测试 |
| 联合工作区完整测试 | 1196 通过，29 失败，2 忽略；失败涉及当前沙箱禁止的 TCP / Unix socket 监听，不记作通过 |
| 新增总览测试 | core 6 项、app 4 项、remote stdio 1 项均通过，已包含在上行计数内 |
| 文档 | lint 127 文件、三语 42 组页面、Astro 117 文件类型检查、130 页构建、126 页搜索索引、324 条 WASM 路由均通过 |
| 补丁应用检查 | 完整 UI 补丁对干净基线、两份评审补丁对现有暂存改动、文档补丁对文档基线均通过 |
| `just qa-app` | Rust `qa` profile 编译成功；Xcode `actool` 无法连接所需系统服务并发生异常，命令失败，未覆盖现有 QA 包 |
| 本轮原生 GUI 验收 | 未执行；当前会话没有所要求的 computer-use `node_repl` / `sky` 工具 |

完整测试分布：CLI 单元 17 通过；CLI smoke 11 通过 / 1 失败；远端 stdio 18 通过 / 2 失败；app 131 通过 / 2 失败；convert 45 通过；core 969 通过 / 24 失败 / 2 忽略；protocol 5 通过。直接失败日志报告 `PermissionDenied / Operation not permitted`，另有网关自动启动未成功和持久 owner 提前关闭的派生失败。必须在允许本地监听的环境重跑，不能用筛掉失败后的结果代替完整验收。

日志保留在 `/tmp/ochub-console-{tests,unified-check,clippy-final,ui-reviewed-tests,qa-build,fmt}.log`。文档日志和详细范围见 [DOCS-VALIDATION.md](design/ochub-console-2026-10-13/patches/DOCS-VALIDATION.md)。先前配置工作台的原生截图不作为本次控制台改版的验收证据。

## 临时提交与网络验证

按要求实际执行了 `gh auth switch --hostname github.com --user caicaijiji`。命令失败，返回 `currently active token for github.com is from default`，没有完成账号切换。后续 `gh api` 连接失败；从临时克隆执行普通 Git push 得到 `Could not resolve host: github.com`。不能仅凭本次 `gh auth status` 的 invalid 提示断定凭据已过期，因为 GitHub 连接本身未建立。

为推进提交工作，在允许写入的 `/tmp` 中克隆了两个实际仓库，保留原始父提交，并应用已经验证的完整补丁。作者身份沿用各源仓库的本地 Git 配置；两份提交均使用现有 SSH 签名配置，并对现有签名公钥完成验证。没有改动全局签名或信任配置。

| 仓库 | 已准备提交 | 临时克隆 / 分支 |
| --- | --- | --- |
| ochub-ui | `d04d3845d8fbd491592a3b149b7db289bc5b051a` | `/tmp/ochub-console-ui-commit` · `console/overview-20261013` |
| ochub-docs | `d421498db16a9fc23ab15e8b13cb0467eec4fa1c` | `/tmp/ochub-console-docs-commit` · `docs/overview-20261013` |

两份提交中的 12 / 18 个变更文件逐字节匹配已验证快照，临时克隆工作树干净。提交另保存在 `/tmp/ochub-console-ui.bundle` 与 `/tmp/ochub-console-docs.bundle`；`git bundle verify` 均通过。它们分别要求下文列出的 UI / docs 基线提交，不能当作不含前置历史的完整仓库使用。

[dependency-pin.patch](design/ochub-console-2026-10-13/patches/dependency-pin.patch) 记录主仓库 `Cargo.toml` 与 `Cargo.lock` 的精确更新，已于 2026-10-14 应用。UI commit 尚未推送，本地通过上文的精确源码快照构建。

## 历史集成清单

以下是解除限制前的清单。相邻仓库同步、主仓库测试与固定包构建现已完成；原生验收进展见联合迭代记录。推送仍按用户要求留待以后。

1. 同步回 `../ochub-ui` 时先核对已有 12 文件暂存改动，避免覆盖；两份评审修复对应 [01-toast-hit-testing.patch](design/ochub-console-2026-10-13/patches/01-toast-hit-testing.patch) 和 [02-console-secondary-text-contrast.patch](design/ochub-console-2026-10-13/patches/02-console-secondary-text-contrast.patch)。已准备签名提交可通过临时克隆或 bundle 导入，无需重新创建。若从干净基线重建，改用完整补丁，不要重复应用。
2. 主仓库依赖版本已更新，本地来源已配置。直接在主仓库执行编译、测试与固定 QA 命令；不再用临时联合工作区替代。
3. 将已准备的 docs 签名提交同步到 `../ochub-docs`。源工作树当前仍干净，HEAD 仍为基线；没有部署。若无法保留临时克隆，可由 bundle 导入，或使用 [docs.patch](design/ochub-console-2026-10-13/patches/docs.patch) 重建。两仓库的推送均按用户要求留待以后。
4. 固定 QA 包已于 2026-10-14 在主仓库重新构建。后续继续使用 `/tmp/OCHUB-QA.app` 和 Bundle ID `io.ochub.debug.qa`。不创建随机包、不改 Bundle ID；验证结束退出应用并保留包壳。
5. 使用 computer-use 打开固定包，读取 AX 树、按索引操作、刷新后截图：960 / 1280、浅色 / 深色；总览搜索与选中一致、时间切换、离线和旧节点、长名称、通知打开关闭与焦点恢复、悬停倒计时、长错误复制、点击不穿透。再验证编辑器脏状态离开保护和固定保存栏不被通知遮盖。Windows / Linux 的实际渲染仍需各自平台验收。

## 补丁基线

| 补丁 | 基线与用途 |
| --- | --- |
| [ochub-ui-complete.patch](design/ochub-console-2026-10-13/patches/ochub-ui-complete.patch) | 干净 UI commit `4c1ae8176bde4288dfc1aa5c175e4ffcd81a5c95`；包含 12 文件及两份评审修复 |
| `01` / `02` 评审补丁 | 当前 `../ochub-ui` 的 12 文件暂存状态；不包含此前已暂存的基础改版 |
| [docs.patch](design/ochub-console-2026-10-13/patches/docs.patch) | 干净 docs commit `e791d241d7a7d7a8566be7cd6efab659deef8038`；18 个 MDX 文件 |
| [dependency-pin.patch](design/ochub-console-2026-10-13/patches/dependency-pin.patch) | 旧 UI rev → `d04d3845d8fbd491592a3b149b7db289bc5b051a`；已在主仓库应用，当前从精确本地快照读取 |

完整 UI 补丁 SHA-256：`574d62246ad244f8926dba387f7231fa0ab136139ca8a909af4e5367731841be`。文档补丁 SHA-256：`b28baa3cc80507e36dfaab30ade9be9ed6913554ff323e3648046ec00b5cf43b`。

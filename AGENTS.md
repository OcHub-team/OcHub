# 调试约定

用户文档站在独立仓库 https://github.com/OcHub-team/ochub-docs 中维护（本机相邻目录 `../ochub-docs`）。涉及用户操作或功能变化时，在该仓库更新文档并验证、提交；不要在主仓库重建文档站副本。主仓库 `docs/` 仅保留开发设计文档与 README 资源。

GPUI 改动后使用 `just qa-app` 构建固定验收包 `/tmp/OCHUB-QA.app`。该命令始终复用 Bundle ID `io.ochub.debug.qa`，并把 `crates/app/assets` 同步到 `Contents/Resources/assets`；禁止为每次验收生成随机路径或新的 Bundle ID。

用 computer-use 的 node_repl/sky 按固定路径打开应用，读取 AX 树，按索引操作并截图；操作后刷新状态，核对布局与文案。验收后退出应用，但保留 `/tmp/OCHUB-QA.app` 供下次覆盖复用，不要清理包壳。

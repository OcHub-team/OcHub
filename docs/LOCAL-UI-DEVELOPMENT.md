# 本地 UI 联调

`Cargo.toml` 和 `Cargo.lock` 将 `ochub-ui` 固定到同一个 Git commit。应用使用新增 UI API 时，必须同时更新这个版本；只改应用代码而保留旧版本会出现 `navigation`、通知中心方法不存在等编译错误。

当 UI commit 已在本地创建、尚未推送时，可以从包含该 commit 的本地仓库建立精确源码快照：

```sh
just use-local-ui ../ochub-ui
just qa-app
```

第一个命令要求 Python 3.12 或更新版本，也可传入另一个本地 UI 克隆路径。它读取的是 `Cargo.toml` 固定的 commit，不使用该仓库的未提交改动或当前分支内容。目标仓库缺少该 commit 时会停止，保留现有配置。

快照和 Cargo 来源配置分别保存在被 Git 忽略的 `.cargo/local-sources/` 与 `.cargo/config.toml`。快照包含 Cargo 校验文件；锁文件继续记录原来的 Git URL 和完整 commit ID，不改成某台机器的路径。已有 Cargo 配置会保留，脚本只管理自己标记的区块。

设置一次之后，`just qa-app`、`just check`、`just test` 等命令照常使用。其他依赖已缓存时，也可运行 `cargo check --workspace --all-targets --locked --offline`。`cargo clean` 不会删除这份源码快照。

更新 UI 固定版本后需重新运行 `just use-local-ui`。恢复直接从上游读取时，只移除 `.cargo/config.toml` 中 `BEGIN ochub local UI source` 与 `END ochub local UI source` 标记之间的区块，保留其它配置。

这里使用的是 Cargo 的 [source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html)：本地快照必须与所替换 commit 的源码一致。

`just qa-app` 固定覆盖 `/tmp/OCHUB-QA.app`。当图标源码和图标构建脚本未变化时，它复用该包已有的 `Assets.car`，但仍更新应用二进制和全部 assets。需要检查新版 Xcode 对相同图标的编译结果时，可使用 `OCHUB_QA_REBUILD_ICON=1 just qa-app` 强制重新编译图标。正式发布的图标构建流程不使用这个 QA 缓存。

如果共同 GPUI 提交也尚未推送，可在同一步指定包含该提交的 Zed 检出目录：

```sh
just use-local-ui ../ochub-ui
python3 scripts/use-local-ui.py ../ochub-ui --gpui-repository ../zed
```

可选参数只为此次 Cargo fetch 设置 Git URL 映射，将固定提交读入本地缓存；不修改全局 Git 配置，不改变 Cargo.lock 的公开来源。两个本地仓库都必须包含清单中的完整提交。对外发布前仍需先推送 GPUI 和 UI 依赖提交。

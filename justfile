# OcHub task runner — `just` 或 `just --list` 查看所有命令

# GPUI/Metal 构建需要完整 Xcode；已设置 DEVELOPER_DIR 时沿用现有值
export DEVELOPER_DIR := env_var_or_default("DEVELOPER_DIR", "/Applications/Xcode.app/Contents/Developer")

# 默认：启动桌面应用
default: run

# 启动日常开发应用：保留符号与增量构建，但使用 opt-level=2。
# 未优化的 GPUI/布局/文本栈会显著放大拖拽和滚动耗时，不适合作为流畅度基准。
run:
    cargo run --profile qa -p ochub-app

# 仅在需要完整未优化调试语义时使用。
run-debug:
    cargo run -p ochub-app

# 启动 GPUI 桌面应用（release，构建慢但流畅）
run-release:
    cargo run --release -p ochub-app

# 构建固定的 computer-use 验收包（复用路径、Bundle ID 与包内 assets）
qa-app:
    ./scripts/package-qa-app.sh

# 启动 headless 控制服务器（不需要 Xcode/Metal）
server:
    cargo run -p ochub-server

# 快速类型检查（全 workspace）
check:
    cargo check --workspace

# 只检查核心库（最快，不碰 GPUI）
check-core:
    cargo check -p ochub-core

# 跑全部测试
test:
    cargo test --workspace

# 只跑核心库测试
test-core:
    cargo test -p ochub-core

# 构建全部（debug）
build:
    cargo build --workspace

# 构建 release 版应用
build-release:
    cargo build --release -p ochub-app

# 格式化 + clippy
lint:
    cargo fmt --all
    cargo clippy --workspace --all-targets

# 与 GitHub Actions 一致的只读质量门禁
ci:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked --no-fail-fast

# 在当前 Mac 上生成 DMG（需先安装 cargo-packager）
package-macos:
    ./scripts/release/package-macos.sh

# 在 Debian/Ubuntu 上安装构建和 AppImage 依赖
setup-linux:
    ./scripts/ci/install-linux-deps.sh

# 在当前 Linux 上生成 AppImage + deb（需先安装 cargo-packager）
package-linux:
    ./scripts/release/package-linux.sh

# 一次性环境准备：下载 Metal 工具链组件（需要 Xcode 26+）
setup-metal:
    xcodebuild -downloadComponent MetalToolchain

# 清理构建产物
clean:
    cargo clean

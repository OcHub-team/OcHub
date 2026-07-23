# OCHub task runner — `just` 或 `just --list` 查看所有命令

# GPUI/Metal 构建需要完整 Xcode；已设置 DEVELOPER_DIR 时沿用现有值
export DEVELOPER_DIR := env_var_or_default("DEVELOPER_DIR", "/Applications/Xcode.app/Contents/Developer")

# 默认：启动桌面应用
default: run

# 启动 GPUI 桌面应用（debug）
run:
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

# 一次性环境准备：下载 Metal 工具链组件（需要 Xcode 26+）
setup-metal:
    xcodebuild -downloadComponent MetalToolchain

# 清理构建产物
clean:
    cargo clean

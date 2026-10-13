# 连接加载与会话读取性能改进

2026-10-13，macOS 27 / Apple Silicon，Rust 1.98.1。基于任务开始时的当前工作树测量和修改，保留原有未提交工作。

本轮改善四条有实测证据的读取路径：连接端点的 N+1 查询、当前连接存在性检查、本地连接页重复读取详情，以及单应用会话查询扫描其他应用历史的问题。没有引入缓存，因此不存在新增的缓存失效等待。

## 测量方法与结果

使用 `qa` 优化配置（`opt-level=2`），修改前后二进制采用相同配置。先保存修改前二进制，再对新旧版本交替运行 3 轮；每轮预热 3 次、采样 9 次。下表为三轮中位数的中位数，单位 ms。

| 数据加载场景 | 修改前 | 修改后 |
| --- | ---: | ---: |
| 判断当前连接是否存在：1,000 个连接 | 60.0351 | 0.0014 |
| 读取完整连接列表：1,000 个连接、3,000 个端点 | 60.2105 | 2.0916 |
| 本地连接页数据：1,000 个连接 | 125.4448 | 2.8912 |
| 本地连接页数据：250 个连接 | 10.4228 | 0.7418 |
| 查询 2 个 Claude 会话，同时存在 2,000 个无关 Codex 会话 | 34.6360 | 0.0830 |

连接页数据阶段在 1,000 个连接的合成负载下降约 **97.7%**，约为原来的 **1/43**。这不是整个应用启动时间或渲染 FPS 的测量，也不能直接套用到 SSH 网络延迟或用户的实际数据规模。

连接数据使用内存 SQLite；每个连接带 1 KiB 配置负载和 3 个端点。会话使用临时目录内的真实 JSONL 文件。夹具创建不计入耗时，文件系统与数据库处于热缓存状态。连接页数字统计 Application 的连接列表、详情、URL 和当前项读取组合，不含实时配置导入、Gateway 辅助查询、IPC 或 GPUI 排版绘制。Benchmark 以子进程注入专用 `OCHUB_TEST_HOME` 和 `XDG_DATA_HOME`，无需生产数据和真实凭据。

复测命令：

```bash
cargo run --profile qa -p ochub-core --example perf_hotpaths --locked
```

基准程序为 `crates/core/examples/perf_hotpaths.rs`。其中 `provider_page_legacy` 保留“列表 + 逐条详情”的调用方式，用于在同一新版本中对照 `provider_page_snapshot`；完整的修改前数字来自修改前保存的二进制，不能把新版本中的 legacy 调用数字误当作修改前结果。

原始记录保存在本机 `/tmp/ochub-perf-20261013/`：`before-run-1..3.json`、`after-run-1..3.json`、`comparison.json`。二进制 SHA-256：

- 修改前：`332f9fe1ee639572c029d614182b4bf239c645c2a9312d4ddd83887a5f6cf641`
- 修改后：`baec0fcb19ebe08d0bcf92403f0ffe1e0d39419483a549011f8ca18041230059`

## 修改内容

1. `crates/core/src/db/dao/providers.rs`：`get_all_providers()` 先读取应用的连接，再一次性读取其端点，以 ID 归入连接元数据。非空列表从 `1 + N` 次查询降为 2 次；空列表只查询一次。保留原有顺序、应用隔离、NULL 时间处理、旧重复端点最后值胜出的语义，以及端点表优先于旧 inline metadata 的约定。
2. `crates/core/src/settings.rs`：当前连接校验改用按 `(app_type, id)` 主键进行的 `EXISTS` 查询。原来为了回答“一个 ID 是否存在”，会读取所有连接、解析配置 JSON 并加载所有端点。缺失本地 ID 时，仍会清除它并回退到数据库 current。
3. `crates/core/src/application/providers.rs` 与 GPUI 的 `ProviderPageLoad`：增加批量 `ProviderSnapshot`，一次获得连接、Base URL 与当前项。本地界面不再拿到列表后逐条重复读取详情。快照和单条读取共用密钥脱敏逻辑；读取不会把脱敏数据写回数据库。连接排序继续与原有列表规则一致。
4. `crates/core/src/session_manager.rs` 与 `application/sessions.rs`：先按应用选择扫描器，再读取历史。空筛选仍并行扫描原有全部来源，多个应用仍并行扫描，重复应用筛选不会产生重复结果。单应用查询和详情定位不再扫描无关应用。

远程连接页保留原有 wire 请求和兼容路径，未新增 CLI 参数或协议字段。共享 core 的数据库优化也能减少服务端读取工作，但远程连接页仍存在逐条详情请求；本轮没有测量 SSH 往返，也不声称消除了这部分网络开销。GUI 会话页首次进入时仍需扫描全部来源；本轮会话性能数字对应单应用查询及其详情定位路径。

## 正确性与验收

- `cargo test --workspace --locked --no-fail-fast`：**1,164 通过、0 失败、2 ignored**。忽略项仍是需要真实凭据的 S3 测试。
- 新增 3 个回归测试，覆盖批量端点的归属、排序、重复/NULL/旧 metadata，快照的顺序/当前项/URL/脱敏与存储保留，以及会话的多应用、重复筛选、查询与详情读取。
- 新增隔离测试目录 helper，恢复环境变量和设置缓存；给此前审计发现的 home 缩写测试补上共享环境锁，避免并行测试在两次 home 读取间切换环境。
- `cargo fmt --all -- --check` 和 `git diff --check` 通过。
- `cargo clippy --workspace --all-targets --locked` 完成；只有修改前已有的 `gateway/server.rs:62` 的 `result_large_err` 警告，以及已有依赖 `block` 的未来兼容提示。没有以此声明严格 `-D warnings` 门禁通过。
- `just qa-app` 构建通过；包路径 `/tmp/OCHUB-QA.app`，Bundle ID `io.ochub.debug.qa`，二进制和 19 个 assets 文件与构建结果一致。

原生验收通过 node_repl/sky 获取 AX 树、按索引操作、截图后复核。临时数据包含 1,000 个自定义连接（另有默认种子）、3,000 个端点和 2,002 个会话：

- 大连接列表显示、切换到另一连接、当前项更新正确。
- 编辑并保存名称成功；数据库、设备设置和实际配置一致，真实测试密钥与 3 个端点保留。
- 切到 Codex 再返回，名称、排序和当前连接重新加载正确。
- 会话列表显示 2,002 项与分页；打开 Claude 会话后，详情正确显示对应的一条消息。
- 验收后退出应用，保留固定 QA 包供后续覆盖复用。

GUI 证据在同一临时记录目录的 `gui-*.txt`、`gui-*.jpeg` 中。此次页面检查证明改动后的操作结果正确，性能结论以独立基准数字为准。

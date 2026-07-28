//! CLI tool version probing and install/update lifecycle.
//!
//! Ported from cc-switch `commands/misc.rs` (`get_tool_versions` /
//! `run_tool_lifecycle_action` / `probe_tool_installations` and their probing
//! machinery). All Tauri wiring is removed; events become `log` calls.
//!
//! **macOS + Linux are fully ported.** Windows native probing/lifecycle support
//! is implemented with `where`/batch commands; the reference project's deeper
//! WSL distro probing remains a future parity item.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const VALID_TOOLS: [&str; 5] = ["claude", "codex", "opencode", "openclaw", "hermes"];

/// 单个工具的版本探测结果。
#[derive(Debug, Clone, Serialize)]
pub struct ToolVersion {
    pub name: String,
    pub version: Option<String>,
    /// 最新版本。
    pub latest_version: Option<String>,
    pub error: Option<String>,
    /// 已定位到可执行文件、但 `--version` 报错退出。
    pub installed_but_broken: bool,
    /// 工具运行环境: "windows", "wsl", "macos", "linux", "unknown"
    pub env_type: String,
    /// 当 env_type 为 "wsl" 时，返回该工具绑定的 WSL distro。
    pub wsl_distro: Option<String>,
}

/// WSL shell 偏好输入（Windows 下按 distro 探测）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WslShellPreferenceInput {
    #[serde(default)]
    pub wsl_shell: Option<String>,
    #[serde(default)]
    pub wsl_shell_flag: Option<String>,
}

/// 单处工具安装。
#[derive(Debug, Clone, Serialize)]
pub struct ToolInstallation {
    pub path: String,
    pub version: Option<String>,
    pub runnable: bool,
    pub error: Option<String>,
    pub source: String,
    pub is_path_default: bool,
    /// canonicalize 解析后的真身路径，用于锚定真身判定。不外露给前端。
    #[serde(skip)]
    pub real: std::path::PathBuf,
}

/// 工具安装分布报告。
#[derive(Debug, Clone, Serialize)]
pub struct ToolInstallationReport {
    pub tool: String,
    pub installs: Vec<ToolInstallation>,
    pub is_conflict: bool,
    pub needs_confirmation: bool,
    pub command: String,
    pub anchored: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ToolLifecycleAction {
    Install,
    Update,
}

impl FromStr for ToolLifecycleAction {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "install" => Ok(Self::Install),
            "update" => Ok(Self::Update),
            _ => Err(format!("Unsupported tool action: {value}")),
        }
    }
}

fn normalize_requested_tools(tools: &[String]) -> Vec<&'static str> {
    let set: std::collections::HashSet<&str> = tools.iter().map(|s| s.as_str()).collect();
    VALID_TOOLS
        .iter()
        .copied()
        .filter(|tool| set.contains(tool))
        .collect()
}

// ===========================================================================
// 公共入口
// ===========================================================================

/// 探测 CLI 工具版本（macOS/Linux 完整实现；Windows 暂未移植）。
pub async fn get_tool_versions(
    tools: Option<Vec<String>>,
    _wsl_shell_by_tool: Option<HashMap<String, WslShellPreferenceInput>>,
) -> Result<Vec<ToolVersion>, String> {
    let requested: Vec<&str> = if let Some(tools) = tools.as_ref() {
        let set: std::collections::HashSet<&str> = tools.iter().map(|s| s.as_str()).collect();
        VALID_TOOLS
            .iter()
            .copied()
            .filter(|t| set.contains(t))
            .collect()
    } else {
        VALID_TOOLS.to_vec()
    };

    #[cfg(target_os = "windows")]
    {
        let mut results = Vec::new();
        for tool in requested {
            results.push(get_single_tool_version_impl_windows(tool).await);
        }
        return Ok(results);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut results = Vec::new();
        for tool in requested {
            results.push(get_single_tool_version_impl(tool).await);
        }
        Ok(results)
    }
}

/// 安装/更新 CLI 工具（macOS/Linux 完整实现；Windows 暂未移植）。
pub fn run_tool_lifecycle_action(
    tools: Vec<String>,
    action: String,
    _wsl_shell_by_tool: Option<HashMap<String, WslShellPreferenceInput>>,
) -> Result<(), String> {
    let action = ToolLifecycleAction::from_str(&action)?;
    let requested = normalize_requested_tools(&tools);
    if requested.is_empty() {
        return Err("No supported tools selected".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let label = match action {
            ToolLifecycleAction::Install => "tool_install",
            ToolLifecycleAction::Update => "tool_update",
        };
        let command_line = build_tool_lifecycle_command(&requested, action)?;
        return run_tool_lifecycle_silently(&command_line, label);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let label = match action {
            ToolLifecycleAction::Install => "tool_install",
            ToolLifecycleAction::Update => "tool_update",
        };
        let command_line = build_tool_lifecycle_command(&requested, action)?;
        run_tool_lifecycle_silently(&command_line, label)
    }
}

/// 探测各工具安装分布（macOS/Linux 完整实现；Windows 暂未移植）。
pub fn probe_tool_installations(tools: Vec<String>) -> Result<Vec<ToolInstallationReport>, String> {
    let requested = normalize_requested_tools(&tools);
    if requested.is_empty() {
        return Err("No supported tools selected".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        return Ok(requested
            .into_iter()
            .map(|tool| {
                let installs = enumerate_tool_installations(tool);
                let (command, needs_confirmation, anchored) = plan_command_for(tool, &installs);
                let is_conflict = is_conflicting(&installs);
                ToolInstallationReport {
                    tool: tool.to_string(),
                    installs,
                    is_conflict,
                    needs_confirmation,
                    command,
                    anchored,
                }
            })
            .collect());
    }

    #[cfg(not(target_os = "windows"))]
    {
        Ok(requested
            .into_iter()
            .map(|tool| {
                let installs = enumerate_tool_installations(tool);
                let (command, needs_confirmation, anchored) = plan_command_for(tool, &installs);
                let is_conflict = is_conflicting(&installs);
                ToolInstallationReport {
                    tool: tool.to_string(),
                    installs,
                    is_conflict,
                    needs_confirmation,
                    command,
                    anchored,
                }
            })
            .collect())
    }
}

// ===========================================================================
// 共享辅助（平台无关）
// ===========================================================================

#[cfg(target_os = "macos")]
fn tool_env_type() -> &'static str {
    "macos"
}

#[cfg(target_os = "linux")]
fn tool_env_type() -> &'static str {
    "linux"
}

#[cfg(target_os = "windows")]
fn tool_env_type() -> &'static str {
    "windows"
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn tool_env_type() -> &'static str {
    "unknown"
}

/// 取文本末尾最多 `n` 行（npm / pip 的关键错误通常出现在输出尾部）。
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn decode_command_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// 预编译的版本号正则表达式
static VERSION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\d+\.\d+\.\d+(-[\w.]+)?").expect("Invalid version regex"));

/// 从版本输出中提取纯版本号
fn extract_version(raw: &str) -> String {
    VERSION_RE
        .find(raw)
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| raw.to_string())
}

/// 工具未安装时的统一错误文案。
const NOT_INSTALLED: &str = "not installed or not executable";

/// CLI 版本探测的三态结果。
enum ShellProbe {
    Found(String),
    FoundButFailed(String),
    NotFound(String),
}

/// Validate that the given shell name is one of the allowed shells.
fn is_valid_shell(shell: &str) -> bool {
    matches!(
        shell.rsplit('/').next().unwrap_or(shell),
        "sh" | "bash" | "zsh" | "fish" | "dash"
    )
}

/// Return the default invocation flag for the given shell.
fn default_flag_for_shell(shell: &str) -> &'static str {
    match shell.rsplit('/').next().unwrap_or(shell) {
        "dash" | "sh" => "-c",
        "fish" => "-lc",
        _ => "-lic",
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn push_unique_path(paths: &mut Vec<std::path::PathBuf>, path: std::path::PathBuf) {
    if path.as_os_str().is_empty() {
        return;
    }
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

#[cfg(not(target_os = "windows"))]
fn push_env_single_dir(paths: &mut Vec<std::path::PathBuf>, value: Option<std::ffi::OsString>) {
    if let Some(raw) = value {
        push_unique_path(paths, std::path::PathBuf::from(raw));
    }
}

#[cfg(not(target_os = "windows"))]
fn extend_from_path_list(
    paths: &mut Vec<std::path::PathBuf>,
    value: Option<std::ffi::OsString>,
    suffix: Option<&str>,
) {
    if let Some(raw) = value {
        for p in std::env::split_paths(&raw) {
            let dir = match suffix {
                Some(s) => p.join(s),
                None => p,
            };
            push_unique_path(paths, dir);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn extend_from_cli_path_env(
    paths: &mut Vec<std::path::PathBuf>,
    value: Option<std::ffi::OsString>,
) {
    if let Some(raw) = value {
        for p in std::env::split_paths(&raw) {
            push_unique_path(paths, p);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn tool_executable_candidates(tool: &str, dir: &Path) -> Vec<std::path::PathBuf> {
    vec![dir.join(tool)]
}

#[cfg(not(target_os = "windows"))]
fn extend_mise_node_search_paths(paths: &mut Vec<std::path::PathBuf>, home: &Path) {
    if home.as_os_str().is_empty() {
        return;
    }

    let mise_base = home.join(".local/share/mise");
    push_unique_path(paths, mise_base.join("shims"));

    let node_installs = mise_base.join("installs").join("node");
    if node_installs.exists()
        && let Ok(entries) = std::fs::read_dir(&node_installs)
    {
        for entry in entries.flatten() {
            let bin_path = entry.path().join("bin");
            if bin_path.exists() {
                push_unique_path(paths, bin_path);
            }
        }
    }
}

/// OpenCode install.sh 路径优先级 + Bun/Go 安装路径。
#[cfg(not(target_os = "windows"))]
fn opencode_extra_search_paths(
    home: &Path,
    opencode_install_dir: Option<std::ffi::OsString>,
    xdg_bin_dir: Option<std::ffi::OsString>,
    gopath: Option<std::ffi::OsString>,
) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();

    push_env_single_dir(&mut paths, opencode_install_dir);
    push_env_single_dir(&mut paths, xdg_bin_dir);

    if !home.as_os_str().is_empty() {
        push_unique_path(&mut paths, home.join("bin"));
        push_unique_path(&mut paths, home.join(".opencode").join("bin"));
        push_unique_path(&mut paths, home.join(".bun").join("bin"));
        push_unique_path(&mut paths, home.join("go").join("bin"));
    }

    extend_from_path_list(&mut paths, gopath, Some("bin"));

    paths
}

/// 构建某工具的候选搜索目录（原生安装优先，PATH 兜底）。
#[cfg(not(target_os = "windows"))]
fn build_tool_search_paths(tool: &str) -> Vec<std::path::PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();

    let mut search_paths: Vec<std::path::PathBuf> = Vec::new();
    if !home.as_os_str().is_empty() {
        push_unique_path(&mut search_paths, home.join(".local/bin"));
        push_unique_path(&mut search_paths, home.join(".npm-global/bin"));
        push_unique_path(&mut search_paths, home.join("n/bin"));
        push_unique_path(&mut search_paths, home.join(".volta/bin"));
        extend_mise_node_search_paths(&mut search_paths, &home);
    }

    #[cfg(target_os = "macos")]
    {
        push_unique_path(
            &mut search_paths,
            std::path::PathBuf::from("/opt/homebrew/bin"),
        );
        push_unique_path(
            &mut search_paths,
            std::path::PathBuf::from("/usr/local/bin"),
        );
        if tool == "hermes" {
            let python_base = home.join("Library").join("Python");
            if python_base.exists()
                && let Ok(entries) = std::fs::read_dir(&python_base)
            {
                for entry in entries.flatten() {
                    let bin_path = entry.path().join("bin");
                    if bin_path.exists() {
                        push_unique_path(&mut search_paths, bin_path);
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        push_unique_path(
            &mut search_paths,
            std::path::PathBuf::from("/usr/local/bin"),
        );
        push_unique_path(&mut search_paths, std::path::PathBuf::from("/usr/bin"));
    }

    let fnm_base = home.join(".local/state/fnm_multishells");
    if fnm_base.exists()
        && let Ok(entries) = std::fs::read_dir(&fnm_base)
    {
        for entry in entries.flatten() {
            let bin_path = entry.path().join("bin");
            if bin_path.exists() {
                push_unique_path(&mut search_paths, bin_path);
            }
        }
    }

    let nvm_base = home.join(".nvm/versions/node");
    if nvm_base.exists()
        && let Ok(entries) = std::fs::read_dir(&nvm_base)
    {
        for entry in entries.flatten() {
            let bin_path = entry.path().join("bin");
            if bin_path.exists() {
                push_unique_path(&mut search_paths, bin_path);
            }
        }
    }

    if tool == "opencode" {
        let extra_paths = opencode_extra_search_paths(
            &home,
            std::env::var_os("OPENCODE_INSTALL_DIR"),
            std::env::var_os("XDG_BIN_DIR"),
            std::env::var_os("GOPATH"),
        );
        for path in extra_paths {
            push_unique_path(&mut search_paths, path);
        }
    }

    let path_env = std::env::var_os("PATH");
    extend_from_cli_path_env(&mut search_paths, path_env);
    search_paths
}

// ===========================================================================
// 版本探测（macOS/Linux）
// ===========================================================================

/// 在非 Windows 平台用用户 shell 执行 `{tool} --version` 探测版本。
#[cfg(not(target_os = "windows"))]
fn try_get_version(tool: &str) -> ShellProbe {
    use std::process::Command;

    let output = {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| is_valid_shell(s))
            .unwrap_or_else(|| "sh".to_string());
        let flag = default_flag_for_shell(&shell);
        Command::new(shell)
            .arg(flag)
            .arg(format!("{tool} --version"))
            .output()
    };

    match output {
        Ok(out) => {
            let stdout = decode_command_output(&out.stdout).trim().to_string();
            let stderr = decode_command_output(&out.stderr).trim().to_string();
            if out.status.success() {
                let raw = if stdout.is_empty() { &stderr } else { &stdout };
                if raw.is_empty() {
                    ShellProbe::NotFound(NOT_INSTALLED.to_string())
                } else {
                    ShellProbe::Found(extract_version(raw))
                }
            } else {
                let err = if stderr.is_empty() { stdout } else { stderr };
                if out.status.code() == Some(127) || err.is_empty() {
                    ShellProbe::NotFound(NOT_INSTALLED.to_string())
                } else {
                    ShellProbe::FoundButFailed(last_lines(err.trim(), 4))
                }
            }
        }
        Err(_) => ShellProbe::NotFound(NOT_INSTALLED.to_string()),
    }
}

/// 扫描常见路径查找 CLI（PATH 主命令未命中时的兜底单探）。
#[cfg(not(target_os = "windows"))]
fn scan_cli_version(tool: &str) -> ShellProbe {
    use std::process::Command;

    let search_paths = build_tool_search_paths(tool);
    let current_path = std::env::var_os("PATH")
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut exec_diagnostic: Option<String> = None;

    for path in &search_paths {
        let new_path = format!("{}:{}", path.display(), current_path);

        for tool_path in tool_executable_candidates(tool, path) {
            if !tool_path.exists() {
                continue;
            }

            let output = Command::new(&tool_path)
                .arg("--version")
                .env("PATH", &new_path)
                .output();

            if let Ok(out) = output {
                let stdout = decode_command_output(&out.stdout).trim().to_string();
                let stderr = decode_command_output(&out.stderr).trim().to_string();
                if out.status.success() {
                    let raw = if stdout.is_empty() { &stderr } else { &stdout };
                    if !raw.is_empty() {
                        return ShellProbe::Found(extract_version(raw));
                    }
                } else if exec_diagnostic.is_none() {
                    let detail = if stderr.is_empty() { stdout } else { stderr };
                    let detail = detail.trim();
                    if !detail.is_empty() {
                        exec_diagnostic = Some(last_lines(detail, 4));
                    }
                }
            }
        }
    }

    match exec_diagnostic {
        Some(detail) => ShellProbe::FoundButFailed(detail),
        None => ShellProbe::NotFound(NOT_INSTALLED.to_string()),
    }
}

/// 获取单个工具的版本信息（内部实现）。
#[cfg(not(target_os = "windows"))]
async fn get_single_tool_version_impl(tool: &str) -> ToolVersion {
    debug_assert!(
        VALID_TOOLS.contains(&tool),
        "unexpected tool name in get_single_tool_version_impl: {tool}"
    );

    let env_type = tool_env_type().to_string();

    // 使用全局 HTTP 客户端以复用连接池。
    let client = crate::http_client::get();

    // 1. 获取本地版本：PATH 第一个命令优先；只有它确实没装才去常见目录兜底扫描。
    let probe = match try_get_version(tool) {
        ShellProbe::NotFound(_) => scan_cli_version(tool),
        found => found,
    };
    let (local_version, local_error, installed_but_broken) = match probe {
        ShellProbe::Found(v) => (Some(v), None, false),
        ShellProbe::FoundButFailed(e) => (None, Some(e), true),
        ShellProbe::NotFound(e) => (None, Some(e), false),
    };

    // 2. 获取远程最新版本
    let local = local_version.as_deref();
    let latest_version = match tool {
        "claude" => {
            fetch_npm_latest_for_tool(&client, "@anthropic-ai/claude-code", tool, local).await
        }
        "codex" => fetch_npm_latest_for_tool(&client, "@openai/codex", tool, local).await,
        "opencode" => {
            if let Some(version) =
                fetch_npm_latest_for_tool(&client, "opencode-ai", tool, local).await
            {
                Some(version)
            } else {
                fetch_github_latest_version(&client, "anomalyco/opencode").await
            }
        }
        "openclaw" => fetch_npm_latest_for_tool(&client, "openclaw", tool, local).await,
        "hermes" => fetch_pypi_latest_version(&client, "hermes-agent").await,
        _ => None,
    };

    ToolVersion {
        name: tool.to_string(),
        version: local_version,
        latest_version,
        error: local_error,
        installed_but_broken,
        env_type,
        wsl_distro: None,
    }
}

#[cfg(target_os = "windows")]
fn windows_where_tool(tool: &str) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    let output = std::process::Command::new("where")
        .arg(tool)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let Ok(output) = output else {
        return paths;
    };
    if !output.status.success() {
        return paths;
    }

    for line in decode_command_output(&output.stdout).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        push_unique_path(&mut paths, std::path::PathBuf::from(trimmed));
    }
    paths
}

#[cfg(target_os = "windows")]
fn run_windows_tool_version(path: &Path) -> ShellProbe {
    let output = std::process::Command::new(path)
        .arg("--version")
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match output {
        Ok(out) => {
            let stdout = decode_command_output(&out.stdout).trim().to_string();
            let stderr = decode_command_output(&out.stderr).trim().to_string();
            if out.status.success() {
                let raw = if stdout.is_empty() { &stderr } else { &stdout };
                if raw.is_empty() {
                    ShellProbe::NotFound(NOT_INSTALLED.to_string())
                } else {
                    ShellProbe::Found(extract_version(raw))
                }
            } else {
                let err = if stderr.is_empty() { stdout } else { stderr };
                if err.trim().is_empty() {
                    ShellProbe::NotFound(NOT_INSTALLED.to_string())
                } else {
                    ShellProbe::FoundButFailed(last_lines(err.trim(), 4))
                }
            }
        }
        Err(err) => ShellProbe::FoundButFailed(err.to_string()),
    }
}

#[cfg(target_os = "windows")]
async fn get_single_tool_version_impl_windows(tool: &str) -> ToolVersion {
    let mut local_version = None;
    let mut local_error = Some(NOT_INSTALLED.to_string());
    let mut installed_but_broken = false;

    for path in windows_where_tool(tool) {
        match run_windows_tool_version(&path) {
            ShellProbe::Found(version) => {
                local_version = Some(version);
                local_error = None;
                installed_but_broken = false;
                break;
            }
            ShellProbe::FoundButFailed(err) => {
                if local_version.is_none() {
                    local_error = Some(err);
                    installed_but_broken = true;
                }
            }
            ShellProbe::NotFound(err) => {
                if !installed_but_broken {
                    local_error = Some(err);
                }
            }
        }
    }

    ToolVersion {
        name: tool.to_string(),
        version: local_version,
        latest_version: None,
        error: local_error,
        installed_but_broken,
        env_type: tool_env_type().to_string(),
        wsl_distro: None,
    }
}

// ===========================================================================
// 最新版本拉取
// ===========================================================================

/// 该工具在 npm 上的预发布通道 tag。
#[cfg(not(target_os = "windows"))]
fn npm_prerelease_tags(tool: &str) -> &'static [&'static str] {
    match tool {
        "claude" => &["next"],
        _ => &[],
    }
}

/// 解析 "2.1.156" / "2.1.156-beta.1" → (主版本三段, 预发布段)。无法解析返回 None。
#[cfg(not(target_os = "windows"))]
fn parse_semver(v: &str) -> Option<([u64; 3], Vec<String>)> {
    let core_and_pre = v.trim().split('+').next().unwrap_or("");
    let (core, pre) = match core_and_pre.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (core_and_pre, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let pre_segments = pre
        .map(|p| p.split('.').map(|s| s.to_string()).collect())
        .unwrap_or_default();
    Some(([major, minor, patch], pre_segments))
}

/// 比较两个版本号（遵循 semver）。任一无法解析返回 None。
#[cfg(not(target_os = "windows"))]
fn compare_semver(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let (ac, ap) = parse_semver(a)?;
    let (bc, bp) = parse_semver(b)?;
    for i in 0..3 {
        match ac[i].cmp(&bc[i]) {
            Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    match (ap.is_empty(), bp.is_empty()) {
        (true, true) => return Some(Ordering::Equal),
        (true, false) => return Some(Ordering::Greater),
        (false, true) => return Some(Ordering::Less),
        (false, false) => {}
    }
    for (x, y) in ap.iter().zip(bp.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(xv), Ok(yv)) => xv.cmp(&yv),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.as_str().cmp(y.as_str()),
        };
        if ord != Ordering::Equal {
            return Some(ord);
        }
    }
    Some(ap.len().cmp(&bp.len()))
}

/// 从一次 registry 请求得到的完整 dist-tags 出发，挑选要展示的"最新版本"。
#[cfg(not(target_os = "windows"))]
fn pick_latest_version(
    dist_tags: &serde_json::Map<String, serde_json::Value>,
    prerelease_tags: &[&str],
    local_version: Option<&str>,
) -> Option<String> {
    use std::cmp::Ordering;
    let latest = dist_tags.get("latest").and_then(|v| v.as_str())?;

    let local_ahead = local_version
        .and_then(|local| compare_semver(local, latest))
        .map(|ord| ord == Ordering::Greater)
        .unwrap_or(false);
    if prerelease_tags.is_empty() || !local_ahead {
        return Some(latest.to_string());
    }

    let mut best = latest.to_string();
    for tag in prerelease_tags {
        if let Some(candidate) = dist_tags.get(*tag).and_then(|v| v.as_str())
            && compare_semver(candidate, &best) == Some(Ordering::Greater)
        {
            best = candidate.to_string();
        }
    }
    Some(best)
}

/// 拉取 npm 包的完整 dist-tags。
#[cfg(not(target_os = "windows"))]
async fn fetch_npm_dist_tags(
    client: &reqwest::Client,
    package: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let url = format!("https://registry.npmjs.org/{package}");
    let resp = client.get(&url).send().await.ok()?;
    let json = resp.json::<serde_json::Value>().await.ok()?;
    json.get("dist-tags")?.as_object().cloned()
}

/// 查询某 npm 工具要展示的"最新版本"。
#[cfg(not(target_os = "windows"))]
async fn fetch_npm_latest_for_tool(
    client: &reqwest::Client,
    package: &str,
    tool: &str,
    local_version: Option<&str>,
) -> Option<String> {
    let dist_tags = fetch_npm_dist_tags(client, package).await?;
    pick_latest_version(&dist_tags, npm_prerelease_tags(tool), local_version)
}

/// 从 GitHub releases 拉取最新版本。
#[cfg(not(target_os = "windows"))]
async fn fetch_github_latest_version(client: &reqwest::Client, repo: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    match client
        .get(&url)
        .header("User-Agent", "OcHub")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => json
                .get("tag_name")
                .and_then(|v| v.as_str())
                .map(|s| s.strip_prefix('v').unwrap_or(s).to_string()),
            _ => None,
        },
        Err(_) => None,
    }
}

/// 从 PyPI 拉取最新版本。
#[cfg(not(target_os = "windows"))]
async fn fetch_pypi_latest_version(client: &reqwest::Client, package: &str) -> Option<String> {
    let url = format!("https://pypi.org/pypi/{package}/json");
    match client.get(&url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => json
                .get("info")
                .and_then(|info| info.get("version"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            _ => None,
        },
        Err(_) => None,
    }
}

// ===========================================================================
// 安装枚举与锚定升级命令（macOS/Linux）
// ===========================================================================

/// 由可执行文件路径前缀推断安装来源。
fn infer_install_source(path: &Path) -> &'static str {
    let s = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if s.contains("/.nvm/") {
        "nvm"
    } else if s.contains("/homebrew/") || s.contains("/cellar/") {
        "homebrew"
    } else if s.contains("/.volta/") || s.contains("/volta/") {
        "volta"
    } else if s.contains("fnm_multishells") {
        "fnm"
    } else if s.contains("/mise/") {
        "mise"
    } else if s.contains("/.bun/") {
        "bun"
    } else if s.contains("/pnpm/") {
        "pnpm"
    } else if s.contains("/scoop/") {
        "scoop"
    } else if s.contains("/library/python")
        || s.contains("/scripts/")
        || s.contains("/site-packages/")
    {
        "pip"
    } else {
        "system"
    }
}

/// 从 shell 输出里挑出第一个绝对路径行。
#[cfg(not(target_os = "windows"))]
fn first_abs_path_line(raw: &str) -> Option<&str> {
    raw.lines().map(str::trim).find(|l| l.starts_with('/'))
}

/// 用登录 shell 解析 PATH 默认命中的可执行文件路径，canonicalize 后作为锚点。
#[cfg(not(target_os = "windows"))]
fn resolve_path_default(tool: &str) -> Option<std::path::PathBuf> {
    use std::process::Command;
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| is_valid_shell(s))
        .unwrap_or_else(|| "sh".to_string());
    let flag = default_flag_for_shell(&shell);
    let out = Command::new(shell)
        .arg(flag)
        .arg(format!("command -v {tool}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = decode_command_output(&out.stdout);
    let first = first_abs_path_line(&raw)?;
    std::fs::canonicalize(first).ok()
}

/// 枚举工具在系统中的所有安装（不短路）。
#[cfg(not(target_os = "windows"))]
fn enumerate_tool_installations(tool: &str) -> Vec<ToolInstallation> {
    use std::process::Command;

    let search_paths = build_tool_search_paths(tool);
    let current_path = std::env::var_os("PATH")
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let path_default = resolve_path_default(tool);

    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    let mut installs: Vec<ToolInstallation> = Vec::new();

    for dir in &search_paths {
        let new_path = format!("{}:{}", dir.display(), current_path);

        for tool_path in tool_executable_candidates(tool, dir) {
            if !tool_path.exists() {
                continue;
            }
            let real = std::fs::canonicalize(&tool_path).unwrap_or_else(|_| tool_path.clone());
            if !seen.insert(real.clone()) {
                continue;
            }

            let output = Command::new(&tool_path)
                .arg("--version")
                .env("PATH", &new_path)
                .output();

            let (version, runnable, error) = match output {
                Ok(out) if out.status.success() => {
                    let stdout = decode_command_output(&out.stdout).trim().to_string();
                    let stderr = decode_command_output(&out.stderr).trim().to_string();
                    let raw = if stdout.is_empty() { stderr } else { stdout };
                    (Some(extract_version(&raw)), true, None)
                }
                Ok(out) => {
                    let stderr = decode_command_output(&out.stderr).trim().to_string();
                    let stdout = decode_command_output(&out.stdout).trim().to_string();
                    let detail = if stderr.is_empty() { stdout } else { stderr };
                    let detail = detail.trim();
                    let error = if detail.is_empty() {
                        None
                    } else {
                        Some(last_lines(detail, 4))
                    };
                    (None, false, error)
                }
                Err(e) => (None, false, Some(e.to_string())),
            };

            let is_path_default = path_default.as_ref() == Some(&real);
            let path_str = tool_path.display().to_string();
            let source = infer_install_source(&tool_path);

            installs.push(ToolInstallation {
                path: path_str,
                version,
                runnable,
                error,
                source: source.to_string(),
                is_path_default,
                real: real.clone(),
            });
        }
    }

    installs.sort_by_key(|i| std::cmp::Reverse(i.is_path_default));
    installs
}

#[cfg(target_os = "windows")]
fn enumerate_tool_installations(tool: &str) -> Vec<ToolInstallation> {
    let paths = windows_where_tool(tool);
    let default_path = paths.first().cloned();
    let mut seen = std::collections::HashSet::new();
    let mut installs = Vec::new();

    for tool_path in paths {
        let real = std::fs::canonicalize(&tool_path).unwrap_or_else(|_| tool_path.clone());
        if !seen.insert(real.clone()) {
            continue;
        }

        let (version, runnable, error) = match run_windows_tool_version(&tool_path) {
            ShellProbe::Found(version) => (Some(version), true, None),
            ShellProbe::FoundButFailed(err) => (None, false, Some(err)),
            ShellProbe::NotFound(err) => (None, false, Some(err)),
        };

        installs.push(ToolInstallation {
            path: tool_path.display().to_string(),
            version,
            runnable,
            error,
            source: infer_install_source(&tool_path).to_string(),
            is_path_default: default_path.as_ref() == Some(&tool_path),
            real,
        });
    }

    installs.sort_by_key(|i| std::cmp::Reverse(i.is_path_default));
    installs
}

/// 工具对应的 npm 包名。
fn npm_package_for(tool: &str) -> Option<&'static str> {
    match tool {
        "claude" => Some("@anthropic-ai/claude-code"),
        "codex" => Some("@openai/codex"),
        "opencode" => Some("opencode-ai"),
        "openclaw" => Some("openclaw"),
        _ => None,
    }
}

/// 取路径的父目录（纯字符串截断，不碰 fs）。
fn parent_dir(p: &str) -> String {
    match p.rfind('\\').max(p.rfind('/')) {
        Some(i) if i > 0 => p[..i].to_string(),
        _ => String::new(),
    }
}

/// 从 canonicalize 后的真身路径提取 Homebrew formula 名。
#[cfg(not(target_os = "windows"))]
fn brew_formula_from_path(real: &str) -> Option<String> {
    let mut segs = real.split('/');
    while let Some(seg) = segs.next() {
        if seg.eq_ignore_ascii_case("Cellar") {
            return segs.next().filter(|s| !s.is_empty()).map(|s| s.to_string());
        }
    }
    None
}

/// 含空格才用 POSIX 单引号包一层。
#[cfg(not(target_os = "windows"))]
fn quote_path_if_spaced(p: &str) -> String {
    if p.contains(' ') {
        shell_single_quote(p)
    } else {
        p.to_string()
    }
}

/// 返回 `<bin_path 同目录>/<exe>` 的绝对路径。
#[cfg(not(target_os = "windows"))]
fn sibling_bin(bin_path: &str, exe: &str) -> Option<String> {
    let dir = parent_dir(bin_path);
    if dir.is_empty() {
        None
    } else {
        Some(format!("{dir}/{exe}"))
    }
}

fn official_update_args(tool: &str) -> Option<&'static str> {
    match tool {
        "claude" | "codex" | "hermes" => Some("update"),
        "openclaw" => Some("update --yes"),
        "opencode" => Some("upgrade"),
        _ => None,
    }
}

#[cfg(not(target_os = "windows"))]
fn anchored_official_update_command(tool: &str, bin_path: &str) -> Option<String> {
    official_update_args(tool).map(|args| format!("{} {args}", quote_path_if_spaced(bin_path)))
}

#[derive(Debug, Clone, Copy)]
enum LifecycleCommandShell {
    Posix,
    #[cfg(target_os = "windows")]
    WindowsBatch,
}

/// 哪些工具的"官方 self-update"优先于包管理器升级。
fn prefers_official_update(tool: &str, shell: LifecycleCommandShell) -> bool {
    match shell {
        LifecycleCommandShell::Posix => {
            matches!(tool, "claude" | "opencode" | "openclaw")
        }
        #[cfg(target_os = "windows")]
        LifecycleCommandShell::WindowsBatch => matches!(tool, "codex" | "hermes"),
    }
}

fn chain_update_commands(
    primary: String,
    fallback: String,
    shell: LifecycleCommandShell,
) -> String {
    if fallback.trim().is_empty() {
        return primary;
    }
    match shell {
        LifecycleCommandShell::Posix => format!("{primary} || {fallback}"),
        #[cfg(target_os = "windows")]
        LifecycleCommandShell::WindowsBatch => format!("{primary} || call {fallback}"),
    }
}

/// Codex 平台分发包损坏的自愈命令（uninstall + install）。
#[cfg(not(target_os = "windows"))]
fn codex_repair_command(bin_path: &str, real: &str) -> Option<String> {
    if brew_formula_from_path(real).is_some() {
        return None;
    }
    if !matches!(
        infer_install_source(Path::new(bin_path)),
        "nvm" | "fnm" | "mise" | "homebrew"
    ) {
        return None;
    }
    let npm = sibling_bin(bin_path, "npm")?;
    let npm = quote_path_if_spaced(&npm);
    let pkg = "@openai/codex";
    Some(format!(
        "{npm} uninstall -g {pkg} || true; {npm} i -g {pkg}@latest"
    ))
}

#[cfg(not(target_os = "windows"))]
fn package_manager_anchored_command_from_paths(
    tool: &str,
    bin_path: &str,
    real_target: &str,
) -> Option<String> {
    if let Some(formula) = brew_formula_from_path(real_target) {
        let brew = sibling_bin(bin_path, "brew")?;
        return Some(format!("{} upgrade {formula}", quote_path_if_spaced(&brew)));
    }
    let pkg = npm_package_for(tool)?;
    match infer_install_source(Path::new(bin_path)) {
        "volta" => {
            let volta = sibling_bin(bin_path, "volta")?;
            return Some(format!("{} install {pkg}", quote_path_if_spaced(&volta)));
        }
        "bun" => {
            let bun = sibling_bin(bin_path, "bun")?;
            return Some(format!(
                "{} add -g {pkg}@latest",
                quote_path_if_spaced(&bun)
            ));
        }
        "nvm" | "fnm" | "mise" | "homebrew" => {}
        _ => return None,
    }
    let npm = sibling_bin(bin_path, "npm")?;
    Some(format!("{} i -g {pkg}@latest", quote_path_if_spaced(&npm)))
}

/// 给定工具、原始 bin 路径、真身路径，推断"写回同一处"的锚定升级命令（POSIX）。
#[cfg(not(target_os = "windows"))]
fn anchored_command_from_paths(tool: &str, bin_path: &str, real_target: &str) -> Option<String> {
    let real_lower = real_target.to_ascii_lowercase();

    if tool == "hermes" {
        return anchored_official_update_command(tool, bin_path);
    }
    if tool == "claude"
        && (real_lower.contains("/.local/share/claude/")
            || real_lower.contains("/claude/versions/"))
    {
        return anchored_official_update_command(tool, bin_path);
    }
    let package_command = package_manager_anchored_command_from_paths(tool, bin_path, real_target);
    if brew_formula_from_path(real_target).is_some() {
        return package_command;
    }
    if prefers_official_update(tool, LifecycleCommandShell::Posix) {
        let update = anchored_official_update_command(tool, bin_path)?;
        return Some(match package_command {
            Some(fallback) => chain_update_commands(update, fallback, LifecycleCommandShell::Posix),
            None => update,
        });
    }
    package_command
}

/// 从枚举结果里取"命令行实际命中的那处"。
fn default_install(installs: &[ToolInstallation]) -> Option<&ToolInstallation> {
    installs.iter().find(|i| i.is_path_default).or_else(|| {
        if installs.len() == 1 {
            installs.first()
        } else {
            None
        }
    })
}

/// 基于已枚举的安装列表生成锚定升级命令。
#[cfg(not(target_os = "windows"))]
fn installs_anchored_command(tool: &str, installs: &[ToolInstallation]) -> Option<String> {
    let inst = default_install(installs)?;
    let real = inst.real.to_string_lossy();
    if tool == "codex"
        && !inst.runnable
        && let Some(cmd) = codex_repair_command(&inst.path, &real)
    {
        return Some(cmd);
    }
    anchored_command_from_paths(tool, &inst.path, &real)
}

// ===========================================================================
// 静态命令 / 安装命令（POSIX）
// ===========================================================================

const CLAUDE_INSTALL_UNIX: &str = "bash -c 'tmp=$(mktemp) && curl -fsSL https://claude.ai/install.sh -o $tmp && bash $tmp; status=$?; rm -f $tmp; exit $status'";
const OPENCODE_INSTALL_UNIX: &str = "bash -c 'tmp=$(mktemp) && curl -fsSL https://opencode.ai/install -o $tmp && bash $tmp; status=$?; rm -f $tmp; exit $status'";
const HERMES_INSTALL_UNIX: &str = "bash -c 'tmp=$(mktemp) && curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh -o $tmp && bash $tmp; status=$?; rm -f $tmp; exit $status'";
const HERMES_UPDATE_UNIX: &str = "hermes update || bash -c 'tmp=$(mktemp) && curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh -o $tmp && bash $tmp; status=$?; rm -f $tmp; exit $status'";
#[cfg(target_os = "windows")]
const HERMES_INSTALL_WINDOWS_SCRIPT: &str = "irm https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.ps1 | iex";

#[cfg(target_os = "windows")]
fn powershell_encoded_command(script: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    STANDARD.encode(bytes)
}

#[cfg(target_os = "windows")]
fn hermes_install_windows_command() -> String {
    format!(
        "powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand {}",
        powershell_encoded_command(HERMES_INSTALL_WINDOWS_SCRIPT)
    )
}

#[cfg(target_os = "windows")]
fn hermes_update_windows_command() -> String {
    format!("hermes update || {}", hermes_install_windows_command())
}

fn npm_install_command_for(tool: &str) -> Option<&'static str> {
    match tool {
        "claude" => Some("npm i -g @anthropic-ai/claude-code@latest"),
        "codex" => Some("npm i -g @openai/codex@latest"),
        "opencode" => Some("npm i -g opencode-ai@latest"),
        "openclaw" => Some("npm i -g openclaw@latest"),
        _ => None,
    }
}

fn bare_official_update_command(tool: &str) -> Option<String> {
    official_update_args(tool).map(|args| format!("{tool} {args}"))
}

fn tool_action_shell_command_for_shell(
    tool: &str,
    action: ToolLifecycleAction,
    shell: LifecycleCommandShell,
) -> Option<String> {
    if tool == "hermes" {
        return Some(
            match (action, shell) {
                (ToolLifecycleAction::Install, LifecycleCommandShell::Posix) => HERMES_INSTALL_UNIX,
                (ToolLifecycleAction::Update, LifecycleCommandShell::Posix) => HERMES_UPDATE_UNIX,
                #[cfg(target_os = "windows")]
                (ToolLifecycleAction::Install, LifecycleCommandShell::WindowsBatch) => {
                    return Some(hermes_install_windows_command());
                }
                #[cfg(target_os = "windows")]
                (ToolLifecycleAction::Update, LifecycleCommandShell::WindowsBatch) => {
                    return Some(hermes_update_windows_command());
                }
            }
            .to_string(),
        );
    }

    let install = npm_install_command_for(tool)?;
    match action {
        ToolLifecycleAction::Install => Some(install.to_string()),
        ToolLifecycleAction::Update => match prefers_official_update(tool, shell)
            .then(|| bare_official_update_command(tool))
            .flatten()
        {
            Some(update) => Some(chain_update_commands(update, install.to_string(), shell)),
            None => Some(install.to_string()),
        },
    }
}

fn tool_action_shell_command(tool: &str, action: ToolLifecycleAction) -> Option<String> {
    #[cfg(target_os = "windows")]
    let shell = LifecycleCommandShell::WindowsBatch;
    #[cfg(not(target_os = "windows"))]
    let shell = LifecycleCommandShell::Posix;

    tool_action_shell_command_for_shell(tool, action, shell)
}

fn static_fallback_command_for(tool: &str, action: ToolLifecycleAction) -> String {
    tool_action_shell_command(tool, action).unwrap_or_default()
}

fn static_fallback_command(tool: &str) -> String {
    static_fallback_command_for(tool, ToolLifecycleAction::Update)
}

fn installer_with_npm_fallback(installer: &str, tool: &str) -> String {
    match npm_install_command_for(tool) {
        Some(npm) => chain_update_commands(
            installer.to_string(),
            npm.to_string(),
            LifecycleCommandShell::Posix,
        ),
        None => installer.to_string(),
    }
}

fn posix_install_command_for(tool: &str) -> String {
    match tool {
        "claude" => installer_with_npm_fallback(CLAUDE_INSTALL_UNIX, tool),
        "opencode" => installer_with_npm_fallback(OPENCODE_INSTALL_UNIX, tool),
        "hermes" => HERMES_INSTALL_UNIX.to_string(),
        _ => static_fallback_command_for(tool, ToolLifecycleAction::Install),
    }
}

#[cfg(not(target_os = "windows"))]
fn install_command_for(tool: &str) -> String {
    posix_install_command_for(tool)
}

// ===========================================================================
// 升级规划 + 冲突判定
// ===========================================================================

/// 计算某工具的升级命令与"是否需确认"。
#[cfg(not(target_os = "windows"))]
fn plan_command_for(tool: &str, installs: &[ToolInstallation]) -> (String, bool, bool) {
    match installs_anchored_command(tool, installs) {
        Some(command) => (command, installs.len() >= 2, true),
        None => (static_fallback_command(tool), installs.len() >= 2, false),
    }
}

/// 多处安装是否构成"真冲突"。
#[cfg(not(target_os = "windows"))]
fn is_conflicting(installs: &[ToolInstallation]) -> bool {
    if installs.len() < 2 {
        return false;
    }
    let distinct_versions: std::collections::HashSet<&Option<String>> =
        installs.iter().map(|i| &i.version).collect();
    let runnable_mixed =
        installs.iter().any(|i| i.runnable) && installs.iter().any(|i| !i.runnable);
    distinct_versions.len() > 1 || runnable_mixed
}

#[cfg(target_os = "windows")]
fn plan_command_for(tool: &str, installs: &[ToolInstallation]) -> (String, bool, bool) {
    (static_fallback_command(tool), installs.len() >= 2, false)
}

#[cfg(target_os = "windows")]
fn is_conflicting(installs: &[ToolInstallation]) -> bool {
    if installs.len() < 2 {
        return false;
    }
    let distinct_versions: std::collections::HashSet<&Option<String>> =
        installs.iter().map(|i| &i.version).collect();
    let runnable_mixed =
        installs.iter().any(|i| i.runnable) && installs.iter().any(|i| !i.runnable);
    distinct_versions.len() > 1 || runnable_mixed
}

fn tool_display_name(tool: &str) -> &'static str {
    match tool {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "openclaw" => "OpenClaw",
        "hermes" => "Hermes",
        _ => "Unknown",
    }
}

// ===========================================================================
// lifecycle 命令构建 + 静默执行（POSIX）
// ===========================================================================

#[cfg(not(target_os = "windows"))]
fn build_tool_action_line(tool: &str, action: ToolLifecycleAction) -> Result<String, String> {
    let command = match action {
        ToolLifecycleAction::Update => {
            let installs = enumerate_tool_installations(tool);
            installs_anchored_command(tool, &installs)
                .unwrap_or_else(|| static_fallback_command(tool))
        }
        ToolLifecycleAction::Install => install_command_for(tool),
    };
    if command.is_empty() {
        return Err(format!("Unsupported tool action target: {tool}"));
    }
    Ok(command)
}

#[cfg(not(target_os = "windows"))]
fn build_tool_lifecycle_command(
    tools: &[&str],
    action: ToolLifecycleAction,
) -> Result<String, String> {
    let mut lines = Vec::new();
    lines.push("set -e".to_string());
    lines.push("set -o pipefail".to_string());

    for tool in tools {
        let label = tool_display_name(tool);
        lines.push(format!("echo ========== {label} =========="));
        let line = build_tool_action_line(tool, action)?;
        lines.push(line);
        lines.push(String::new());
    }

    Ok(lines.join("\n"))
}

#[cfg(target_os = "windows")]
fn build_tool_action_line(tool: &str, action: ToolLifecycleAction) -> Result<String, String> {
    let command = static_fallback_command_for(tool, action);
    if command.is_empty() {
        return Err(format!("Unsupported tool action target: {tool}"));
    }
    Ok(format!("call {command}"))
}

#[cfg(target_os = "windows")]
fn build_tool_lifecycle_command(
    tools: &[&str],
    action: ToolLifecycleAction,
) -> Result<String, String> {
    let mut lines = Vec::new();
    lines.push("@echo off".to_string());

    for tool in tools {
        let label = tool_display_name(tool);
        lines.push(format!("echo ========== {label} =========="));
        lines.push(build_tool_action_line(tool, action)?);
        lines.push("if errorlevel 1 exit /b %errorlevel%".to_string());
        lines.push(String::new());
    }

    Ok(lines.join("\r\n"))
}

/// 把子进程退出结果转成 `Result`。
fn finish_lifecycle_output(output: &std::process::Output) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = decode_command_output(&output.stderr);
    let stdout = decode_command_output(&output.stdout);
    let raw = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let detail = last_lines(raw, 8);
    Err(if detail.is_empty() {
        format!("命令执行失败 (exit code: {:?})", output.status.code())
    } else {
        detail
    })
}

/// 静默执行工具安装/更新脚本。
#[cfg(not(target_os = "windows"))]
fn run_tool_lifecycle_silently(command_line: &str, _label: &str) -> Result<(), String> {
    use std::process::Command;
    let output = Command::new("bash")
        .arg("-c")
        .arg(command_line)
        .output()
        .map_err(|e| format!("启动安装进程失败: {e}"))?;
    finish_lifecycle_output(&output)
}

#[cfg(target_os = "windows")]
fn run_tool_lifecycle_silently(command_line: &str, label: &str) -> Result<(), String> {
    let bat_file = std::env::temp_dir().join(format!("ochub_{label}_{}.bat", std::process::id()));
    std::fs::write(&bat_file, command_line).map_err(|e| format!("写入批处理文件失败: {e}"))?;

    let output = std::process::Command::new("cmd")
        .arg("/C")
        .arg(&bat_file)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = std::fs::remove_file(&bat_file);

    finish_lifecycle_output(&output.map_err(|e| format!("启动安装进程失败: {e}"))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_parses_actions() {
        assert!(matches!(
            ToolLifecycleAction::from_str("install"),
            Ok(ToolLifecycleAction::Install)
        ));
        assert!(matches!(
            ToolLifecycleAction::from_str("update"),
            Ok(ToolLifecycleAction::Update)
        ));
        assert!(ToolLifecycleAction::from_str("bogus").is_err());
    }

    #[test]
    fn normalize_filters_unknown_tools() {
        let got = normalize_requested_tools(&[
            "claude".to_string(),
            "nope".to_string(),
            "codex".to_string(),
        ]);
        assert_eq!(got, vec!["claude", "codex"]);
    }

    #[test]
    fn extract_version_picks_semver() {
        assert_eq!(extract_version("claude 2.1.156"), "2.1.156");
        assert_eq!(extract_version("v1.0.0-beta.1 (build)"), "1.0.0-beta.1");
    }

    #[test]
    fn infer_install_source_matches_prefixes() {
        assert_eq!(
            infer_install_source(Path::new("/Users/me/.nvm/versions/node/v20/bin/claude")),
            "nvm"
        );
        assert_eq!(
            infer_install_source(Path::new("/opt/homebrew/Cellar/some-cli/0.1/bin/some-cli")),
            "homebrew"
        );
        assert_eq!(
            infer_install_source(Path::new("/usr/local/bin/codex")),
            "system"
        );
    }

    #[test]
    fn parent_dir_takes_rightmost_separator() {
        assert_eq!(parent_dir("/a/b/npm"), "/a/b");
        assert_eq!(parent_dir("npm"), "");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn brew_formula_extracts_name() {
        assert_eq!(
            brew_formula_from_path("/opt/homebrew/Cellar/some-cli/0.13.0/bin/some-cli").as_deref(),
            Some("some-cli")
        );
        assert_eq!(brew_formula_from_path("/opt/homebrew/bin/some-cli"), None);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn is_conflicting_thresholds() {
        let make = |version: Option<&str>, runnable: bool| ToolInstallation {
            path: "/x".to_string(),
            version: version.map(str::to_string),
            runnable,
            error: None,
            source: "nvm".to_string(),
            is_path_default: false,
            real: std::path::PathBuf::from("/x"),
        };
        assert!(!is_conflicting(&[make(Some("1.0.0"), true)]));
        assert!(!is_conflicting(&[
            make(Some("1.0.0"), true),
            make(Some("1.0.0"), true)
        ]));
        assert!(is_conflicting(&[
            make(Some("1.0.0"), true),
            make(Some("2.0.0"), true)
        ]));
        assert!(is_conflicting(&[
            make(Some("1.0.0"), true),
            make(Some("1.0.0"), false)
        ]));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn claude_install_prefers_native_with_npm_fallback() {
        let cmd = install_command_for("claude");
        assert!(cmd.contains("https://claude.ai/install.sh"), "{cmd}");
        assert!(cmd.contains("@anthropic-ai/claude-code@latest"), "{cmd}");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn first_abs_path_line_skips_noise() {
        assert_eq!(
            first_abs_path_line("🚀 Welcome back!\n/Users/me/.local/bin/claude\n"),
            Some("/Users/me/.local/bin/claude")
        );
        assert_eq!(first_abs_path_line("welcome\nbye\n"), None);
    }
}

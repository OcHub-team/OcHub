//! Skills 服务层
//!
//! 封装 Vercel `skills` CLI（`npx -y skills`，探测于 v1.5.14）：
//! - 文件落盘与各应用目录同步全部交给 CLI（全局仓库 `~/.agents/skills/`）
//! - SQLite（`skills` / `skill_repos` 表）仍是登记表：记录来源仓库与各应用启用状态
//! - 每次变更后从 CLI 的 `list -g --json` 与 `~/.agents/.skill-lock.json` 回填登记表
//!
//! skills.sh 搜索与存储无关，保持原有 HTTP 实现。

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::time::timeout;

use crate::app_type::AppType;
use crate::db::Database;
use crate::db::legacy_json::{InstalledSkill, SkillApps, SkillRepo};
use crate::error::format_skill_error;
use crate::paths::{get_app_config_dir, get_claude_config_dir};
use crate::settings::SkillStorageLocation;

// ========== 数据结构 ==========

/// 可发现的技能（来自仓库）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverableSkill {
    /// 唯一标识: "owner/name:directory"
    pub key: String,
    /// 显示名称（CLI 报告的 skill 名）
    pub name: String,
    /// 技能描述
    pub description: String,
    /// 目录名称（同时是 CLI `-s` 的技能名）
    pub directory: String,
    /// GitHub README URL
    #[serde(rename = "readmeUrl")]
    pub readme_url: Option<String>,
    /// 仓库所有者
    #[serde(rename = "repoOwner")]
    pub repo_owner: String,
    /// 仓库名称
    #[serde(rename = "repoName")]
    pub repo_name: String,
    /// 分支名称
    #[serde(rename = "repoBranch")]
    pub repo_branch: String,
}

/// 技能对象（兼容旧 API，内部使用 DiscoverableSkill）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// 唯一标识: "owner/name:directory" 或 "local:directory"
    pub key: String,
    /// 显示名称
    pub name: String,
    /// 技能描述
    pub description: String,
    /// 目录名称
    pub directory: String,
    /// GitHub README URL
    #[serde(rename = "readmeUrl")]
    pub readme_url: Option<String>,
    /// 是否已安装
    pub installed: bool,
    /// 仓库所有者
    #[serde(rename = "repoOwner")]
    pub repo_owner: Option<String>,
    /// 仓库名称
    #[serde(rename = "repoName")]
    pub repo_name: Option<String>,
    /// 分支名称
    #[serde(rename = "repoBranch")]
    pub repo_branch: Option<String>,
}

/// Skill 卸载结果（备份机制已由 CLI 接管，字段保留兼容旧客户端）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillUninstallResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<String>,
}

/// Skill 更新检测结果（内容哈希检测已废弃；`check_updates` 恒返回空列表）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillUpdateInfo {
    /// Skill ID
    pub id: String,
    /// Skill 名称
    pub name: String,
    /// 当前本地哈希
    pub current_hash: Option<String>,
    /// 远程最新哈希
    pub remote_hash: String,
}

// ========== skills.sh API 类型 ==========

/// skills.sh API 原始响应
///
/// 注意：API 命名不一致（searchType 是 camelCase，duration_ms 是 snake_case），
/// 因此不能用 rename_all，需要逐字段指定。
#[derive(Debug, Clone, Deserialize)]
struct SkillsShApiResponse {
    pub query: String,
    #[serde(rename = "searchType")]
    #[allow(dead_code)]
    pub search_type: String,
    pub skills: Vec<SkillsShApiSkill>,
    pub count: usize,
    #[allow(dead_code)]
    pub duration_ms: u64,
}

/// skills.sh API 原始技能条目
#[derive(Debug, Clone, Deserialize)]
struct SkillsShApiSkill {
    pub id: String,
    #[serde(rename = "skillId")]
    pub skill_id: String,
    pub name: String,
    pub installs: u64,
    pub source: String,
}

/// skills.sh 搜索结果（返回给前端）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsShSearchResult {
    pub skills: Vec<SkillsShDiscoverableSkill>,
    pub total_count: usize,
    pub query: String,
}

/// skills.sh 可安装技能（返回给前端）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsShDiscoverableSkill {
    pub key: String,
    pub name: String,
    pub directory: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub repo_branch: String,
    pub installs: u64,
    pub readme_url: Option<String>,
}

/// 技能元数据 (从 SKILL.md 解析)
#[derive(Debug, Clone, Deserialize)]
pub struct SkillMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
}

// ========== ~/.agents/ lock 文件解析 ==========

/// `~/.agents/.skill-lock.json` 文件结构
#[derive(Deserialize)]
struct AgentsLockFile {
    skills: HashMap<String, AgentsLockSkill>,
}

/// lock 文件中单个 skill 的信息
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentsLockSkill {
    source: Option<String>,
    source_type: Option<String>,
    source_url: Option<String>,
    skill_path: Option<String>,
    branch: Option<String>,
    source_branch: Option<String>,
}

#[derive(Debug, Clone)]
struct LockRepoInfo {
    owner: String,
    repo: String,
    skill_path: Option<String>,
    branch: Option<String>,
}

fn normalize_optional_branch(branch: Option<String>) -> Option<String> {
    branch.and_then(|b| {
        let trimmed = b.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn parse_branch_from_source_url(source_url: Option<&str>) -> Option<String> {
    let source_url = source_url?;
    let source_url = source_url.trim();
    if source_url.is_empty() {
        return None;
    }

    // 支持 https://github.com/owner/repo/tree/<branch>/...
    if let Some((_, after_tree)) = source_url.split_once("/tree/") {
        let branch = after_tree
            .split('/')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        return Some(branch.to_string());
    }

    // 支持 URL fragment: ...git#branch
    if let Some((_, fragment)) = source_url.split_once('#') {
        let branch = fragment
            .split('&')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        return Some(branch.to_string());
    }

    // 支持 query: ...?branch=xxx / ?ref=xxx
    if let Some((_, query)) = source_url.split_once('?') {
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            if matches!(key, "branch" | "ref") {
                let branch = value.trim();
                if !branch.is_empty() {
                    return Some(branch.to_string());
                }
            }
        }
    }

    None
}

/// 解析 lock 文件内容，返回 skill_name -> 仓库信息
fn parse_agents_lock_content(content: &str) -> HashMap<String, LockRepoInfo> {
    let lock: AgentsLockFile = match serde_json::from_str(content) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("解析 agents lock 文件失败: {e}");
            return HashMap::new();
        }
    };
    lock.skills
        .into_iter()
        .filter_map(|(name, skill)| {
            let source = skill.source?;
            if skill.source_type.as_deref() != Some("github") {
                return None;
            }
            let (owner, repo) = source.split_once('/')?;
            let branch = normalize_optional_branch(skill.branch)
                .or_else(|| normalize_optional_branch(skill.source_branch))
                .or_else(|| parse_branch_from_source_url(skill.source_url.as_deref()));
            Some((
                name,
                LockRepoInfo {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    skill_path: skill.skill_path,
                    branch,
                },
            ))
        })
        .collect()
}

/// 解析 `~/.agents/.skill-lock.json`，返回 skill_name -> 仓库信息
fn parse_agents_lock() -> HashMap<String, LockRepoInfo> {
    let path = match dirs::home_dir() {
        Some(h) => h.join(".agents").join(".skill-lock.json"),
        None => {
            log::warn!("无法获取 HOME 目录，跳过解析 agents lock 文件");
            return HashMap::new();
        }
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                log::debug!("未找到 agents lock 文件: {}", path.display());
            } else {
                log::warn!("读取 agents lock 文件失败 ({}): {}", path.display(), e);
            }
            return HashMap::new();
        }
    };
    parse_agents_lock_content(&content)
}

// ========== CLI 输出解析 ==========

/// 去除 ANSI 转义序列（SGR/光标控制/擦除序列）与回车符。
///
/// 实测 `skills` CLI 无法通过 NO_COLOR/FORCE_COLOR 关闭着色，
/// 所有人类可读输出都必须先清洗再解析。
fn strip_ansi(input: &str) -> String {
    use std::sync::OnceLock;
    static ANSI_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = ANSI_RE
        .get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("valid ANSI regex"));
    re.replace_all(input, "").replace('\r', "\n")
}

/// `skills list -g --json` 的单条记录
#[derive(Debug, Deserialize)]
struct CliInstalledSkill {
    name: String,
    path: String,
    #[serde(default)]
    agents: Vec<String>,
}

/// 解析 `list -g --json` 输出（stdout 中可能混有 agent 检测横幅，取首个 JSON 数组）
fn parse_list_json(stdout: &str) -> Result<Vec<CliInstalledSkill>> {
    let start = stdout.find('[');
    let end = stdout.rfind(']');
    let json = match (start, end) {
        (Some(s), Some(e)) if e >= s => &stdout[s..=e],
        _ => {
            return Err(anyhow!(format_skill_error(
                "CLI_OUTPUT_UNPARSEABLE",
                &[("output", stdout.trim())],
                None,
            )));
        }
    };
    serde_json::from_str(json).map_err(|e| {
        anyhow!(format_skill_error(
            "CLI_OUTPUT_UNPARSEABLE",
            &[("error", &e.to_string())],
            None,
        ))
    })
}

/// 解析 `skills add <repo> -l` 的 Clack 风格文本输出，返回 (名称, 描述) 列表。
///
/// 结构（清洗 ANSI 后）：
/// ```text
/// ◇  Available Skills
/// │
/// │    <skill-name>        （│ + 4 空格缩进）
/// │
/// │      <description>     （│ + 6 空格缩进，可换行）
/// └  Use --skill <name> to install specific skills
/// ```
fn parse_add_list_output(raw: &str) -> Vec<(String, String)> {
    let clean = strip_ansi(raw);
    let mut skills: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;
    let mut in_list = false;

    for line in clean.lines() {
        if !in_list {
            if line.contains("Available Skills") {
                in_list = true;
            }
            continue;
        }
        if line.starts_with('└') {
            break;
        }
        let Some(rest) = line.strip_prefix('│') else {
            continue;
        };
        let content = rest.trim();
        if content.is_empty() {
            continue;
        }
        let indent = rest.len() - rest.trim_start().len();
        // 4 空格缩进且不含空白 = 技能名；更深缩进或含空白 = 描述续行
        if indent <= 5 && !content.contains(char::is_whitespace) {
            if let Some(done) = current.take() {
                skills.push(done);
            }
            current = Some((content.to_string(), String::new()));
        } else if let Some((_, desc)) = current.as_mut() {
            if !desc.is_empty() {
                desc.push(' ');
            }
            desc.push_str(content);
        }
    }
    if let Some(done) = current.take() {
        skills.push(done);
    }
    skills
}

// ========== App <-> agent slug 映射 ==========

/// AppType -> `skills` CLI agent slug（对照 CLI v1.5.14 的合法 agent 列表）
///
/// 注意：OpenClaw 虽是受管应用，但 `SkillApps` 无对应字段（启用状态无法登记/回填），
/// 因此这里映射为 `None`，让技能安装/切换路由以 `UNSUPPORTED_AGENT` 拒绝，
/// 与 DB schema 及重写前的行为保持一致。
fn agent_slug(app: &AppType) -> Option<&'static str> {
    match app {
        AppType::Claude => Some("claude-code"),
        AppType::Codex => Some("codex"),
        AppType::OpenCode => Some("opencode"),
        AppType::Hermes => Some("hermes-agent"),
        AppType::GrokBuild | AppType::OpenClaw | AppType::ClaudeDesktop => None,
    }
}

/// 卸载时对所有受管 agent 生效（不含 OpenClaw：见 `agent_slug`）
const ALL_MANAGED_AGENT_SLUGS: &str = "claude-code,codex,opencode,hermes-agent";

/// 卸载时需要清理残留符号链接/拷贝的受管应用（与 `ALL_MANAGED_AGENT_SLUGS` 对应）
const MANAGED_SKILL_APPS: &[AppType] = &[
    AppType::Claude,
    AppType::Codex,
    AppType::OpenCode,
    AppType::Hermes,
];

/// 某受管应用的技能目录（`<应用配置目录>/skills`），CLI 会在其中放置
/// 每个技能的符号链接或拷贝。不受管的应用返回 `None`。
fn get_app_skills_dir(app: &AppType) -> Option<PathBuf> {
    let config_dir = match app {
        AppType::Claude => get_claude_config_dir(),
        AppType::Codex => crate::apps::codex::get_codex_config_dir(),
        AppType::OpenCode => crate::apps::opencode::get_opencode_dir(),
        AppType::Hermes => crate::apps::hermes::get_hermes_dir(),
        AppType::GrokBuild | AppType::OpenClaw | AppType::ClaudeDesktop => return None,
    };
    Some(config_dir.join("skills"))
}

/// 尽力删除某受管应用技能目录下的单个技能（符号链接或目录），失败仅记录日志。
///
/// 删除前校验路径包含关系：`directory` 必须是不含 `..`/绝对路径分量的普通相对名，
/// 且拼接后仍位于 `skills_dir` 之内，防止目录穿越误删。
fn remove_skill_from_app_dir(skills_dir: &Path, directory: &str, app: &AppType) {
    let rel = Path::new(directory);
    if rel.as_os_str().is_empty()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        log::warn!("跳过可疑的技能目录名 '{directory}'（{app:?}），拒绝删除");
        return;
    }
    let target = skills_dir.join(rel);
    if !target.starts_with(skills_dir) {
        log::warn!(
            "技能路径 {} 越出 {} 之外，拒绝删除",
            target.display(),
            skills_dir.display()
        );
        return;
    }

    let meta = match fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            log::warn!("读取技能路径 {} 失败: {}", target.display(), e);
            return;
        }
    };

    let result = if meta.file_type().is_symlink() || meta.is_file() {
        fs::remove_file(&target)
    } else {
        fs::remove_dir_all(&target)
    };
    match result {
        Ok(()) => log::info!("已清理 {:?} 的残留技能: {}", app, target.display()),
        Err(e) => log::warn!("清理 {:?} 技能 {} 失败: {}", app, target.display(), e),
    }
}

/// 将 CLI `list --json` 报告的 agent 显示名（如 "Claude Code"）折算为启用标志
fn apps_from_agent_labels(labels: &[String]) -> SkillApps {
    let mut apps = SkillApps::default();
    for label in labels {
        let normalized: String = label
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match normalized.as_str() {
            "claudecode" | "claude" => apps.claude = true,
            "codex" => apps.codex = true,
            "opencode" => apps.opencode = true,
            "hermesagent" | "hermes" => apps.hermes = true,
            _ => {}
        }
    }
    apps
}

// ========== skills CLI 封装 ==========

#[cfg(windows)]
const NPX_NAMES: &[&str] = &["npx.cmd", "npx.exe"];
#[cfg(not(windows))]
const NPX_NAMES: &[&str] = &["npx"];

#[cfg(windows)]
const SKILLS_BIN_NAMES: &[&str] = &["skills.cmd", "skills.exe"];
#[cfg(not(windows))]
const SKILLS_BIN_NAMES: &[&str] = &["skills"];

/// 单次 CLI 调用上限：仓库克隆可能很慢，给足余量
const CLI_TIMEOUT_SECS: u64 = 300;

fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

struct CliOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Vercel `skills` CLI 的异步子进程封装。
///
/// - 优先 `npx -y skills`，回退 PATH 上的 `skills` 可执行文件
/// - stdin 置空（无 TTY 即自动非交互），仍显式传 `-y` 防止跳过删除
/// - 输出统一清洗 ANSI 后再解析
pub struct SkillsCli {
    program: PathBuf,
    base_args: Vec<&'static str>,
}

impl SkillsCli {
    pub fn resolve() -> Result<Self> {
        if let Some(npx) = find_in_path(NPX_NAMES) {
            return Ok(Self {
                program: npx,
                base_args: vec!["-y", "skills"],
            });
        }
        if let Some(bin) = find_in_path(SKILLS_BIN_NAMES) {
            return Ok(Self {
                program: bin,
                base_args: vec![],
            });
        }
        Err(anyhow!(format_skill_error(
            "NPX_MISSING",
            &[("hint", "Node.js (npx) or a `skills` binary is required")],
            Some("installNode"),
        )))
    }

    async fn run(&self, args: &[&str]) -> Result<CliOutput> {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.base_args)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = timeout(
            std::time::Duration::from_secs(CLI_TIMEOUT_SECS),
            cmd.output(),
        )
        .await
        .map_err(|_| {
            anyhow!(format_skill_error(
                "CLI_TIMEOUT",
                &[
                    ("command", &format!("skills {}", args.join(" "))),
                    ("timeout", &CLI_TIMEOUT_SECS.to_string()),
                ],
                Some("checkNetwork"),
            ))
        })?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!(format_skill_error(
                    "NPX_MISSING",
                    &[("program", &self.program.display().to_string())],
                    Some("installNode"),
                ))
            } else {
                anyhow!(format_skill_error(
                    "CLI_SPAWN_FAILED",
                    &[("error", &e.to_string())],
                    Some("checkPermission"),
                ))
            }
        })?;

        Ok(CliOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: strip_ansi(&String::from_utf8_lossy(&output.stdout)),
            stderr: strip_ansi(&String::from_utf8_lossy(&output.stderr)),
        })
    }

    /// 执行并要求退出码为 0，失败时映射到 skill 错误码
    async fn run_ok(&self, args: &[&str]) -> Result<CliOutput> {
        let out = self.run(args).await?;
        if out.code == 0 {
            return Ok(out);
        }
        Err(Self::map_failure(args, &out))
    }

    fn map_failure(args: &[&str], out: &CliOutput) -> anyhow::Error {
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        let tail: String = combined
            .lines()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        let command = format!("skills {}", args.join(" "));

        if combined.contains("Invalid agents") {
            return anyhow!(format_skill_error(
                "UNSUPPORTED_AGENT",
                &[("command", &command), ("detail", &tail)],
                None,
            ));
        }
        // 注意：CLI 对“仓库不存在”会回退 git clone 并报 SSL/clone 错误，
        // 不能据字面判断是网络问题，统一按下载失败提示检查仓库地址。
        if combined.contains("Failed to clone") || combined.contains("Installation failed") {
            return anyhow!(format_skill_error(
                "DOWNLOAD_FAILED",
                &[("command", &command), ("detail", &tail)],
                Some("checkRepoUrl"),
            ));
        }
        anyhow!(format_skill_error(
            "CLI_FAILED",
            &[
                ("command", &command),
                ("code", &out.code.to_string()),
                ("detail", &tail),
            ],
            None,
        ))
    }
}

/// 依据存储位置设置在两个候选目录间选择 SSOT 目录（纯函数，便于测试）。
///
/// 优先返回配置指定的目录；仅当它不存在而另一目录存在时回退；两者皆无时
/// 返回配置指定目录（由调用方负责创建）。
fn resolve_ssot_dir(
    location: SkillStorageLocation,
    unified_dir: PathBuf,
    ochub_dir: PathBuf,
) -> PathBuf {
    let (preferred, alternate) = match location {
        SkillStorageLocation::Ochub => (ochub_dir, unified_dir),
        SkillStorageLocation::Unified => (unified_dir, ochub_dir),
    };
    if preferred.is_dir() {
        return preferred;
    }
    if alternate.is_dir() {
        return alternate;
    }
    preferred
}

// ========== SkillService ==========

pub struct SkillService;

impl Default for SkillService {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillService {
    pub fn new() -> Self {
        Self
    }

    /// 构建 Skill 文档 URL（指向仓库中的 SKILL.md 文件）
    fn build_skill_doc_url(owner: &str, repo: &str, branch: &str, doc_path: &str) -> String {
        format!("https://github.com/{owner}/{repo}/blob/{branch}/{doc_path}")
    }

    // ========== 路径管理 ==========

    /// 获取技能全局仓库目录（WebDAV/S3 同步会打包该目录）。
    ///
    /// 目录选择遵循持久化的 `SkillStorageLocation` 设置，避免同步快照在两个
    /// 存储位置间意外漂移（否则 OcHub 用户的远端 skills.zip 会被近乎空的
    /// 新目录覆盖）：
    /// - `SkillStorageLocation::Ochub`   → `~/.ochub/skills/`
    /// - `SkillStorageLocation::Unified`  → `~/.agents/skills/`
    ///
    /// 仅当配置目录不存在、而另一目录已存在时才回退到另一目录；两者皆无则
    /// 创建配置指定的目录。
    pub fn get_ssot_dir() -> Result<PathBuf> {
        let home = dirs::home_dir().context(format_skill_error(
            "GET_HOME_DIR_FAILED",
            &[],
            Some("checkPermission"),
        ))?;
        let unified_dir = home.join(".agents").join("skills");
        let ochub_dir = get_app_config_dir().join("skills");
        let location = crate::settings::get_settings().skill_storage_location;
        let dir = resolve_ssot_dir(location, unified_dir, ochub_dir);
        if !dir.is_dir() {
            fs::create_dir_all(&dir)?;
        }
        Ok(dir)
    }

    // ========== 登记表读取 ==========

    /// 获取所有已安装的 Skills
    pub fn get_all_installed(db: &Arc<Database>) -> Result<Vec<InstalledSkill>> {
        let skills = db.get_all_installed_skills()?;
        Ok(skills.into_values().collect())
    }

    // ========== 安装 / 卸载 / 切换 ==========

    /// 安装 Skill：`skills add <owner/repo> -g -y -s <name> -a <agent>`，
    /// 然后从 CLI 状态回填登记表并确保当前应用启用。
    pub async fn install(
        &self,
        db: &Arc<Database>,
        skill: &DiscoverableSkill,
        current_app: &AppType,
    ) -> Result<InstalledSkill> {
        let slug = agent_slug(current_app).ok_or_else(|| {
            anyhow!(format_skill_error(
                "UNSUPPORTED_AGENT",
                &[("app", current_app.as_str())],
                None,
            ))
        })?;

        let source = format!("{}/{}", skill.repo_owner, skill.repo_name);
        let skill_name = skill
            .directory
            .rsplit('/')
            .next()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&skill.name)
            .to_string();

        let cli = SkillsCli::resolve()?;
        cli.run_ok(&["add", &source, "-g", "-y", "-s", &skill_name, "-a", slug])
            .await?;

        Self::reconcile_registry(db, &cli).await?;

        // 保证登记表存在该 skill 且当前应用已启用
        let mut row = Self::find_by_directory(db, &skill_name)?.unwrap_or_else(|| InstalledSkill {
            id: skill.key.clone(),
            name: skill.name.clone(),
            description: if skill.description.is_empty() {
                None
            } else {
                Some(skill.description.clone())
            },
            directory: skill_name.clone(),
            repo_owner: Some(skill.repo_owner.clone()),
            repo_name: Some(skill.repo_name.clone()),
            repo_branch: Some(skill.repo_branch.clone()),
            readme_url: skill.readme_url.clone(),
            apps: SkillApps::default(),
            installed_at: chrono::Utc::now().timestamp(),
            content_hash: None,
            updated_at: 0,
        });
        if row.repo_owner.is_none() {
            row.repo_owner = Some(skill.repo_owner.clone());
            row.repo_name = Some(skill.repo_name.clone());
            row.repo_branch = Some(skill.repo_branch.clone());
        }
        row.apps.set_enabled_for(current_app, true);
        db.save_skill(&row)?;

        log::info!("Skill {} 安装成功，已启用 {:?}", row.name, current_app);
        Ok(row)
    }

    /// 卸载 Skill：`skills remove -g -y -s <name> -a <all agents>`。
    ///
    /// CLI 调用失败（如 npx 缺失或 skill 不受 CLI 管理）时降级为仅清理
    /// 全局仓库残留目录，登记表记录始终删除。
    pub async fn uninstall(db: &Arc<Database>, id: &str) -> Result<SkillUninstallResult> {
        let skill = db
            .get_installed_skill(id)?
            .ok_or_else(|| anyhow!("Skill not found: {id}"))?;

        match SkillsCli::resolve() {
            Ok(cli) => {
                match cli
                    .run(&[
                        "remove",
                        "-g",
                        "-y",
                        "-s",
                        &skill.directory,
                        "-a",
                        ALL_MANAGED_AGENT_SLUGS,
                    ])
                    .await
                {
                    Ok(out) if out.code == 0 => {}
                    Ok(out) => log::warn!(
                        "skills remove 退出码 {}，将继续清理本地记录: {}",
                        out.code,
                        out.stderr.trim()
                    ),
                    Err(e) => log::warn!("skills remove 执行失败，将继续清理本地记录: {e}"),
                }
            }
            Err(e) => log::warn!("未找到 skills CLI，将仅清理本地记录: {e}"),
        }

        // 清理全局仓库中的残留目录（CLI 未纳管的旧数据）
        if let Ok(ssot_dir) = Self::get_ssot_dir() {
            let leftover = ssot_dir.join(&skill.directory);
            if leftover.is_dir() {
                let _ = fs::remove_dir_all(&leftover);
            }
        }

        // 重写前安装的技能不在 CLI lock 中，`skills remove` 无法清除各应用技能目录下
        // 的符号链接/拷贝。逐个受管应用尽力清理，避免卸载后仍残留可用技能。
        for app in MANAGED_SKILL_APPS {
            if let Some(skills_dir) = get_app_skills_dir(app) {
                remove_skill_from_app_dir(&skills_dir, &skill.directory, app);
            }
        }

        db.delete_skill(id)?;
        log::info!("Skill {} 卸载成功", skill.name);
        Ok(SkillUninstallResult { backup_path: None })
    }

    /// 切换应用启用状态：启用走 `skills add -a <agent>`（需要已记录来源仓库），
    /// 禁用走 `skills remove -a <agent>`。
    pub async fn toggle_app(
        db: &Arc<Database>,
        id: &str,
        app: &AppType,
        enabled: bool,
    ) -> Result<()> {
        let mut skill = db
            .get_installed_skill(id)?
            .ok_or_else(|| anyhow!("Skill not found: {id}"))?;

        let slug = agent_slug(app).ok_or_else(|| {
            anyhow!(format_skill_error(
                "UNSUPPORTED_AGENT",
                &[("app", app.as_str())],
                None,
            ))
        })?;

        let cli = SkillsCli::resolve()?;
        if enabled {
            let source = match (&skill.repo_owner, &skill.repo_name) {
                (Some(owner), Some(name)) => format!("{owner}/{name}"),
                _ => {
                    return Err(anyhow!(format_skill_error(
                        "SKILL_SOURCE_UNKNOWN",
                        &[("skill", &skill.name)],
                        Some("uninstallFirst"),
                    )));
                }
            };
            cli.run_ok(&[
                "add",
                &source,
                "-g",
                "-y",
                "-s",
                &skill.directory,
                "-a",
                slug,
            ])
            .await?;
        } else {
            cli.run_ok(&["remove", "-g", "-y", "-s", &skill.directory, "-a", slug])
                .await?;
        }

        if let Err(e) = Self::reconcile_registry(db, &cli).await {
            log::warn!("回填技能登记表失败: {e}");
        }

        // 显式写入用户意图，覆盖回填得出的推断值
        skill.apps.set_enabled_for(app, enabled);
        db.update_skill_apps(id, &skill.apps)?;

        log::info!("Skill {} 的 {:?} 状态已更新为 {}", skill.name, app, enabled);
        Ok(())
    }

    // ========== 更新 ==========

    /// 内容哈希更新检测已废弃：恒返回空列表，前端以“全部更新”按钮代替。
    pub async fn check_updates(&self, _db: &Arc<Database>) -> Result<Vec<SkillUpdateInfo>> {
        Ok(Vec::new())
    }

    /// 更新全部已安装 Skills：`skills update -g -y`，随后回填登记表。
    pub async fn update_all(&self, db: &Arc<Database>) -> Result<()> {
        let cli = SkillsCli::resolve()?;
        cli.run_ok(&["update", "-g", "-y"]).await?;
        Self::reconcile_registry(db, &cli).await?;
        log::info!("Skills 已全部更新");
        Ok(())
    }

    /// 更新单个 Skill。CLI 仅提供全量更新，等价执行 `update -g -y`
    /// 后返回该 skill 的最新登记记录。
    pub async fn update_skill(&self, db: &Arc<Database>, skill_id: &str) -> Result<InstalledSkill> {
        let skill = db
            .get_installed_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill not found: {skill_id}"))?;

        self.update_all(db).await?;

        let refreshed = db.get_installed_skill(skill_id)?.unwrap_or(skill);
        log::info!("Skill {} 更新成功", refreshed.name);
        Ok(refreshed)
    }

    // ========== 登记表回填 ==========

    /// 以 CLI 状态（`list -g --json` + lock 文件）为准回填 SQLite 登记表。
    ///
    /// 仅做“新增/刷新”：CLI 已知的 skill 会被写入或更新（启用标志与既有
    /// 记录取并集，显式禁用由调用方在回填后落库）；CLI 不知道的旧记录保留。
    async fn reconcile_registry(db: &Arc<Database>, cli: &SkillsCli) -> Result<()> {
        let out = cli.run_ok(&["list", "-g", "--json"]).await?;
        let cli_skills = parse_list_json(&out.stdout)?;
        let lock = parse_agents_lock();
        let existing = db.get_all_installed_skills()?;

        // 将 lock 文件中发现的仓库补录到 skill_repos，供后续“发现”使用
        save_repos_from_lock(db, &lock, cli_skills.iter().map(|s| s.name.as_str()));

        let by_directory: HashMap<String, &InstalledSkill> = existing
            .values()
            .map(|s| (s.directory.to_lowercase(), s))
            .collect();

        for cli_skill in &cli_skills {
            let (id, repo_owner, repo_name, repo_branch, readme_url) =
                build_repo_info_from_lock(&lock, &cli_skill.name);
            let skill_md = Path::new(&cli_skill.path).join("SKILL.md");
            let (name, description) = Self::read_skill_name_desc(&skill_md, &cli_skill.name);
            let cli_apps = apps_from_agent_labels(&cli_skill.agents);

            let row = match by_directory.get(&cli_skill.name.to_lowercase()) {
                Some(existing_row) => {
                    let mut updated = (*existing_row).clone();
                    updated.name = name;
                    updated.description = description;
                    if updated.repo_owner.is_none() {
                        updated.repo_owner = repo_owner;
                        updated.repo_name = repo_name;
                        updated.repo_branch = repo_branch;
                        updated.readme_url = readme_url;
                    }
                    // 启用标志取并集：文件探测只能证实存在，不能证伪
                    let mut apps = updated.apps.clone();
                    for app in cli_apps.enabled_apps() {
                        apps.set_enabled_for(&app, true);
                    }
                    updated.apps = apps;
                    updated
                }
                None => InstalledSkill {
                    id,
                    name,
                    description,
                    directory: cli_skill.name.clone(),
                    repo_owner,
                    repo_name,
                    repo_branch,
                    readme_url,
                    apps: cli_apps,
                    installed_at: chrono::Utc::now().timestamp(),
                    content_hash: None,
                    updated_at: 0,
                },
            };
            db.save_skill(&row)?;
        }

        Ok(())
    }

    fn find_by_directory(db: &Arc<Database>, directory: &str) -> Result<Option<InstalledSkill>> {
        let skills = db.get_all_installed_skills()?;
        Ok(skills
            .into_values()
            .find(|s| s.directory.eq_ignore_ascii_case(directory)))
    }

    // ========== 发现功能 ==========

    /// 列出所有可发现的技能：对每个启用仓库执行 `skills add <owner/repo> -l` 并解析。
    pub async fn discover_available(
        &self,
        repos: Vec<SkillRepo>,
    ) -> Result<Vec<DiscoverableSkill>> {
        let cli = SkillsCli::resolve()?;
        let mut skills = Vec::new();

        for repo in repos.into_iter().filter(|repo| repo.enabled) {
            match Self::fetch_repo_skills(&cli, &repo).await {
                Ok(repo_skills) => skills.extend(repo_skills),
                Err(e) => log::warn!("获取仓库 {}/{} 技能失败: {}", repo.owner, repo.name, e),
            }
        }

        Self::deduplicate_discoverable_skills(&mut skills);
        skills.sort_by_key(|skill| skill.name.to_lowercase());
        Ok(skills)
    }

    async fn fetch_repo_skills(
        cli: &SkillsCli,
        repo: &SkillRepo,
    ) -> Result<Vec<DiscoverableSkill>> {
        let source = format!("{}/{}", repo.owner, repo.name);
        let out = cli.run_ok(&["add", &source, "-l"]).await?;
        let parsed = parse_add_list_output(&out.stdout);

        Ok(parsed
            .into_iter()
            .map(|(name, description)| DiscoverableSkill {
                key: format!("{}/{}:{name}", repo.owner, repo.name),
                description,
                directory: name.clone(),
                readme_url: None,
                repo_owner: repo.owner.clone(),
                repo_name: repo.name.clone(),
                repo_branch: repo.branch.clone(),
                name,
            })
            .collect())
    }

    /// 列出所有技能（兼容旧 API）：可发现技能 + 本地已安装标记
    pub async fn list_skills(
        &self,
        repos: Vec<SkillRepo>,
        db: &Arc<Database>,
    ) -> Result<Vec<Skill>> {
        let discoverable = self.discover_available(repos).await?;

        let installed = db.get_all_installed_skills()?;
        let installed_dirs: HashSet<String> =
            installed.values().map(|s| s.directory.clone()).collect();

        let mut skills: Vec<Skill> = discoverable
            .into_iter()
            .map(|d| {
                let install_name = Path::new(&d.directory)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| d.directory.clone());

                Skill {
                    key: d.key,
                    name: d.name,
                    description: d.description,
                    directory: d.directory,
                    readme_url: d.readme_url,
                    installed: installed_dirs.contains(&install_name),
                    repo_owner: Some(d.repo_owner),
                    repo_name: Some(d.repo_name),
                    repo_branch: Some(d.repo_branch),
                }
            })
            .collect();

        // 添加本地已安装但不在仓库中的技能
        for skill in installed.values() {
            let already_in_list = skills.iter().any(|s| {
                let s_install_name = Path::new(&s.directory)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| s.directory.clone());
                s_install_name == skill.directory
            });

            if !already_in_list {
                skills.push(Skill {
                    key: skill.id.clone(),
                    name: skill.name.clone(),
                    description: skill.description.clone().unwrap_or_default(),
                    directory: skill.directory.clone(),
                    readme_url: skill.readme_url.clone(),
                    installed: true,
                    repo_owner: skill.repo_owner.clone(),
                    repo_name: skill.repo_name.clone(),
                    repo_branch: skill.repo_branch.clone(),
                });
            }
        }

        skills.sort_by_key(|skill| skill.name.to_lowercase());
        Ok(skills)
    }

    /// 解析技能元数据（SKILL.md YAML front-matter）
    fn parse_skill_metadata_static(path: &Path) -> Result<SkillMetadata> {
        let content = fs::read_to_string(path)?;
        let content = content.trim_start_matches('\u{feff}');

        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() < 3 {
            return Ok(SkillMetadata {
                name: None,
                description: None,
            });
        }

        let front_matter = parts[1].trim();
        let meta: SkillMetadata = serde_yaml::from_str(front_matter).unwrap_or(SkillMetadata {
            name: None,
            description: None,
        });

        Ok(meta)
    }

    /// 从 SKILL.md 读取名称和描述，不存在则用目录名兜底
    fn read_skill_name_desc(skill_md: &Path, fallback_name: &str) -> (String, Option<String>) {
        if skill_md.exists() {
            match Self::parse_skill_metadata_static(skill_md) {
                Ok(meta) => (
                    meta.name.unwrap_or_else(|| fallback_name.to_string()),
                    meta.description,
                ),
                Err(_) => (fallback_name.to_string(), None),
            }
        } else {
            (fallback_name.to_string(), None)
        }
    }

    /// 去重技能列表（基于完整 key，不同仓库的同名 skill 分开显示）
    fn deduplicate_discoverable_skills(skills: &mut Vec<DiscoverableSkill>) {
        let mut seen = HashSet::new();
        skills.retain(|skill| seen.insert(skill.key.to_lowercase()));
    }

    // ========== skills.sh 搜索 ==========

    /// 搜索 skills.sh 公共目录
    pub async fn search_skills_sh(
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SkillsShSearchResult> {
        let client = crate::http_client::get();
        let url = url::Url::parse_with_params(
            "https://skills.sh/api/search",
            &[
                ("q", query),
                ("limit", &limit.to_string()),
                ("offset", &offset.to_string()),
            ],
        )?;

        let resp = client
            .get(url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()?
            .json::<SkillsShApiResponse>()
            .await?;

        let skills = resp
            .skills
            .into_iter()
            .filter_map(|s| {
                let parts: Vec<&str> = s.source.splitn(2, '/').collect();
                if parts.len() != 2 {
                    return None;
                }
                let (owner, repo) = (parts[0].to_string(), parts[1].to_string());
                if owner.contains('.') || repo.contains('.') {
                    return None;
                }
                Some(SkillsShDiscoverableSkill {
                    key: s.id,
                    name: s.name,
                    directory: s.skill_id.clone(),
                    repo_owner: owner.clone(),
                    repo_name: repo.clone(),
                    repo_branch: "main".to_string(),
                    installs: s.installs,
                    readme_url: Some(format!("https://github.com/{}/{}", owner, repo)),
                })
            })
            .collect();

        Ok(SkillsShSearchResult {
            skills,
            total_count: resp.count,
            query: resp.query,
        })
    }
}

// ========== lock 文件 -> 登记表辅助 ==========

/// 从 lock 文件信息构建 skill 的 ID、仓库字段和 readme URL
///
/// 返回 (id, repo_owner, repo_name, repo_branch, readme_url)
fn build_repo_info_from_lock(
    lock: &HashMap<String, LockRepoInfo>,
    dir_name: &str,
) -> (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    match lock.get(dir_name) {
        Some(info) => {
            let branch = info.branch.clone();
            let url_branch = branch.clone().unwrap_or_else(|| "HEAD".to_string());
            // 优先使用 lock 文件中的 skillPath，否则回退到 dir_name/SKILL.md
            let fallback = format!("{dir_name}/SKILL.md");
            let doc_path = info.skill_path.as_deref().unwrap_or(&fallback);
            let url = Some(SkillService::build_skill_doc_url(
                &info.owner,
                &info.repo,
                &url_branch,
                doc_path,
            ));
            (
                format!("{}/{}:{dir_name}", info.owner, info.repo),
                Some(info.owner.clone()),
                Some(info.repo.clone()),
                branch,
                url,
            )
        }
        None => (format!("local:{dir_name}"), None, None, None, None),
    }
}

/// 将 lock 文件中发现的仓库保存到 skill_repos（去重）
fn save_repos_from_lock(
    db: &Arc<Database>,
    lock: &HashMap<String, LockRepoInfo>,
    directories: impl Iterator<Item = impl AsRef<str>>,
) {
    let existing_repos: HashSet<(String, String)> = db
        .get_skill_repos()
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.owner, r.name))
        .collect();
    let mut added = HashSet::new();

    for dir_name in directories {
        if let Some(info) = lock.get(dir_name.as_ref()) {
            let key = (info.owner.clone(), info.repo.clone());
            if !existing_repos.contains(&key) && added.insert(key) {
                let skill_repo = SkillRepo {
                    owner: info.owner.clone(),
                    name: info.repo.clone(),
                    // 未知分支时使用 HEAD 语义，后续安装由 CLI 自行解析默认分支。
                    branch: info.branch.clone().unwrap_or_else(|| "HEAD".to_string()),
                    enabled: true,
                };
                if let Err(e) = db.save_skill_repo(&skill_repo) {
                    log::warn!("保存 skill 仓库 {}/{} 失败: {}", info.owner, info.repo, e);
                } else {
                    log::info!(
                        "从 agents lock 文件发现并添加仓库: {}/{} ({})",
                        info.owner,
                        info.repo,
                        skill_repo.branch
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `npx -y skills list -g`（空仓库）实测输出，含 ANSI dim-gray 着色
    const LIST_EMPTY_SAMPLE: &str = "\x1b[38;5;102mNo global skills found.\x1b[0m\n\x1b[38;5;102mTry listing project skills without -g\x1b[0m\n";

    // `npx -y skills list -g --json` 实测输出
    const LIST_JSON_SAMPLE: &str = r#"[
  {
    "name": "deploy-to-vercel",
    "path": "/tmp/tmp.S2XweRCxKG/.agents/skills/deploy-to-vercel",
    "scope": "global",
    "agents": ["Claude Code"]
  }
]"#;

    // `~/.agents/.skill-lock.json` 实测样本（version 3）
    const LOCK_SAMPLE: &str = r#"{
  "version": 3,
  "skills": {
    "deploy-to-vercel": {
      "source": "vercel-labs/agent-skills",
      "sourceType": "github",
      "sourceUrl": "https://github.com/vercel-labs/agent-skills.git",
      "skillPath": "skills/deploy-to-vercel/SKILL.md",
      "skillFolderHash": "1378aa506439f26c809dbcfc61515cbd70f93d69",
      "installedAt": "2026-07-04T17:14:09.392Z",
      "updatedAt": "2026-07-04T17:14:20.204Z"
    }
  },
  "dismissed": {}
}"#;

    // `npx -y skills add vercel-labs/agent-skills -l` 输出节选：
    // 含 agent 横幅、spinner 帧（◒ + \x1b[999D\x1b[J 擦除序列）与 Clack 框线
    fn add_list_sample() -> String {
        [
            "\x1b[?25l\u{25d2} Fetching skills\x1b[999D\x1b[J\u{25d0} Fetching skills\x1b[999D\x1b[J\x1b[?25h",
            "\u{25cf}   claude-code_2-1-201_agent  Agent detected \u{2014} installing non-interactively",
            "\u{2502}",
            "\u{25c7}  Source: https://github.com/vercel-labs/agent-skills.git",
            "\u{25c7}  Found 9 skills",
            "\u{2502}",
            "\u{25c7}  \x1b[1mAvailable Skills\x1b[22m",
            "\u{2502}",
            "\u{2502}    \x1b[36mvercel-composition-patterns\x1b[39m",
            "\u{2502}",
            "\u{2502}      \x1b[2mReact composition patterns for scalable component design.\x1b[22m",
            "\u{2502}      \x1b[2mUse when refactoring components.\x1b[22m",
            "\u{2502}",
            "\u{2502}    \x1b[36mdeploy-to-vercel\x1b[39m",
            "\u{2502}",
            "\u{2502}      \x1b[2mDeploy applications and websites to Vercel.\x1b[22m",
            "\u{2502}",
            "\u{2502}    \x1b[36mweb-design-guidelines\x1b[39m",
            "\u{2502}",
            "\u{2502}      \x1b[2mReview interfaces for accessibility issues.\x1b[22m",
            "\u{2514}  Use --skill <name> to install specific skills",
            "",
        ]
        .join("\n")
    }

    #[test]
    fn strip_ansi_cleans_colored_list_output() {
        let clean = strip_ansi(LIST_EMPTY_SAMPLE);
        assert_eq!(
            clean,
            "No global skills found.\nTry listing project skills without -g\n"
        );
    }

    #[test]
    fn strip_ansi_removes_cursor_and_erase_sequences() {
        let clean = strip_ansi("\x1b[?25lspin\x1b[999D\x1b[Jdone\x1b[?25h\r\n");
        assert_eq!(clean, "spindone\n\n");
    }

    #[test]
    fn parse_list_json_reads_installed_skills() {
        let skills = parse_list_json(LIST_JSON_SAMPLE).expect("parse list json");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "deploy-to-vercel");
        assert_eq!(
            skills[0].path,
            "/tmp/tmp.S2XweRCxKG/.agents/skills/deploy-to-vercel"
        );
        assert_eq!(skills[0].agents, vec!["Claude Code".to_string()]);
    }

    #[test]
    fn parse_list_json_tolerates_banner_noise() {
        let noisy = format!("Agent detected \u{2014} whatever\n{LIST_JSON_SAMPLE}\n");
        let skills = parse_list_json(&noisy).expect("parse noisy list json");
        assert_eq!(skills.len(), 1);
    }

    #[test]
    fn parse_list_json_empty_array() {
        let skills = parse_list_json("[]").expect("parse empty");
        assert!(skills.is_empty());
    }

    #[test]
    fn parse_add_list_output_extracts_names_and_descriptions() {
        let parsed = parse_add_list_output(&add_list_sample());
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].0, "vercel-composition-patterns");
        assert_eq!(
            parsed[0].1,
            "React composition patterns for scalable component design. Use when refactoring components."
        );
        assert_eq!(parsed[1].0, "deploy-to-vercel");
        assert_eq!(parsed[1].1, "Deploy applications and websites to Vercel.");
        assert_eq!(parsed[2].0, "web-design-guidelines");
    }

    #[test]
    fn parse_add_list_output_without_marker_returns_empty() {
        let parsed = parse_add_list_output("\u{25a0}  Failed to clone repository\n");
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_agents_lock_content_reads_github_sources() {
        let lock = parse_agents_lock_content(LOCK_SAMPLE);
        let info = lock.get("deploy-to-vercel").expect("lock entry");
        assert_eq!(info.owner, "vercel-labs");
        assert_eq!(info.repo, "agent-skills");
        assert_eq!(
            info.skill_path.as_deref(),
            Some("skills/deploy-to-vercel/SKILL.md")
        );
        assert_eq!(info.branch, None);
    }

    #[test]
    fn build_repo_info_from_lock_builds_id_and_doc_url() {
        let lock = parse_agents_lock_content(LOCK_SAMPLE);
        let (id, owner, repo, branch, readme) =
            build_repo_info_from_lock(&lock, "deploy-to-vercel");
        assert_eq!(id, "vercel-labs/agent-skills:deploy-to-vercel");
        assert_eq!(owner.as_deref(), Some("vercel-labs"));
        assert_eq!(repo.as_deref(), Some("agent-skills"));
        assert_eq!(branch, None);
        assert_eq!(
            readme.as_deref(),
            Some(
                "https://github.com/vercel-labs/agent-skills/blob/HEAD/skills/deploy-to-vercel/SKILL.md"
            )
        );
    }

    #[test]
    fn apps_from_agent_labels_maps_display_names() {
        let apps = apps_from_agent_labels(&["Claude Code".to_string()]);
        assert!(apps.claude);
        assert!(!apps.codex && !apps.opencode && !apps.hermes);

        let apps = apps_from_agent_labels(&[
            "Codex".to_string(),
            "OpenCode".to_string(),
            "hermes-agent".to_string(),
            "Some Unknown Agent".to_string(),
        ]);
        assert!(apps.codex && apps.opencode && apps.hermes);
        assert!(!apps.claude);
    }

    #[test]
    fn agent_slug_covers_cli_supported_apps() {
        assert_eq!(agent_slug(&AppType::Claude), Some("claude-code"));
        assert_eq!(agent_slug(&AppType::Codex), Some("codex"));
        assert_eq!(agent_slug(&AppType::OpenCode), Some("opencode"));
        assert_eq!(agent_slug(&AppType::Hermes), Some("hermes-agent"));
        // OpenClaw 无 SkillApps 字段，映射为 None → 路由以 UNSUPPORTED_AGENT 拒绝
        assert_eq!(agent_slug(&AppType::OpenClaw), None);
        assert_eq!(agent_slug(&AppType::ClaudeDesktop), None);
    }

    #[test]
    fn all_managed_agent_slugs_excludes_openclaw() {
        assert!(!ALL_MANAGED_AGENT_SLUGS.contains("openclaw"));
        // 其余受管 agent 仍在列
        for slug in ["claude-code", "codex", "opencode", "hermes-agent"] {
            assert!(
                ALL_MANAGED_AGENT_SLUGS.contains(slug),
                "missing slug: {slug}"
            );
        }
    }

    #[test]
    fn managed_skill_apps_excludes_openclaw_and_desktop() {
        assert!(!MANAGED_SKILL_APPS.contains(&AppType::OpenClaw));
        assert!(!MANAGED_SKILL_APPS.contains(&AppType::ClaudeDesktop));
        // 每个受管应用都能解析出技能目录
        for app in MANAGED_SKILL_APPS {
            assert!(
                get_app_skills_dir(app).is_some(),
                "no skills dir for {app:?}"
            );
        }
        assert!(get_app_skills_dir(&AppType::OpenClaw).is_none());
        assert!(get_app_skills_dir(&AppType::ClaudeDesktop).is_none());
    }

    #[test]
    fn resolve_ssot_dir_honors_configured_location_when_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let unified = tmp.path().join("agents-skills");
        let ochub = tmp.path().join("ochub-skills");
        fs::create_dir_all(&unified).unwrap();
        fs::create_dir_all(&ochub).unwrap();

        // 两目录都存在时，各自遵循配置，不发生漂移
        assert_eq!(
            resolve_ssot_dir(SkillStorageLocation::Ochub, unified.clone(), ochub.clone()),
            ochub
        );
        assert_eq!(
            resolve_ssot_dir(
                SkillStorageLocation::Unified,
                unified.clone(),
                ochub.clone()
            ),
            unified
        );
    }

    #[test]
    fn resolve_ssot_dir_falls_back_only_when_configured_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let unified = tmp.path().join("agents-skills");
        let ochub = tmp.path().join("ochub-skills");
        // 仅 unified 存在
        fs::create_dir_all(&unified).unwrap();

        // 配置为 OcHub 但其目录缺失，另一目录存在 → 回退到 unified
        assert_eq!(
            resolve_ssot_dir(SkillStorageLocation::Ochub, unified.clone(), ochub.clone()),
            unified
        );
        // 配置为 Unified 且其目录存在 → 保持 unified
        assert_eq!(
            resolve_ssot_dir(
                SkillStorageLocation::Unified,
                unified.clone(),
                ochub.clone()
            ),
            unified
        );
    }

    #[test]
    fn resolve_ssot_dir_returns_configured_when_neither_exists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let unified = tmp.path().join("agents-skills");
        let ochub = tmp.path().join("ochub-skills");
        // 两者都不存在 → 返回配置目录（调用方负责创建）
        assert_eq!(
            resolve_ssot_dir(SkillStorageLocation::Ochub, unified.clone(), ochub.clone()),
            ochub
        );
        assert_eq!(
            resolve_ssot_dir(SkillStorageLocation::Unified, unified.clone(), ochub),
            unified
        );
    }

    #[test]
    fn remove_skill_from_app_dir_removes_directory_and_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills_dir = tmp.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();

        // 真实目录形式的技能
        let dir_skill = skills_dir.join("copied-skill");
        fs::create_dir_all(&dir_skill).unwrap();
        fs::write(dir_skill.join("SKILL.md"), "x").unwrap();
        assert!(dir_skill.exists());
        remove_skill_from_app_dir(&skills_dir, "copied-skill", &AppType::Claude);
        assert!(!dir_skill.exists(), "directory skill should be removed");

        // 符号链接形式的技能：删除链接但不动目标
        #[cfg(unix)]
        {
            let target = tmp.path().join("real-skill-src");
            fs::create_dir_all(&target).unwrap();
            let link = skills_dir.join("linked-skill");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(fs::symlink_metadata(&link).is_ok());
            remove_skill_from_app_dir(&skills_dir, "linked-skill", &AppType::Claude);
            assert!(
                fs::symlink_metadata(&link).is_err(),
                "symlink should be removed"
            );
            assert!(target.exists(), "symlink target must be preserved");
        }
    }

    #[test]
    fn remove_skill_from_app_dir_rejects_traversal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills_dir = tmp.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        // 在 skills_dir 之外的兄弟目录，绝不能被穿越删除
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "x").unwrap();

        remove_skill_from_app_dir(&skills_dir, "../outside", &AppType::Claude);
        assert!(outside.exists(), "traversal must not delete outside dir");
        assert!(outside.join("keep.txt").exists());

        // 绝对路径同样被拒绝
        remove_skill_from_app_dir(&skills_dir, outside.to_str().unwrap(), &AppType::Claude);
        assert!(outside.exists());
    }

    #[test]
    fn remove_skill_from_app_dir_missing_is_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills_dir = tmp.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        // 不存在的技能名不应 panic 或报错
        remove_skill_from_app_dir(&skills_dir, "nope", &AppType::Codex);
    }
}

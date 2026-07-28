use crate::application::{Application, ApplicationError, ApplicationResult};
use crate::db::{InstalledSkill, SkillRepo};
use crate::services::skill::{
    DiscoverableSkill, Skill, SkillService, SkillUninstallResult, SkillUpdateInfo,
    SkillsShSearchResult,
};
use crate::{AppId, AppType};

impl Application {
    pub fn list_installed_skills(&self) -> ApplicationResult<Vec<InstalledSkill>> {
        SkillService::get_all_installed(&self.state.db).map_err(ApplicationError::from_skill_error)
    }

    pub fn get_installed_skill(&self, id: &str) -> ApplicationResult<InstalledSkill> {
        self.state
            .db
            .get_installed_skill(id)?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "skill",
                id: id.to_string(),
            })
    }

    pub async fn search_skills(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> ApplicationResult<SkillsShSearchResult> {
        if query.trim().is_empty() {
            return Err(ApplicationError::InvalidInput(
                "skill search query cannot be empty".to_string(),
            ));
        }
        SkillService::search_skills_sh(query, limit.clamp(1, 100), offset)
            .await
            .map_err(ApplicationError::from_skill_error)
    }

    pub async fn discover_skills(
        &self,
        repo: Option<SkillRepo>,
    ) -> ApplicationResult<Vec<DiscoverableSkill>> {
        let repos = match repo {
            Some(repo) => vec![repo],
            None => self.state.db.get_skill_repos()?,
        };
        SkillService::new()
            .discover_available(repos)
            .await
            .map_err(ApplicationError::from_skill_error)
    }

    pub async fn skill_catalog(&self, repo: Option<SkillRepo>) -> ApplicationResult<Vec<Skill>> {
        let repos = match repo {
            Some(repo) => vec![repo],
            None => self.state.db.get_skill_repos()?,
        };
        SkillService::new()
            .list_skills(repos, &self.state.db)
            .await
            .map_err(ApplicationError::from_skill_error)
    }

    pub async fn install_skill(
        &self,
        skill: &DiscoverableSkill,
        app: &AppId,
    ) -> ApplicationResult<InstalledSkill> {
        let app_type = self.require_builtin_skill_app(app)?;
        SkillService::new()
            .install(&self.state.db, skill, &app_type)
            .await
            .map_err(ApplicationError::from_skill_error)
    }

    pub async fn uninstall_skill(&self, id: &str) -> ApplicationResult<SkillUninstallResult> {
        self.get_installed_skill(id)?;
        SkillService::uninstall(&self.state.db, id)
            .await
            .map_err(ApplicationError::from_skill_error)
    }

    pub async fn set_skill_app_enabled(
        &self,
        id: &str,
        app: &AppId,
        enabled: bool,
    ) -> ApplicationResult<InstalledSkill> {
        self.get_installed_skill(id)?;
        let app_type = self.require_builtin_skill_app(app)?;
        SkillService::toggle_app(&self.state.db, id, &app_type, enabled)
            .await
            .map_err(ApplicationError::from_skill_error)?;
        self.get_installed_skill(id)
    }

    pub async fn check_skill_updates(&self) -> ApplicationResult<Vec<SkillUpdateInfo>> {
        SkillService::new()
            .check_updates(&self.state.db)
            .await
            .map_err(ApplicationError::from_skill_error)
    }

    pub async fn update_skill(&self, id: &str) -> ApplicationResult<InstalledSkill> {
        self.get_installed_skill(id)?;
        SkillService::new()
            .update_skill(&self.state.db, id)
            .await
            .map_err(ApplicationError::from_skill_error)
    }

    pub async fn update_all_skills(&self) -> ApplicationResult<Vec<InstalledSkill>> {
        SkillService::new()
            .update_all(&self.state.db)
            .await
            .map_err(ApplicationError::from_skill_error)?;
        self.list_installed_skills()
    }

    pub fn list_skill_repos(&self) -> ApplicationResult<Vec<SkillRepo>> {
        Ok(self.state.db.get_skill_repos()?)
    }

    pub fn get_skill_repo(&self, owner: &str, name: &str) -> ApplicationResult<SkillRepo> {
        self.list_skill_repos()?
            .into_iter()
            .find(|repo| repo.owner == owner && repo.name == name)
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "skill-repo",
                id: format!("{owner}/{name}"),
            })
    }

    pub fn save_skill_repo(&self, repo: SkillRepo) -> ApplicationResult<SkillRepo> {
        validate_repo_part("owner", &repo.owner)?;
        validate_repo_part("name", &repo.name)?;
        if repo.branch.trim().is_empty() {
            return Err(ApplicationError::InvalidInput(
                "skill repository branch cannot be empty".to_string(),
            ));
        }
        self.state.db.save_skill_repo(&repo)?;
        Ok(repo)
    }

    pub fn delete_skill_repo(&self, owner: &str, name: &str) -> ApplicationResult<()> {
        self.get_skill_repo(owner, name)?;
        self.state.db.delete_skill_repo(owner, name)?;
        Ok(())
    }

    fn require_builtin_skill_app(&self, app: &AppId) -> ApplicationResult<AppType> {
        let summary = self.get_app(app)?;
        if !summary.supports_skills {
            return Err(ApplicationError::CapabilityUnsupported {
                app: app.to_string(),
                capability: "skills",
            });
        }
        AppType::from_app_id(app).ok_or_else(|| ApplicationError::CapabilityUnsupported {
            app: app.to_string(),
            capability: "skills.live-sync",
        })
    }
}

fn validate_repo_part(label: &str, value: &str) -> ApplicationResult<()> {
    let valid = !value.trim().is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-._".contains(character));
    if valid {
        Ok(())
    } else {
        Err(ApplicationError::InvalidInput(format!(
            "invalid skill repository {label}: {value}"
        )))
    }
}

/// Parse `owner/repo`, a GitHub repository URL, or either form with an
/// explicit branch (`owner/repo@branch` or `/tree/<branch>`).
pub fn parse_skill_repo_spec(raw: &str) -> ApplicationResult<SkillRepo> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ApplicationError::InvalidInput(
            "skill repository cannot be empty".to_string(),
        ));
    }

    let (owner, repo, branch) = if raw.starts_with("http://") || raw.starts_with("https://") {
        let url = url::Url::parse(raw)
            .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
        if !matches!(url.host_str(), Some("github.com" | "www.github.com")) {
            return Err(ApplicationError::InvalidInput(
                "skill repository URL must use github.com".to_string(),
            ));
        }
        let segments = url
            .path_segments()
            .map(|segments| {
                segments
                    .filter(|segment| !segment.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if segments.len() < 2 {
            return Err(ApplicationError::InvalidInput(
                "GitHub repository URL must include owner and repository".to_string(),
            ));
        }
        let branch = if segments.get(2) == Some(&"tree") {
            segments.get(3).copied().unwrap_or("main")
        } else {
            "main"
        };
        (
            segments[0].to_string(),
            segments[1].trim_end_matches(".git").to_string(),
            branch.to_string(),
        )
    } else {
        let (repo_spec, branch) = raw.rsplit_once('@').unwrap_or((raw, "main"));
        let (owner, repo) = repo_spec.split_once('/').ok_or_else(|| {
            ApplicationError::InvalidInput(
                "skill repository must be owner/name or a GitHub URL".to_string(),
            )
        })?;
        (
            owner.to_string(),
            repo.trim_end_matches(".git").to_string(),
            branch.to_string(),
        )
    };

    validate_repo_part("owner", &owner)?;
    validate_repo_part("name", &repo)?;
    if branch.trim().is_empty() || branch.contains("..") || branch.starts_with('/') {
        return Err(ApplicationError::InvalidInput(format!(
            "invalid skill repository branch: {branch}"
        )));
    }
    Ok(SkillRepo {
        owner,
        name: repo,
        branch,
        enabled: true,
    })
}

/// Parse a concrete install source. Supported forms:
/// `owner/repo:skill`, a GitHub `/tree/<branch>/<path>` URL, and
/// `https://skills.sh/owner/repo/skill`.
pub fn parse_skill_source(raw: &str) -> ApplicationResult<DiscoverableSkill> {
    let raw = raw.trim();
    let (owner, repo, branch, directory) =
        if raw.starts_with("http://") || raw.starts_with("https://") {
            let url = url::Url::parse(raw)
                .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?;
            let segments = url
                .path_segments()
                .map(|segments| {
                    segments
                        .filter(|segment| !segment.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            match url.host_str() {
                Some("github.com" | "www.github.com") if segments.len() >= 5 => {
                    if segments[2] != "tree" {
                        return Err(ApplicationError::InvalidInput(
                            "GitHub skill URL must contain /tree/<branch>/<path>".to_string(),
                        ));
                    }
                    (
                        segments[0].to_string(),
                        segments[1].trim_end_matches(".git").to_string(),
                        segments[3].to_string(),
                        segments[4..].join("/"),
                    )
                }
                Some("skills.sh" | "www.skills.sh") if segments.len() >= 3 => (
                    segments[0].to_string(),
                    segments[1].to_string(),
                    "main".to_string(),
                    segments[2..].join("/"),
                ),
                _ => {
                    return Err(ApplicationError::InvalidInput(
                        "unsupported skill URL; use skills.sh or a GitHub tree URL".to_string(),
                    ));
                }
            }
        } else {
            let (repo_spec, directory) = raw.rsplit_once(':').ok_or_else(|| {
                ApplicationError::InvalidInput(
                    "skill source must be owner/repo:skill or a supported URL".to_string(),
                )
            })?;
            let repo = parse_skill_repo_spec(repo_spec)?;
            (repo.owner, repo.name, repo.branch, directory.to_string())
        };

    validate_repo_part("owner", &owner)?;
    validate_repo_part("name", &repo)?;
    let path = std::path::Path::new(&directory);
    if directory.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ApplicationError::InvalidInput(format!(
            "invalid skill directory: {directory}"
        )));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ApplicationError::InvalidInput("skill directory is not valid UTF-8".to_string())
        })?
        .to_string();
    Ok(DiscoverableSkill {
        key: format!("{owner}/{repo}:{directory}"),
        name,
        description: String::new(),
        directory,
        readme_url: Some(format!("https://github.com/{owner}/{repo}")),
        repo_owner: owner,
        repo_name: repo,
        repo_branch: branch,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_skill_repo_spec, parse_skill_source};

    #[test]
    fn parses_skill_sources_and_rejects_traversal() {
        let source = parse_skill_source("openai/skills:skills/docs").unwrap();
        assert_eq!(source.name, "docs");
        assert_eq!(source.repo_owner, "openai");
        assert!(parse_skill_source("openai/skills:../secret").is_err());

        let repo = parse_skill_repo_spec("https://github.com/openai/skills/tree/release").unwrap();
        assert_eq!(repo.owner, "openai");
        assert_eq!(repo.name, "skills");
        assert_eq!(repo.branch, "release");
    }
}

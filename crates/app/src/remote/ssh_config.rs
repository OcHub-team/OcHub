use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::RemoteClientError;
use super::store::validate_ssh_alias;

const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_INCLUDE_DEPTH: usize = 8;
const MAX_INCLUDED_FILES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshConfigEntry {
    pub alias: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: u16,
    pub identity_file: Option<String>,
    pub source: PathBuf,
}

#[derive(Debug, Clone)]
struct HostRule {
    patterns: Vec<String>,
    values: HashMap<String, String>,
}

/// Discover concrete `Host` aliases from the user's OpenSSH configuration.
///
/// Includes are expanded in-place, wildcard blocks are applied as defaults,
/// and only aliases accepted by the remote command allowlist are returned.
pub(crate) fn discover_ssh_connections() -> Result<Vec<SshConfigEntry>, RemoteClientError> {
    let home = ochub_core::paths::get_home_dir();
    discover_ssh_connections_at(&home.join(".ssh").join("config"), &home)
}

fn discover_ssh_connections_at(
    root: &Path,
    home: &Path,
) -> Result<Vec<SshConfigEntry>, RemoteClientError> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut parser = Parser {
        home,
        rules: Vec::new(),
        aliases: Vec::new(),
        alias_sources: HashMap::new(),
        visited: HashSet::new(),
    };
    parser.parse_file(root, 0)?;

    let mut seen = HashSet::new();
    let mut entries = parser
        .aliases
        .into_iter()
        .filter(|alias| seen.insert(alias.clone()))
        .filter_map(|alias| {
            if validate_ssh_alias(&alias).is_err() {
                return None;
            }
            let mut resolved = HashMap::<String, String>::new();
            for rule in &parser.rules {
                if host_rule_matches(&rule.patterns, &alias) {
                    for (key, value) in &rule.values {
                        resolved.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
            let hostname = resolved
                .get("hostname")
                .cloned()
                .unwrap_or_else(|| alias.clone());
            if validate_ssh_alias(&hostname).is_err() {
                return None;
            }
            let port = resolved
                .get("port")
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|port| *port > 0)
                .unwrap_or(22);
            Some(SshConfigEntry {
                source: parser
                    .alias_sources
                    .get(&alias)
                    .cloned()
                    .unwrap_or_else(|| root.to_path_buf()),
                alias,
                hostname,
                user: resolved.get("user").cloned(),
                port,
                identity_file: resolved.get("identityfile").cloned(),
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.alias
            .to_lowercase()
            .cmp(&right.alias.to_lowercase())
            .then_with(|| left.alias.cmp(&right.alias))
    });
    Ok(entries)
}

struct Parser<'a> {
    home: &'a Path,
    rules: Vec<HostRule>,
    aliases: Vec<String>,
    alias_sources: HashMap<String, PathBuf>,
    visited: HashSet<PathBuf>,
}

impl Parser<'_> {
    fn parse_file(&mut self, path: &Path, depth: usize) -> Result<(), RemoteClientError> {
        if depth > MAX_INCLUDE_DEPTH || self.visited.len() >= MAX_INCLUDED_FILES {
            return Ok(());
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !self.visited.insert(canonical.clone()) {
            return Ok(());
        }
        let metadata = fs::metadata(&canonical).map_err(|source| RemoteClientError::File {
            path: canonical.clone(),
            source,
        })?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return Ok(());
        }
        let content = fs::read_to_string(&canonical).map_err(|source| RemoteClientError::File {
            path: canonical.clone(),
            source,
        })?;
        let mut active: Option<HostRule> = None;

        for raw_line in content.lines() {
            let words = ssh_words(raw_line);
            let Some((key, values)) = words.split_first() else {
                continue;
            };
            let key = key.to_ascii_lowercase();
            match key.as_str() {
                "include" => {
                    if let Some(rule) = active.take() {
                        self.rules.push(rule);
                    }
                    for pattern in values {
                        for included in expand_include(pattern, self.home) {
                            self.parse_file(&included, depth + 1)?;
                        }
                    }
                }
                "host" => {
                    if let Some(rule) = active.take() {
                        self.rules.push(rule);
                    }
                    let patterns = values.to_vec();
                    for alias in patterns.iter().filter(|pattern| is_concrete_alias(pattern)) {
                        self.aliases.push(alias.clone());
                        self.alias_sources
                            .entry(alias.clone())
                            .or_insert_with(|| canonical.clone());
                    }
                    active = Some(HostRule {
                        patterns,
                        values: HashMap::new(),
                    });
                }
                "match" => {
                    if let Some(rule) = active.take() {
                        self.rules.push(rule);
                    }
                }
                _ => {
                    if let (Some(rule), Some(value)) = (active.as_mut(), values.first())
                        && matches!(key.as_str(), "hostname" | "user" | "port" | "identityfile")
                    {
                        rule.values
                            .entry(key)
                            .or_insert_with(|| expand_home(value, self.home));
                    }
                }
            }
        }
        if let Some(rule) = active {
            self.rules.push(rule);
        }
        Ok(())
    }
}

fn expand_include(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let expanded = expand_home(pattern, home);
    let path = PathBuf::from(&expanded);
    let pattern = if path.is_absolute() {
        path
    } else {
        home.join(".ssh").join(path)
    };
    let Some(pattern) = pattern.to_str() else {
        return Vec::new();
    };
    let Ok(paths) = glob::glob(pattern) else {
        return Vec::new();
    };
    let mut paths = paths.filter_map(Result::ok).collect::<Vec<_>>();
    paths.sort();
    paths
}

fn expand_home(value: &str, home: &Path) -> String {
    if value == "~" {
        home.to_string_lossy().into_owned()
    } else if let Some(suffix) = value.strip_prefix("~/") {
        home.join(suffix).to_string_lossy().into_owned()
    } else {
        value.to_string()
    }
}

fn is_concrete_alias(pattern: &str) -> bool {
    !pattern.starts_with('!')
        && !pattern.contains('*')
        && !pattern.contains('?')
        && !pattern.trim().is_empty()
}

fn host_rule_matches(patterns: &[String], alias: &str) -> bool {
    let mut matched = false;
    for pattern in patterns {
        if let Some(negated) = pattern.strip_prefix('!') {
            if wildcard_matches(negated, alias) {
                return false;
            }
        } else if wildcard_matches(pattern, alias) {
            matched = true;
        }
    }
    matched
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star, mut checkpoint) = (None, 0);
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            checkpoint = value_index;
            pattern_index += 1;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            checkpoint += 1;
            value_index = checkpoint;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn ssh_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '#' => break,
            '=' | ' ' | '\t' => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_includes_and_applies_wildcard_defaults() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        let config_dir = ssh.join("config.d");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            ssh.join("config"),
            "Include config.d/*\nHost *\n  User deploy\n  Port 2200\n",
        )
        .unwrap();
        fs::write(
            config_dir.join("servers"),
            "Host beta\n  HostName 10.0.0.2\nHost alpha\n  HostName alpha.example\n  User root\n  IdentityFile ~/.ssh/id_alpha\n",
        )
        .unwrap();

        let entries = discover_ssh_connections_at(&ssh.join("config"), home.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].alias, "alpha");
        assert_eq!(entries[0].hostname, "alpha.example");
        assert_eq!(entries[0].user.as_deref(), Some("root"));
        assert_eq!(entries[0].port, 2200);
        assert_eq!(
            entries[0].identity_file.as_deref(),
            Some(home.path().join(".ssh/id_alpha").to_string_lossy().as_ref())
        );
        assert_eq!(entries[1].alias, "beta");
        assert_eq!(entries[1].user.as_deref(), Some("deploy"));
    }

    #[test]
    fn lexer_handles_quotes_equals_and_comments() {
        assert_eq!(
            ssh_words(r#"HostName="host name" # comment"#),
            vec!["HostName", "host name"]
        );
        assert_eq!(
            ssh_words(r#"IdentityFile ~/.ssh/a\ key"#),
            vec!["IdentityFile", "~/.ssh/a key"]
        );
    }

    #[test]
    fn wildcard_matching_honors_negation() {
        assert!(host_rule_matches(
            &["*".to_string(), "!prod-*".to_string()],
            "staging-1"
        ));
        assert!(!host_rule_matches(
            &["*".to_string(), "!prod-*".to_string()],
            "prod-1"
        ));
    }
}

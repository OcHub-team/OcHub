//! Row identity, and the search index built out of it.
//!
//! Every root row is named once, here, by a [`RowId`] whose [`RowEntry`] holds
//! the element id, the group/label/description keys and the extra search terms.
//! `root.rs` reads all four from [`entry`] and never writes an id literal, so a
//! new row cannot be rendered without an entry — the exhaustive match below
//! will not compile until one exists. That is what makes search coverage a
//! compiler guarantee rather than a table someone has to remember to update.

use gpui::{div, prelude::*, AnyElement, Context, SharedString};

use crate::components;
use crate::i18n::{k, raw, t, Key};
use crate::icons::IconName;
use crate::tf;
use crate::theme;

use super::SettingsView;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RowId {
    Language,
    Terminal,
    StartupLogin,
    StartupHidden,
    WindowKeepRunning,
    WindowTray,
    AppsOpen,
    DataDir,
    DataDirReset,
    BackupInterval,
    BackupRetain,
    SyncTarget,
    SyncAuto,
    SyncOpen,
    AboutUpdate,
    AboutRelease,
}

pub(super) struct RowEntry {
    /// Element id. Not prose: it keys gpui's focus handle and must stay stable.
    pub id: &'static str,
    pub group: Key,
    pub label: Key,
    pub desc: Key,
    /// Terms a user might search for that the visible copy does not contain.
    pub keywords: &'static [&'static str],
}

pub(super) fn entry(row: RowId) -> &'static RowEntry {
    match row {
        RowId::Language => &RowEntry {
            id: "general-language",
            group: k::SETTINGS_GENERAL_TITLE,
            label: k::SETTINGS_BASIC_LANGUAGE_LABEL,
            desc: k::SETTINGS_BASIC_LANGUAGE_DESC,
            keywords: &["language", "locale", "语言", "言語", "english", "日本語"],
        },
        RowId::Terminal => &RowEntry {
            id: "general-terminal",
            group: k::SETTINGS_GENERAL_TITLE,
            label: k::SETTINGS_GENERAL_TERMINAL_LABEL,
            desc: k::SETTINGS_GENERAL_TERMINAL_DESC,
            keywords: &[
                "terminal",
                "iterm",
                "ghostty",
                "wezterm",
                "kitty",
                "warp",
                "powershell",
                "终端",
                "ターミナル",
            ],
        },
        RowId::StartupLogin => &RowEntry {
            id: "startup-login",
            group: k::SETTINGS_STARTUP_TITLE,
            label: k::SETTINGS_BASIC_LAUNCH_STARTUP_LABEL,
            desc: k::SETTINGS_BASIC_LAUNCH_STARTUP_DESC,
            keywords: &["startup", "login", "autostart", "开机", "自启", "ログイン"],
        },
        RowId::StartupHidden => &RowEntry {
            id: "startup-hidden",
            group: k::SETTINGS_STARTUP_TITLE,
            label: k::SETTINGS_BASIC_SILENT_STARTUP_LABEL,
            desc: k::SETTINGS_BASIC_SILENT_STARTUP_DESC,
            keywords: &["silent", "hidden", "静默", "隐藏", "非表示"],
        },
        RowId::WindowKeepRunning => &RowEntry {
            id: "window-keep-running",
            group: k::SETTINGS_STARTUP_TITLE,
            label: k::SETTINGS_BASIC_MINIMIZE_LABEL,
            desc: k::SETTINGS_BASIC_MINIMIZE_DESC,
            keywords: &["close", "quit", "background", "关闭", "后台", "終了"],
        },
        RowId::WindowTray => &RowEntry {
            id: "window-tray",
            group: k::SETTINGS_STARTUP_TITLE,
            label: k::SETTINGS_BASIC_TRAY_LABEL,
            desc: k::SETTINGS_BASIC_TRAY_DESC,
            keywords: &["tray", "menu bar", "菜单", "托盘", "メニュー"],
        },
        RowId::AppsOpen => &RowEntry {
            id: "apps-open",
            group: k::SETTINGS_APPS_TITLE,
            label: k::SETTINGS_APPS_OPEN_LABEL,
            desc: k::SETTINGS_APPS_OPEN_DESC,
            keywords: &[
                "claude",
                "codex",
                "gemini",
                "plugin",
                "manifest",
                "插件",
                "应用",
                "アプリ",
            ],
        },
        RowId::DataDir => &RowEntry {
            id: "data-dir",
            group: k::SETTINGS_DATA_TITLE,
            label: k::SETTINGS_DATA_DIR_LABEL,
            desc: k::SETTINGS_DATA_DIR_DESC,
            keywords: &[
                "path",
                "folder",
                "directory",
                "database",
                "目录",
                "路径",
                "フォルダー",
            ],
        },
        RowId::DataDirReset => &RowEntry {
            id: "data-dir-reset",
            group: k::SETTINGS_DATA_TITLE,
            label: k::SETTINGS_DATA_DIR_RESET_LABEL,
            desc: k::SETTINGS_DATA_DIR_RESET_DESC,
            keywords: &["reset", "default", "恢复", "默认", "既定"],
        },
        RowId::BackupInterval => &RowEntry {
            id: "backup-interval",
            group: k::SETTINGS_DATA_TITLE,
            label: k::SETTINGS_BACKUP_INTERVAL_LABEL,
            desc: k::SETTINGS_BACKUP_INTERVAL_DESC,
            keywords: &["backup", "hours", "备份", "间隔", "バックアップ"],
        },
        RowId::BackupRetain => &RowEntry {
            id: "backup-retain",
            group: k::SETTINGS_DATA_TITLE,
            label: k::SETTINGS_BACKUP_RETAIN_LABEL,
            desc: k::SETTINGS_BACKUP_RETAIN_DESC,
            keywords: &["backup", "retain", "备份", "保留", "バックアップ"],
        },
        RowId::SyncTarget => &RowEntry {
            id: "sync-target",
            group: k::SETTINGS_SYNC_TITLE,
            label: k::SETTINGS_SYNC_TARGET_LABEL,
            desc: k::SETTINGS_SYNC_TARGET_DESC,
            keywords: &["webdav", "s3", "sync", "remote", "同步", "同期"],
        },
        RowId::SyncAuto => &RowEntry {
            id: "sync-auto",
            group: k::SETTINGS_SYNC_TITLE,
            label: k::SETTINGS_SYNC_AUTO_LABEL,
            desc: k::SETTINGS_SYNC_AUTO_DESC,
            keywords: &["auto", "sync", "自动", "自動"],
        },
        RowId::SyncOpen => &RowEntry {
            id: "sync-open",
            group: k::SETTINGS_SYNC_TITLE,
            label: k::SETTINGS_SYNC_OPEN_LABEL,
            desc: k::SETTINGS_SYNC_OPEN_DESC,
            keywords: &[
                "webdav", "s3", "bucket", "region", "endpoint", "password", "backup", "密码",
                "备份", "同期",
            ],
        },
        RowId::AboutUpdate => &RowEntry {
            id: "about-update",
            group: k::SETTINGS_ABOUT_TITLE,
            label: k::SETTINGS_ABOUT_UPDATE_LABEL,
            desc: k::SETTINGS_ABOUT_UPDATE_DESC,
            keywords: &["update", "version", "更新", "版本", "バージョン"],
        },
        RowId::AboutRelease => &RowEntry {
            id: "about-release",
            group: k::SETTINGS_ABOUT_TITLE,
            label: k::SETTINGS_ABOUT_RELEASE_LABEL,
            desc: k::SETTINGS_ABOUT_RELEASE_DESC,
            keywords: &["release", "github", "download", "发布", "下载", "リリース"],
        },
    }
}

/// Case-insensitive substring match over the label, the description and the
/// keywords, ranked label-prefix > label-substring > description > keyword.
pub(super) fn matches(query: &str, rows: &[RowId]) -> Vec<RowId> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u8, usize, RowId)> = Vec::new();
    for (position, row) in rows.iter().enumerate() {
        let entry = entry(*row);
        let label = raw(entry.label).to_lowercase();
        let rank = if label.starts_with(&needle) {
            Some(0)
        } else if label.contains(&needle) {
            Some(1)
        } else if raw(entry.desc).to_lowercase().contains(&needle) {
            Some(2)
        } else if entry
            .keywords
            .iter()
            .any(|keyword| keyword.to_lowercase().contains(&needle))
        {
            Some(3)
        } else {
            None
        };
        if let Some(rank) = rank {
            hits.push((rank, position, *row));
        }
    }
    hits.sort_by_key(|(rank, position, _)| (*rank, *position));
    hits.into_iter().map(|(_, _, row)| row).collect()
}

impl SettingsView {
    /// The search result block: every hit renders the **real** row, fully
    /// operable in place, under a muted caption naming the group it came from.
    pub(super) fn render_search_results(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hits = matches(&self.query, &self.visible_rows());
        if hits.is_empty() {
            return div()
                .w_full()
                .pb_3()
                .child(components::empty_state(
                    IconName::Search,
                    t(k::SETTINGS_SEARCH_EMPTY_TITLE),
                    t(k::SETTINGS_SEARCH_EMPTY_HINT),
                    None,
                ))
                .into_any_element();
        }
        let rows: Vec<AnyElement> = hits
            .into_iter()
            .map(|row| {
                let group = t(entry(row).group);
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .child(
                        div()
                            .px_4()
                            .pt_2()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(SharedString::from(tf!(
                                k::SETTINGS_SEARCH_GROUP_CAPTION,
                                group = group
                            ))),
                    )
                    .child(self.render_row(row, cx))
                    .into_any_element()
            })
            .collect();
        super::rows::group_block(t(k::SETTINGS_SEARCH_TITLE), t(k::SETTINGS_PAGE_DESC), rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const ALL: &[RowId] = &[
        RowId::Language,
        RowId::Terminal,
        RowId::StartupLogin,
        RowId::StartupHidden,
        RowId::WindowKeepRunning,
        RowId::WindowTray,
        RowId::AppsOpen,
        RowId::DataDir,
        RowId::DataDirReset,
        RowId::BackupInterval,
        RowId::BackupRetain,
        RowId::SyncTarget,
        RowId::SyncAuto,
        RowId::SyncOpen,
        RowId::AboutUpdate,
        RowId::AboutRelease,
    ];

    #[test]
    fn every_row_id_is_unique() {
        let ids: BTreeSet<&str> = ALL.iter().map(|row| entry(*row).id).collect();
        assert_eq!(ids.len(), ALL.len(), "row element ids must be unique");
    }

    #[test]
    fn every_row_resolves_its_copy() {
        for row in ALL {
            let entry = entry(*row);
            assert!(!raw(entry.group).is_empty(), "{} group", entry.id);
            assert!(!raw(entry.label).is_empty(), "{} label", entry.id);
            assert!(!raw(entry.desc).is_empty(), "{} desc", entry.id);
        }
    }

    #[test]
    fn keywords_reach_rows_whose_copy_omits_the_term() {
        // "s3" appears in no label or description, only in the keywords of the
        // two sync rows that lead to it.
        let hits = matches("s3", ALL);
        assert!(hits.contains(&RowId::SyncOpen));
        assert!(hits.contains(&RowId::SyncTarget));
    }

    #[test]
    fn a_blank_query_matches_nothing() {
        assert!(matches("   ", ALL).is_empty());
    }
}

//! Pure data and pure functions: the preset option tables, and the two
//! formatters the pages share. No view state lives here.

use chrono::{Local, TimeZone};
use gpui::SharedString;
use ochub_core::i18n::Locale;

use crate::components;
use crate::i18n::{k, raw, t, Key};
use crate::tf;

/// Which platforms can launch a given terminal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Os {
    Mac,
    Windows,
    Linux,
}

#[cfg(target_os = "macos")]
const CURRENT_OS: Os = Os::Mac;
#[cfg(target_os = "windows")]
const CURRENT_OS: Os = Os::Windows;
#[cfg(all(unix, not(target_os = "macos")))]
const CURRENT_OS: Os = Os::Linux;

/// One terminal the session launcher knows how to drive.
///
/// `id` is what lands in `settings.preferredTerminal`, and it must match the
/// launcher's match arms exactly — `session_manager::provider_terminal` falls
/// through to the platform default on anything it does not recognise, silently.
/// That is why this is a preset list and no longer a free-text field: the old
/// placeholder suggested typing "iTerm", which never matched `"iterm2"`.
pub(super) struct TerminalOption {
    pub id: &'static str,
    pub label: Key,
    /// Where this terminal can actually be launched. Filtering one table at
    /// runtime rather than `#[cfg]`-ing three keeps every label referenced on
    /// every platform, so a name that falls out of the catalog is a build
    /// failure everywhere rather than only on the machine that ships it.
    platforms: &'static [Os],
}

const MAC: &[Os] = &[Os::Mac];
const WINDOWS: &[Os] = &[Os::Windows];
const LINUX: &[Os] = &[Os::Linux];
const MAC_LINUX: &[Os] = &[Os::Mac, Os::Linux];

/// Ordered so that filtering by platform yields a sensible list on each: the
/// platform-native terminals first, then the cross-platform ones.
const TERMINALS: &[TerminalOption] = &[
    TerminalOption {
        id: "terminal",
        label: k::SETTINGS_GENERAL_TERMINAL_TERMINAL,
        platforms: MAC,
    },
    TerminalOption {
        id: "iterm2",
        label: k::SETTINGS_GENERAL_TERMINAL_ITERM2,
        platforms: MAC,
    },
    TerminalOption {
        id: "warp",
        label: k::SETTINGS_GENERAL_TERMINAL_WARP,
        platforms: MAC,
    },
    TerminalOption {
        id: "cmd",
        label: k::SETTINGS_GENERAL_TERMINAL_CMD,
        platforms: WINDOWS,
    },
    TerminalOption {
        id: "powershell",
        label: k::SETTINGS_GENERAL_TERMINAL_POWERSHELL,
        platforms: WINDOWS,
    },
    TerminalOption {
        id: "wt",
        label: k::SETTINGS_GENERAL_TERMINAL_WT,
        platforms: WINDOWS,
    },
    TerminalOption {
        id: "gnome-terminal",
        label: k::SETTINGS_GENERAL_TERMINAL_GNOME,
        platforms: LINUX,
    },
    TerminalOption {
        id: "konsole",
        label: k::SETTINGS_GENERAL_TERMINAL_KONSOLE,
        platforms: LINUX,
    },
    TerminalOption {
        id: "xfce4-terminal",
        label: k::SETTINGS_GENERAL_TERMINAL_XFCE4,
        platforms: LINUX,
    },
    TerminalOption {
        id: "mate-terminal",
        label: k::SETTINGS_GENERAL_TERMINAL_MATE,
        platforms: LINUX,
    },
    TerminalOption {
        id: "lxterminal",
        label: k::SETTINGS_GENERAL_TERMINAL_LXTERMINAL,
        platforms: LINUX,
    },
    TerminalOption {
        id: "alacritty",
        label: k::SETTINGS_GENERAL_TERMINAL_ALACRITTY,
        platforms: MAC_LINUX,
    },
    TerminalOption {
        id: "kitty",
        label: k::SETTINGS_GENERAL_TERMINAL_KITTY,
        platforms: MAC_LINUX,
    },
    TerminalOption {
        id: "ghostty",
        label: k::SETTINGS_GENERAL_TERMINAL_GHOSTTY,
        platforms: MAC_LINUX,
    },
    TerminalOption {
        id: "wezterm",
        label: k::SETTINGS_GENERAL_TERMINAL_WEZTERM,
        platforms: MAC,
    },
    TerminalOption {
        id: "kaku",
        label: k::SETTINGS_GENERAL_TERMINAL_KAKU,
        platforms: MAC,
    },
];

/// The terminals this build can actually launch.
fn available_terminals() -> impl Iterator<Item = &'static TerminalOption> {
    TERMINALS
        .iter()
        .filter(|option| option.platforms.contains(&CURRENT_OS))
}

pub(super) const BACKUP_INTERVALS: &[u32] = &[6, 12, 24, 48];
pub(super) const BACKUP_RETAINS: &[u32] = &[5, 10, 20, 50];

/// The interval used when nothing is stored, so the select has something to
/// point at without the page having to write a value on first paint.
pub(super) const DEFAULT_BACKUP_INTERVAL: u32 = 24;
pub(super) const DEFAULT_BACKUP_RETAIN: u32 = 10;

/// `None` = follow the OS, then one entry per shipped locale.
pub(super) fn language_choices() -> Vec<Option<Locale>> {
    std::iter::once(None).chain(Locale::ALL.map(Some)).collect()
}

/// Index of `stored` among the presets, **appending it** when it is not one of
/// them.
///
/// This is what keeps a hand-edited `settings.json` intact: a
/// `backupIntervalHours: 72`, or a Linux terminal the preset list has never
/// heard of, shows up as an extra selected option instead of being silently
/// rewritten to the nearest preset the first time the page is opened.
pub(super) fn select_index<T: PartialEq>(
    values: &mut Vec<T>,
    labels: &mut Vec<String>,
    stored: T,
    label_of: impl FnOnce(&T) -> String,
) -> usize {
    if let Some(index) = values.iter().position(|value| *value == stored) {
        return index;
    }
    labels.push(label_of(&stored));
    values.push(stored);
    values.len() - 1
}

/// The terminal select: "自动" plus one entry per known terminal, plus the
/// stored value when it is none of them.
pub(super) fn terminal_choices(
    stored: Option<String>,
) -> (Vec<Option<String>>, Vec<String>, usize) {
    let mut values: Vec<Option<String>> = vec![None];
    let mut labels: Vec<String> = vec![raw(k::SETTINGS_GENERAL_TERMINAL_AUTO).to_string()];
    for option in available_terminals() {
        values.push(Some(option.id.to_string()));
        labels.push(raw(option.label).to_string());
    }
    let selected = select_index(&mut values, &mut labels, stored, |value| {
        value.clone().unwrap_or_default()
    });
    (values, labels, selected)
}

pub(super) fn backup_interval_choices(stored: Option<u32>) -> (Vec<u32>, Vec<String>, usize) {
    let mut values = BACKUP_INTERVALS.to_vec();
    let mut labels: Vec<String> = values
        .iter()
        .map(|hours| tf!(k::SETTINGS_BACKUP_INTERVAL_OPTION, hours = hours))
        .collect();
    let selected = select_index(
        &mut values,
        &mut labels,
        stored.unwrap_or(DEFAULT_BACKUP_INTERVAL),
        |hours| tf!(k::SETTINGS_BACKUP_INTERVAL_OPTION, hours = hours),
    );
    (values, labels, selected)
}

pub(super) fn backup_retain_choices(stored: Option<u32>) -> (Vec<u32>, Vec<String>, usize) {
    let mut values = BACKUP_RETAINS.to_vec();
    let mut labels: Vec<String> = values
        .iter()
        .map(|count| tf!(k::SETTINGS_BACKUP_RETAIN_OPTION, count = count))
        .collect();
    let selected = select_index(
        &mut values,
        &mut labels,
        stored.unwrap_or(DEFAULT_BACKUP_RETAIN),
        |count| tf!(k::SETTINGS_BACKUP_RETAIN_OPTION, count = count),
    );
    (values, labels, selected)
}

/// A last-sync time a person can read: "刚刚" under a minute, "{n} 分钟前"
/// within the hour, a clock time later today, a date and time before that.
///
/// The page used to print `last_sync_at` as a bare Unix integer, which is a
/// machine value pretending to be a status line.
pub(super) fn format_last_sync(timestamp: i64) -> SharedString {
    let Some(time) = Local.timestamp_opt(timestamp, 0).single() else {
        return SharedString::from(components::format_local_timestamp(timestamp, false));
    };
    let now = Local::now();
    let elapsed = now.signed_duration_since(time);
    // A clock skew (a remote timestamp slightly ahead of us) reads as "just
    // now" rather than as a negative age.
    if elapsed.num_seconds() < 60 {
        return t(k::SETTINGS_SYNC_STATUS_JUST_NOW);
    }
    if elapsed.num_minutes() < 60 {
        return SharedString::from(tf!(
            k::SETTINGS_SYNC_STATUS_MINUTES_AGO,
            minutes = elapsed.num_minutes()
        ));
    }
    if time.date_naive() == now.date_naive() {
        return SharedString::from(time.format("%H:%M").to_string());
    }
    SharedString::from(time.format("%m-%d %H:%M").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_value_outside_the_presets_is_appended_and_selected() {
        let mut values = BACKUP_INTERVALS.to_vec();
        let mut labels: Vec<String> = values.iter().map(|hours| hours.to_string()).collect();
        let selected = select_index(&mut values, &mut labels, 72, |hours| hours.to_string());
        assert_eq!(selected, BACKUP_INTERVALS.len());
        assert_eq!(values.last(), Some(&72));
        assert_eq!(labels.last().map(String::as_str), Some("72"));
    }

    #[test]
    fn a_stored_preset_selects_in_place() {
        let mut values = BACKUP_RETAINS.to_vec();
        let mut labels: Vec<String> = values.iter().map(|count| count.to_string()).collect();
        let selected = select_index(&mut values, &mut labels, 10, |count| count.to_string());
        assert_eq!(values.len(), BACKUP_RETAINS.len(), "nothing was appended");
        assert_eq!(values[selected], 10);
    }

    #[test]
    fn an_unset_interval_points_at_the_default() {
        let (values, _, selected) = backup_interval_choices(None);
        assert_eq!(values[selected], DEFAULT_BACKUP_INTERVAL);
    }

    #[test]
    fn the_terminal_list_always_opens_with_auto() {
        let (values, _, selected) = terminal_choices(None);
        assert_eq!(values[selected], None);
        assert_eq!(selected, 0);
    }

    #[test]
    fn the_platform_filter_yields_a_non_empty_list_with_no_duplicates() {
        let ids: Vec<&str> = available_terminals().map(|option| option.id).collect();
        assert!(!ids.is_empty());
        let unique: std::collections::BTreeSet<&&str> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn a_hand_edited_terminal_name_survives_being_displayed() {
        let (values, labels, selected) = terminal_choices(Some("foot".to_string()));
        assert_eq!(values[selected].as_deref(), Some("foot"));
        assert_eq!(labels[selected], "foot");
    }
}

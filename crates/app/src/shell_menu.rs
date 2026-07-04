//! Native shell menus for macOS menu bar and Dock/Windows taskbar menus.
//!
//! GPUI exposes native application menus and dock/taskbar context menus, but not
//! a Tauri-style status item/tray icon. This module ports the useful cc-switch
//! tray command surface onto the native menu APIs that are available here.

use std::sync::Arc;

use gpui::{actions, App, Menu, MenuItem, SharedString, SystemMenuType};
use routedeck_core::services::provider::ProviderService;
use routedeck_core::services::subscription::{
    SubscriptionQuota, TIER_FIVE_HOUR, TIER_GEMINI_FLASH, TIER_GEMINI_FLASH_LITE, TIER_GEMINI_PRO,
    TIER_SEVEN_DAY, TIER_SEVEN_DAY_OPUS, TIER_SEVEN_DAY_SONNET, TIER_WEEKLY_LIMIT,
};
use routedeck_core::{settings, AppState, AppType, Provider, UsageResult};

use crate::app_ui::notify_open_roots;
use crate::notifications::NotificationLevel;

actions!(routedeck, [OpenMainWindow, QuitApp, RefreshShellMenus]);

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = routedeck, no_json)]
pub struct SwitchProviderFromMenu {
    app: String,
    provider_id: String,
}

pub fn install(app: Arc<AppState>, cx: &mut App) {
    let switch_app = app.clone();
    cx.on_action(move |action: &SwitchProviderFromMenu, cx| {
        switch_provider_from_menu(switch_app.clone(), action, cx);
    });

    let refresh_app = app.clone();
    cx.on_action(move |_: &RefreshShellMenus, cx| {
        apply_shell_menus(&refresh_app, cx);
    });

    cx.on_action(|_: &OpenMainWindow, cx| {
        activate_first_window(cx);
    });
    cx.on_action(|_: &QuitApp, cx| {
        cx.quit();
    });

    apply_shell_menus(&app, cx);
}

pub fn refresh(app: &Arc<AppState>, cx: &mut App) {
    apply_shell_menus(app, cx);
}

fn apply_shell_menus(app: &Arc<AppState>, cx: &mut App) {
    let settings = settings::get_settings();
    let quick_switch_enabled = settings.show_in_tray;
    let mut menus = vec![Menu::new("RouteDeck").items([
        MenuItem::action("打开主窗口", OpenMainWindow),
        MenuItem::separator(),
        MenuItem::os_submenu("服务", SystemMenuType::Services),
        MenuItem::separator(),
        MenuItem::action("退出 RouteDeck", QuitApp),
    ])];

    if quick_switch_enabled {
        menus.push(Menu::new("供应商").items(provider_submenus(app)));
    }
    cx.set_menus(menus);

    let dock_items = if cfg!(target_os = "windows") {
        windows_taskbar_items(app, quick_switch_enabled)
    } else {
        let mut dock_items = vec![
            MenuItem::action("打开主窗口", OpenMainWindow),
            MenuItem::separator(),
        ];
        if quick_switch_enabled {
            dock_items.extend(provider_submenus(app));
            dock_items.push(MenuItem::separator());
        }
        dock_items.push(MenuItem::action("退出 RouteDeck", QuitApp));
        dock_items
    };
    cx.set_dock_menu(dock_items);
}

fn provider_submenus(app: &Arc<AppState>) -> Vec<MenuItem> {
    let visible = settings::get_settings().visible_apps.unwrap_or_default();

    AppType::all()
        .filter(|app_type| visible.is_visible(app_type))
        .map(|app_type| provider_submenu(app, app_type))
        .collect()
}

fn provider_submenu(app: &Arc<AppState>, app_type: AppType) -> MenuItem {
    let providers = match ProviderService::list(app, app_type) {
        Ok(providers) => providers,
        Err(err) => {
            return MenuItem::submenu(Menu::new(app_label(app_type)).items([
                MenuItem::action(format!("加载失败: {err}"), RefreshShellMenus).disabled(true),
            ]));
        }
    };

    if providers.is_empty() {
        return MenuItem::submenu(
            Menu::new(app_label(app_type)).items([MenuItem::action(
                "没有供应商",
                RefreshShellMenus,
            )
            .disabled(true)]),
        );
    }

    let current = ProviderService::current(app, app_type).unwrap_or_default();
    let mut providers = providers.into_iter().collect::<Vec<_>>();
    providers.sort_by(|(left_id, left), (right_id, right)| {
        provider_sort_key(left_id, left).cmp(&provider_sort_key(right_id, right))
    });

    let label = provider_submenu_label(app, app_type, &providers, &current);

    MenuItem::submenu(
        Menu::new(label).items(providers.into_iter().map(|(id, provider)| {
            let checked = provider_is_selected(app_type, &current, &provider);
            MenuItem::action(
                provider_item_label(app_type, &provider),
                SwitchProviderFromMenu {
                    app: app_type.as_str().to_string(),
                    provider_id: id,
                },
            )
            .checked(checked)
        })),
    )
}

fn windows_taskbar_items(app: &Arc<AppState>, quick_switch_enabled: bool) -> Vec<MenuItem> {
    let mut items = vec![MenuItem::action("打开主窗口", OpenMainWindow)];
    if quick_switch_enabled {
        items.extend(windows_provider_items(app));
    }
    items.push(MenuItem::action("退出 RouteDeck", QuitApp));
    items
}

fn windows_provider_items(app: &Arc<AppState>) -> Vec<MenuItem> {
    let visible = settings::get_settings().visible_apps.unwrap_or_default();
    let mut items = Vec::new();

    for app_type in AppType::all().filter(|app_type| visible.is_visible(app_type)) {
        let Ok(providers) = ProviderService::list(app, app_type) else {
            continue;
        };
        let current = ProviderService::current(app, app_type).unwrap_or_default();
        let mut providers = providers.into_iter().collect::<Vec<_>>();
        providers.sort_by(|(left_id, left), (right_id, right)| {
            provider_sort_key(left_id, left).cmp(&provider_sort_key(right_id, right))
        });

        for (id, provider) in providers {
            let status = if provider_is_selected(app_type, &current, &provider) {
                if app_type.is_additive_mode() {
                    "已添加"
                } else {
                    "当前"
                }
            } else if app_type.is_additive_mode() {
                "添加"
            } else {
                "切换"
            };
            items.push(MenuItem::action(
                format!("{}: {} ({status})", app_label(app_type), provider.name),
                SwitchProviderFromMenu {
                    app: app_type.as_str().to_string(),
                    provider_id: id,
                },
            ));
        }
    }

    items
}

fn provider_submenu_label(
    app: &Arc<AppState>,
    app_type: AppType,
    providers: &[(String, Provider)],
    current: &str,
) -> String {
    if app_type.is_additive_mode() {
        let managed_count = providers
            .iter()
            .filter(|(_, provider)| provider_live_config_managed(app_type, provider))
            .count();
        return format!("{} · 已添加 {}", app_label(app_type), managed_count);
    }

    providers
        .iter()
        .find(|(id, _)| id == current)
        .map(|(_, provider)| {
            let suffix = usage_suffix(app, app_type, provider, current).unwrap_or_default();
            format!("{} · {}{}", app_label(app_type), provider.name, suffix)
        })
        .unwrap_or_else(|| app_label(app_type).to_string())
}

fn provider_item_label(app_type: AppType, provider: &Provider) -> String {
    if !app_type.is_additive_mode() {
        return provider.name.clone();
    }
    if provider_live_config_managed(app_type, provider) {
        format!("{} · 已添加", provider.name)
    } else {
        format!("{} · 添加到工具配置", provider.name)
    }
}

fn provider_is_selected(app_type: AppType, current: &str, provider: &Provider) -> bool {
    if app_type.is_additive_mode() {
        provider_live_config_managed(app_type, provider)
    } else {
        provider.id == current
    }
}

fn provider_live_config_managed(app_type: AppType, provider: &Provider) -> bool {
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.live_config_managed)
        .unwrap_or(!app_type.is_additive_mode())
}

fn provider_sort_key(id: &str, provider: &Provider) -> (usize, i64, String, String) {
    (
        provider.sort_index.unwrap_or(usize::MAX),
        provider.created_at.unwrap_or(i64::MAX),
        provider.name.to_ascii_lowercase(),
        id.to_string(),
    )
}

fn switch_provider_from_menu(app: Arc<AppState>, action: &SwitchProviderFromMenu, cx: &mut App) {
    let app_type = match action.app.parse::<AppType>() {
        Ok(app_type) => app_type,
        Err(err) => {
            report_to_roots(
                cx,
                None,
                NotificationLevel::Error,
                "菜单操作失败".to_string(),
                Some(format!("无法识别应用类型 {}: {err}", action.app)),
            );
            return;
        }
    };

    let provider = ProviderService::list(&app, app_type)
        .ok()
        .and_then(|providers| providers.get(&action.provider_id).cloned());
    let provider_name = provider
        .as_ref()
        .map(|provider| provider.name.clone())
        .unwrap_or_else(|| action.provider_id.clone());
    let removing_from_additive = provider
        .as_ref()
        .map(|provider| {
            app_type.is_additive_mode() && provider_live_config_managed(app_type, provider)
        })
        .unwrap_or(false);

    let result = if removing_from_additive {
        ProviderService::remove_from_live_config(&app, app_type, &action.provider_id)
            .map(|_| Default::default())
    } else {
        ProviderService::switch(&app, app_type, &action.provider_id)
    };
    apply_shell_menus(&app, cx);

    match result {
        Ok(result) if result.warnings.is_empty() => {
            let title = if removing_from_additive {
                format!("{} 已从工具配置移除", provider_name)
            } else if app_type.is_additive_mode() {
                format!("{} 已添加到工具配置", provider_name)
            } else {
                format!("{} 已切换到 {}", app_label(app_type), provider_name)
            };
            report_to_roots(cx, Some(app_type), NotificationLevel::Success, title, None);
        }
        Ok(result) => report_to_roots(
            cx,
            Some(app_type),
            NotificationLevel::Warning,
            format!("{} 已切换", app_label(app_type)),
            Some(format!(
                "应用工具配置时返回 {} 个警告",
                result.warnings.len()
            )),
        ),
        Err(err) => report_to_roots(
            cx,
            Some(app_type),
            NotificationLevel::Error,
            format!("切换 {} 供应商失败", app_label(app_type)),
            Some(err.to_string()),
        ),
    }
}

const TEMPLATE_TYPE_OFFICIAL_SUBSCRIPTION: &str = "official_subscription";
const H_TIER_NAMES: &[&str] = &[TIER_FIVE_HOUR];
const W_TIER_NAMES: &[&str] = &[
    TIER_WEEKLY_LIMIT,
    TIER_SEVEN_DAY,
    TIER_SEVEN_DAY_OPUS,
    TIER_SEVEN_DAY_SONNET,
];
const GEMINI_PRO_TIER_NAMES: &[&str] = &[TIER_GEMINI_PRO];
const GEMINI_FLASH_TIER_NAMES: &[&str] = &[TIER_GEMINI_FLASH];
const GEMINI_FLASH_LITE_TIER_NAMES: &[&str] = &[TIER_GEMINI_FLASH_LITE];
const TIER_LABEL_GROUPS: &[(&str, &[&str])] = &[
    ("h", H_TIER_NAMES),
    ("w", W_TIER_NAMES),
    ("p", GEMINI_PRO_TIER_NAMES),
    ("f", GEMINI_FLASH_TIER_NAMES),
    ("l", GEMINI_FLASH_LITE_TIER_NAMES),
];

fn usage_suffix(
    app: &Arc<AppState>,
    app_type: AppType,
    provider: &Provider,
    provider_id: &str,
) -> Option<String> {
    let usage_script = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.usage_script.as_ref());
    let is_official_provider = provider.category.as_deref() == Some("official");
    let can_use_script = provider.has_usage_script_enabled()
        && (!is_official_provider || provider_uses_official_subscription(provider));

    if can_use_script {
        if let Some(Some(summary)) =
            app.usage_cache
                .with_script(&app_type, provider_id, format_script_summary)
        {
            return Some(format!(" · {summary}"));
        }
        if provider_uses_official_subscription(provider) {
            if let Some(Some(summary)) = app
                .usage_cache
                .with_subscription(&app_type, format_subscription_summary)
            {
                return Some(format!(" · {summary}"));
            }
        }
    } else {
        app.usage_cache.invalidate_script(&app_type, provider_id);
    }

    if usage_script.and_then(|script| script.template_type.as_deref())
        != Some(TEMPLATE_TYPE_OFFICIAL_SUBSCRIPTION)
    {
        app.usage_cache.invalidate_subscription(&app_type);
    }

    None
}

fn provider_uses_official_subscription(provider: &Provider) -> bool {
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.usage_script.as_ref())
        .map(|script| {
            script.enabled
                && script.template_type.as_deref() == Some(TEMPLATE_TYPE_OFFICIAL_SUBSCRIPTION)
        })
        .unwrap_or(false)
}

fn format_subscription_summary(quota: &SubscriptionQuota) -> Option<String> {
    if !quota.success {
        return None;
    }

    let entries = quota
        .tiers
        .iter()
        .map(|tier| (tier.name.as_str(), tier.utilization))
        .collect::<Vec<_>>();
    format_tier_parts(&entries)
}

fn format_script_summary(result: &UsageResult) -> Option<String> {
    if !result.success {
        return None;
    }

    let data = result.data.as_ref()?;
    let entries = data
        .iter()
        .filter_map(|item| {
            let total = item.total?;
            if total <= 0.0 {
                return None;
            }
            let used = item.used?;
            Some((item.plan_name.as_deref()?, used / total * 100.0))
        })
        .collect::<Vec<_>>();
    if let Some(summary) = format_tier_parts(&entries) {
        return Some(summary);
    }

    let first = data.first()?;
    let total = first.total?;
    if total <= 0.0 {
        return None;
    }
    let used = first.used?;
    let pct = (used / total * 100.0).round() as i64;
    first
        .plan_name
        .as_ref()
        .filter(|plan| !plan.is_empty())
        .map(|plan| format!("{plan} {pct}%"))
        .or_else(|| Some(format!("{pct}%")))
}

fn format_tier_parts(entries: &[(&str, f64)]) -> Option<String> {
    let parts = TIER_LABEL_GROUPS
        .iter()
        .filter_map(|(label, names)| {
            let pct = entries
                .iter()
                .filter(|(name, _)| names.contains(name))
                .map(|(_, pct)| *pct)
                .filter(|pct| pct.is_finite())
                .max_by(|left, right| {
                    left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
                })?;
            Some(format!("{label}{}%", pct.round() as i64))
        })
        .collect::<Vec<_>>();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn report_to_roots(
    cx: &mut App,
    app_type: Option<AppType>,
    level: NotificationLevel,
    title: String,
    message: Option<String>,
) {
    if !notify_open_roots(cx, app_type, level, title.clone(), message.clone()) {
        log::warn!(
            "{title}{}",
            message.map(|m| format!(": {m}")).unwrap_or_default()
        );
    }
}

fn activate_first_window(cx: &mut App) {
    if let Some(window) = cx.windows().into_iter().next() {
        let _ = window.update(cx, |_root, window, _cx| {
            window.activate_window();
        });
    }
    cx.activate(true);
}

fn app_label(app: AppType) -> &'static str {
    match app {
        AppType::Claude => "Claude Code",
        AppType::ClaudeDesktop => "Claude Desktop",
        AppType::Codex => "Codex",
        AppType::Gemini => "Gemini CLI",
        AppType::OpenCode => "OpenCode",
        AppType::OpenClaw => "OpenClaw",
        AppType::Hermes => "Hermes",
    }
}

#[allow(dead_code)]
fn shared(label: &'static str) -> SharedString {
    SharedString::from(label)
}

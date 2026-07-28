//! Native shell menus and the optional macOS/Windows status icon.

#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(any(target_os = "macos", target_os = "windows"))]
use gpui::Global;
use gpui::{App, AppContext, Menu, MenuItem, OsAction, SharedString, SystemMenuType, actions};
use ochub_core::services::provider::ProviderService;
use ochub_core::services::subscription::{
    SubscriptionQuota, TIER_FIVE_HOUR, TIER_SEVEN_DAY, TIER_SEVEN_DAY_OPUS, TIER_SEVEN_DAY_SONNET,
    TIER_WEEKLY_LIMIT,
};
use ochub_core::{AppState, AppType, Provider, UsageResult, settings};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tray_icon::menu::{
    CheckMenuItem as TrayCheckMenuItem, Menu as TrayMenu, MenuEvent, MenuItem as TrayMenuItem,
    PredefinedMenuItem as TrayPredefinedMenuItem, Submenu as TraySubmenu,
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tray_icon::{Icon as TrayIconImage, TrayIcon, TrayIconBuilder};
#[cfg(target_os = "windows")]
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

use crate::app_ui::{notify_open_roots, open_settings_in_roots};
use crate::i18n::{k, raw, t};
use crate::notifications::NotificationLevel;
use crate::shortcuts::{CloseWindow, OpenSettings, Save};
use crate::text_input::{Copy, Cut, Find, FindNext, FindPrevious, Paste, Redo, SelectAll, Undo};
use crate::tf;

actions!(ochub, [OpenMainWindow, QuitApp, RefreshShellMenus]);

static MENU_REFRESH_GENERATION: AtomicU64 = AtomicU64::new(0);

struct ShellMenuSnapshot {
    quick_switch_enabled: bool,
    tray_resident_enabled: bool,
    apps: Vec<AppProviderMenu>,
}

struct AppProviderMenu {
    app_type: AppType,
    providers: Result<Vec<(String, Provider)>, String>,
    current: String,
}

impl ShellMenuSnapshot {
    fn load(app: &Arc<AppState>) -> Self {
        let preferences = settings::get_settings();
        let quick_switch_enabled = preferences.show_in_tray;
        let tray_resident_enabled =
            preferences.minimize_to_tray_on_close && preferences.tray_resident_mode;
        let apps = if quick_switch_enabled {
            enabled_app_types()
                .into_iter()
                .map(|app_type| {
                    let providers = ProviderService::list(app, app_type)
                        .map(|providers| {
                            let mut providers = providers.into_iter().collect::<Vec<_>>();
                            providers.sort_by(|(left_id, left), (right_id, right)| {
                                provider_sort_key(left_id, left)
                                    .cmp(&provider_sort_key(right_id, right))
                            });
                            providers
                        })
                        .map_err(|error| error.to_string());
                    let current = ProviderService::current(app, app_type).unwrap_or_default();
                    AppProviderMenu {
                        app_type,
                        providers,
                        current,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        Self {
            quick_switch_enabled,
            tray_resident_enabled,
            apps,
        }
    }
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = ochub, no_json)]
pub struct SwitchProviderFromMenu {
    app: String,
    provider_id: String,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Clone)]
enum TrayCommand {
    OpenMainWindow,
    OpenSettings,
    SwitchProvider(SwitchProviderFromMenu),
    Quit,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
enum TrayEvent {
    Menu(String),
    #[cfg(target_os = "windows")]
    Activate,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Default)]
struct SystemTrayState {
    icon: Option<TrayIcon>,
    commands: HashMap<String, TrayCommand>,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Global for SystemTrayState {}

pub fn install(app: Arc<AppState>, cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.bind_keys([gpui::KeyBinding::new("cmd-q", QuitApp, None)]);
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    cx.bind_keys([gpui::KeyBinding::new("ctrl-q", QuitApp, None)]);

    let switch_app = app.clone();
    cx.on_action(move |action: &SwitchProviderFromMenu, cx| {
        switch_provider_from_menu(switch_app.clone(), action, cx);
    });

    let refresh_app = app.clone();
    cx.on_action(move |_: &RefreshShellMenus, cx| {
        refresh(&refresh_app, cx);
    });

    cx.on_action(|_: &OpenMainWindow, cx| {
        activate_first_window(cx);
    });
    cx.on_action(|_: &OpenSettings, cx| {
        cx.defer(|cx| {
            activate_first_window(cx);
            open_settings_in_roots(cx);
        });
    });
    cx.on_action(|_: &CloseWindow, cx| {
        cx.defer(crate::close_main_window);
    });
    cx.on_action(|_: &QuitApp, cx| {
        cx.quit();
    });

    install_system_tray(app.clone(), cx);
    refresh(&app, cx);
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn install_system_tray(app: Arc<AppState>, cx: &mut App) {
    cx.set_global(SystemTrayState::default());

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let menu_sender = sender.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = menu_sender.send(TrayEvent::Menu(event.id.0));
    }));

    #[cfg(target_os = "windows")]
    {
        let tray_sender = sender;
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                let _ = tray_sender.send(TrayEvent::Activate);
            }
        }));
    }
    #[cfg(target_os = "macos")]
    let _ = sender;

    let app_for_events = app.clone();
    cx.spawn(async move |cx| {
        while let Some(event) = receiver.recv().await {
            let app = app_for_events.clone();
            cx.update(move |cx| handle_tray_event(app, event, cx));
        }
    })
    .detach();

    // Establish the recovery entry before the main window is opened. The full
    // provider menu is loaded asynchronously by `refresh` immediately after
    // this; this small first menu prevents a Dock flash on macOS.
    let preferences = settings::get_settings();
    if preferences.minimize_to_tray_on_close && preferences.tray_resident_mode {
        let snapshot = ShellMenuSnapshot {
            quick_switch_enabled: false,
            tray_resident_enabled: true,
            apps: Vec::new(),
        };
        apply_system_tray(&app, &snapshot, cx);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn install_system_tray(_app: Arc<AppState>, _cx: &mut App) {}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn handle_tray_event(app: Arc<AppState>, event: TrayEvent, cx: &mut App) {
    let command = match event {
        #[cfg(target_os = "windows")]
        TrayEvent::Activate => Some(TrayCommand::OpenMainWindow),
        TrayEvent::Menu(id) => cx
            .try_global::<SystemTrayState>()
            .and_then(|state| state.commands.get(&id))
            .cloned(),
    };

    match command {
        Some(TrayCommand::OpenMainWindow) => activate_first_window(cx),
        Some(TrayCommand::OpenSettings) => {
            activate_first_window(cx);
            open_settings_in_roots(cx);
        }
        Some(TrayCommand::SwitchProvider(action)) => {
            switch_provider_from_menu(app, &action, cx);
        }
        Some(TrayCommand::Quit) => cx.quit(),
        None => {}
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn tray_resident_active(cx: &App) -> bool {
    cx.try_global::<SystemTrayState>()
        .is_some_and(|state| state.icon.is_some())
}

pub fn refresh(app: &Arc<AppState>, cx: &mut App) {
    let generation = MENU_REFRESH_GENERATION
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    let app_for_work = app.clone();
    let app_for_apply = app.clone();
    let task = cx.background_spawn(async move { ShellMenuSnapshot::load(&app_for_work) });
    cx.spawn(async move |cx| {
        let snapshot = task.await;
        cx.update(move |cx| {
            if MENU_REFRESH_GENERATION.load(Ordering::Acquire) == generation {
                apply_shell_menus(&app_for_apply, snapshot, cx);
            }
        });
    })
    .detach();
}

fn apply_shell_menus(app: &Arc<AppState>, snapshot: ShellMenuSnapshot, cx: &mut App) {
    let quick_switch_enabled = snapshot.quick_switch_enabled;
    // "OcHub" is the product name, not prose: the macOS application menu is
    // titled after the app in every locale.
    let mut menus = vec![
        Menu::new("OcHub").items([
            MenuItem::action(t(k::MENU_APP_OPEN_MAIN_WINDOW), OpenMainWindow),
            MenuItem::action(t(k::MENU_APP_SETTINGS), OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu(t(k::MENU_APP_SERVICES), SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(t(k::MENU_APP_QUIT), QuitApp),
        ]),
        Menu::new(t(k::MENU_FILE_TITLE)).items([
            MenuItem::action(t(k::MENU_FILE_SAVE), Save),
            MenuItem::separator(),
            MenuItem::action(t(k::MENU_FILE_CLOSE_WINDOW), CloseWindow),
        ]),
        Menu::new(t(k::MENU_EDIT_TITLE)).items([
            MenuItem::os_action(t(k::MENU_EDIT_UNDO), Undo, OsAction::Undo),
            MenuItem::os_action(t(k::MENU_EDIT_REDO), Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action(t(k::MENU_EDIT_CUT), Cut, OsAction::Cut),
            MenuItem::os_action(t(k::MENU_EDIT_COPY), Copy, OsAction::Copy),
            MenuItem::os_action(t(k::MENU_EDIT_PASTE), Paste, OsAction::Paste),
            MenuItem::os_action(t(k::MENU_EDIT_SELECT_ALL), SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action(t(k::MENU_EDIT_FIND), Find),
            MenuItem::action(t(k::MENU_EDIT_FIND_NEXT), FindNext),
            MenuItem::action(t(k::MENU_EDIT_FIND_PREVIOUS), FindPrevious),
        ]),
    ];

    if quick_switch_enabled {
        menus.push(
            Menu::new(t(k::MENU_PROVIDER_TITLE)).items(provider_submenus(app, &snapshot.apps)),
        );
    }
    cx.set_menus(menus);

    let dock_items = if cfg!(target_os = "windows") {
        windows_taskbar_items(&snapshot.apps, quick_switch_enabled)
    } else {
        let mut dock_items = vec![
            MenuItem::action(t(k::MENU_APP_OPEN_MAIN_WINDOW), OpenMainWindow),
            MenuItem::action(t(k::MENU_APP_SETTINGS), OpenSettings),
            MenuItem::separator(),
        ];
        if quick_switch_enabled {
            dock_items.extend(provider_submenus(app, &snapshot.apps));
            dock_items.push(MenuItem::separator());
        }
        dock_items.push(MenuItem::action(t(k::MENU_APP_QUIT), QuitApp));
        dock_items
    };
    cx.set_dock_menu(dock_items);
    apply_system_tray(app, &snapshot, cx);
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn apply_system_tray(app: &Arc<AppState>, snapshot: &ShellMenuSnapshot, cx: &mut App) {
    if !snapshot.tray_resident_enabled {
        let icon = {
            let state = cx.global_mut::<SystemTrayState>();
            state.commands.clear();
            state.icon.take()
        };
        drop(icon);
        #[cfg(target_os = "macos")]
        set_macos_accessory_mode(false);
        return;
    }

    let generation = MENU_REFRESH_GENERATION.load(Ordering::Acquire);
    let (menu, commands) = match build_tray_menu(app, snapshot, generation) {
        Ok(menu) => menu,
        Err(error) => {
            log::error!("failed to build the system tray menu: {error}");
            return;
        }
    };

    if let Some(icon) = cx.global::<SystemTrayState>().icon.as_ref().cloned() {
        icon.set_menu(Some(Box::new(menu)));
        cx.global_mut::<SystemTrayState>().commands = commands;
        #[cfg(target_os = "macos")]
        set_macos_accessory_mode(true);
        return;
    }

    let icon = match load_tray_icon(cx).and_then(|image| {
        TrayIconBuilder::new()
            .with_id("ochub.system-tray")
            .with_tooltip("OcHub")
            .with_icon(image)
            .with_icon_as_template(false)
            .with_menu(Box::new(menu))
            // macOS convention opens the menu from either button. On Windows
            // a left click restores the window and a right click opens it.
            .with_menu_on_left_click(cfg!(target_os = "macos"))
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(icon) => icon,
        Err(error) => {
            log::error!("failed to create the system tray icon: {error}");
            // Never remove the last visible recovery path when native tray
            // integration is unavailable.
            #[cfg(target_os = "macos")]
            set_macos_accessory_mode(false);
            return;
        }
    };

    {
        let state = cx.global_mut::<SystemTrayState>();
        state.icon = Some(icon);
        state.commands = commands;
    }
    #[cfg(target_os = "macos")]
    set_macos_accessory_mode(true);
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn apply_system_tray(_app: &Arc<AppState>, snapshot: &ShellMenuSnapshot, _cx: &mut App) {
    let _ = snapshot.tray_resident_enabled;
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn load_tray_icon(cx: &App) -> Result<TrayIconImage, String> {
    let bytes = cx
        .asset_source()
        .load("app-icons/ochub-32.png")
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "app-icons/ochub-32.png is missing from packaged assets".to_string())?;
    let rgba = image::load_from_memory(&bytes)
        .map_err(|error| error.to_string())?
        .into_rgba8();
    let (width, height) = rgba.dimensions();
    TrayIconImage::from_rgba(rgba.into_raw(), width, height).map_err(|error| error.to_string())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn build_tray_menu(
    app: &Arc<AppState>,
    snapshot: &ShellMenuSnapshot,
    generation: u64,
) -> Result<(TrayMenu, HashMap<String, TrayCommand>), String> {
    const OPEN_ID: &str = "ochub.tray.open";
    const SETTINGS_ID: &str = "ochub.tray.settings";
    const QUIT_ID: &str = "ochub.tray.quit";

    let menu = TrayMenu::new();
    let mut commands = HashMap::new();

    let open = TrayMenuItem::with_id(OPEN_ID, t(k::MENU_APP_OPEN_MAIN_WINDOW), true, None);
    menu.append(&open).map_err(|error| error.to_string())?;
    commands.insert(OPEN_ID.to_string(), TrayCommand::OpenMainWindow);

    let settings = TrayMenuItem::with_id(SETTINGS_ID, t(k::MENU_APP_SETTINGS), true, None);
    menu.append(&settings).map_err(|error| error.to_string())?;
    commands.insert(SETTINGS_ID.to_string(), TrayCommand::OpenSettings);

    menu.append(&TrayPredefinedMenuItem::separator())
        .map_err(|error| error.to_string())?;

    if snapshot.quick_switch_enabled {
        for (app_index, provider_menu) in snapshot.apps.iter().enumerate() {
            let submenu = build_tray_provider_submenu(
                app,
                provider_menu,
                app_index,
                generation,
                &mut commands,
            )?;
            menu.append(&submenu).map_err(|error| error.to_string())?;
        }
        menu.append(&TrayPredefinedMenuItem::separator())
            .map_err(|error| error.to_string())?;
    }

    let quit = TrayMenuItem::with_id(QUIT_ID, t(k::MENU_APP_QUIT), true, None);
    menu.append(&quit).map_err(|error| error.to_string())?;
    commands.insert(QUIT_ID.to_string(), TrayCommand::Quit);

    Ok((menu, commands))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn build_tray_provider_submenu(
    app: &Arc<AppState>,
    provider_menu: &AppProviderMenu,
    app_index: usize,
    generation: u64,
    commands: &mut HashMap<String, TrayCommand>,
) -> Result<TraySubmenu, String> {
    let app_type = provider_menu.app_type;
    let providers = match &provider_menu.providers {
        Ok(providers) => providers,
        Err(error) => {
            let submenu = TraySubmenu::new(app_label(app_type), true);
            let item = TrayMenuItem::new(
                tf!(k::MENU_PROVIDER_LOAD_FAILED, error = error),
                false,
                None,
            );
            submenu.append(&item).map_err(|error| error.to_string())?;
            return Ok(submenu);
        }
    };

    if providers.is_empty() {
        let submenu = TraySubmenu::new(app_label(app_type), true);
        let item = TrayMenuItem::new(t(k::MENU_PROVIDER_EMPTY), false, None);
        submenu.append(&item).map_err(|error| error.to_string())?;
        return Ok(submenu);
    }

    let submenu = TraySubmenu::new(
        provider_submenu_label(app, app_type, providers, &provider_menu.current),
        true,
    );
    for (provider_index, (provider_id, provider)) in providers.iter().enumerate() {
        let id = format!("ochub.tray.switch.{generation}.{app_index}.{provider_index}");
        let item = TrayCheckMenuItem::with_id(
            id.clone(),
            provider_item_label(app_type, provider),
            true,
            provider_is_selected(app_type, &provider_menu.current, provider),
            None,
        );
        submenu.append(&item).map_err(|error| error.to_string())?;
        commands.insert(
            id,
            TrayCommand::SwitchProvider(SwitchProviderFromMenu {
                app: app_type.as_str().to_string(),
                provider_id: provider_id.clone(),
            }),
        );
    }
    Ok(submenu)
}

#[cfg(target_os = "macos")]
fn set_macos_accessory_mode(accessory: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let Some(main_thread) = MainThreadMarker::new() else {
        log::error!("cannot change the Dock activation policy off the main thread");
        return;
    };
    let application = NSApplication::sharedApplication(main_thread);
    let policy = if accessory {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    };
    if application.activationPolicy() != policy && !application.setActivationPolicy(policy) {
        log::error!("macOS rejected the requested Dock activation policy");
    }
}

fn provider_submenus(app: &Arc<AppState>, apps: &[AppProviderMenu]) -> Vec<MenuItem> {
    apps.iter()
        .map(|provider_menu| provider_submenu(app, provider_menu))
        .collect()
}

fn enabled_app_types() -> Vec<AppType> {
    ochub_core::plugin::enabled_plugins()
        .iter()
        .filter_map(|plugin| AppType::from_app_id(plugin.id()))
        .collect()
}

fn provider_submenu(app: &Arc<AppState>, provider_menu: &AppProviderMenu) -> MenuItem {
    let app_type = provider_menu.app_type;
    let providers = match &provider_menu.providers {
        Ok(providers) => providers,
        Err(error) => {
            return MenuItem::submenu(
                Menu::new(app_label(app_type)).items([MenuItem::action(
                    tf!(k::MENU_PROVIDER_LOAD_FAILED, error = error),
                    RefreshShellMenus,
                )
                .disabled(true)]),
            );
        }
    };

    if providers.is_empty() {
        return MenuItem::submenu(Menu::new(app_label(app_type)).items([
            MenuItem::action(t(k::MENU_PROVIDER_EMPTY), RefreshShellMenus).disabled(true),
        ]));
    }

    let label = provider_submenu_label(app, app_type, providers, &provider_menu.current);

    MenuItem::submenu(
        Menu::new(label).items(providers.iter().map(|(id, provider)| {
            let checked = provider_is_selected(app_type, &provider_menu.current, provider);
            MenuItem::action(
                provider_item_label(app_type, provider),
                SwitchProviderFromMenu {
                    app: app_type.as_str().to_string(),
                    provider_id: id.clone(),
                },
            )
            .checked(checked)
        })),
    )
}

fn windows_taskbar_items(apps: &[AppProviderMenu], quick_switch_enabled: bool) -> Vec<MenuItem> {
    let mut items = vec![
        MenuItem::action(t(k::MENU_APP_OPEN_MAIN_WINDOW), OpenMainWindow),
        MenuItem::action(t(k::MENU_APP_SETTINGS), OpenSettings),
    ];
    if quick_switch_enabled {
        items.extend(windows_provider_items(apps));
    }
    items.push(MenuItem::action(t(k::MENU_APP_QUIT), QuitApp));
    items
}

fn windows_provider_items(apps: &[AppProviderMenu]) -> Vec<MenuItem> {
    let mut items = Vec::new();

    for provider_menu in apps {
        let app_type = provider_menu.app_type;
        let Ok(providers) = &provider_menu.providers else {
            continue;
        };

        for (id, provider) in providers {
            // One whole label per state rather than a status word pasted into a
            // shared template: the parenthetical is grammar, not a value.
            let label = if provider_is_selected(app_type, &provider_menu.current, provider) {
                if app_type.is_additive_mode() {
                    k::MENU_PROVIDER_TASKBAR_ADDED
                } else {
                    k::MENU_PROVIDER_TASKBAR_CURRENT
                }
            } else if app_type.is_additive_mode() {
                k::MENU_PROVIDER_TASKBAR_ADD
            } else {
                k::MENU_PROVIDER_TASKBAR_SWITCH
            };
            items.push(MenuItem::action(
                tf!(label, app = app_label(app_type), provider = provider.name),
                SwitchProviderFromMenu {
                    app: app_type.as_str().to_string(),
                    provider_id: id.clone(),
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
        return tf!(
            k::MENU_PROVIDER_ADDED_COUNT,
            app = app_label(app_type),
            count = managed_count,
        );
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
        tf!(k::MENU_PROVIDER_ITEM_ADDED, name = provider.name)
    } else {
        tf!(k::MENU_PROVIDER_ITEM_ADD, name = provider.name)
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
                raw(k::MENU_SWITCH_ACTION_FAILED).to_string(),
                Some(tf!(
                    k::MENU_SWITCH_UNKNOWN_APP,
                    app = action.app,
                    error = err,
                )),
            );
            return;
        }
    };

    let provider_id = action.provider_id.clone();
    let app_for_work = app.clone();
    let app_for_refresh = app.clone();
    let task =
        cx.background_spawn(
            async move { perform_menu_switch(&app_for_work, app_type, &provider_id) },
        );
    cx.spawn(async move |cx| {
        let report = task.await;
        cx.update(move |cx| {
            refresh(&app_for_refresh, cx);
            report_to_roots(
                cx,
                Some(report.app_type),
                report.level,
                report.title,
                report.message,
            );
        });
    })
    .detach();
}

struct MenuSwitchReport {
    app_type: AppType,
    level: NotificationLevel,
    title: String,
    message: Option<String>,
}

fn perform_menu_switch(
    app: &Arc<AppState>,
    app_type: AppType,
    provider_id: &str,
) -> MenuSwitchReport {
    let provider = ProviderService::list(app, app_type)
        .ok()
        .and_then(|providers| providers.get(provider_id).cloned());
    let provider_name = provider
        .as_ref()
        .map(|provider| provider.name.clone())
        .unwrap_or_else(|| provider_id.to_string());
    let removing_from_additive = provider
        .as_ref()
        .map(|provider| {
            app_type.is_additive_mode() && provider_live_config_managed(app_type, provider)
        })
        .unwrap_or(false);

    let result = if removing_from_additive {
        ProviderService::remove_from_live_config(app, app_type, provider_id)
            .map(|_| Default::default())
    } else {
        ProviderService::switch(app, app_type, provider_id)
    };

    match result {
        Ok(result) if result.warnings.is_empty() => {
            let title = if removing_from_additive {
                tf!(k::MENU_SWITCH_REMOVED, name = provider_name)
            } else if app_type.is_additive_mode() {
                tf!(k::MENU_SWITCH_ADDED, name = provider_name)
            } else {
                tf!(
                    k::MENU_SWITCH_SWITCHED,
                    app = app_label(app_type),
                    provider = provider_name,
                )
            };
            MenuSwitchReport {
                app_type,
                level: NotificationLevel::Success,
                title,
                message: None,
            }
        }
        Ok(result) => MenuSwitchReport {
            app_type,
            level: NotificationLevel::Warning,
            title: tf!(
                k::MENU_SWITCH_SWITCHED_WITH_WARNINGS,
                app = app_label(app_type),
            ),
            message: Some(tf!(k::MENU_SWITCH_WARNINGS, count = result.warnings.len(),)),
        },
        Err(error) => MenuSwitchReport {
            app_type,
            level: NotificationLevel::Error,
            title: tf!(k::MENU_SWITCH_FAILED, app = app_label(app_type)),
            message: Some(error.to_string()),
        },
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
const TIER_LABEL_GROUPS: &[(&str, &[&str])] = &[("h", H_TIER_NAMES), ("w", W_TIER_NAMES)];

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
        if provider_uses_official_subscription(provider)
            && let Some(Some(summary)) = app
                .usage_cache
                .with_subscription(&app_type, format_subscription_summary)
        {
            return Some(format!(" · {summary}"));
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

pub(crate) fn activate_first_window(cx: &mut App) {
    // `App::hide` maps to NSApplication.hide on macOS. Cocoa requires an
    // explicit `unhide:` before `activateIgnoringOtherApps` and
    // `makeKeyAndOrderFront` can restore the existing window.
    #[cfg(target_os = "macos")]
    unhide_application();
    cx.activate(true);
    if let Some(window) = cx.windows().into_iter().next() {
        let _ = window.update(cx, |_root, window, _cx| {
            #[cfg(target_os = "windows")]
            let _ = crate::set_windows_window_visible(window, true);
            window.activate_window();
        });
    }
}

#[cfg(target_os = "macos")]
fn unhide_application() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    if let Some(main_thread) = MainThreadMarker::new() {
        NSApplication::sharedApplication(main_thread).unhide(None);
    }
}

fn app_label(app: AppType) -> gpui::SharedString {
    crate::app_meta::label(app)
}

#[allow(dead_code)]
fn shared(label: &'static str) -> SharedString {
    SharedString::from(label)
}

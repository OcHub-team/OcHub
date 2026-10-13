//! Managed apps: plugin source, management state, and explicit row actions.
//! Built-in adapters and user manifests share the same management contract.
//! Local manifests share the desktop editor. Unknown remote adapters remain
//! explicit about their unavailable editor protocol.

use std::process::Command;

use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, prelude::*, px};
use ochub_core::plugin::AppPlugin;
use ochub_core::settings;
use ochub_core::{AppId, AppType};

use crate::components::{self, ButtonSize, ButtonTone};
use crate::i18n::{k, t};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::shell_menu;
use crate::tf;
use crate::theme;

use super::{SettingsEvent, SettingsView};

struct ManagedAppRow {
    id: AppId,
    label: SharedString,
    enabled: bool,
    user_manifest: bool,
    supports_provider: bool,
}

impl SettingsView {
    pub(super) fn render_apps(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        layout::page()
            .relative()
            .child(self.sub_page_header(t(k::SETTINGS_APPS_PAGE_TITLE), cx))
            .child(layout::virtual_body(
                "settings-apps-body",
                gpui::list(
                    self.apps_list.clone(),
                    cx.processor(|this, ix, window, cx| this.render_apps_block(ix, window, cx)),
                ),
                &self.apps_list,
            ))
    }

    fn render_apps_block(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match ix {
            0 => {
                if self.workspace_remote {
                    if self.remote_apps_error.is_some() {
                        return gpui::Empty.into_any_element();
                    }
                    if self.remote_apps_loading || self.remote_apps.is_empty() {
                        let message = if self.remote_apps_loading {
                            t(k::SETTINGS_APPS_REMOTE_LOADING)
                        } else if !self.workspace_available {
                            t(k::SETTINGS_APPS_REMOTE_UNAVAILABLE)
                        } else {
                            t(k::SETTINGS_APPS_REMOTE_EMPTY)
                        };
                        return super::rows::group_block(
                            t(k::SETTINGS_APPS_SECTION_ENABLED),
                            vec![
                                layout::row()
                                    .child(
                                        div().text_sm().text_color(theme::muted()).child(message),
                                    )
                                    .into_any_element(),
                            ],
                        );
                    }
                }
                let apps = self.managed_apps();
                let enabled_count = apps.iter().filter(|app| app.enabled).count();
                let rows: Vec<AnyElement> = apps
                    .iter()
                    .map(|app| {
                        let id = app.id.to_string();
                        let label = app.label.clone();
                        let enabled = app.enabled;
                        let last_one = enabled && enabled_count <= 1;
                        let busy = self.toggling.contains(&id);
                        let source = if app.user_manifest {
                            t(k::SETTINGS_APPS_SOURCE_USER)
                        } else {
                            t(k::SETTINGS_APPS_SOURCE_BUILTIN)
                        };
                        let state = if enabled {
                            t(k::SETTINGS_APPS_STATE_ENABLED)
                        } else {
                            t(k::SETTINGS_APPS_STATE_DISABLED)
                        };
                        let action = if enabled {
                            t(k::SETTINGS_APPS_MANAGE_DISABLE)
                        } else {
                            t(k::SETTINGS_APPS_MANAGE_ENABLE)
                        };
                        let editor_notice =
                            if self.workspace_remote && AppType::from_app_id(&app.id).is_none() {
                                Some(t(k::SETTINGS_APPS_REMOTE_EDITOR_UNAVAILABLE))
                            } else if !app.supports_provider {
                                Some(t(k::SETTINGS_APPS_EDITOR_UNAVAILABLE))
                            } else {
                                None
                            };
                        let aria_label = SharedString::from(tf!(
                            k::SETTINGS_APPS_MANAGE_ARIA,
                            action = action,
                            app = label
                        ));
                        let mut aria_description = format!("{source} · {state}");
                        if let Some(notice) = &editor_notice {
                            aria_description.push_str(&format!(" · {notice}"));
                        }
                        if last_one {
                            aria_description
                                .push_str(&format!(" · {}", t(k::SETTINGS_APPS_KEEP_ONE_ENABLED)));
                        }
                        let button_id = SharedString::from(format!("app-{id}-manage"));
                        let toggle_id = id.clone();
                        let control = if busy {
                            components::busy_button(
                                button_id,
                                action,
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                                true,
                            )
                        } else if last_one || !self.workspace_available {
                            components::disabled_button(
                                button_id,
                                action,
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                                true,
                            )
                        } else {
                            components::button(
                                button_id,
                                action,
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.set_app_enabled(&toggle_id, cx);
                                },
                            ))
                        }
                        .aria_label(aria_label)
                        .aria_description(SharedString::from(aria_description))
                        .flex_none()
                        .whitespace_nowrap();
                        let metadata = div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(source)
                            .child(
                                div()
                                    .text_color(if enabled {
                                        theme::green()
                                    } else {
                                        theme::muted()
                                    })
                                    .child(state),
                            )
                            .when_some(editor_notice, |row, notice| row.child(notice));
                        let identity = div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme::text())
                                    .child(label),
                            )
                            .child(metadata)
                            .when(last_one, |column| {
                                column.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::muted())
                                        .child(t(k::SETTINGS_APPS_KEEP_ONE_ENABLED)),
                                )
                            });
                        layout::row()
                            .px_3()
                            .py_2()
                            .gap_3()
                            .child(identity)
                            .child(control)
                            .into_any_element()
                    })
                    .collect();
                super::rows::group_block(t(k::SETTINGS_APPS_SECTION_ENABLED), rows)
            }
            1 => {
                if self.workspace_remote {
                    let Some(error) = self.remote_apps_error.clone() else {
                        return gpui::Empty.into_any_element();
                    };
                    return super::rows::group_block(
                        t(k::SETTINGS_APPS_SECTION_ERRORS),
                        vec![
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .px_3()
                                .py_2()
                                .child(components::field_error(error))
                                .child(
                                    components::button(
                                        "remote-apps-retry",
                                        t(k::SETTINGS_APPS_REMOTE_RETRY),
                                        ButtonTone::Neutral,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| this.load_workspace_apps(cx),
                                    )),
                                )
                                .into_any_element(),
                        ],
                    );
                }
                if self.plugin_load_errors.is_empty() {
                    return gpui::Empty.into_any_element();
                }
                // Display-only, so plain rows: making them focusable would put
                // a tab stop on something that cannot be operated.
                let rows: Vec<AnyElement> = self
                    .plugin_load_errors
                    .iter()
                    .map(|err| {
                        layout::row()
                            .child(components::field_error(SharedString::from(tf!(
                                k::SETTINGS_APPS_PLUGIN_LOAD_FAILED,
                                path = err.path,
                                message = err.message
                            ))))
                            .into_any_element()
                    })
                    .collect();
                super::rows::group_block(t(k::SETTINGS_APPS_SECTION_ERRORS), rows)
            }
            2 => {
                if self.workspace_remote {
                    return super::rows::group_block(
                        t(k::SETTINGS_APPS_SECTION_USER),
                        vec![
                            layout::row()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme::muted())
                                        .child(t(k::SETTINGS_APPS_REMOTE_PLUGINS_NOTICE)),
                                )
                                .into_any_element(),
                        ],
                    );
                }
                let dir = ochub_core::plugin::user_plugins_dir();
                let reload = if self.settings_busy {
                    components::disabled_button(
                        "apps-reload",
                        t(k::SETTINGS_APPS_RELOAD_ACTION),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                        true,
                    )
                } else {
                    components::button(
                        "apps-reload",
                        t(k::SETTINGS_APPS_RELOAD_ACTION),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| this.reload_user_plugins(cx)))
                };
                let open = if self.settings_busy {
                    components::disabled_button(
                        "apps-dir",
                        t(k::SETTINGS_APPS_PLUGINS_DIR_OPEN),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                        true,
                    )
                } else {
                    components::button(
                        "apps-dir",
                        t(k::SETTINGS_APPS_PLUGINS_DIR_OPEN),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(
                        cx.listener(|this, _event, _window, cx| this.open_user_plugins_dir(cx)),
                    )
                };
                super::rows::group_block(
                    t(k::SETTINGS_APPS_SECTION_USER),
                    vec![
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .min_w_0()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .truncate()
                                    .child(SharedString::from(dir.to_string_lossy().to_string())),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(reload)
                                    .child(open),
                            )
                            .into_any_element(),
                    ],
                )
            }
            _ => gpui::Empty.into_any_element(),
        }
    }

    pub(super) fn app_is_enabled(&self, plugin: &dyn AppPlugin) -> bool {
        self.settings
            .app_enabled(plugin.id().as_str())
            .unwrap_or_else(|| plugin.enabled_by_default())
    }

    fn managed_apps(&self) -> Vec<ManagedAppRow> {
        if self.workspace_remote {
            self.remote_apps
                .iter()
                .filter_map(|app| {
                    Some(ManagedAppRow {
                        id: AppId::parse(&app.id).ok()?,
                        label: app.display_name.clone().into(),
                        enabled: app.enabled,
                        user_manifest: app.user_manifest,
                        supports_provider: app.supports_provider,
                    })
                })
                .collect()
        } else {
            self.plugins
                .iter()
                .map(|plugin| ManagedAppRow {
                    id: plugin.id().clone(),
                    label: plugin.display_name().to_string().into(),
                    enabled: self.app_is_enabled(plugin.as_ref()),
                    user_manifest: plugin.is_user_manifest(),
                    supports_provider: plugin.provider_config().is_some(),
                })
                .collect()
        }
    }

    pub(super) fn managed_apps_summary(&self) -> SharedString {
        if self.workspace_remote && self.remote_apps_loading {
            return t(k::SETTINGS_APPS_REMOTE_LOADING);
        }
        if self.workspace_remote && (self.remote_apps_error.is_some() || !self.workspace_available)
        {
            return t(k::SETTINGS_APPS_REMOTE_UNAVAILABLE);
        }
        let (enabled, total) = if self.workspace_remote {
            (
                self.remote_apps.iter().filter(|app| app.enabled).count(),
                self.remote_apps.len(),
            )
        } else {
            (
                self.plugins
                    .iter()
                    .filter(|plugin| self.app_is_enabled(plugin.as_ref()))
                    .count(),
                self.plugins.len(),
            )
        };
        tf!(k::SETTINGS_APPS_COUNT, enabled = enabled, total = total).into()
    }

    pub(super) fn load_workspace_apps(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_remote || !self.workspace_available || self.remote_apps_loading {
            return;
        }
        let backend = self.backend.clone();
        let generation = self.workspace_generation;
        self.remote_apps_loading = true;
        self.remote_apps_error = None;
        self.apps_list.remeasure();
        self.root_list.remeasure();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move { backend.list_apps().await }).await;
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                this.remote_apps_loading = false;
                match result {
                    Ok(apps) => this.remote_apps = apps,
                    Err(error) => {
                        this.remote_apps_error =
                            Some(tf!(k::SETTINGS_APPS_REMOTE_LOAD_FAILED, error = error).into())
                    }
                }
                this.apps_list.remeasure();
                this.root_list.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Flip one app, off the render thread.
    ///
    /// `toggling` is keyed per app rather than one global "busy" flag, so only
    /// the row actually in flight goes inert while the rest of the page stays
    /// usable.
    fn set_app_enabled(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.toggling.contains(id) || !self.workspace_available {
            return;
        }
        let Some(app) = self
            .managed_apps()
            .into_iter()
            .find(|app| app.id.as_str() == id)
        else {
            return;
        };
        let currently = app.enabled;
        let app_id = app.id;
        let backend = self.backend.clone();
        let remote = self.workspace_remote;
        let generation = self.workspace_generation;
        let key = id.to_string();
        self.toggling.insert(key.clone());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend.set_app_enabled(&app_id, !currently).await
            })
            .await;
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                this.toggling.remove(&key);
                match result {
                    Ok(updated) => {
                        if remote {
                            if let Some(app) =
                                this.remote_apps.iter_mut().find(|app| app.id == updated.id)
                            {
                                *app = updated.clone();
                            }
                        } else {
                            this.settings = settings::get_settings();
                        }
                        this.set_status(
                            NotificationLevel::Success,
                            if currently {
                                t(k::SETTINGS_APPS_DISABLED)
                            } else {
                                t(k::SETTINGS_APPS_ENABLED)
                            },
                            cx,
                        );
                    }
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_APPS_ACTION_FAILED, error = err),
                        cx,
                    ),
                }
                if !remote {
                    shell_menu::refresh(&this.app, cx);
                }
                cx.emit(SettingsEvent::AppsChanged);
                this.apps_list.remeasure();
                this.root_list.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reload_user_plugins(&mut self, cx: &mut Context<Self>) {
        if self.settings_busy || self.workspace_remote || !self.workspace_available {
            return;
        }
        let generation = self.workspace_generation;
        self.settings_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (plugins, errors, plugin_count) = cx
                .background_spawn(async {
                    let errors = ochub_core::plugin::reload_user_plugins();
                    let plugins = ochub_core::plugin::all_plugins_snapshot();
                    let plugin_count = plugins
                        .iter()
                        .filter(|plugin| plugin.is_user_manifest())
                        .count();
                    (plugins, errors, plugin_count)
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                this.settings_busy = false;
                this.plugins = plugins;
                this.plugin_load_errors = errors.clone().into();
                // Failed candidate validation preserves the entire previously
                // loaded registry. Report retained adapters, not partial success.
                let (level, message) = if errors.is_empty() {
                    (
                        NotificationLevel::Success,
                        tf!(k::SETTINGS_APPS_PLUGINS_RELOADED, count = plugin_count),
                    )
                } else {
                    (
                        NotificationLevel::Warning,
                        tf!(
                            k::SETTINGS_APPS_PLUGINS_RELOADED_PARTIAL,
                            loaded = plugin_count,
                            failed = errors.len()
                        ),
                    )
                };
                this.set_status(level, message, cx);
                shell_menu::refresh(&this.app, cx);
                cx.emit(SettingsEvent::AppsChanged);
                this.apps_list.remeasure();
                this.root_list.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_user_plugins_dir(&mut self, cx: &mut Context<Self>) {
        if self.settings_busy || self.workspace_remote || !self.workspace_available {
            return;
        }
        let generation = self.workspace_generation;
        let dir = ochub_core::plugin::user_plugins_dir();
        self.settings_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    std::fs::create_dir_all(&dir).map_err(|error| {
                        tf!(k::SETTINGS_APPS_PLUGINS_DIR_CREATE_FAILED, error = error)
                    })?;
                    #[cfg(target_os = "macos")]
                    let status = Command::new("open").arg(&dir).status();
                    #[cfg(target_os = "windows")]
                    let status = Command::new("explorer").arg(&dir).status();
                    #[cfg(all(unix, not(target_os = "macos")))]
                    let status = Command::new("xdg-open").arg(&dir).status();
                    match status {
                        Ok(status) if status.success() => Ok(()),
                        Ok(status) => {
                            Err(tf!(k::SETTINGS_APPS_OPEN_FAILED_STATUS, status = status))
                        }
                        Err(error) => Err(tf!(k::SETTINGS_APPS_OPEN_FAILED, error = error)),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                this.settings_busy = false;
                match result {
                    Ok(()) => this.set_status(
                        NotificationLevel::Success,
                        t(k::SETTINGS_APPS_PLUGINS_DIR_OPENED),
                        cx,
                    ),
                    Err(error) => this.set_status(NotificationLevel::Error, error, cx),
                }
            })
            .ok();
        })
        .detach();
    }
}

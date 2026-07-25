//! The apps sub-page: one switch per plugin, the manifest errors, and the two
//! user-plugin commands.
//!
//! The switches are here rather than inlined on the root page because the
//! plugin list is unbounded — the same viewport failure the WebDAV block used
//! to cause. Enabling an app is a background call now: the service it delegates
//! to is `async fn` with no `.await` in its body, so the old
//! `tokio::runtime::Builder…block_on` on the render thread was pure waste, and
//! it froze the window on every toggle.

use std::process::Command;

use gpui::{prelude::*, AnyElement, Context, SharedString, Window};
use ochub_core::plugin::AppPlugin;
use ochub_core::settings;

use crate::components::{self, ButtonTone};
use crate::i18n::{k, t};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::shell_menu;
use crate::tf;

use super::{SettingsEvent, SettingsView};

impl SettingsView {
    pub(super) fn render_apps(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        layout::page()
            .relative()
            .child(self.sub_page_header(
                t(k::SETTINGS_APPS_PAGE_TITLE),
                t(k::SETTINGS_APPS_PAGE_DESC),
                cx,
            ))
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
                let plugins = ochub_core::plugin::all_plugins();
                let enabled_count = plugins
                    .iter()
                    .filter(|plugin| self.app_is_enabled(plugin.as_ref()))
                    .count();
                let rows: Vec<AnyElement> = plugins
                    .iter()
                    .map(|plugin| {
                        let id = plugin.id().as_str().to_string();
                        let label = plugin.display_name().to_string();
                        let description = if plugin.is_user_manifest() {
                            tf!(k::SETTINGS_APPS_PLUGIN_DESC_USER, app = label)
                        } else {
                            tf!(k::SETTINGS_APPS_PLUGIN_DESC, app = label)
                        };
                        let enabled = self.app_is_enabled(plugin.as_ref());
                        // "至少保留一个启用的应用" is a rule about which switch
                        // can be flipped, so it disables the switch and says why
                        // in the description. Erroring after the click made the
                        // refusal look like a failure.
                        let last_one = enabled && enabled_count <= 1;
                        let busy = self.toggling.contains(&id);
                        let description = if last_one {
                            t(k::SETTINGS_APPS_KEEP_ONE_ENABLED)
                        } else {
                            SharedString::from(description)
                        };
                        let toggle_id = id.clone();
                        layout::switch_row(
                            SharedString::from(format!("app-{id}")),
                            SharedString::from(label),
                            description,
                            enabled,
                            last_one || busy,
                            row_handler(cx.listener(move |this, _event: &(), _window, cx| {
                                this.set_app_enabled(&toggle_id, cx);
                            })),
                        )
                        .into_any_element()
                    })
                    .collect();
                super::rows::group_block(
                    t(k::SETTINGS_APPS_SECTION_ENABLED),
                    t(k::SETTINGS_APPS_SECTION_ENABLED_DESC),
                    rows,
                )
            }
            1 => {
                let errors = ochub_core::plugin::manifest_load_errors();
                if errors.is_empty() {
                    return gpui::Empty.into_any_element();
                }
                // Display-only, so plain rows: making them focusable would put
                // a tab stop on something that cannot be operated.
                let rows: Vec<AnyElement> = errors
                    .into_iter()
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
                super::rows::group_block(
                    t(k::SETTINGS_APPS_SECTION_ERRORS),
                    t(k::SETTINGS_APPS_SECTION_ERRORS_DESC),
                    rows,
                )
            }
            2 => {
                let dir = ochub_core::plugin::user_plugins_dir();
                let rows = vec![
                    layout::action_row(
                        "apps-reload",
                        t(k::SETTINGS_APPS_RELOAD_LABEL),
                        t(k::SETTINGS_APPS_RELOAD_DESC),
                        t(k::SETTINGS_APPS_RELOAD_ACTION),
                        ButtonTone::Neutral,
                        false,
                        row_handler(cx.listener(|this, _event: &(), _window, cx| {
                            this.reload_user_plugins(cx)
                        })),
                    )
                    .into_any_element(),
                    layout::action_row(
                        "apps-dir",
                        t(k::SETTINGS_APPS_PLUGINS_DIR_LABEL),
                        SharedString::from(dir.to_string_lossy().to_string()),
                        t(k::SETTINGS_ACTION_OPEN),
                        ButtonTone::Neutral,
                        false,
                        row_handler(cx.listener(|this, _event: &(), _window, cx| {
                            this.open_user_plugins_dir(cx)
                        })),
                    )
                    .into_any_element(),
                ];
                super::rows::group_block(
                    t(k::SETTINGS_APPS_SECTION_USER),
                    t(k::SETTINGS_APPS_SECTION_USER_DESC),
                    rows,
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

    /// Flip one app, off the render thread.
    ///
    /// `toggling` is keyed per app rather than one global "busy" flag, so only
    /// the row actually in flight goes inert while the rest of the page stays
    /// usable.
    fn set_app_enabled(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.toggling.contains(id) {
            return;
        }
        let plugins = ochub_core::plugin::all_plugins();
        let Some(plugin) = plugins.iter().find(|plugin| plugin.id().as_str() == id) else {
            return;
        };
        let currently = self.app_is_enabled(plugin.as_ref());
        let app = self.app.clone();
        let app_id = plugin.id().clone();
        let key = id.to_string();
        self.toggling.insert(key.clone());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    ochub_core::services::apps::set_app_enabled(&app, &app_id, !currently).await
                })
                .await;
            this.update(cx, |this, cx| {
                this.toggling.remove(&key);
                this.settings = settings::get_settings();
                match result {
                    Ok(()) => this.set_status(
                        NotificationLevel::Success,
                        if currently {
                            t(k::SETTINGS_APPS_DISABLED)
                        } else {
                            t(k::SETTINGS_APPS_ENABLED)
                        },
                        cx,
                    ),
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_APPS_ACTION_FAILED, error = err),
                        cx,
                    ),
                }
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

    fn reload_user_plugins(&mut self, cx: &mut Context<Self>) {
        let errors = ochub_core::plugin::reload_user_plugins();
        let plugin_count = ochub_core::plugin::all_plugins()
            .iter()
            .filter(|plugin| plugin.is_user_manifest())
            .count();
        // A partial reload still leaves broken manifests behind, so it is a
        // warning rather than a clean success.
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
        self.set_status(level, message, cx);
        shell_menu::refresh(&self.app, cx);
        cx.emit(SettingsEvent::AppsChanged);
        self.apps_list.remeasure();
        cx.notify();
    }

    fn open_user_plugins_dir(&mut self, cx: &mut Context<Self>) {
        let dir = ochub_core::plugin::user_plugins_dir();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.set_status(
                NotificationLevel::Error,
                tf!(k::SETTINGS_APPS_PLUGINS_DIR_CREATE_FAILED, error = err),
                cx,
            );
            return;
        }
        #[cfg(target_os = "macos")]
        let result = Command::new("open").arg(&dir).status();
        #[cfg(target_os = "windows")]
        let result = Command::new("explorer").arg(&dir).status();
        #[cfg(all(unix, not(target_os = "macos")))]
        let result = Command::new("xdg-open").arg(&dir).status();
        match result {
            Ok(status) if status.success() => self.set_status(
                NotificationLevel::Success,
                t(k::SETTINGS_APPS_PLUGINS_DIR_OPENED),
                cx,
            ),
            Ok(status) => self.set_status(
                NotificationLevel::Error,
                tf!(k::SETTINGS_APPS_OPEN_FAILED_STATUS, status = status),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::SETTINGS_APPS_OPEN_FAILED, error = err),
                cx,
            ),
        }
    }
}

/// Adapt a `cx.listener` (which takes an event argument) to the
/// `Fn(&mut Window, &mut App)` the `layout` rows want. `rows.rs` does the same
/// for the root page; these rows are built inline because their ids are
/// per-plugin rather than drawn from the `RowId` table.
fn row_handler(
    listener: impl Fn(&(), &mut Window, &mut gpui::App) + 'static,
) -> impl Fn(&mut Window, &mut gpui::App) + 'static {
    move |window, cx| listener(&(), window, cx)
}

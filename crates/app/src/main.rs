//! OCHUB desktop application (GPUI).
//!
//! Initializes the `ochub-core` `AppState` (SQLite store + services), hosts the
//! `ochub-server` axum control API in-process on loopback, and renders the GPUI UI.

mod app_meta;
mod app_settings_view;
mod app_ui;
mod chart;
mod code_editor;
mod components;
mod fold;
mod gallery_view;
mod gateway_view;
mod highlight;
mod icons;
mod layout;
mod mcp_view;
mod notifications;
mod provider_editor;
mod sessions_view;
mod settings_view;
mod shell_menu;
mod shell_support;
mod shortcuts;
mod skills_view;
mod text_input;
mod theme;
mod theme_view;
mod tools_view;
mod usage_view;

use std::borrow::Cow;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use gpui::{
    point, prelude::*, px, size, App, AssetSource, Bounds, SharedString, TitlebarOptions,
    WindowBounds, WindowOptions,
};
use gpui_platform::application;
use ochub_core::db::Database;
use ochub_core::AppState;

use app_ui::AppRoot;

struct Assets {
    base: PathBuf,
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        fs::read(self.base.join(path))
            .map(|data| Some(Cow::Owned(data)))
            .map_err(Into::into)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        fs::read_dir(self.base.join(path))
            .map(|entries| {
                entries
                    .filter_map(|entry| {
                        entry
                            .ok()
                            .and_then(|entry| entry.file_name().into_string().ok())
                            .map(SharedString::from)
                    })
                    .collect()
            })
            .map_err(Into::into)
    }
}

/// Resolve assets from a macOS app bundle first so packaged debug builds do not
/// reach back into a source checkout under a TCC-protected directory.
fn assets_base() -> PathBuf {
    if let Ok(executable) = std::env::current_exe() {
        if let Some(bundled) = bundled_assets_path(&executable).filter(|path| path.is_dir()) {
            return bundled;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets")
}

fn bundled_assets_path(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    Some(contents.join("Resources").join("assets"))
}

#[cfg(test)]
mod asset_path_tests {
    use super::*;

    #[test]
    fn resolves_assets_inside_a_macos_app_bundle() {
        assert_eq!(
            bundled_assets_path(Path::new("/tmp/OCHUB-QA.app/Contents/MacOS/ochub")),
            Some(PathBuf::from("/tmp/OCHUB-QA.app/Contents/Resources/assets"))
        );
    }

    #[test]
    fn ignores_a_bare_debug_executable() {
        assert_eq!(
            bundled_assets_path(Path::new("/workspace/target/debug/ochub")),
            None
        );
    }
}

/// Spawn the axum control API on a dedicated thread with its own tokio runtime,
/// sharing the same `AppState` as the UI.
fn control_api_port() -> u16 {
    std::env::var("MS_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(8787)
}

fn spawn_control_api(app: Arc<AppState>, port: u16) {
    std::thread::Builder::new()
        .name("ochub-server".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    log::error!("failed to build server runtime: {err}");
                    return;
                }
            };
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            if let Err(err) = runtime.block_on(ochub_server::serve_with_app(app, addr)) {
                log::error!("control API server error: {err}");
            }
        })
        .ok();
}

fn main() {
    shell_support::setup_panic_hook();
    env_logger_init();

    let port = control_api_port();
    let _instance_lock = match shell_support::acquire_single_instance(port) {
        Ok(lock) => lock,
        Err(running_port) => {
            println!("OCHUB 已在运行（控制 API 端口 {running_port}），本次启动已退出。");
            return;
        }
    };

    let db = match Database::init() {
        Ok(db) => Arc::new(db),
        Err(err) => {
            log::error!("failed to initialize database: {err}");
            return;
        }
    };
    let app_state = Arc::new(AppState::new(db));
    app_state.bootstrap();

    spawn_control_api(app_state.clone(), port);

    application()
        .with_assets(Assets {
            base: assets_base(),
        })
        .run(move |cx: &mut App| {
            text_input::bind_keys(cx);
            code_editor::bind_keys(cx);
            shortcuts::bind_keys(cx);
            shell_menu::install(app_state.clone(), cx);
            let appearance_settings = ochub_core::settings::get_settings();
            theme::install_selected(
                &appearance_settings.theme_family,
                appearance_settings.theme_mode,
                cx.window_appearance(),
            );
            // Pin to the primary display (avoids landing on a secondary monitor)
            // and use a roomier default size for the denser, redesigned UI.
            let display_id = cx.primary_display().map(|display| display.id());
            let display_bounds: Vec<_> = cx
                .displays()
                .iter()
                .map(|display| display.bounds())
                .collect();
            let bounds = shell_support::load_window_bounds()
                .filter(|bounds| {
                    shell_support::bounds_visible_on_displays(*bounds, &display_bounds)
                })
                .unwrap_or_else(|| Bounds::centered(display_id, size(px(1200.), px(820.)), cx));
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: None,
                        // Content extends behind the native titlebar. Only the macOS
                        // traffic lights remain, embedded directly in the sidebar.
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(18.), px(18.))),
                    }),
                    ..Default::default()
                },
                {
                    let app_state = app_state.clone();
                    move |window, cx| {
                        window
                            .observe_window_appearance(|window, cx| {
                                let settings = ochub_core::settings::get_settings();
                                if settings.theme_mode == ochub_core::settings::ThemeMode::System {
                                    theme::install_selected(
                                        &settings.theme_family,
                                        settings.theme_mode,
                                        window.appearance(),
                                    );
                                    cx.refresh_windows();
                                }
                            })
                            .detach();
                        cx.new(|cx| AppRoot::new(app_state.clone(), cx))
                    }
                },
            );
            let window = match window {
                Ok(window) => window,
                Err(err) => {
                    log::error!("failed to open window: {err}");
                    return;
                }
            };
            window
                .update(cx, |_root, window, cx| {
                    window.on_window_should_close(cx, |window, _cx| {
                        shell_support::save_window_bounds(window.window_bounds().get_bounds());
                        true
                    });
                })
                .ok();
            cx.activate(true);
        });
}

/// Minimal stderr logger so init failures are never silent.
/// Level via `RUST_LOG` (error/warn/info/debug/trace), default info.
struct StderrLogger;

static LOGGER: StderrLogger = StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "[{}] {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

fn env_logger_init() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Info);
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}

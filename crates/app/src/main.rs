//! OCHUB desktop application (GPUI).
//!
//! Initializes the `ochub-core` `AppState` (SQLite store + services), hosts the
//! `ochub-server` axum control API in-process on loopback, and renders the GPUI UI.

mod app_meta;
mod app_settings_view;
mod app_ui;
mod auth_view;
mod chart;
mod code_editor;
mod components;
mod fold;
mod gateway_view;
mod highlight;
mod icons;
mod layout;
mod mcp_view;
mod notifications;
mod prompts_view;
mod provider_editor;
mod proxy_view;
mod sessions_view;
mod settings_view;
mod shell_menu;
mod skills_view;
mod text_input;
mod theme;
mod tools_view;
mod usage_view;
mod workspace_view;

use std::borrow::Cow;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
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

/// Spawn the axum control API on a dedicated thread with its own tokio runtime,
/// sharing the same `AppState` as the UI.
fn spawn_control_api(app: Arc<AppState>) {
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
            let port: u16 = std::env::var("MS_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8787);
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            if let Err(err) = runtime.block_on(ochub_server::serve_with_app(app, addr)) {
                log::error!("control API server error: {err}");
            }
        })
        .ok();
}

fn main() {
    env_logger_init();

    let db = match Database::init() {
        Ok(db) => Arc::new(db),
        Err(err) => {
            log::error!("failed to initialize database: {err}");
            return;
        }
    };
    let app_state = Arc::new(AppState::new(db));
    app_state.bootstrap();

    spawn_control_api(app_state.clone());

    application()
        .with_assets(Assets {
            base: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets"),
        })
        .run(move |cx: &mut App| {
            text_input::bind_keys(cx);
            code_editor::bind_keys(cx);
            shell_menu::install(app_state.clone(), cx);
            // Pin to the primary display (avoids landing on a secondary monitor)
            // and use a roomier default size for the denser, redesigned UI.
            let display_id = cx.primary_display().map(|display| display.id());
            let bounds = Bounds::centered(display_id, size(px(1200.), px(820.)), cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("OCHUB".into()),
                        // Blend the titlebar into our own chrome (Surge-style unified
                        // toolbar). We draw a custom draggable top bar in `app_ui`.
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(18.), px(18.))),
                    }),
                    ..Default::default()
                },
                {
                    let app_state = app_state.clone();
                    move |_, cx| cx.new(|cx| AppRoot::new(app_state.clone(), cx))
                },
            );
            if let Err(err) = window {
                log::error!("failed to open window: {err}");
                return;
            }
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

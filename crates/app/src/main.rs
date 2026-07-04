//! RouteDeck desktop application (GPUI).
//!
//! Initializes the `routedeck-core` `AppState` (SQLite store + services), hosts the
//! `routedeck-server` axum control API in-process on loopback, and renders the GPUI UI.

mod app_settings_view;
mod app_ui;
mod auth_view;
mod chart;
mod components;
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
mod universal_view;
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
use routedeck_core::db::Database;
use routedeck_core::AppState;

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
        .name("routedeck-server".into())
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
            if let Err(err) = runtime.block_on(routedeck_server::serve_with_app(app, addr)) {
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
            shell_menu::install(app_state.clone(), cx);
            // Pin to the primary display (avoids landing on a secondary monitor)
            // and use a roomier default size for the denser, redesigned UI.
            let display_id = cx.primary_display().map(|display| display.id());
            let bounds = Bounds::centered(display_id, size(px(1200.), px(820.)), cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("RouteDeck".into()),
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

fn env_logger_init() {
    // GPUI brings its own logging expectations; keep this minimal and resilient.
    let _ = std::panic::catch_unwind(|| {
        if std::env::var("RUST_LOG").is_err() {
            std::env::set_var("RUST_LOG", "info");
        }
    });
}

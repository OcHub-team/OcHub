//! OcHub desktop application (GPUI).
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
mod i18n;
mod icons;
mod layout;
mod mcp_view;
mod notifications;
mod provider_editor;
mod scrollbar;
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
use std::io;
use std::net::{SocketAddr, TcpListener};
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

use app_ui::{AppRoot, StartupNotice};

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

fn control_api_port() -> u16 {
    parse_control_api_port(std::env::var("MS_PORT").ok().as_deref())
}

fn parse_control_api_port(value: Option<&str>) -> u16 {
    value
        .and_then(|port| port.trim().parse().ok())
        .filter(|port| *port != 0)
        .unwrap_or(8787)
}

fn bind_control_api(port: u16) -> std::result::Result<TcpListener, StartupNotice> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).map_err(|err| {
        if err.kind() == io::ErrorKind::AddrInUse {
            StartupNotice::new(
                "控制 API 未启动",
                format!(
                    "端口 {port} 已被其他进程占用。OcHub 界面与转发站仍可使用；依赖控制 API 的外部集成暂不可用。请关闭占用程序后重启 OcHub，或使用 MS_PORT 指定其他端口。"
                ),
            )
        } else {
            StartupNotice::new(
                "控制 API 未启动",
                format!(
                    "无法监听 127.0.0.1:{port}：{err}。OcHub 界面与转发站仍可使用，但依赖控制 API 的外部集成暂不可用。"
                ),
            )
        }
    })?;
    listener.set_nonblocking(true).map_err(|err| {
        StartupNotice::new(
            "控制 API 未启动",
            format!(
                "无法配置 127.0.0.1:{port} 的监听器：{err}。OcHub 界面与转发站仍可使用，但依赖控制 API 的外部集成暂不可用。"
            ),
        )
    })?;
    Ok(listener)
}

/// Spawn gateway autostart and, when available, the already-bound control API
/// listener on one dedicated tokio runtime. Binding happens synchronously in
/// [`bind_control_api`], so the UI never mistakes a failed listener for ready.
fn spawn_app_services(app: Arc<AppState>, control_listener: Option<TcpListener>) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;
    std::thread::Builder::new()
        .name("ochub-server".into())
        .spawn(move || {
            runtime.block_on(async move {
                app.gateway.maybe_autostart().await;
                if let Some(listener) = control_listener {
                    if let Err(err) = ochub_server::serve_with_app_on_listener(app, listener).await
                    {
                        log::error!("control API server error: {err}");
                    }
                }
            });
        })
        .map(|_| ())
}

#[cfg(test)]
mod control_api_startup_tests {
    use super::*;

    #[test]
    fn invalid_or_ephemeral_configured_ports_fall_back_to_default() {
        assert_eq!(parse_control_api_port(None), 8787);
        assert_eq!(parse_control_api_port(Some("")), 8787);
        assert_eq!(parse_control_api_port(Some("invalid")), 8787);
        assert_eq!(parse_control_api_port(Some("0")), 8787);
        assert_eq!(parse_control_api_port(Some(" 9191 ")), 9191);
    }

    #[test]
    fn occupied_port_returns_a_degraded_mode_notice() {
        let blocker = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .expect("bind blocking listener");
        let port = blocker.local_addr().expect("blocking address").port();

        let notice = bind_control_api(port).expect_err("port conflict must be reported");
        assert_eq!(notice.title, "控制 API 未启动");
        assert!(notice.message.contains(&format!("端口 {port}")));
        assert!(notice.message.contains("其他进程占用"));
        assert!(notice.message.contains("界面与转发站仍可使用"));
    }

    #[test]
    fn available_port_is_reserved_before_the_ui_starts() {
        let probe =
            TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind port probe");
        let port = probe.local_addr().expect("probe address").port();
        drop(probe);

        let listener = bind_control_api(port).expect("reserve available port");
        assert_eq!(
            listener.local_addr().expect("listener address").port(),
            port
        );
        assert!(TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).is_err());
    }
}

/// Whether this process was started by its login item rather than by the user.
///
/// `ochub_core::autostart` registers [`SILENT_ARG`] on the login item, so its
/// presence in argv is the signal. `--hidden` is accepted too because that is
/// the spelling macOS login items use when registered through AppleScript.
///
/// [`SILENT_ARG`]: ochub_core::autostart::SILENT_ARG
fn launched_by_login_item<I>(args: I) -> bool
where
    I: IntoIterator<Item = String>,
{
    args.into_iter()
        .skip(1)
        .any(|arg| arg == ochub_core::autostart::SILENT_ARG || arg == "--hidden")
}

#[cfg(test)]
mod silent_startup_tests {
    use super::*;

    fn args(rest: &[&str]) -> Vec<String> {
        std::iter::once("/path/to/ochub".to_string())
            .chain(rest.iter().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn detects_the_login_item_flag() {
        assert!(launched_by_login_item(args(&["--silent"])));
        assert!(launched_by_login_item(args(&["--hidden"])));
        assert!(launched_by_login_item(args(&["--other", "--silent"])));
    }

    #[test]
    fn a_plain_launch_is_not_silent() {
        assert!(!launched_by_login_item(args(&[])));
        assert!(!launched_by_login_item(args(&["--verbose"])));
    }

    #[test]
    fn the_executable_path_is_never_treated_as_a_flag() {
        assert!(!launched_by_login_item(vec!["--silent".to_string()]));
    }
}

/// Keep the process-level quit policy in step with the close-behaviour setting.
///
/// With "keep running on close" on, the app must outlive its windows; with it
/// off, closing the last window is the user's quit gesture — and on macOS
/// gpui's default is to stay alive, so that case has to be asked for
/// explicitly. Re-run this whenever the setting changes.
pub(crate) fn apply_quit_mode(cx: &mut App) {
    let keep_running = ochub_core::settings::get_settings().minimize_to_tray_on_close;
    cx.set_quit_mode(if keep_running {
        gpui::QuitMode::Explicit
    } else {
        gpui::QuitMode::LastWindowClosed
    });
}

fn main() {
    shell_support::setup_panic_hook();
    env_logger_init();

    let port = control_api_port();
    let (_instance_lock, mut activation_rx) = match shell_support::acquire_single_instance(port) {
        Ok(shell_support::InstanceAcquire::Acquired {
            lock,
            activation_server,
        }) => {
            let activation_rx = match activation_server.start() {
                Ok(receiver) => receiver,
                Err(err) => {
                    log::error!("failed to start instance activation listener: {err}");
                    return;
                }
            };
            (lock, activation_rx)
        }
        Ok(shell_support::InstanceAcquire::AlreadyRunning(existing)) => {
            let port = existing
                .control_port
                .map(|port| port.to_string())
                .unwrap_or_else(|| "未知".to_string());
            let pid = existing
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "未知".to_string());
            if existing.activation_requested {
                println!("OcHub 已在运行（PID {pid}，控制 API 端口 {port}），已请求显示现有窗口。");
            } else {
                println!(
                    "OcHub 已在运行（PID {pid}，控制 API 端口 {port}），但无法请求显示现有窗口。"
                );
            }
            return;
        }
        Err(err) => {
            log::error!("failed to acquire single-instance lock: {err}");
            return;
        }
    };

    let (control_listener, mut startup_notice) = match bind_control_api(port) {
        Ok(listener) => (Some(listener), None),
        Err(notice) => {
            log::warn!("{}: {}", notice.title, notice.message);
            (None, Some(notice))
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

    if let Err(err) = spawn_app_services(app_state.clone(), control_listener) {
        log::error!("failed to start application services: {err}");
        startup_notice = Some(StartupNotice::new(
            "后台服务未启动",
            format!(
                "无法启动 OcHub 后台服务线程：{err}。界面仍可浏览，但控制 API 与转发站自动启动均不可用；请重启 OcHub。"
            ),
        ));
    }

    application()
        .with_assets(Assets {
            base: assets_base(),
        })
        .run(move |cx: &mut App| {
            text_input::bind_keys(cx);
            code_editor::bind_keys(cx);
            shortcuts::bind_keys(cx);
            shell_menu::install(app_state.clone(), cx);
            apply_quit_mode(cx);
            let appearance_settings = ochub_core::settings::get_settings();
            ochub_core::i18n::install(ochub_core::i18n::resolve(
                appearance_settings.language.as_deref(),
            ));
            // Gate on the stored setting as well as the flag, so a stale login
            // item cannot keep hiding the window after the user turns it off.
            let start_hidden = launched_by_login_item(std::env::args().collect::<Vec<_>>())
                && appearance_settings.silent_startup;
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
                    // Create the window but never order it in: the menu bar,
                    // Dock menu and activation channel can all surface it
                    // later, and every observer set up below stays valid.
                    show: !start_hidden,
                    focus: !start_hidden,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(960.), px(640.))),
                    window_background: theme::window_background_appearance(),
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
                    let startup_notice = startup_notice.clone();
                    move |window, cx| {
                        window
                            .observe_window_appearance(|window, cx| {
                                let settings = ochub_core::settings::get_settings();
                                if settings.theme_mode == ochub_core::settings::ThemeMode::System
                                    && !theme::is_previewing()
                                {
                                    theme::install_selected(
                                        &settings.theme_family,
                                        settings.theme_mode,
                                        window.appearance(),
                                    );
                                    theme::apply_window_background(window);
                                }
                                cx.refresh_windows();
                            })
                            .detach();
                        cx.new(|cx| AppRoot::new(app_state.clone(), startup_notice.clone(), cx))
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
                    window.on_window_should_close(cx, |window, cx| {
                        shell_support::save_window_bounds(window.window_bounds().get_bounds());
                        // Read fresh rather than capturing, so toggling the
                        // setting takes effect without a restart.
                        if !ochub_core::settings::get_settings().minimize_to_tray_on_close {
                            return true;
                        }
                        // Keep the root window alive while background services
                        // continue running. The native close button must match
                        // the CloseWindow action; otherwise the last window is
                        // destroyed and a later activation has nothing to show.
                        #[cfg(target_os = "macos")]
                        {
                            cx.hide();
                            false
                        }
                        #[cfg(any(target_os = "windows", target_os = "linux"))]
                        {
                            window.minimize_window();
                            false
                        }
                        #[cfg(not(any(
                            target_os = "macos",
                            target_os = "windows",
                            target_os = "linux"
                        )))]
                        {
                            true
                        }
                    });
                })
                .ok();
            // gpui's shutdown clears its window map directly and never consults
            // `on_window_should_close`, so quitting (Cmd-Q, or closing with the
            // setting off) would otherwise never persist the window bounds.
            cx.on_app_quit(move |cx| {
                window
                    .update(cx, |_root, window, _cx| {
                        shell_support::save_window_bounds(window.window_bounds().get_bounds());
                    })
                    .ok();
                async {}
            })
            .detach();
            cx.spawn(async move |cx| {
                while activation_rx.recv().await.is_some() {
                    cx.update(shell_menu::activate_first_window);
                }
            })
            .detach();
            if !start_hidden {
                cx.activate(true);
            }
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

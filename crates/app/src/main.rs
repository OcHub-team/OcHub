#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! OcHub desktop application (GPUI).
//!
//! Initializes the `ochub-core` `AppState` (SQLite store + services), hosts the
//! `ochub-server` axum control API in-process on loopback, and renders the GPUI UI.

mod about_view;
mod app_meta;
mod app_settings_view;
mod app_ui;
mod chart;
mod code_editor;
mod components;
mod core_async;
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
    point, prelude::*, px, size, App, AssetSource, Bounds, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};
use gpui_platform::application;
use ochub_core::db::Database;
use ochub_core::AppState;

use app_ui::{AppRoot, StartupNotice};
use i18n::{k, raw};

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

/// Resolve packaged assets before falling back to the source checkout.
///
/// `cargo-packager` places resources in different roots per platform:
/// macOS uses `Contents/Resources`, Windows keeps them beside the executable,
/// and Linux uses `/usr/lib/<binary>` (inside `APPDIR` for AppImage).
fn assets_base() -> PathBuf {
    if let Ok(executable) = std::env::current_exe() {
        let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
        if let Some(packaged) = packaged_assets_paths(&executable, appdir.as_deref())
            .into_iter()
            .find(|path| path.is_dir())
        {
            return packaged;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets")
}

fn packaged_assets_paths(executable: &Path, appdir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(bundled) = bundled_assets_path(executable) {
        paths.push(bundled);
    }

    if let Some(executable_dir) = executable.parent() {
        paths.push(executable_dir.join("assets"));
    }

    if let Some(executable_name) = executable.file_name() {
        if let Some(appdir) = appdir {
            paths.push(
                appdir
                    .join("usr")
                    .join("lib")
                    .join(executable_name)
                    .join("assets"),
            );
        }
        paths.push(
            Path::new("/usr")
                .join("lib")
                .join(executable_name)
                .join("assets"),
        );
    }
    paths
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

    #[test]
    fn resolves_assets_beside_a_windows_or_portable_executable() {
        let paths = packaged_assets_paths(Path::new("/opt/ochub/ochub.exe"), None);
        assert_eq!(paths[0], PathBuf::from("/opt/ochub/assets"));
    }

    #[test]
    fn resolves_deb_and_appimage_resource_roots() {
        let paths = packaged_assets_paths(
            Path::new("/tmp/.mount_OcHub/usr/bin/ochub"),
            Some(Path::new("/tmp/.mount_OcHub")),
        );
        assert!(paths.contains(&PathBuf::from("/tmp/.mount_OcHub/usr/lib/ochub/assets")));
        assert!(paths.contains(&PathBuf::from("/usr/lib/ochub/assets")));
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

/// Reserve the control API port, reporting a degradation rather than a message.
///
/// This runs before the UI, so it cannot produce translated text: the notice
/// names the condition and carries the port and OS error, and the banner
/// renders it once a locale exists.
fn bind_control_api(port: u16) -> std::result::Result<TcpListener, StartupNotice> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).map_err(|err| {
        if err.kind() == io::ErrorKind::AddrInUse {
            StartupNotice::ControlApiPortInUse { port }
        } else {
            StartupNotice::ControlApiBindFailed {
                port,
                error: err.to_string(),
            }
        }
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|err| StartupNotice::ControlApiListenerFailed {
            port,
            error: err.to_string(),
        })?;
    Ok(listener)
}

/// Spawn gateway autostart and, when available, the already-bound control API
/// listener on one dedicated tokio runtime. Binding happens synchronously in
/// [`bind_control_api`], so the UI never mistakes a failed listener for ready.
fn spawn_app_services(app: Arc<AppState>, control_listener: Option<TcpListener>) -> io::Result<()> {
    // Parks on the shared runtime rather than building a private one, so the
    // UI and the server drive their futures on the same reactor.
    let handle = core_async::handle().clone();
    std::thread::Builder::new()
        .name("ochub-server".into())
        .spawn(move || {
            handle.block_on(async move {
                ochub_core::services::pricing_catalog::start_background_pricing_sync(
                    app.db.clone(),
                );
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
        assert_eq!(notice, StartupNotice::ControlApiPortInUse { port });
        // The notice is deliberately not a sentence, so assert on the one thing
        // the rendered text must still carry. The port reads the same in every
        // locale, so this holds whichever one happens to be installed.
        let message = notice.message();
        assert!(message.contains(&port.to_string()), "{message}");
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

/// Apply the platform-specific "closed, but still running" presentation.
///
/// macOS keeps a hidden application reachable through either the Dock or the
/// optional status item. Windows only removes the taskbar button when the
/// notification-area icon was created successfully; otherwise minimizing
/// leaves a recovery path. Linux retains the existing minimize behaviour.
fn keep_main_window_in_background(_window: &Window, _cx: &mut App) {
    #[cfg(target_os = "macos")]
    _cx.hide();

    #[cfg(target_os = "windows")]
    {
        if shell_menu::tray_resident_active(_cx) {
            if !set_windows_window_visible(_window, false) {
                _window.minimize_window();
            }
        } else {
            _window.minimize_window();
        }
    }

    #[cfg(target_os = "linux")]
    _window.minimize_window();

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    _window.minimize_window();
}

/// Close the root window exactly as the native close button would.
///
/// Keeping this at application scope makes Cmd/Ctrl-W work even when no GPUI
/// element currently owns keyboard focus.
pub(crate) fn close_main_window(cx: &mut App) {
    let Some(handle) = cx.windows().into_iter().next() else {
        return;
    };
    let _ = handle.update(cx, |_root, window, cx| {
        shell_support::save_window_bounds(window.window_bounds().get_bounds());
        if ochub_core::settings::get_settings().minimize_to_tray_on_close {
            keep_main_window_in_background(window, cx);
        } else {
            window.remove_window();
        }
    });
}

#[cfg(target_os = "windows")]
pub(crate) fn set_windows_window_visible(window: &Window, visible: bool) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_RESTORE};

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return false;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
    unsafe {
        let _ = ShowWindow(hwnd, if visible { SW_RESTORE } else { SW_HIDE });
    }
    true
}

fn main() {
    if version_requested(std::env::args_os()) {
        println!("OcHub {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    shell_support::setup_panic_hook();
    env_logger_init();
    // Startup can print and log before a window ever opens — a second launch
    // reports the running instance and exits right here. Resolve the locale from
    // the persisted setting first so those lines are in the user's language;
    // the UI re-resolves nothing, it simply reads the same installed locale.
    ochub_core::i18n::install(ochub_core::i18n::resolve(
        ochub_core::settings::get_settings().language.as_deref(),
    ));

    // Every crossing from the UI into ochub-core's async surface needs this,
    // so it must exist before any of them can be reached.
    if let Err(err) = core_async::init() {
        log::error!("failed to build the shared async runtime: {err}");
        return;
    }

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
            let unknown = || raw(k::STARTUP_INSTANCE_UNKNOWN).to_string();
            let port = existing
                .control_port
                .map(|port| port.to_string())
                .unwrap_or_else(unknown);
            let pid = existing
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(unknown);
            // Read by whoever launched this second copy from a terminal, so it
            // follows the persisted language setting rather than a running UI.
            println!(
                "{}",
                if existing.activation_requested {
                    tf!(k::STARTUP_INSTANCE_ACTIVATED, pid = pid, port = port)
                } else {
                    tf!(
                        k::STARTUP_INSTANCE_ACTIVATION_FAILED,
                        pid = pid,
                        port = port
                    )
                }
            );
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
            log::warn!("{}: {}", notice.title(), notice.message());
            (None, Some(notice))
        }
    };

    let asset_root = assets_base();
    let db = match Database::init() {
        Ok(db) => Arc::new(db),
        Err(err) => {
            log::error!("failed to initialize database: {err}");
            return;
        }
    };
    let bundled_pricing_path = asset_root.join("data/litellm-model-prices.json");
    match fs::read_to_string(&bundled_pricing_path) {
        Ok(snapshot) => match db.install_bundled_pricing_catalog(&snapshot) {
            Ok(outcome) if outcome.installed => log::info!(
                "installed bundled LiteLLM pricing catalog: {} entries at {}",
                outcome.entry_count,
                outcome.source_revision
            ),
            Ok(_) => {}
            Err(err) => log::warn!(
                "failed to install bundled LiteLLM pricing catalog from {}: {err}",
                bundled_pricing_path.display()
            ),
        },
        Err(err) => log::warn!(
            "bundled LiteLLM pricing catalog is unavailable at {}: {err}",
            bundled_pricing_path.display()
        ),
    }
    let app_state = Arc::new(AppState::new(db));
    app_state.bootstrap();

    if let Err(err) = spawn_app_services(app_state.clone(), control_listener) {
        log::error!("failed to start application services: {err}");
        startup_notice = Some(StartupNotice::ServicesUnavailable {
            error: err.to_string(),
        });
    }

    application()
        .with_assets(Assets { base: asset_root })
        .run(move |cx: &mut App| {
            text_input::bind_keys(cx);
            code_editor::bind_keys(cx);
            shortcuts::bind_keys(cx);
            layout::bind_keys(cx);
            shell_menu::install(app_state.clone(), cx);
            apply_quit_mode(cx);
            // The locale is already installed (see the top of `main`); this only
            // needs the appearance and startup fields.
            let appearance_settings = ochub_core::settings::get_settings();
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
                    window.on_window_should_close(cx, |window, _cx| {
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
                        keep_main_window_in_background(window, _cx);
                        false
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

fn version_requested<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    args.into_iter()
        .skip(1)
        .any(|arg| arg.as_ref() == "--version" || arg.as_ref() == "-V")
}

#[cfg(test)]
mod version_arg_tests {
    use super::version_requested;

    #[test]
    fn recognizes_version_flags_after_the_executable() {
        assert!(version_requested(["ochub", "--version"]));
        assert!(version_requested(["ochub", "-V"]));
        assert!(!version_requested(["ochub"]));
        assert!(!version_requested(["--version"]));
    }
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

//! App-shell robustness helpers: panic-report hook, window-bounds persistence,
//! single-instance lock, and first-run notice state. Kept out of `main.rs` so
//! the shell wiring there stays readable.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::panic;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{point, px, size, Bounds, Pixels};
use serde::{Deserialize, Serialize};

use ochub_core::paths::get_app_config_dir;
use ochub_core::settings;

/// App version (from the app crate's Cargo.toml).
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Panic hook — crash report to `<app_config_dir>/crash.log`. Mirrors cc-switch
// `panic_hook.rs`: timestamp + system info + panic message + location + backtrace.
// ---------------------------------------------------------------------------

fn crash_log_path() -> PathBuf {
    get_app_config_dir().join("crash.log")
}

/// Environment snapshot for the crash report (never panics).
fn system_info() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let family = std::env::consts::FAMILY;
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("unnamed").to_string();
    let thread_id = format!("{:?}", thread.id());
    format!(
        "OS: {os} ({family})\n\
         Arch: {arch}\n\
         App Version: {APP_VERSION}\n\
         Working Dir: {cwd}\n\
         Thread: {thread_name} (ID: {thread_id})"
    )
}

/// Install a panic hook that writes a crash report before the default hook runs.
/// Call once, as early as possible in `main`.
pub fn setup_panic_hook() {
    // Ensure a backtrace is captured even in release builds.
    if std::env::var("RUST_BACKTRACE").is_err() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    let default_hook = panic::take_hook();

    panic::set_hook(Box::new(move |panic_info| {
        let log_path = crash_log_path();
        if let Some(parent) = log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        // Guard the time formatting so a nested panic can't mask the report.
        let timestamp = panic::catch_unwind(|| {
            chrono::Local::now()
                .format("%Y-%m-%d %H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| format!("unix:{}.{:03}", d.as_secs(), d.subsec_millis()))
                .unwrap_or_else(|_| "unknown".to_string())
        });

        let system_info = panic::catch_unwind(system_info)
            .unwrap_or_else(|_| "Failed to get system info".to_string());

        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            format!("{panic_info}")
        };

        let location = if let Some(loc) = panic_info.location() {
            format!(
                "File: {}\n         Line: {}\n         Column: {}",
                loc.file(),
                loc.line(),
                loc.column()
            )
        } else {
            "Unknown location".to_string()
        };

        let backtrace = std::backtrace::Backtrace::force_capture();
        let separator = "=".repeat(80);
        let sub = "-".repeat(40);
        let entry = format!(
            "\n{separator}\n[CRASH REPORT] {timestamp}\n{separator}\n\n\
             {sub}\nSystem Information\n{sub}\n{system_info}\n\n\
             {sub}\nError Details\n{sub}\nMessage: {message}\n\nLocation: {location}\n\n\
             {sub}\nStack Trace (Backtrace)\n{sub}\n{backtrace}\n\n{separator}\n"
        );

        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = file.write_all(entry.as_bytes());
            let _ = file.flush();
            eprintln!("\n[OCHub] Crash log saved to: {}", log_path.display());
        }
        eprintln!("{entry}");

        default_hook(panic_info);
    }));
}

// ---------------------------------------------------------------------------
// Window-bounds persistence — device-level, following the settings.json pattern
// (a small JSON file under the app config dir). Kept separate from `AppSettings`
// so it stays entirely within the app crate.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowState {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn window_state_path() -> PathBuf {
    get_app_config_dir().join("window.json")
}

/// Restore the last persisted window bounds, if any and sane. Returns `None`
/// (fall back to the centered default) when the file is missing or invalid.
pub fn load_window_bounds() -> Option<Bounds<Pixels>> {
    let content = fs::read_to_string(window_state_path()).ok()?;
    let state: WindowState = serde_json::from_str(&content).ok()?;
    // Reject garbage / degenerate sizes so we never restore an unusable window.
    let sane = [state.x, state.y, state.width, state.height]
        .iter()
        .all(|v| v.is_finite())
        && state.width >= 400.0
        && state.height >= 300.0;
    if !sane {
        return None;
    }
    Some(Bounds::new(
        point(px(state.x), px(state.y)),
        size(px(state.width), px(state.height)),
    ))
}

/// Minimum overlap (in pixels, on each axis) a restored window must share with a
/// connected display for us to trust it. Enough to keep the draggable title bar
/// reachable so the user can always reposition an otherwise-offscreen window.
const MIN_VISIBLE_MARGIN: f32 = 50.0;

/// Returns `true` if `bounds` overlaps at least one of the connected
/// `display_bounds` by `MIN_VISIBLE_MARGIN` on both axes. Guards against
/// restoring a window onto a monitor that has since been unplugged or moved,
/// which would otherwise leave it permanently off-screen. When no displays are
/// reported we conservatively accept the bounds (nothing to validate against).
pub fn bounds_visible_on_displays(
    bounds: Bounds<Pixels>,
    display_bounds: &[Bounds<Pixels>],
) -> bool {
    if display_bounds.is_empty() {
        return true;
    }
    display_bounds.iter().any(|display| {
        let overlap = bounds.intersect(display);
        f32::from(overlap.size.width) >= MIN_VISIBLE_MARGIN
            && f32::from(overlap.size.height) >= MIN_VISIBLE_MARGIN
    })
}

/// Persist the window bounds (best-effort; failures are logged, not fatal).
pub fn save_window_bounds(bounds: Bounds<Pixels>) {
    let state = WindowState {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    };
    let path = window_state_path();
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            log::warn!("创建窗口状态目录失败: {err}");
            return;
        }
    }
    match serde_json::to_string_pretty(&state) {
        Ok(json) => {
            if let Err(err) = fs::write(&path, json) {
                log::warn!("保存窗口状态失败: {err}");
            }
        }
        Err(err) => log::warn!("序列化窗口状态失败: {err}"),
    }
}

// ---------------------------------------------------------------------------
// Single-instance guard — a lock file holding the running instance's control
// API port. On a second launch, if that port answers `GET /api/health` the
// existing instance is alive; otherwise the lock is stale and we take it over.
// ---------------------------------------------------------------------------

fn lock_file_path() -> PathBuf {
    get_app_config_dir().join("ochub.lock")
}

/// Removes the lock file on drop so a clean shutdown doesn't leave a stale lock.
/// (An unclean exit leaves the file behind; the health probe handles that case.)
pub struct InstanceLock {
    path: PathBuf,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Probe whether a control API is alive on `port` by issuing a raw
/// `GET /api/health` and checking for a 200 status line. Uses std sockets to
/// avoid pulling an HTTP client into the app crate.
fn instance_is_alive(port: u16) -> bool {
    let Ok(addr) = format!("127.0.0.1:{port}").parse::<SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(400)));
    let req =
        format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 256];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => String::from_utf8_lossy(&buf[..n]).contains("200"),
        _ => false,
    }
}

/// Try to acquire the single-instance lock for this process's control `port`.
///
/// - `Ok(lock)`: we own the instance; keep `lock` alive for the process lifetime.
/// - `Err(port)`: another live instance already holds the lock on that port.
pub fn acquire_single_instance(port: u16) -> Result<InstanceLock, u16> {
    let path = lock_file_path();

    if let Ok(existing) = fs::read_to_string(&path) {
        if let Ok(existing_port) = existing.trim().parse::<u16>() {
            if instance_is_alive(existing_port) {
                return Err(existing_port);
            }
            // Stale lock (previous instance gone): fall through and take over.
        }
    }

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(err) = fs::write(&path, port.to_string()) {
        // If we can't write the lock, don't block startup — just skip the guard.
        log::warn!("写入单实例锁文件失败: {err}");
    }
    Ok(InstanceLock { path })
}

// ---------------------------------------------------------------------------
// First-run notice — reads/writes the device-level `first_run_notice_confirmed`
// flag via the settings service. UI is rendered by the app root.
// ---------------------------------------------------------------------------

/// Whether the first-run notice still needs to be shown (flag not yet `true`).
pub fn first_run_notice_pending() -> bool {
    settings::get_settings().first_run_notice_confirmed != Some(true)
}

/// Persist acknowledgement of the first-run notice.
pub fn confirm_first_run_notice() {
    if let Err(err) = settings::mutate_settings(|s| s.first_run_notice_confirmed = Some(true)) {
        log::warn!("保存首次运行提示确认状态失败: {err}");
    }
}

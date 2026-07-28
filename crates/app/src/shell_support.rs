//! App-shell robustness helpers: panic-report hook, window-bounds persistence,
//! single-instance lock, and first-run notice state. Kept out of `main.rs` so
//! the shell wiring there stays readable.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic;
use std::path::PathBuf;
use std::time::Duration;

use fs2::FileExt;
use gpui::{Bounds, Pixels, point, px, size};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

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
    // The report below captures its own backtrace with `force_capture`, which
    // ignores RUST_BACKTRACE, so this hook does not need the variable set. It
    // used to set it here; all that bought was a second copy of the trace from
    // the default hook we forward to.
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
            eprintln!("\n[OcHub] Crash log saved to: {}", log_path.display());
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
    if let Some(parent) = path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        log::warn!("创建窗口状态目录失败: {err}");
        return;
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
// Single-instance guard — an OS-backed exclusive file lock plus a dedicated
// loopback activation channel.
// ---------------------------------------------------------------------------

fn lock_file_path() -> PathBuf {
    get_app_config_dir().join("ochub.lock")
}

const INSTANCE_PROTOCOL_VERSION: u8 = 1;
const ACTIVATION_COMMAND: &str = "activate";
const ACTIVATION_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct InstanceRecord {
    protocol_version: u8,
    pid: u32,
    activation_port: u16,
    token: String,
}

/// Keeps the operating-system file lock alive for the process lifetime. The
/// metadata file intentionally remains on disk after exit; the OS releases the
/// actual lock even after a crash, so stale metadata is harmless and can be
/// atomically replaced by the next owner.
pub struct InstanceLock {
    _file: File,
}

pub struct ActivationServer {
    listener: TcpListener,
    token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExistingInstance {
    pub pid: Option<u32>,
    pub activation_requested: bool,
}

pub enum InstanceAcquire {
    Acquired {
        lock: InstanceLock,
        activation_server: ActivationServer,
    },
    AlreadyRunning(ExistingInstance),
}

impl ActivationServer {
    /// Start accepting authenticated activation requests. The receiver is
    /// consumed by GPUI after the first window opens; requests arriving during
    /// database/bootstrap work remain queued.
    pub fn start(self) -> std::io::Result<UnboundedReceiver<()>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("ochub-activation".into())
            .spawn(move || activation_loop(self.listener, self.token, sender))?;
        Ok(receiver)
    }
}

fn activation_loop(listener: TcpListener, token: String, sender: UnboundedSender<()>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(ACTIVATION_TIMEOUT));
        let _ = stream.set_write_timeout(Some(ACTIVATION_TIMEOUT));

        let mut line = String::new();
        let read_ok = {
            let mut reader = BufReader::new(&mut stream);
            reader.read_line(&mut line).is_ok()
        };
        let expected = format!("{ACTIVATION_COMMAND} {token}\n");
        if read_ok && line == expected {
            let _ = sender.send(());
            let _ = stream.write_all(b"ok\n");
        } else {
            let _ = stream.write_all(b"error\n");
        }
    }
}

fn read_instance_record(file: &mut File) -> Option<InstanceRecord> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut content = String::new();
    file.read_to_string(&mut content).ok()?;
    serde_json::from_str(content.trim()).ok()
}

fn write_instance_record(file: &mut File, record: &InstanceRecord) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer(&mut *file, record).map_err(std::io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_data()
}

fn request_activation(record: &InstanceRecord) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], record.activation_port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, ACTIVATION_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(ACTIVATION_TIMEOUT));
    let _ = stream.set_write_timeout(Some(ACTIVATION_TIMEOUT));
    let request = format!("{ACTIVATION_COMMAND} {}\n", record.token);
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = [0u8; 16];
    matches!(stream.read(&mut response), Ok(n) if &response[..n] == b"ok\n")
}

#[cfg(target_os = "macos")]
fn request_native_activation(pid: u32) -> bool {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};

    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let Some(application) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
    else {
        return false;
    };

    // Send both requests. `unhide` restores an app hidden by the window close
    // action, while ActivateAllWindows brings every existing window forward.
    // This originates in the second process, so it does not depend on the
    // hidden app's GPUI event loop processing the activation channel first.
    let unhidden = application.unhide();
    let activated =
        application.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    unhidden || activated
}

#[cfg(not(target_os = "macos"))]
fn request_native_activation(_pid: u32) -> bool {
    false
}

fn acquire_single_instance_at(path: PathBuf) -> std::io::Result<InstanceAcquire> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
            let activation_port = listener.local_addr()?.port();
            let token = Uuid::new_v4().to_string();
            let record = InstanceRecord {
                protocol_version: INSTANCE_PROTOCOL_VERSION,
                pid: std::process::id(),
                activation_port,
                token: token.clone(),
            };
            write_instance_record(&mut file, &record)?;
            Ok(InstanceAcquire::Acquired {
                lock: InstanceLock { _file: file },
                activation_server: ActivationServer { listener, token },
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            let record = read_instance_record(&mut file)
                .filter(|record| record.protocol_version == INSTANCE_PROTOCOL_VERSION);
            let activation_requested = record.as_ref().is_some_and(|record| {
                let channel_requested = request_activation(record);
                let native_requested = request_native_activation(record.pid);
                channel_requested || native_requested
            });
            Ok(InstanceAcquire::AlreadyRunning(ExistingInstance {
                pid: record.as_ref().map(|record| record.pid),
                activation_requested,
            }))
        }
        Err(err) => Err(err),
    }
}

/// Try to acquire the process-wide OcHub instance lock. A live owner is
/// contacted through its dedicated activation channel.
pub fn acquire_single_instance() -> std::io::Result<InstanceAcquire> {
    acquire_single_instance_at(lock_file_path())
}

// ---------------------------------------------------------------------------
// First-run notice — reads/writes the device-level `first_run_notice_confirmed`
// flag via the settings service. UI is rendered by the app root.
// ---------------------------------------------------------------------------

/// Whether the first-run notice still needs to be shown (flag not yet `true`).
pub fn first_run_notice_pending() -> bool {
    settings::get_settings().first_run_notice_confirmed != Some(true)
}

#[cfg(test)]
mod instance_lock_tests {
    use super::*;

    fn acquired(result: InstanceAcquire) -> (InstanceLock, ActivationServer) {
        match result {
            InstanceAcquire::Acquired {
                lock,
                activation_server,
            } => (lock, activation_server),
            InstanceAcquire::AlreadyRunning(existing) => {
                panic!("expected lock acquisition, got existing instance: {existing:?}")
            }
        }
    }

    #[test]
    fn second_instance_requests_activation_instead_of_starting() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("ochub.lock");
        let (_lock, activation_server) =
            acquired(acquire_single_instance_at(path.clone()).expect("first instance"));
        let mut activation_rx = activation_server.start().expect("activation server");

        let second = acquire_single_instance_at(path).expect("second instance probe");
        let InstanceAcquire::AlreadyRunning(existing) = second else {
            panic!("second instance must not acquire the lock");
        };
        assert_eq!(existing.pid, Some(std::process::id()));
        assert!(existing.activation_requested);
        assert_eq!(activation_rx.blocking_recv(), Some(()));
    }

    #[test]
    fn unlocked_metadata_is_replaced_by_the_next_owner() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("ochub.lock");
        let (lock, activation_server) =
            acquired(acquire_single_instance_at(path.clone()).expect("first instance"));
        drop(activation_server);
        drop(lock);

        let (_next_lock, _next_activation_server) =
            acquired(acquire_single_instance_at(path.clone()).expect("next instance"));
        let record: InstanceRecord =
            serde_json::from_str(&fs::read_to_string(path).expect("read replacement metadata"))
                .expect("parse replacement metadata");
        assert_eq!(record.protocol_version, INSTANCE_PROTOCOL_VERSION);
    }
}

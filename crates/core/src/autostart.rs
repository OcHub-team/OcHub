//! Login-item registration ("launch at startup").
//!
//! This lives in core because two front ends drive the same OS state: the
//! desktop settings toggle and the control API's `/auto-launch` endpoints. They
//! used to carry independent copies of the setup, and both copies had the same
//! macOS bug (see [`launch_target`]).
//!
//! Deliberately synchronous: every backend is a file write or a registry write,
//! and callers need the result before they can report success.

use std::path::{Path, PathBuf};

use auto_launch::{AutoLaunch, AutoLaunchBuilder};

use crate::error::AppError;

/// launchd `Label` / Windows `Run` value name / XDG desktop file stem.
const APP_NAME: &str = "OcHub";

/// Argument handed to the login item so the launched process can tell it was
/// started at login rather than by the user. Consumed by the desktop shell.
pub const SILENT_ARG: &str = "--silent";

/// The executable to register, plus the bundle it belongs to on macOS.
///
/// The path MUST be the executable itself, never the `.app` directory.
/// `auto-launch`'s default macOS mode is `LaunchAgent`, which writes the path
/// verbatim as `ProgramArguments[0]`; launchd cannot exec a directory, so
/// registering the bundle produces a login item that silently never runs —
/// and `is_enabled()` in that mode only stats the plist, so the UI would
/// cheerfully report it as enabled.
fn launch_target() -> Result<PathBuf, AppError> {
    std::env::current_exe()
        .map_err(|err| AppError::Message(format!("无法获取应用路径: {err}")))
        .and_then(|exe| {
            if exe.is_absolute() {
                Ok(exe)
            } else {
                std::fs::canonicalize(&exe)
                    .map_err(|err| AppError::Message(format!("无法解析应用路径: {err}")))
            }
        })
}

/// The enclosing `.app` bundle for an executable, if it sits inside one.
pub fn macos_app_bundle_path(exe_path: &Path) -> Option<PathBuf> {
    let path = exe_path.to_string_lossy();
    path.find(".app/Contents/MacOS/")
        .map(|pos| PathBuf::from(&path[..pos + 4]))
}

/// Read `CFBundleIdentifier` out of a bundle's `Info.plist`.
///
/// Only used to populate `AssociatedBundleIdentifiers`, which lets macOS 13+
/// show the item under the app's own name in System Settings › Login Items.
/// Purely cosmetic, so every failure degrades to `None` rather than erroring.
#[cfg(any(target_os = "macos", test))]
fn macos_bundle_identifier(bundle: &Path) -> Option<String> {
    let plist = std::fs::read_to_string(bundle.join("Contents/Info.plist")).ok()?;
    let key = plist.find("<key>CFBundleIdentifier</key>")?;
    let open = plist[key..].find("<string>")? + key + "<string>".len();
    let close = plist[open..].find("</string>")? + open;
    let id = plist[open..close].trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Why registration is impossible right now, or `None` when it is available.
///
/// On macOS an unbundled build is refused rather than registered: the login
/// item would pin a path under `target/`, which breaks on the next `cargo
/// clean` and leaves a dangling entry behind in the user's Login Items.
pub fn unsupported_reason() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let exe = launch_target().ok()?;
        if macos_app_bundle_path(&exe).is_none() {
            return Some("开发构建不支持开机启动（未打包为 .app）".to_string());
        }
    }
    None
}

/// Build a login-item handle for the running executable.
///
/// `silent` adds [`SILENT_ARG`] to the registered command line. All three
/// backends we use deliver it as real argv: macOS `LaunchAgent` appends to
/// `ProgramArguments`, Windows appends to the `Run` value, and XDG appends to
/// `Exec=`.
pub fn handle(silent: bool) -> Result<AutoLaunch, AppError> {
    if let Some(reason) = unsupported_reason() {
        return Err(AppError::Message(reason));
    }

    let exe = launch_target()?;
    let mut builder = AutoLaunchBuilder::new();
    builder
        .set_app_name(APP_NAME)
        .set_app_path(&exe.to_string_lossy());

    let args: &[&str] = if silent { &[SILENT_ARG] } else { &[] };
    builder.set_args(args);

    #[cfg(target_os = "macos")]
    {
        use auto_launch::MacOSLaunchMode;
        // Pin the mode rather than relying on the default: AppleScript costs an
        // Automation (TCC) prompt on every enable/disable/is_enabled, and
        // SMAppService needs macOS 13+ and a signed bundle.
        builder.set_macos_launch_mode(MacOSLaunchMode::LaunchAgent);
        if let Some(id) = macos_app_bundle_path(&exe)
            .as_deref()
            .and_then(macos_bundle_identifier)
        {
            builder.set_bundle_identifiers(&[id]);
        }
    }

    #[cfg(target_os = "windows")]
    {
        use auto_launch::WindowsEnableMode;
        // Per-user, so enabling never attempts an elevation prompt.
        builder.set_windows_enable_mode(WindowsEnableMode::CurrentUser);
    }

    #[cfg(target_os = "linux")]
    {
        use auto_launch::LinuxLaunchMode;
        // A plain file write; the systemd mode needs a live user bus.
        builder.set_linux_launch_mode(LinuxLaunchMode::XdgAutostart);
    }

    builder
        .build()
        .map_err(|err| AppError::Message(format!("创建开机启动配置失败: {err}")))
}

/// Whether the OS currently has a login item registered for this app.
pub fn is_enabled() -> Result<bool, AppError> {
    handle(false)?
        .is_enabled()
        .map_err(|err| AppError::Message(format!("读取开机启动状态失败: {err}")))
}

/// Register or remove the login item.
///
/// Enabling always rewrites the entry, so the recorded executable path and
/// `--silent` state follow the app rather than drifting after a move or an
/// upgrade — every backend truncates and rewrites on enable.
pub fn set_enabled(enabled: bool, silent: bool) -> Result<(), AppError> {
    let handle = handle(silent)?;
    if enabled {
        handle
            .enable()
            .map_err(|err| AppError::Message(format!("启用开机启动失败: {err}")))
    } else {
        handle
            .disable()
            .map_err(|err| AppError::Message(format!("关闭开机启动失败: {err}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_path_is_the_app_dir_not_the_executable() {
        let exe = Path::new("/Applications/OcHub.app/Contents/MacOS/ochub");
        assert_eq!(
            macos_app_bundle_path(exe),
            Some(PathBuf::from("/Applications/OcHub.app"))
        );
    }

    #[test]
    fn unbundled_executables_have_no_bundle_path() {
        assert_eq!(
            macos_app_bundle_path(Path::new("/repo/target/debug/ochub")),
            None
        );
    }

    #[test]
    fn bundle_identifier_is_read_from_info_plist() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("OcHub.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            r#"<plist version="1.0"><dict>
                 <key>CFBundleExecutable</key><string>ochub</string>
                 <key>CFBundleIdentifier</key><string>io.ochub.debug.qa</string>
               </dict></plist>"#,
        )
        .unwrap();
        assert_eq!(
            macos_bundle_identifier(&bundle).as_deref(),
            Some("io.ochub.debug.qa")
        );
    }

    #[test]
    fn missing_or_malformed_info_plist_degrades_to_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(macos_bundle_identifier(dir.path()), None);

        let bundle = dir.path().join("Broken.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(bundle.join("Contents/Info.plist"), "<plist></plist>").unwrap();
        assert_eq!(macos_bundle_identifier(&bundle), None);
    }
}

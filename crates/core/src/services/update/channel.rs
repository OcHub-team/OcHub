//! How this copy of OcHub was installed, and whether it can replace itself.
//!
//! The same binary ships through package formats with very different ownership
//! rules, and self-updating the wrong one corrupts a system the user did not
//! ask us to touch. A `.deb` install is owned by dpkg: overwriting those files
//! behind the package manager's back leaves the package database lying about
//! what is on disk, and the next `apt upgrade` silently reverts the update. A
//! Windows portable unzip has no installer to re-run. Both are detected here
//! and degrade to "open the release page" rather than being handled by a
//! best-effort install path.
//!
//! Detection is intentionally conservative: anything unrecognized becomes
//! [`InstallChannel::Unknown`], which cannot self-install.

use std::path::{Path, PathBuf};

/// The packaging this process is running from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallChannel {
    /// A macOS `.app` bundle. Replaced wholesale by swapping the directory.
    MacOsAppBundle,
    /// A Windows NSIS install. Updated by re-running the installer silently.
    WindowsNsis,
    /// A Windows portable unzip. No installer exists to re-run.
    WindowsPortable,
    /// A Linux AppImage. Updated by replacing the single file.
    LinuxAppImage,
    /// A distro package (`.deb`). Owned by the package manager.
    LinuxSystemPackage,
    /// A `cargo run` build, or a layout we do not recognize.
    Unknown,
}

impl InstallChannel {
    /// Whether this channel supports replacing itself in place.
    ///
    /// Signature configuration is checked separately by
    /// [`super::manifest::signing_configured`]; both must hold before an
    /// install is offered.
    pub fn supports_self_install(self) -> bool {
        matches!(
            self,
            Self::MacOsAppBundle | Self::WindowsNsis | Self::LinuxAppImage
        )
    }

    /// A stable identifier for user interfaces and serialized output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MacOsAppBundle => "macos-app",
            Self::WindowsNsis => "windows-nsis",
            Self::WindowsPortable => "windows-portable",
            Self::LinuxAppImage => "linux-appimage",
            Self::LinuxSystemPackage => "linux-system-package",
            Self::Unknown => "unknown",
        }
    }
}

/// Detect the channel of the running process.
pub fn detect() -> InstallChannel {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            log::warn!("[Update] cannot resolve current exe, update install disabled: {error}");
            return InstallChannel::Unknown;
        }
    };
    detect_from(&exe, |name| std::env::var(name).ok())
}

/// The testable core of [`detect`], with the filesystem and environment
/// injected so every branch can be exercised on any host.
fn detect_from(exe: &Path, env: impl Fn(&str) -> Option<String>) -> InstallChannel {
    if cfg!(target_os = "macos") {
        return if is_inside_app_bundle(exe) {
            InstallChannel::MacOsAppBundle
        } else {
            InstallChannel::Unknown
        };
    }

    if cfg!(target_os = "windows") {
        // Mirrors the existing `portable_mode` control-API probe, which reports
        // a portable install by the marker file next to the executable.
        let portable = exe
            .parent()
            .map(|dir| dir.join("portable.ini").is_file())
            .unwrap_or(false);
        return if portable {
            InstallChannel::WindowsPortable
        } else {
            InstallChannel::WindowsNsis
        };
    }

    // The AppImage runtime exports the path of the .AppImage file itself;
    // without it we are not running from one, whatever the exe path looks like.
    if let Some(appimage) = env("APPIMAGE").filter(|value| !value.trim().is_empty()) {
        if Path::new(&appimage).is_file() {
            return InstallChannel::LinuxAppImage;
        }
        log::warn!("[Update] APPIMAGE is set to a missing path: {appimage}");
    }
    if exe.starts_with("/usr") || exe.starts_with("/opt") {
        return InstallChannel::LinuxSystemPackage;
    }
    InstallChannel::Unknown
}

/// The `.app` directory containing this executable, if any.
///
/// A bundled macOS binary lives at `Foo.app/Contents/MacOS/foo`, so the bundle
/// root is three levels up. Anything else — a bare `cargo run` binary, or a
/// loose copy of the executable — is not a bundle.
pub fn macos_app_bundle_path(exe: &Path) -> Option<PathBuf> {
    let bundle = exe.parent()?.parent()?.parent()?;
    let is_bundle = bundle.extension().is_some_and(|ext| ext == "app")
        && exe.parent().is_some_and(|dir| dir.ends_with("MacOS"));
    is_bundle.then(|| bundle.to_path_buf())
}

fn is_inside_app_bundle(exe: &Path) -> bool {
    macos_app_bundle_path(exe).is_some()
}

/// The AppImage file backing this process, if any.
pub fn appimage_path() -> Option<PathBuf> {
    std::env::var("APPIMAGE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn app_bundle_root_is_three_levels_above_the_executable() {
        let exe = Path::new("/Applications/OcHub.app/Contents/MacOS/ochub");
        assert_eq!(
            macos_app_bundle_path(exe),
            Some(PathBuf::from("/Applications/OcHub.app"))
        );
    }

    #[test]
    fn a_bare_binary_is_not_a_bundle() {
        // `cargo run` must never be mistaken for something replaceable.
        assert_eq!(
            macos_app_bundle_path(Path::new("/Users/x/OcHub/target/debug/ochub")),
            None
        );
    }

    #[test]
    fn a_binary_beside_a_bundle_is_not_a_bundle() {
        assert_eq!(
            macos_app_bundle_path(Path::new(
                "/Applications/OcHub.app/Contents/Resources/ochub"
            )),
            None
        );
    }

    #[test]
    fn only_replaceable_channels_advertise_self_install() {
        assert!(InstallChannel::MacOsAppBundle.supports_self_install());
        assert!(InstallChannel::WindowsNsis.supports_self_install());
        assert!(InstallChannel::LinuxAppImage.supports_self_install());
        // The three that would damage a system or silently no-op.
        assert!(!InstallChannel::LinuxSystemPackage.supports_self_install());
        assert!(!InstallChannel::WindowsPortable.supports_self_install());
        assert!(!InstallChannel::Unknown.supports_self_install());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_deb_install_is_detected_as_package_managed() {
        assert_eq!(
            detect_from(Path::new("/usr/bin/ochub"), no_env),
            InstallChannel::LinuxSystemPackage
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn appimage_detection_requires_the_file_to_exist() {
        // A stale APPIMAGE pointing at a deleted file must not be treated as
        // replaceable, or the update would write a new file nobody launches.
        let missing =
            |name: &str| (name == "APPIMAGE").then(|| "/nonexistent/OcHub.AppImage".to_string());
        assert_eq!(
            detect_from(Path::new("/home/x/OcHub.AppImage"), missing),
            InstallChannel::Unknown
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_appimage_path_is_detected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_string_lossy().to_string();
        let env = move |name: &str| (name == "APPIMAGE").then(|| path.clone());
        assert_eq!(
            detect_from(Path::new("/tmp/.mount_xxx/usr/bin/ochub"), env),
            InstallChannel::LinuxAppImage
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_bundle_is_detected() {
        assert_eq!(
            detect_from(
                Path::new("/Applications/OcHub.app/Contents/MacOS/ochub"),
                no_env
            ),
            InstallChannel::MacOsAppBundle
        );
        assert_eq!(
            detect_from(Path::new("/Users/x/OcHub/target/debug/ochub"), no_env),
            InstallChannel::Unknown
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn a_windows_install_without_a_portable_marker_is_nsis() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("ochub.exe");
        assert_eq!(detect_from(&exe, no_env), InstallChannel::WindowsNsis);

        std::fs::write(dir.path().join("portable.ini"), "").unwrap();
        assert_eq!(detect_from(&exe, no_env), InstallChannel::WindowsPortable);
    }
}

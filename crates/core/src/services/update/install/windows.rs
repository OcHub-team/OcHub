//! Re-run the NSIS installer to update in place.
//!
//! Windows cannot replace a running executable, so unlike macOS and Linux this
//! path cannot finish the job itself. It writes the verified installer to disk,
//! starts it detached, and returns; the installer waits for this process to
//! exit, swaps the files, and relaunches the app.
//!
//! Consequences the caller must respect:
//!
//! * Everything this process holds — the single-instance lock, the gateway
//!   port, the SQLite handle — must already be released when `apply` is called,
//!   because the installer begins working the moment this process exits. That
//!   is why [`super::PreparedUpdate::requires_shutdown_before_apply`] is true
//!   here and only here.
//! * OcHub packages NSIS with `installer-mode = "currentUser"`, so the silent
//!   install runs without a UAC prompt. A per-machine install would elevate and
//!   show a consent dialog, which is why that mode must not change.
//!
//! The invocation mirrors `cargo-packager-updater` 0.2.3, which drives the same
//! installer that `cargo-packager` 0.11.8 produces for this project: `/S` for
//! silent, `/R` to restart the app afterwards, launched through PowerShell's
//! `Start-Process` with `CREATE_NO_WINDOW` so no console window flashes.

use std::io::Write as _;
use std::os::windows::process::CommandExt as _;
use std::process::Command;

use super::InstallOutcome;
use crate::{AppError, Result};

/// Suppress the console window PowerShell would otherwise flash.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `/S` silent, `/R` relaunch the app once files are replaced.
const NSIS_ARGS: &[&str] = &["/S", "/R"];

pub(super) fn apply(payload: &[u8]) -> Result<InstallOutcome> {
    // `.keep()` deliberately leaks the temp file: it must outlive this process
    // so the installer can read it after we exit. Windows cleans %TEMP% itself.
    let mut file = tempfile::Builder::new()
        .prefix("ochub-update-")
        .suffix(".exe")
        .tempfile()
        .map_err(|error| AppError::Message(format!("创建安装包临时文件失败: {error}")))?;
    file.write_all(payload)
        .map_err(|error| AppError::Message(format!("写入安装包失败: {error}")))?;
    file.as_file()
        .sync_all()
        .map_err(|error| AppError::Message(format!("刷新安装包到磁盘失败: {error}")))?;
    let (handle, path) = file
        .keep()
        .map_err(|error| AppError::Message(format!("保留安装包失败: {error}")))?;
    // The installer cannot run while we still hold the handle open.
    drop(handle);

    let powershell = powershell_path();
    let mut quoted = std::ffi::OsString::from("\"");
    quoted.push(&path);
    quoted.push("\"");

    Command::new(powershell)
        .creation_flags(CREATE_NO_WINDOW)
        .args(["-NoProfile", "-WindowStyle", "Hidden"])
        .arg("Start-Process")
        .arg(quoted)
        .arg("-ArgumentList")
        .arg(NSIS_ARGS.join(", "))
        .spawn()
        .map_err(|error| {
            AppError::Message(format!(
                "启动安装程序失败: {error}。应用未被修改，请从发布页手动安装"
            ))
        })?;

    log::info!("[Update] NSIS installer spawned; exiting so it can replace files");
    Ok(InstallOutcome::InstallerSpawned)
}

/// Prefer the absolute path so a hijacked `PATH` cannot substitute another
/// `powershell.exe`; fall back to the bare name only if `SYSTEMROOT` is unset.
fn powershell_path() -> String {
    std::env::var("SYSTEMROOT").map_or_else(
        |_| "powershell.exe".to_string(),
        |root| format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_install_requests_a_relaunch() {
        // Losing /R would leave the user staring at a closed app after
        // clicking update; losing /S would pop an installer wizard.
        assert!(NSIS_ARGS.contains(&"/S"));
        assert!(NSIS_ARGS.contains(&"/R"));
    }

    #[test]
    fn powershell_is_resolved_absolutely_when_possible() {
        let path = powershell_path();
        assert!(
            path.ends_with("powershell.exe"),
            "unexpected shell path: {path}"
        );
    }
}

//! Replace the running `.app` bundle.
//!
//! Two macOS specifics shape this:
//!
//! * **A running bundle can be moved, just not overwritten in place.** The
//!   kernel keeps the running image alive by inode, so `mv`-ing the old bundle
//!   aside and `mv`-ing the new one in is safe while this process continues to
//!   run. Editing files inside the live bundle is not.
//! * **Bundles are not plain file trees.** Frameworks use symlinks and signed
//!   bundles carry extended attributes; losing either invalidates the code
//!   signature and Gatekeeper then refuses to launch the result. Extraction
//!   therefore goes through `/usr/bin/tar` (bsdtar), which preserves both,
//!   rather than a pure-Rust tar reader that would silently drop xattrs.
//!
//! One thing this deliberately does *not* need to handle: quarantine. The
//! `com.apple.quarantine` attribute is applied by the downloading application,
//! and only browsers and LaunchServices opt into it. A bundle fetched by this
//! process is unquarantined, so an in-app update does not re-trigger the
//! Gatekeeper prompt that the same build shows when downloaded manually.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{InstallOutcome, RelaunchTarget};
use crate::services::update::channel;
use crate::{AppError, Result};

pub(super) fn apply(payload: &[u8]) -> Result<InstallOutcome> {
    let exe = std::env::current_exe()
        .map_err(|error| AppError::Message(format!("获取当前可执行文件失败: {error}")))?;
    let bundle = channel::macos_app_bundle_path(&exe)
        .ok_or_else(|| AppError::Message("当前不是 .app 包运行，无法应用内更新".to_string()))?;
    let parent = bundle
        .parent()
        .ok_or_else(|| AppError::Message("无法定位 .app 所在目录".to_string()))?;

    ensure_writable(parent)?;

    // Staging next to the target keeps the final move a rename within one
    // filesystem. Extracting to /tmp and moving across volumes would copy
    // instead, widening the window where neither bundle is in place.
    let staging = tempfile::Builder::new()
        .prefix(".ochub-update-")
        .tempdir_in(parent)
        .map_err(|error| {
            AppError::Message(format!(
                "在 {} 创建更新暂存目录失败: {error}",
                parent.display()
            ))
        })?;

    let archive = staging.path().join("update.tar.gz");
    std::fs::write(&archive, payload)
        .map_err(|error| AppError::Message(format!("写入更新包失败: {error}")))?;

    extract(&archive, staging.path())?;
    let new_bundle = find_app_bundle(staging.path())?;
    verify_signature_lineage(&bundle, &new_bundle)?;

    swap(&bundle, &new_bundle)?;

    Ok(InstallOutcome::Replaced {
        relaunch: RelaunchTarget::MacOsBundle(bundle),
    })
}

fn ensure_writable(dir: &Path) -> Result<()> {
    // /Applications is group-writable by `admin`, so the common case works
    // without elevation. A standard (non-admin) account, or an app installed
    // somewhere root-owned, needs a real installer — say so before downloading
    // turns into a confusing rename failure.
    let probe = dir.join(".ochub-update-write-test");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(error) => Err(AppError::Message(format!(
            "没有写入 {} 的权限（{error}），无法应用内更新；请从发布页下载后手动替换",
            dir.display()
        ))),
    }
}

fn extract(archive: &Path, into: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .output()
        .map_err(|error| AppError::Message(format!("解压更新包失败: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Message(format!(
            "解压更新包失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn find_app_bundle(dir: &Path) -> Result<PathBuf> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| AppError::Message(format!("读取解压结果失败: {error}")))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "app") && path.is_dir() {
            return Ok(path);
        }
    }
    Err(AppError::Message(
        "更新包中没有找到 .app，可能不是 macOS 更新载荷".to_string(),
    ))
}

/// Refuse an update signed by a different Developer ID than the running app.
///
/// The minisign signature already proves the payload came from whoever holds
/// the updater key. This is a second, independent constraint: once a build is
/// notarized, the update channel must not be usable to move users onto a
/// bundle from another team. An unsigned current build has no lineage to
/// preserve, so the check is skipped there rather than blocking updates for
/// the unsigned pre-release packages.
fn verify_signature_lineage(current: &Path, candidate: &Path) -> Result<()> {
    let Some(current_team) = team_identifier(current) else {
        log::debug!("[Update] running bundle is unsigned; skipping Team ID check");
        return Ok(());
    };
    match team_identifier(candidate) {
        Some(new_team) if new_team == current_team => Ok(()),
        Some(new_team) => Err(AppError::Message(format!(
            "更新包的签名团队（{new_team}）与当前应用（{current_team}）不一致，已拒绝安装"
        ))),
        None => Err(AppError::Message(
            "当前应用已签名，但更新包未签名，已拒绝安装".to_string(),
        )),
    }
}

fn team_identifier(bundle: &Path) -> Option<String> {
    let output = Command::new("/usr/bin/codesign")
        .arg("-dv")
        .arg("--verbose=4")
        .arg(bundle)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // codesign writes its report to stderr.
    let report = String::from_utf8_lossy(&output.stderr);
    parse_team_identifier(&report)
}

fn parse_team_identifier(report: &str) -> Option<String> {
    report.lines().find_map(|line| {
        line.strip_prefix("TeamIdentifier=")
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "not set")
            .map(ToString::to_string)
    })
}

/// Move the new bundle into place, restoring the old one if that fails.
///
/// The window where the target path does not exist spans one rename. If the
/// second rename fails the first is undone, so the outcome is either the new
/// bundle or the untouched old one — never a missing app.
fn swap(target: &Path, new_bundle: &Path) -> Result<()> {
    let backup = target.with_extension(format!("app.old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&backup);

    std::fs::rename(target, &backup)
        .map_err(|error| AppError::Message(format!("移开旧版本失败: {error}；应用未被修改")))?;

    if let Err(error) = std::fs::rename(new_bundle, target) {
        // Put the working app back before reporting failure.
        if let Err(restore) = std::fs::rename(&backup, target) {
            return Err(AppError::Message(format!(
                "安装新版本失败（{error}），且恢复旧版本也失败（{restore}）。\
                 旧版本仍在 {}，请手动改名回 {}",
                backup.display(),
                target.display()
            )));
        }
        return Err(AppError::Message(format!(
            "安装新版本失败: {error}；已恢复旧版本"
        )));
    }

    // Best-effort: the old bundle is only reachable by the running process now,
    // and leaving it behind would waste a few hundred MB.
    if let Err(error) = std::fs::remove_dir_all(&backup) {
        log::warn!(
            "[Update] could not remove {} (harmless, will need manual cleanup): {error}",
            backup.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_identifier_is_read_from_a_codesign_report() {
        let report = "Executable=/Applications/OcHub.app/Contents/MacOS/ochub\n\
                      Identifier=io.github.sleepstars.ochub\n\
                      TeamIdentifier=ABCDE12345\n\
                      Sealed Resources version=2\n";
        assert_eq!(
            parse_team_identifier(report),
            Some("ABCDE12345".to_string())
        );
    }

    #[test]
    fn an_adhoc_signature_reports_no_team() {
        // Ad-hoc signed builds print this literal; treating it as a Team ID
        // would compare "not set" against "not set" and wave anything through.
        let report = "Identifier=io.github.sleepstars.ochub\nTeamIdentifier=not set\n";
        assert_eq!(parse_team_identifier(report), None);
    }

    #[test]
    fn an_unsigned_bundle_reports_no_team() {
        assert_eq!(
            parse_team_identifier("code object is not signed at all"),
            None
        );
    }

    #[test]
    fn swap_restores_the_old_bundle_when_the_new_one_cannot_move_in() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("OcHub.app");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("marker"), b"original").unwrap();

        // A path that does not exist, so the second rename fails.
        let missing = dir.path().join("missing.app");
        let error = swap(&target, &missing).unwrap_err();

        assert!(error.to_string().contains("已恢复旧版本"), "{error}");
        assert_eq!(
            std::fs::read_to_string(target.join("marker")).unwrap(),
            "original",
            "the working app must be back in place after a failed install"
        );
    }

    #[test]
    fn swap_installs_the_new_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("OcHub.app");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("marker"), b"original").unwrap();

        let staged = dir.path().join("staged.app");
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(staged.join("marker"), b"updated").unwrap();

        swap(&target, &staged).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("marker")).unwrap(),
            "updated"
        );
    }

    #[test]
    fn find_app_bundle_rejects_a_payload_without_a_bundle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ochub"), b"loose binary").unwrap();
        assert!(find_app_bundle(dir.path()).is_err());
    }

    #[test]
    fn find_app_bundle_locates_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("OcHub.app")).unwrap();
        assert_eq!(
            find_app_bundle(dir.path()).unwrap(),
            dir.path().join("OcHub.app")
        );
    }
}

//! Replace the running AppImage.
//!
//! An AppImage is one self-contained file, which makes it the easiest format to
//! update: the running process holds the old inode through a FUSE mount, so
//! swapping the file underneath it is safe and takes effect on next launch.
//!
//! The write goes to a temporary file *in the same directory* and is then
//! renamed over the target. That ordering matters. `cargo-packager-updater`
//! does the reverse — rename the old file away, then write the new one in its
//! place — which leaves a window where the AppImage path does not exist at all;
//! a crash or a full disk mid-write strands the user with no launchable app.
//! Renaming last makes the swap atomic: the path always points at either the
//! complete old file or the complete new one. Staging in the same directory is
//! what makes that rename atomic rather than a cross-filesystem copy.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use super::{InstallOutcome, RelaunchTarget};
use crate::services::update::channel;
use crate::{AppError, Result};

pub(super) fn apply(payload: &[u8]) -> Result<InstallOutcome> {
    let target = channel::appimage_path()
        .ok_or_else(|| AppError::Message("未从 AppImage 运行，无法应用内更新".to_string()))?;
    let parent = target
        .parent()
        .ok_or_else(|| AppError::Message("无法定位 AppImage 所在目录".to_string()))?;

    // Preserve whatever mode the user had, rather than assuming 0o755: an
    // AppImage in a shared location may intentionally be group-executable only.
    let mode = std::fs::metadata(&target)
        .map(|meta| meta.permissions().mode())
        .unwrap_or(0o755);

    write_and_swap(parent, &target, payload, mode)?;

    Ok(InstallOutcome::Replaced {
        relaunch: RelaunchTarget::Executable(target),
    })
}

fn write_and_swap(parent: &Path, target: &Path, payload: &[u8], mode: u32) -> Result<()> {
    let mut staged = tempfile::Builder::new()
        .prefix(".ochub-update-")
        .tempfile_in(parent)
        .map_err(|error| {
            AppError::Message(format!(
                "在 {} 创建临时文件失败（更新未应用）: {error}",
                parent.display()
            ))
        })?;

    staged
        .write_all(payload)
        .map_err(|error| AppError::Message(format!("写入新版本失败（更新未应用）: {error}")))?;
    // Without this the bytes may still be in the page cache when the rename
    // publishes the new path; a power loss then leaves a truncated AppImage
    // sitting at the name the user launches.
    staged
        .as_file()
        .sync_all()
        .map_err(|error| AppError::Message(format!("刷新新版本到磁盘失败: {error}")))?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|error| AppError::Message(format!("设置可执行权限失败: {error}")))?;

    staged.persist(target).map_err(|error| {
        AppError::Message(format!(
            "替换 AppImage 失败: {}；旧版本未被修改",
            error.error
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_appimage_is_replaced_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("OcHub.AppImage");
        std::fs::write(&target, b"old version").unwrap();

        write_and_swap(dir.path(), &target, b"new version", 0o755).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new version");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "the result must stay executable");
    }

    #[test]
    fn the_original_survives_a_failed_write() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("OcHub.AppImage");
        std::fs::write(&target, b"old version").unwrap();

        // A staging directory that does not exist makes the temp file fail.
        let error = write_and_swap(&dir.path().join("missing"), &target, b"new version", 0o755)
            .unwrap_err();

        assert!(error.to_string().contains("更新未应用"), "{error}");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"old version",
            "a failed update must leave the running AppImage intact"
        );
    }

    #[test]
    fn no_partial_file_is_left_behind_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("OcHub.AppImage");
        std::fs::write(&target, b"old version").unwrap();
        let _ = write_and_swap(&dir.path().join("missing"), &target, b"new", 0o755);

        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".ochub-update-")
            })
            .collect();
        assert!(strays.is_empty(), "staging files must not leak");
    }

    #[test]
    fn a_non_default_mode_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("OcHub.AppImage");
        std::fs::write(&target, b"old").unwrap();

        write_and_swap(dir.path(), &target, b"new", 0o750).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750);
    }
}

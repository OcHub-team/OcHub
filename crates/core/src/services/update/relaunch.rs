//! Start the updated build after this process has fully exited.
//!
//! Relaunching by simply spawning the new binary races against our own
//! shutdown, and OcHub loses that race in a way users would notice: the new
//! process calls [`crate::services::update`]'s sibling startup path, finds the
//! single-instance lock still held by this dying process, decides it is a
//! second copy, tries to activate the first — and exits. The update would then
//! look like "the app closed and never came back".
//!
//! The same ordering problem applies to the gateway's listening port and the
//! SQLite handle. All three are released by the kernel when this process ends,
//! so rather than tearing each one down by hand and hoping the order is right,
//! the relaunch is handed to a detached `sh` that polls for our PID to
//! disappear and only then starts the new build. One mechanism covers every
//! resource, including any added later.
//!
//! Windows does not use this: there, the NSIS installer is given `/R` and does
//! the relaunch itself once it has replaced the files.

use std::path::Path;

use super::install::RelaunchTarget;
use crate::{AppError, Result};

/// Give up waiting after this long and start the new build anyway.
///
/// A process that has not exited in this window is wedged; launching over it is
/// still better than leaving the user with nothing running. The new instance's
/// own single-instance check remains the backstop against two live copies.
const MAX_WAIT_SECONDS: u32 = 30;

/// Poll interval for the exit watcher, in seconds (as `sleep` understands it).
const POLL_INTERVAL: &str = "0.2";

/// Spawn a detached watcher that launches `target` once this process exits.
///
/// Returns as soon as the watcher is running; the caller should then shut down
/// normally.
pub fn after_exit(target: &RelaunchTarget) -> Result<()> {
    let (program, argument) = match target {
        // `open` hands the bundle to LaunchServices, which is the supported way
        // to start a .app. Exec'ing the inner binary directly would skip the
        // bundle's Info.plist and leave the process without its activation
        // policy or icon.
        RelaunchTarget::MacOsBundle(bundle) => ("open", bundle.as_path()),
        RelaunchTarget::Executable(path) => {
            let path: &Path = path.as_path();
            ("exec", path)
        }
    };
    spawn_watcher(std::process::id(), program, argument)
}

#[cfg(unix)]
fn spawn_watcher(pid: u32, program: &str, argument: &Path) -> Result<()> {
    use std::process::{Command, Stdio};

    // Arguments are passed positionally rather than interpolated into the
    // script, so a path containing spaces or shell metacharacters cannot break
    // out of the command.
    let script = format!(
        r#"pid="$1"; program="$2"; target="$3"; waited=0
while kill -0 "$pid" 2>/dev/null; do
  sleep {POLL_INTERVAL}
  waited=$((waited + 1))
  if [ "$waited" -gt {ticks} ]; then break; fi
done
if [ "$program" = "open" ]; then
  exec open "$target"
else
  exec "$target"
fi"#,
        ticks = MAX_WAIT_SECONDS * 5,
    );

    Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("ochub-relaunch")
        .arg(pid.to_string())
        .arg(program)
        .arg(argument)
        // Detach from our streams so the watcher is not killed by, and does not
        // write into, a terminal that goes away with us.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|child| {
            log::info!(
                "[Update] relaunch watcher {} armed for {}",
                child.id(),
                argument.display()
            );
        })
        .map_err(|error| {
            AppError::Message(format!(
                "更新已安装，但自动重启失败: {error}。请手动启动 OcHub"
            ))
        })
}

#[cfg(not(unix))]
fn spawn_watcher(_pid: u32, _program: &str, _argument: &Path) -> Result<()> {
    // Only reachable if a non-Windows, non-unix platform ever returns
    // `InstallOutcome::Replaced`; Windows relaunches from the installer.
    Err(AppError::Message(
        "当前平台不支持自动重启，请手动启动 OcHub".to_string(),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The watcher must not act while the "app" is still alive, and must act
    /// promptly once it exits — the property the instance-lock race depends on.
    #[test]
    fn the_watcher_waits_for_the_process_to_exit() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("relaunched");

        // A stand-in for the app: sleeps, then exits.
        let mut app = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 1")
            .spawn()
            .unwrap();

        // `touch` the marker instead of launching anything real.
        spawn_watcher(app.id(), "exec", &touch_script(dir.path(), &marker)).unwrap();

        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !marker.exists(),
            "the watcher fired while the process was still running"
        );

        app.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(marker.exists(), "the watcher never fired after exit");
    }

    /// Paths with spaces must survive the trip through `sh`.
    #[test]
    fn a_path_with_spaces_is_not_word_split() {
        let dir = tempfile::tempdir().unwrap();
        let awkward = dir.path().join("Oc Hub Updates");
        std::fs::create_dir(&awkward).unwrap();
        let marker = awkward.join("relaunched");

        let mut app = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 0.2")
            .spawn()
            .unwrap();
        spawn_watcher(app.id(), "exec", &touch_script(&awkward, &marker)).unwrap();
        app.wait().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(marker.exists(), "quoting broke on a path containing spaces");
    }

    /// Write an executable script that creates `marker`, and return its path.
    fn touch_script(dir: &Path, marker: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let script = dir.join("relaunch-stub.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch \"{}\"\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }
}

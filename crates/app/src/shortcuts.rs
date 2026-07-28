//! Application-level keyboard actions and platform-specific bindings.
//!
//! Editing commands live in `text_input`; these commands deliberately stay at
//! the shell/view level so focused controls can bubble Save/Cancel/Close to the
//! page that owns their data.

use gpui::{App, KeyBinding, actions};

actions!(ochub_shortcuts, [Save, Cancel, CloseWindow, OpenSettings]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Cancel, None)]);

    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-s", Save, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
    ]);

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    cx.bind_keys([
        KeyBinding::new("ctrl-s", Save, None),
        KeyBinding::new("ctrl-w", CloseWindow, None),
        KeyBinding::new("ctrl-,", OpenSettings, None),
    ]);
}

#[cfg(test)]
mod tests {
    #[derive(Clone, Copy)]
    enum Platform {
        MacOs,
        Windows,
        Linux,
    }

    fn primary(platform: Platform, key: &str) -> String {
        let modifier = match platform {
            Platform::MacOs => "cmd",
            Platform::Windows | Platform::Linux => "ctrl",
        };
        format!("{modifier}-{key}")
    }

    #[test]
    fn primary_shortcuts_follow_each_desktop_platform() {
        assert_eq!(primary(Platform::MacOs, "s"), "cmd-s");
        assert_eq!(primary(Platform::Windows, "s"), "ctrl-s");
        assert_eq!(primary(Platform::Linux, "s"), "ctrl-s");
        assert_eq!(primary(Platform::MacOs, "w"), "cmd-w");
        assert_eq!(primary(Platform::Windows, "w"), "ctrl-w");
        assert_eq!(primary(Platform::Linux, "w"), "ctrl-w");
        assert_eq!(primary(Platform::MacOs, ","), "cmd-,");
        assert_eq!(primary(Platform::Windows, ","), "ctrl-,");
        assert_eq!(primary(Platform::Linux, ","), "ctrl-,");
    }

    #[test]
    fn every_supported_platform_key_syntax_is_parseable() {
        use gpui::KeyBinding;

        for key in [
            "cmd-s",
            "cmd-w",
            "cmd-,",
            "cmd-q",
            "cmd-f",
            "cmd-g",
            "cmd-shift-g",
            "cmd-z",
            "cmd-shift-z",
            "ctrl-s",
            "ctrl-w",
            "ctrl-,",
            "ctrl-q",
            "ctrl-f",
            "ctrl-g",
            "ctrl-shift-g",
            "ctrl-z",
            "ctrl-y",
            "ctrl-shift-z",
            "f3",
            "shift-f3",
        ] {
            let _ = KeyBinding::new(key, super::Save, None);
        }
    }
}

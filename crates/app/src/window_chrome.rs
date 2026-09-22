//! OcHub's client-side window chrome.
//!
//! Windows uses ordinary accessible buttons with explicit click handlers. The
//! native hit-test-only implementation exposed caption semantics to Windows
//! but left GPUI with no actionable controls, which made the three buttons
//! ambiguous to assistive technology and unreliable to click.

use std::rc::Rc;

use gpui::{AnyElement, App, Window};

pub type CloseWindowHandler = Rc<dyn Fn(&mut App)>;

#[allow(unused_variables)]
pub fn title_bar(
    window: &mut Window,
    cx: &mut App,
    close_window: CloseWindowHandler,
) -> Option<AnyElement> {
    #[cfg(target_os = "windows")]
    {
        use gpui::IntoElement as _;
        return Some(windows::title_bar(window, close_window).into_any_element());
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Keep Windows-only accessibility labels covered by the generated
        // catalog's dead-code check on other build hosts.
        let _ = (
            crate::i18n::k::SHELL_WINDOW_MINIMIZE,
            crate::i18n::k::SHELL_WINDOW_MAXIMIZE,
            crate::i18n::k::SHELL_WINDOW_RESTORE,
            crate::i18n::k::SHELL_WINDOW_CLOSE,
        );
        ochub_ui::window_chrome::title_bar(window, cx, close_window)
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use gpui::{Role, SharedString, Window, WindowControlArea, div, prelude::*, px};

    use super::CloseWindowHandler;
    use crate::i18n::{k, t};
    use crate::icons::{IconName, icon};
    use crate::theme;

    const BAR_HEIGHT: f32 = 32.;
    const BUTTON_WIDTH: f32 = 46.;

    pub(super) fn title_bar(
        window: &mut Window,
        close_window: CloseWindowHandler,
    ) -> impl IntoElement {
        let maximized = window.is_maximized();
        div()
            .id("window-title-bar")
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(BAR_HEIGHT))
            .flex_shrink_0()
            .bg(theme::sidebar_background())
            .child(
                div()
                    .id("window-title-bar-drag")
                    .flex_1()
                    .h_full()
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(caption_button(
                CaptionButton::Minimize,
                t(k::SHELL_WINDOW_MINIMIZE),
                close_window.clone(),
            ))
            .child(caption_button(
                if maximized {
                    CaptionButton::Restore
                } else {
                    CaptionButton::Maximize
                },
                if maximized {
                    t(k::SHELL_WINDOW_RESTORE)
                } else {
                    t(k::SHELL_WINDOW_MAXIMIZE)
                },
                close_window.clone(),
            ))
            .child(caption_button(
                CaptionButton::Close,
                t(k::SHELL_WINDOW_CLOSE),
                close_window,
            ))
    }

    #[derive(Clone, Copy)]
    enum CaptionButton {
        Minimize,
        Maximize,
        Restore,
        Close,
    }

    impl CaptionButton {
        fn id(self) -> &'static str {
            match self {
                Self::Minimize => "window-minimize",
                Self::Maximize => "window-maximize",
                Self::Restore => "window-restore",
                Self::Close => "window-close",
            }
        }

        fn icon(self) -> IconName {
            match self {
                Self::Minimize => IconName::WindowMinimize,
                Self::Maximize => IconName::WindowMaximize,
                Self::Restore => IconName::WindowRestore,
                Self::Close => IconName::Close,
            }
        }
    }

    fn caption_button(
        button: CaptionButton,
        label: SharedString,
        close_window: CloseWindowHandler,
    ) -> impl IntoElement {
        let (hover_background, hover_foreground) = match button {
            CaptionButton::Close => (theme::red(), theme::c(0xffffff)),
            _ => (theme::surface_hover(), theme::text()),
        };
        let group = SharedString::new_static(button.id());
        div()
            .id(button.id())
            .group(group.clone())
            .role(Role::Button)
            .aria_label(label)
            .flex()
            .items_center()
            .justify_center()
            .w(px(BUTTON_WIDTH))
            .h_full()
            .flex_shrink_0()
            .cursor_pointer()
            .hover(|style| style.bg(hover_background))
            .on_click(move |_event, window, cx| {
                cx.stop_propagation();
                match button {
                    CaptionButton::Minimize => window.minimize_window(),
                    CaptionButton::Maximize | CaptionButton::Restore => window.zoom_window(),
                    CaptionButton::Close => close_window(cx),
                }
            })
            .child(
                icon(button.icon(), theme::text(), 12.)
                    .group_hover(group, move |style| style.text_color(hover_foreground)),
            )
    }
}

//! OcHub's client-side window chrome.
//!
//! Windows uses ordinary accessible buttons with explicit click handlers. The
//! native hit-test-only implementation exposed caption semantics to Windows
//! but left GPUI with no actionable controls, which made the three buttons
//! ambiguous to assistive technology and unreliable to click.

use std::rc::Rc;

use gpui::{AnyElement, App, Window};

pub type CloseWindowHandler = Rc<dyn Fn(&mut App)>;

/// Linux client decorations have no native resize border. These hit areas
/// live on the very edge of the content surface and delegate resizing to the
/// compositor; native macOS, Windows and Linux server frames stay in charge
/// of their own edges.
#[allow(unused_variables)]
pub fn resize_frame(window: &Window) -> Option<AnyElement> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        return linux_frame::resize_frame(window);
    }
    #[allow(unreachable_code)]
    None
}

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

#[cfg(any(target_os = "windows", test))]
mod windows {
    use gpui::{Role, SharedString, Window, WindowControlArea, div, prelude::*, px};

    use super::CloseWindowHandler;
    use crate::i18n::{k, t};
    use crate::icons::{IconName, icon};
    use crate::theme;

    const BAR_HEIGHT: f32 = ochub_ui::window_chrome::TITLE_BAR_HEIGHT;
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
        ochub_ui::keyboard::interactive(div().id(button.id()))
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

    #[cfg(test)]
    mod tests {
        #[test]
        fn windows_caption_render_typechecks_on_every_ci_host() {
            let _render = super::title_bar;
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
mod linux_frame {
    use gpui::{
        AnyElement, CursorStyle, Decorations, MouseButton, ResizeEdge, Tiling, Window, div,
        prelude::*, px,
    };

    const EDGE_WIDTH: f32 = 4.;
    const CORNER_SIZE: f32 = 8.;

    fn resize_edges(tiling: Tiling) -> Vec<ResizeEdge> {
        use ResizeEdge::*;
        [
            (Top, !tiling.top),
            (Bottom, !tiling.bottom),
            (Left, !tiling.left),
            (Right, !tiling.right),
            (TopLeft, !tiling.top && !tiling.left),
            (TopRight, !tiling.top && !tiling.right),
            (BottomLeft, !tiling.bottom && !tiling.left),
            (BottomRight, !tiling.bottom && !tiling.right),
        ]
        .into_iter()
        .filter_map(|(edge, available)| available.then_some(edge))
        .collect()
    }

    pub(super) fn resize_frame(window: &Window) -> Option<AnyElement> {
        let Decorations::Client { tiling } = window.window_decorations() else {
            return None;
        };
        if window.is_maximized() || window.is_fullscreen() {
            return None;
        }

        Some(
            div()
                .id("linux-window-frame")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .border_color(crate::theme::border())
                .when(!tiling.top, |frame| frame.border_t_1())
                .when(!tiling.bottom, |frame| frame.border_b_1())
                .when(!tiling.left, |frame| frame.border_l_1())
                .when(!tiling.right, |frame| frame.border_r_1())
                .when(window.is_resizable(), |frame| {
                    frame.children(resize_edges(tiling).into_iter().map(resize_handle))
                })
                .into_any_element(),
        )
    }

    fn resize_handle(edge: ResizeEdge) -> impl IntoElement {
        use ResizeEdge::*;
        let (id, cursor) = match edge {
            Top => ("window-resize-top", CursorStyle::ResizeUpDown),
            Bottom => ("window-resize-bottom", CursorStyle::ResizeUpDown),
            Left => ("window-resize-left", CursorStyle::ResizeLeftRight),
            Right => ("window-resize-right", CursorStyle::ResizeLeftRight),
            TopLeft => ("window-resize-top-left", CursorStyle::ResizeUpLeftDownRight),
            TopRight => (
                "window-resize-top-right",
                CursorStyle::ResizeUpRightDownLeft,
            ),
            BottomLeft => (
                "window-resize-bottom-left",
                CursorStyle::ResizeUpRightDownLeft,
            ),
            BottomRight => (
                "window-resize-bottom-right",
                CursorStyle::ResizeUpLeftDownRight,
            ),
        };
        div()
            .id(id)
            .absolute()
            .occlude()
            .cursor(cursor)
            .map(|handle| match edge {
                Top => handle.top_0().left_0().right_0().h(px(EDGE_WIDTH)),
                Bottom => handle.bottom_0().left_0().right_0().h(px(EDGE_WIDTH)),
                Left => handle.left_0().top_0().bottom_0().w(px(EDGE_WIDTH)),
                Right => handle.right_0().top_0().bottom_0().w(px(EDGE_WIDTH)),
                TopLeft => handle.top_0().left_0().size(px(CORNER_SIZE)),
                TopRight => handle.top_0().right_0().size(px(CORNER_SIZE)),
                BottomLeft => handle.bottom_0().left_0().size(px(CORNER_SIZE)),
                BottomRight => handle.bottom_0().right_0().size(px(CORNER_SIZE)),
            })
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                window.start_window_resize(edge);
            })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn client_resize_preserves_compositor_owned_tiled_edges() {
            assert_eq!(resize_edges(Tiling::default()).len(), 8);
            assert!(resize_edges(Tiling::tiled()).is_empty());
            assert_eq!(
                resize_edges(Tiling {
                    top: true,
                    left: true,
                    ..Tiling::default()
                }),
                [
                    ResizeEdge::Bottom,
                    ResizeEdge::Right,
                    ResizeEdge::BottomRight
                ],
            );
            // Keep the Linux-only render branch type-checked in macOS tests.
            let _render: fn(&Window) -> Option<AnyElement> = resize_frame;
        }
    }
}

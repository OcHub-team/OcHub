//! Shared always-visible vertical scrollbar for tracked GPUI scroll containers.

use std::time::{Duration, Instant};

use gpui::{
    App, BorderStyle, Bounds, Context, Corners, Edges, ElementId, Hsla, IntoElement, ListState,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, RenderOnce,
    ScrollHandle, ScrollWheelEvent, Task, Window, canvas, div, point, prelude::*, px, quad, size,
};

use crate::anim::Transition;
use crate::theme;

const TRACK_WIDTH: Pixels = px(10.);
const THUMB_WIDTH: Pixels = px(6.);
const IDLE_THUMB_WIDTH: Pixels = px(2.);
const TRACK_INSET: Pixels = px(4.);
const MIN_THUMB_HEIGHT: Pixels = px(28.);
const SCROLL_ACTIVE_DURATION: Duration = Duration::from_millis(650);
/// How long the bar takes to wake or settle between its idle hairline and its
/// full width.
const ACTIVITY_TRANSITION: Duration = Duration::from_millis(160);

pub trait ScrollableHandle: Clone + 'static {
    fn max_offset(&self) -> Point<Pixels>;
    fn offset(&self) -> Point<Pixels>;
    fn viewport(&self) -> Bounds<Pixels>;
    fn set_offset(&self, offset: Point<Pixels>);
    fn drag_started(&self) {}
    fn drag_ended(&self) {}
}

pub fn contain_vertical_scroll<T: ScrollableHandle>(
    handle: T,
) -> impl Fn(&ScrollWheelEvent, &mut Window, &mut App) + 'static {
    move |_event, _window, cx| {
        if handle.max_offset().y > px(0.) {
            cx.stop_propagation();
        }
    }
}

impl ScrollableHandle for ScrollHandle {
    fn max_offset(&self) -> Point<Pixels> {
        self.max_offset()
    }

    fn offset(&self) -> Point<Pixels> {
        self.offset()
    }

    fn viewport(&self) -> Bounds<Pixels> {
        self.bounds()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.set_offset(offset);
    }
}

impl ScrollableHandle for ListState {
    fn max_offset(&self) -> Point<Pixels> {
        self.max_offset_for_scrollbar()
    }

    fn offset(&self) -> Point<Pixels> {
        self.scroll_px_offset_for_scrollbar()
    }

    fn viewport(&self) -> Bounds<Pixels> {
        self.viewport_bounds()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.set_offset_from_scrollbar(offset);
    }

    fn drag_started(&self) {
        self.scrollbar_drag_started();
    }

    fn drag_ended(&self) {
        self.scrollbar_drag_ended();
    }
}

#[derive(Clone, Copy, Debug)]
struct VerticalScrollbarGeometry {
    track_bounds: Bounds<Pixels>,
    thumb_bounds: Bounds<Pixels>,
    max_scroll: Pixels,
}

fn vertical_scrollbar_geometry(
    viewport: Bounds<Pixels>,
    track_right: Pixels,
    offset: Pixels,
    max_scroll: Pixels,
) -> Option<VerticalScrollbarGeometry> {
    if viewport.size.height <= px(0.) || max_scroll <= px(0.) {
        return None;
    }

    let track_height = (viewport.size.height - TRACK_INSET * 2.).max(px(0.));
    if track_height <= px(0.) {
        return None;
    }

    let viewport_height = viewport.size.height;
    let content_height = viewport_height + max_scroll;
    let visible_fraction = f32::from(viewport_height) / f32::from(content_height);
    let thumb_height = px((f32::from(track_height) * visible_fraction)
        .max(f32::from(MIN_THUMB_HEIGHT))
        .min(f32::from(track_height)));
    let travel = (track_height - thumb_height).max(px(0.));
    let progress =
        (f32::from((-offset).clamp(px(0.), max_scroll)) / f32::from(max_scroll)).clamp(0., 1.);
    let thumb_top = TRACK_INSET + travel * progress;
    let track_left = track_right - TRACK_WIDTH;
    let thumb_left = track_right - (TRACK_WIDTH + THUMB_WIDTH) / 2.;

    Some(VerticalScrollbarGeometry {
        track_bounds: Bounds::new(
            point(track_left, viewport.top() + TRACK_INSET),
            size(TRACK_WIDTH, track_height),
        ),
        thumb_bounds: Bounds::new(
            point(thumb_left, viewport.top() + thumb_top),
            size(THUMB_WIDTH, thumb_height),
        ),
        max_scroll,
    })
}

fn scroll_amount_for_thumb_top(scrollbar: &VerticalScrollbarGeometry, thumb_top: Pixels) -> Pixels {
    let travel = scrollbar.track_bounds.size.height - scrollbar.thumb_bounds.size.height;
    if travel <= px(0.) {
        return px(0.);
    }
    let position = thumb_top - scrollbar.track_bounds.top();
    scrollbar.max_scroll * (f32::from(position) / f32::from(travel)).clamp(0., 1.)
}

#[derive(IntoElement)]
pub struct VerticalScrollbar<T: ScrollableHandle> {
    id: ElementId,
    handle: T,
}

impl<T: ScrollableHandle> VerticalScrollbar<T> {
    pub fn new(id: impl Into<ElementId>, handle: T) -> Self {
        Self {
            id: id.into(),
            handle,
        }
    }
}

impl<T: ScrollableHandle> RenderOnce for VerticalScrollbar<T> {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let handle = self.handle.clone();
        let state = window.use_keyed_state(self.id, cx, move |_window, _cx| {
            VerticalScrollbarState::new(handle)
        });
        state.update(cx, |state, _cx| state.handle = self.handle);
        state
    }
}

struct VerticalScrollbarState<T: ScrollableHandle> {
    handle: T,
    drag_offset: Option<Pixels>,
    hovered: bool,
    last_offset: Pixels,
    scrolling: bool,
    idle_task: Option<Task<()>>,
    /// `0` idle, `1` active. Reversible, because the bar can go quiet and be
    /// woken again before it has finished settling.
    activity: Transition,
}

impl<T: ScrollableHandle> VerticalScrollbarState<T> {
    fn new(handle: T) -> Self {
        let last_offset = handle.offset().y;
        Self {
            handle,
            drag_offset: None,
            hovered: false,
            last_offset,
            scrolling: false,
            idle_task: None,
            activity: Transition::settled(0., ACTIVITY_TRANSITION),
        }
    }

    fn geometry(&self) -> Option<VerticalScrollbarGeometry> {
        let viewport = self.handle.viewport();
        vertical_scrollbar_geometry(
            viewport,
            viewport.right(),
            self.handle.offset().y,
            self.handle.max_offset().y,
        )
    }

    fn drag_to(&mut self, pointer_y: Pixels, window: &mut Window) {
        let (Some(scrollbar), Some(drag_offset)) = (self.geometry(), self.drag_offset) else {
            return;
        };
        let scroll = scroll_amount_for_thumb_top(&scrollbar, pointer_y - drag_offset);
        let current = self.handle.offset();
        let next_y = -scroll;
        // Pointer devices can emit several events for the same physical pixel.
        // Avoid invalidating the window until the scroll position has actually
        // crossed a paintable distance.
        if f32::from((current.y - next_y).abs()) < 0.5 {
            return;
        }
        self.handle.set_offset(point(current.x, next_y));
        window.refresh();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scrollbar) = self
            .geometry()
            // The component's 12 px hitbox already owns the horizontal check.
            // Geometry gets its vertical range from the tracked viewport, while
            // painting may anchor the rail to a wider parent (for centered page
            // content), so do not compare the pointer against the viewport's x.
            .filter(|scrollbar| {
                event.position.y >= scrollbar.track_bounds.top()
                    && event.position.y <= scrollbar.track_bounds.bottom()
            })
        else {
            return;
        };

        self.handle.drag_started();
        self.drag_offset = Some(
            if event.position.y >= scrollbar.thumb_bounds.top()
                && event.position.y <= scrollbar.thumb_bounds.bottom()
            {
                event.position.y - scrollbar.thumb_bounds.top()
            } else {
                scrollbar.thumb_bounds.size.height * 0.5
            },
        );
        self.drag_to(event.position.y, window);
        cx.stop_propagation();
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_offset.is_some() && event.pressed_button == Some(MouseButton::Left) {
            self.drag_to(event.position.y, window);
            cx.stop_propagation();
        }
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag_offset.take().is_some() {
            self.handle.drag_ended();
            cx.stop_propagation();
            cx.notify();
        }
    }
}

impl<T: ScrollableHandle> Render for VerticalScrollbarState<T> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let offset = self.handle.offset().y;
        if offset != self.last_offset {
            self.last_offset = offset;
            self.scrolling = true;
            self.idle_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SCROLL_ACTIVE_DURATION).await;
                this.update(cx, |this, cx| {
                    this.scrolling = false;
                    cx.notify();
                })
                .ok();
            }));
        }

        let handle = self.handle.clone();
        let active = self.hovered || self.drag_offset.is_some() || self.scrolling;
        // The bar used to snap between its two shapes, which made a quiet
        // scroll ending feel like something had blinked out of the page.
        let now = Instant::now();
        let reduce_motion = cx.reduce_motion();
        self.activity
            .retarget(if active { 1. } else { 0. }, now, reduce_motion);
        if self.activity.is_animating(now, reduce_motion) {
            window.request_animation_frame();
        }
        let activity = self.activity.value(now, reduce_motion);
        div()
            .id(("vertical-scrollbar", cx.entity_id().as_u64()))
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(12.))
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if this.hovered != *hovered {
                    this.hovered = *hovered;
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(
                canvas(
                    |_bounds, _window, _cx| (),
                    move |bounds, _state, window, _cx| {
                        let Some(scrollbar) = vertical_scrollbar_geometry(
                            handle.viewport(),
                            bounds.right(),
                            handle.offset().y,
                            handle.max_offset().y,
                        ) else {
                            return;
                        };
                        if activity > 0. {
                            window.paint_quad(quad(
                                scrollbar.track_bounds,
                                Corners::all(px(5.)),
                                theme::border().alpha(0.18 * activity),
                                Edges::default(),
                                Hsla::transparent_black(),
                                BorderStyle::Solid,
                            ));
                        }

                        // The thumb widens about its own centre line, so it
                        // grows in place rather than drifting toward the edge.
                        let width = IDLE_THUMB_WIDTH + (THUMB_WIDTH - IDLE_THUMB_WIDTH) * activity;
                        let thumb_bounds = Bounds::new(
                            point(
                                scrollbar.thumb_bounds.left() + (THUMB_WIDTH - width) / 2.,
                                scrollbar.thumb_bounds.top(),
                            ),
                            size(width, scrollbar.thumb_bounds.size.height),
                        );
                        window.paint_quad(quad(
                            thumb_bounds,
                            Corners::all(px(3.)),
                            theme::muted().alpha(0.46 + 0.26 * activity),
                            Edges::default(),
                            Hsla::transparent_black(),
                            BorderStyle::Solid,
                        ));
                    },
                )
                .size_full(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_maps_full_scroll_range() {
        let viewport = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(400.)));
        let top =
            vertical_scrollbar_geometry(viewport, viewport.right(), px(0.), px(1_600.)).unwrap();
        let bottom =
            vertical_scrollbar_geometry(viewport, viewport.right(), px(-1_600.), px(1_600.))
                .unwrap();
        assert_eq!(top.thumb_bounds.top(), top.track_bounds.top());
        assert_eq!(bottom.thumb_bounds.bottom(), bottom.track_bounds.bottom());
    }

    #[test]
    fn scrollbar_is_hidden_when_content_fits() {
        let viewport = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(400.)));
        assert!(vertical_scrollbar_geometry(viewport, viewport.right(), px(0.), px(0.)).is_none());
    }

    #[test]
    fn scrollbar_can_anchor_beyond_a_centered_viewport() {
        let viewport = Bounds::new(point(px(100.), px(20.)), size(px(800.), px(400.)));
        let page_right = px(1_200.);
        let geometry =
            vertical_scrollbar_geometry(viewport, page_right, px(0.), px(1_600.)).unwrap();

        assert_eq!(geometry.track_bounds.right(), page_right);
        assert_eq!(geometry.thumb_bounds.right(), page_right - px(2.));
        assert_eq!(geometry.track_bounds.top(), viewport.top() + TRACK_INSET);
    }
}

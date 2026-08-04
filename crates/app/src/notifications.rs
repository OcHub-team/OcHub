//! App-level notification center and toast host.
//!
//! This is intentionally UI-owned: core services return success/warnings/errors,
//! while the shell decides whether a result should be silent, inline, or global.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt, App, Bounds, ClipboardItem, Context, FontWeight, IntoElement,
    RenderOnce, Rgba, SharedString, Window, canvas, div, fill, point, prelude::*, px, size,
};

use crate::anim::Transition;
use crate::icons::{IconName, icon};
use crate::theme;

const MAX_VISIBLE: usize = 3;
const MAX_HISTORY: usize = 64;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const ERROR_TIMEOUT: Duration = Duration::from_secs(5);
const STACK_LAYER_OFFSET: f32 = 6.;
const STACK_LAYER_INSET: f32 = 5.;
const TOAST_WIDTH: f32 = 380.;
/// Vertical breathing room between toasts once the stack stands open.
const TOAST_GAP: f32 = 8.;
const STACK_TRANSITION: Duration = Duration::from_millis(220);
const TOAST_ENTRY: Duration = Duration::from_millis(240);
/// How far a new toast travels in from its anchored right edge.
const TOAST_ENTRY_SLIDE: f32 = 24.;
/// Height a toast is assumed to want for the single frame between joining a
/// stack and being measured. Roughly a title-only card, which is the shortest a
/// toast gets — erring low keeps the deck from visibly settling downward.
const ASSUMED_TOAST_HEIGHT: f32 = 56.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl NotificationLevel {
    fn colors(self) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba, IconName) {
        match self {
            Self::Info => (
                theme::accent_soft(),
                theme::accent(),
                theme::text(),
                IconName::Message,
            ),
            Self::Success => (
                theme::green_soft(),
                theme::green(),
                theme::text(),
                IconName::Check,
            ),
            Self::Warning => (
                theme::yellow_soft(),
                theme::yellow(),
                theme::text(),
                IconName::Diamond,
            ),
            Self::Error => (
                theme::red_soft(),
                theme::red(),
                theme::text(),
                IconName::Close,
            ),
        }
    }

    fn default_timeout(self) -> Duration {
        match self {
            Self::Error => ERROR_TIMEOUT,
            _ => DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Clone)]
pub struct Notification {
    id: u64,
    level: NotificationLevel,
    title: SharedString,
    message: Option<SharedString>,
    source: Option<SharedString>,
    auto_dismiss_after: Option<Duration>,
    remaining: Option<Duration>,
    /// `None` means the countdown is held: either the host is hovered, or the
    /// toast has not been given a start instant yet. `remaining` then stands on
    /// its own as the amount of time still owed.
    countdown_started_at: Option<Instant>,
    timer_epoch: u64,
}

impl Notification {
    /// Invalidates any in-flight dismiss timer for this toast, so a pause that
    /// races with an about-to-fire dismissal still wins.
    fn invalidate_timer(&mut self) -> u64 {
        self.timer_epoch = self.timer_epoch.wrapping_add(1);
        self.timer_epoch
    }

    /// Freezes the countdown, folding time already served into `remaining`.
    /// Idempotent: holding an already-held toast leaves it untouched.
    fn hold(&mut self, now: Instant) {
        if let (Some(remaining), Some(started_at)) =
            (self.remaining.as_mut(), self.countdown_started_at.take())
        {
            *remaining = remaining.saturating_sub(now.saturating_duration_since(started_at));
        }
    }

    /// Restarts the countdown, returning the duration still owed. `None` means
    /// the toast has nothing left to serve and is due for dismissal.
    fn resume(&mut self, now: Instant) -> Option<Duration> {
        let remaining = self.remaining.filter(|remaining| !remaining.is_zero())?;
        self.countdown_started_at = Some(now);
        Some(remaining)
    }
}

pub struct NotificationRequest {
    level: NotificationLevel,
    title: SharedString,
    message: Option<SharedString>,
    source: Option<SharedString>,
    persistent: bool,
    timeout: Option<Duration>,
}

impl NotificationRequest {
    pub fn new(level: NotificationLevel, title: impl Into<SharedString>) -> Self {
        Self {
            level,
            title: title.into(),
            message: None,
            source: None,
            persistent: false,
            timeout: None,
        }
    }

    pub fn message(mut self, message: impl Into<SharedString>) -> Self {
        self.message = Some(message.into());
        self
    }

    #[allow(dead_code)]
    pub fn source(mut self, source: impl Into<SharedString>) -> Self {
        self.source = Some(source.into());
        self
    }

    #[allow(dead_code)]
    pub fn persistent(mut self, persistent: bool) -> Self {
        self.persistent = persistent;
        self
    }

    #[allow(dead_code)]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

pub struct NotificationHost {
    next_id: u64,
    visible: VecDeque<Notification>,
    history: VecDeque<Notification>,
    /// Pointer presence over the host. Drives both the countdown hold and, for
    /// a multi-toast stack, whether it stands expanded.
    hovered: bool,
    /// `0` is the collapsed deck, `1` the open stack. Reversible mid-flight, so
    /// a pointer that leaves halfway retreats from where it was rather than
    /// finishing the trip first.
    stack: Transition,
    /// Natural height of each toast, keyed by id, captured the first time it is
    /// laid out. A toast always debuts at the front of the stack where nothing
    /// constrains its height, so this records the real content height even
    /// though later frames may squeeze it into the deck.
    ///
    /// Shared with the measuring canvas, which runs during prepaint — after
    /// `render` has already handed back its element tree.
    heights: Rc<RefCell<HashMap<u64, f32>>>,
}

/// A fixed-size canvas keeps the countdown animation out of layout. The wider,
/// translucent layer is the glow; the one-pixel layer is the crisp progress line.
///
/// The rail redraws itself once per display frame while the countdown is
/// running. Driving it from a background timer instead would sample the
/// countdown at a rate unrelated to the refresh cadence, so each repaint would
/// advance the line by an uneven distance and the motion would read as stutter.
#[derive(IntoElement)]
struct ToastProgress {
    accent: Rgba,
    duration: Duration,
    remaining: Duration,
    started_at: Option<Instant>,
}

impl ToastProgress {
    fn new(
        accent: Rgba,
        duration: Duration,
        remaining: Duration,
        started_at: Option<Instant>,
    ) -> Self {
        Self {
            accent,
            duration,
            remaining,
            started_at,
        }
    }
}

impl RenderOnce for ToastProgress {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let accent = self.accent;
        let duration = self.duration;
        let remaining = self.remaining;
        let started_at = self.started_at;

        canvas(
            move |_bounds, _window, _cx| (),
            move |bounds, _prepaint, window, cx| {
                let x = f32::from(bounds.origin.x);
                let y = f32::from(bounds.origin.y);
                let width = f32::from(bounds.size.width).max(0.);
                let reduce_motion = cx.reduce_motion();
                let progress = if reduce_motion {
                    1.
                } else {
                    remaining_duration_fraction(duration, remaining, started_at)
                };

                // `started_at` is cleared while the toast is hovered, which holds the
                // rail at its paused width; there is nothing to animate until the
                // pointer leaves and the host re-renders us with a fresh instant.
                let is_running = !reduce_motion && started_at.is_some() && progress > 0.;
                if is_running {
                    window.request_animation_frame();
                }

                // A faint full-width rail remains visible when reduced motion is enabled.
                window.paint_quad(fill(
                    Bounds::new(point(px(x), px(y + 1.)), size(px(width), px(1.))),
                    accent.alpha(0.13),
                ));

                let active_width = width * progress;
                if active_width <= 0. {
                    return;
                }

                window.paint_quad(
                    fill(
                        Bounds::new(point(px(x), px(y)), size(px(active_width), px(3.))),
                        accent.alpha(0.22),
                    )
                    .corner_radii(px(1.5)),
                );
                window.paint_quad(
                    fill(
                        Bounds::new(point(px(x), px(y + 1.)), size(px(active_width), px(1.))),
                        accent.alpha(0.96),
                    )
                    .corner_radii(px(0.5)),
                );
            },
        )
        .w_full()
        .h_full()
    }
}

fn remaining_fraction(duration: Duration, elapsed: Duration) -> f32 {
    if duration.is_zero() {
        return 0.;
    }

    (duration.saturating_sub(elapsed).as_secs_f32() / duration.as_secs_f32()).clamp(0., 1.)
}

fn remaining_duration_fraction(
    duration: Duration,
    remaining: Duration,
    started_at: Option<Instant>,
) -> f32 {
    let elapsed = started_at.map_or(Duration::ZERO, |started_at| started_at.elapsed());
    remaining_fraction(
        duration,
        duration.saturating_sub(remaining.saturating_sub(elapsed)),
    )
}

/// Where one toast sits at a given point in the collapse/expand transition.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ToastSlot {
    top: f32,
    /// Horizontal inset on both sides. Deeper cards sit narrower so the deck
    /// reads as depth rather than as a misaligned column.
    inset: f32,
    height: f32,
    /// Fade for the card's contents. The card chrome stays put so a collapsed
    /// neighbour still reads as a card edge rather than vanishing.
    content_opacity: f32,
}

/// Interpolates the stack layout between the collapsed deck (`progress` 0) and
/// the open column (`progress` 1).
///
/// `heights` are natural content heights, front of the stack first. Collapsed,
/// every card is squeezed to the front card's height so the deck's lower edge
/// is even; open, each card takes its own height and they queue up with
/// [`TOAST_GAP`] between them.
fn stack_layout(heights: &[f32], progress: f32) -> Vec<ToastSlot> {
    let Some(&front_height) = heights.first() else {
        return Vec::new();
    };
    let progress = progress.clamp(0., 1.);

    let mut open_top = 0.;
    heights
        .iter()
        .enumerate()
        .map(|(depth, &height)| {
            let deck_top = depth as f32 * STACK_LAYER_OFFSET;
            let slot = ToastSlot {
                top: deck_top + (open_top - deck_top) * progress,
                inset: depth as f32 * STACK_LAYER_INSET * (1. - progress),
                height: front_height + (height - front_height) * progress,
                content_opacity: if depth == 0 { 1. } else { progress },
            };
            open_top += height + TOAST_GAP;
            slot
        })
        .collect()
}

/// Overall height the host reserves, so its hover target tracks what is drawn
/// instead of always claiming the fully expanded span.
fn stack_height(heights: &[f32], progress: f32) -> f32 {
    stack_layout(heights, progress)
        .iter()
        .map(|slot| slot.top + slot.height)
        .fold(0., f32::max)
}

fn auto_dismiss_timeout(
    level: NotificationLevel,
    persistent: bool,
    timeout: Option<Duration>,
) -> Option<Duration> {
    (!persistent).then(|| timeout.unwrap_or_else(|| level.default_timeout()))
}

impl NotificationHost {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            visible: VecDeque::new(),
            history: VecDeque::new(),
            hovered: false,
            stack: Transition::settled(0., STACK_TRANSITION),
            heights: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn info(&mut self, title: impl Into<SharedString>, cx: &mut Context<Self>) -> u64 {
        self.notify(NotificationRequest::new(NotificationLevel::Info, title), cx)
    }

    pub fn success(&mut self, title: impl Into<SharedString>, cx: &mut Context<Self>) -> u64 {
        self.notify(
            NotificationRequest::new(NotificationLevel::Success, title),
            cx,
        )
    }

    pub fn warning(
        &mut self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.notify(
            NotificationRequest::new(NotificationLevel::Warning, title).message(message),
            cx,
        )
    }

    pub fn error(
        &mut self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.notify(
            NotificationRequest::new(NotificationLevel::Error, title)
                .message(message)
                .persistent(false),
            cx,
        )
    }

    /// Status toast with an explicit level.
    ///
    /// `None` means the source did not classify the message and it is shown as
    /// [`NotificationLevel::Info`]. Severity used to be guessed from Chinese
    /// substrings, which mis-classified messages even in Chinese and could not
    /// survive translation at all; every view now states its own level.
    pub fn status_leveled(
        &mut self,
        level: Option<NotificationLevel>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let message = message.into();
        let level = level.unwrap_or(NotificationLevel::Info);
        self.notify(NotificationRequest::new(level, message), cx)
    }

    pub fn notify(&mut self, request: NotificationRequest, cx: &mut Context<Self>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let timeout = auto_dismiss_timeout(request.level, request.persistent, request.timeout);
        // A toast that lands while the user is already reading the stack starts
        // held, exactly as if it had been paused on arrival. Starting its clock
        // here would let it expire under a pointer that never left.
        let countdown_started_at = timeout.filter(|_| !self.hovered).map(|_| Instant::now());
        let notification = Notification {
            id,
            level: request.level,
            title: request.title,
            message: request.message,
            source: request.source,
            auto_dismiss_after: timeout,
            remaining: timeout,
            countdown_started_at,
            timer_epoch: 0,
        };

        self.visible.push_front(notification.clone());
        self.history.push_front(notification);
        while self.visible.len() > MAX_VISIBLE {
            self.visible.pop_back();
        }
        while self.history.len() > MAX_HISTORY {
            self.history.pop_back();
        }
        self.prune_heights();

        if let Some(timeout) = timeout
            && !self.hovered
        {
            Self::schedule_dismiss(id, timeout, 0, cx);
        }

        id
    }

    fn schedule_dismiss(id: u64, timeout: Duration, timer_epoch: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(timeout).await;
            this.update(cx, |this, cx| {
                let should_dismiss = !this.hovered
                    && this.visible.iter().any(|notification| {
                        notification.id == id && notification.timer_epoch == timer_epoch
                    });
                if should_dismiss {
                    this.dismiss(id);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Pointer presence over the host holds *every* visible countdown, not just
    /// the toast under the cursor.
    ///
    /// Pausing only the hovered toast let its neighbours keep expiring while the
    /// user was reading them, and the stack collapsed out from under the pointer
    /// as soon as it dropped to a single toast. Whether the stack is collapsed,
    /// expanded, or a lone toast, the host is one hover target and one clock.
    fn set_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.hovered == hovered {
            return;
        }
        self.hovered = hovered;

        let now = Instant::now();
        let mut resume = Vec::new();
        let mut expired = Vec::new();
        for notification in &mut self.visible {
            if notification.auto_dismiss_after.is_none() {
                continue;
            }

            let timer_epoch = notification.invalidate_timer();
            if hovered {
                notification.hold(now);
            } else {
                match notification.resume(now) {
                    Some(remaining) => resume.push((notification.id, remaining, timer_epoch)),
                    None => expired.push(notification.id),
                }
            }
        }

        for id in expired {
            self.dismiss(id);
        }
        for (id, remaining, timer_epoch) in resume {
            Self::schedule_dismiss(id, remaining, timer_epoch, cx);
        }
        cx.notify();
    }

    pub fn dismiss(&mut self, id: u64) {
        self.visible.retain(|item| item.id != id);
        self.prune_heights();
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.visible.clear();
        self.prune_heights();
    }

    /// Measurements only matter for toasts still on screen; history keeps no
    /// layout of its own.
    fn prune_heights(&mut self) {
        self.heights
            .borrow_mut()
            .retain(|id, _| self.visible.iter().any(|item| item.id == *id));
    }

    #[allow(dead_code)]
    pub fn history(&self) -> impl Iterator<Item = &Notification> {
        self.history.iter()
    }

    /// Records the natural height of a toast the first time it is laid out.
    ///
    /// The measurement runs in a canvas's prepaint, which is why it writes
    /// through a shared cell rather than through the entity: `render` has
    /// already returned by then, and the value is read on the next frame.
    fn measure_height(&self, id: u64) -> impl IntoElement + use<> {
        let heights = self.heights.clone();
        div().absolute().inset_0().child(
            canvas(
                move |bounds, _window, _cx| {
                    let height = f32::from(bounds.size.height);
                    if height > 0. {
                        heights.borrow_mut().entry(id).or_insert(height);
                    }
                },
                |_bounds, _prepaint, _window, _cx| (),
            )
            .size_full(),
        )
    }

    /// `fill_slot` stretches the card to its positioned wrapper, which is how
    /// the deck keeps an even lower edge. It must stay `false` until the toast
    /// has been measured, or [`measure_height`](Self::measure_height) would
    /// record the imposed height instead of the natural one.
    fn render_notification(
        &self,
        notification: Notification,
        content_opacity: f32,
        fill_slot: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = notification.id;
        let (bg, accent, fg, icon_name) = notification.level.colors();
        let element_id = SharedString::from(format!("notification-{id}"));
        let auto_dismiss_after = notification.auto_dismiss_after;
        let remaining = notification.remaining;
        let countdown_started_at = notification.countdown_started_at;
        let copy_text = [
            Some(notification.title.as_ref()),
            notification.message.as_deref(),
            notification.source.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");

        // A toast arrives rather than appears: it slides the last few pixels in
        // from the right edge it is anchored to while fading up. One-shot, so
        // `with_animation` is the right tool — and it parks the card in place
        // under reduced motion for free.
        let entry = Animation::new(TOAST_ENTRY).with_easing(crate::anim::ease_out_quint);

        div()
            .id(element_id)
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .when(fill_slot, |card| card.h_full())
            .rounded_lg()
            .overflow_hidden()
            .border_1()
            .border_color(accent.alpha(0.32))
            .bg(bg.alpha(0.96))
            .shadow(theme::shadow_popover())
            .child(self.measure_height(id))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_3()
                    .px_3()
                    .py_3()
                    .opacity(content_opacity)
                    .child(
                        div()
                            .mt_0p5()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(24.))
                            .h(px(24.))
                            .rounded_md()
                            .bg(accent.alpha(0.12))
                            .child(icon(icon_name, accent, 15.)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .text_color(fg)
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .line_height(px(18.))
                                    .child(notification.title),
                            )
                            .when_some(notification.message, |s, message| {
                                s.child(
                                    div()
                                        .text_color(theme::subtext())
                                        .text_xs()
                                        .line_height(px(17.))
                                        .child(message),
                                )
                            })
                            .when_some(notification.source, |s, source| {
                                s.child(
                                    div()
                                        .text_color(theme::muted())
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(source),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id(("notification-copy", id))
                                    .role(gpui::Role::Button)
                                    .aria_label("复制通知内容")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .w(px(22.))
                                    .h(px(22.))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(theme::muted())
                                    .hover(|s| s.bg(accent.alpha(0.12)).text_color(theme::text()))
                                    .child(icon(IconName::Copy, theme::muted(), 13.))
                                    .on_click(move |_event, _window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_text.clone(),
                                        ));
                                    }),
                            )
                            .child(
                                div()
                                    .id(("notification-close", id))
                                    .role(gpui::Role::Button)
                                    .aria_label("关闭通知")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .w(px(22.))
                                    .h(px(22.))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(theme::muted())
                                    .hover(|s| s.bg(accent.alpha(0.12)).text_color(theme::text()))
                                    .child(icon(IconName::Close, theme::muted(), 13.))
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.dismiss(id);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when_some(auto_dismiss_after, |toast, timeout| {
                toast.child(
                    div()
                        .absolute()
                        .bottom(px(0.))
                        .left(px(0.))
                        .right(px(0.))
                        .h(px(3.))
                        .child(ToastProgress::new(
                            accent,
                            timeout,
                            remaining.unwrap_or(timeout),
                            countdown_started_at,
                        )),
                )
            })
            .with_animation(("notification-entry", id), entry, |card, delta| {
                card.opacity(delta)
                    .left(px((1. - delta) * TOAST_ENTRY_SLIDE))
            })
    }
}

/// A child view with legacy status state that is forwarded into the single
/// app-level toast host. Taking the value prevents an inline banner and avoids
/// emitting the same state again on unrelated redraws.
pub trait ToastSource {
    fn take_toast(&mut self) -> Option<SharedString>;
    /// Explicit level for the toast just taken. Default `None` keeps keyword
    /// inference; views with blocking/refusal toasts should set it explicitly.
    fn take_toast_level(&mut self) -> Option<NotificationLevel> {
        None
    }
}

/// Wires a view's `status` / `status_level` pair into the toast host.
macro_rules! impl_status_toasts_leveled {
    ($view:ty) => {
        impl $crate::notifications::ToastSource for $view {
            fn take_toast(&mut self) -> Option<gpui::SharedString> {
                self.status.take()
            }
            fn take_toast_level(&mut self) -> Option<$crate::notifications::NotificationLevel> {
                self.status_level.take()
            }
        }
    };
}

pub(crate) use impl_status_toasts_leveled;

impl Render for NotificationHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notifications = self.visible.iter().cloned().collect::<Vec<_>>();
        let is_stack = notifications.len() > 1;
        let reduce_motion = cx.reduce_motion();
        let now = Instant::now();

        self.stack.retarget(
            if is_stack && self.hovered { 1. } else { 0. },
            now,
            reduce_motion,
        );
        if self.stack.is_animating(now, reduce_motion) {
            window.request_animation_frame();
        }
        let progress = self.stack.value(now, reduce_motion);

        let host = div()
            .id("notification-host")
            .absolute()
            .top(px(56.))
            .right_4()
            .w(px(TOAST_WIDTH))
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                this.set_hovered(*hovered, cx);
            }));

        // A lone toast never needs positioning arithmetic, and plain flow is
        // also what gives a brand new toast the unconstrained layout its
        // measurement depends on.
        if !is_stack {
            return host.flex().flex_col().gap(px(TOAST_GAP)).children(
                notifications
                    .into_iter()
                    .map(|notification| self.render_notification(notification, 1., false, cx)),
            );
        }

        // A toast joining an existing stack has not been measured yet. It is
        // laid out at its natural height for this one frame — its slot still
        // reserves an estimate, so the deck around it holds its shape instead
        // of springing open and snapping back while the newcomer is sized.
        let measured = self.heights.borrow().clone();
        let heights = notifications
            .iter()
            .map(|notification| {
                measured
                    .get(&notification.id)
                    .copied()
                    .unwrap_or(ASSUMED_TOAST_HEIGHT)
            })
            .collect::<Vec<f32>>();

        // Keep the host itself absolute. Calling `relative()` on `host` here
        // overrides that positioning mode in GPUI, which makes a multi-toast
        // stack participate in AppRoot's column layout and steal an equally
        // tall strip from the bottom of the window. The inner canvas owns the
        // relative coordinate space needed by the absolutely positioned cards.
        let mut stack = div()
            .relative()
            .w_full()
            .h(px(stack_height(&heights, progress)));
        let slots = stack_layout(&heights, progress);
        // Back to front: the newest toast is painted last so it fronts the deck.
        for (notification, slot) in notifications.into_iter().zip(slots).rev() {
            // Stretching an unmeasured toast to its slot would have the canvas
            // record the height the deck imposed rather than the one its
            // content wants, and the estimate would then be self-fulfilling.
            let fill_slot = measured.contains_key(&notification.id);
            stack = stack.child(
                div()
                    .absolute()
                    .top(px(slot.top))
                    .left(px(slot.inset))
                    .right(px(slot.inset))
                    .h(px(slot.height))
                    .child(self.render_notification(
                        notification,
                        slot.content_opacity,
                        fill_slot,
                        cx,
                    )),
            );
        }
        host.child(stack)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        DEFAULT_TIMEOUT, ERROR_TIMEOUT, MAX_VISIBLE, Notification, NotificationLevel,
        STACK_LAYER_INSET, STACK_LAYER_OFFSET, TOAST_GAP, auto_dismiss_timeout, remaining_fraction,
        stack_height, stack_layout,
    };

    fn running_toast(timeout: Duration, started_at: Instant) -> Notification {
        Notification {
            id: 1,
            level: NotificationLevel::Info,
            title: "t".into(),
            message: None,
            source: None,
            auto_dismiss_after: Some(timeout),
            remaining: Some(timeout),
            countdown_started_at: Some(started_at),
            timer_epoch: 0,
        }
    }

    #[test]
    fn holding_folds_served_time_into_the_remaining_debt() {
        let start = Instant::now();
        let mut toast = running_toast(Duration::from_secs(3), start);

        toast.hold(start + Duration::from_secs(1));

        assert_eq!(toast.remaining, Some(Duration::from_secs(2)));
        assert_eq!(
            toast.countdown_started_at, None,
            "a held toast has no clock"
        );
    }

    #[test]
    fn holding_twice_does_not_double_charge_the_countdown() {
        let start = Instant::now();
        let mut toast = running_toast(Duration::from_secs(3), start);

        toast.hold(start + Duration::from_secs(1));
        toast.hold(start + Duration::from_secs(9));

        assert_eq!(toast.remaining, Some(Duration::from_secs(2)));
    }

    #[test]
    fn resuming_restarts_the_clock_and_owes_only_what_is_left() {
        let start = Instant::now();
        let mut toast = running_toast(Duration::from_secs(3), start);
        toast.hold(start + Duration::from_secs(1));

        let resumed_at = start + Duration::from_secs(60);
        let owed = toast.resume(resumed_at);

        assert_eq!(owed, Some(Duration::from_secs(2)));
        assert_eq!(
            toast.countdown_started_at,
            Some(resumed_at),
            "the hovered stretch must not count against the toast"
        );
    }

    #[test]
    fn a_toast_held_past_its_timeout_is_due_immediately_on_resume() {
        let start = Instant::now();
        let mut toast = running_toast(Duration::from_secs(3), start);

        toast.hold(start + Duration::from_secs(5));

        assert_eq!(toast.remaining, Some(Duration::ZERO));
        assert_eq!(
            toast.resume(start + Duration::from_secs(6)),
            None,
            "nothing left to serve means dismiss rather than restart"
        );
    }

    #[test]
    fn a_collapsed_deck_fans_cards_by_a_fixed_step() {
        let slots = stack_layout(&[100., 60., 140.], 0.);

        for (depth, slot) in slots.iter().enumerate() {
            assert_eq!(slot.top, depth as f32 * STACK_LAYER_OFFSET);
            assert_eq!(slot.inset, depth as f32 * STACK_LAYER_INSET);
            assert_eq!(
                slot.height, 100.,
                "the deck squeezes every card to the front card so its lower edge stays even"
            );
        }
        assert_eq!(slots[0].content_opacity, 1.);
        assert_eq!(
            (slots[1].content_opacity, slots[2].content_opacity),
            (0., 0.),
            "cards behind the front one read as bare plates"
        );
    }

    #[test]
    fn an_open_stack_queues_cards_at_their_own_height() {
        let slots = stack_layout(&[100., 60., 140.], 1.);

        assert_eq!(slots[0].top, 0.);
        assert_eq!(slots[1].top, 100. + TOAST_GAP);
        assert_eq!(slots[2].top, 100. + TOAST_GAP + 60. + TOAST_GAP);
        assert_eq!(
            slots.iter().map(|slot| slot.height).collect::<Vec<_>>(),
            vec![100., 60., 140.]
        );
        for slot in &slots {
            assert_eq!(slot.inset, 0., "an open stack is a flush column");
            assert_eq!(slot.content_opacity, 1.);
        }
    }

    #[test]
    fn the_transition_stays_between_its_two_end_states() {
        let heights = [100., 60., 140.];
        let collapsed = stack_layout(&heights, 0.);
        let open = stack_layout(&heights, 1.);
        let midway = stack_layout(&heights, 0.5);

        for depth in 0..heights.len() {
            let (from, to, at) = (collapsed[depth], open[depth], midway[depth]);
            assert!(
                at.top >= from.top && at.top <= to.top,
                "card {depth} left the span between its two resting places"
            );
            assert!(at.inset <= from.inset && at.inset >= to.inset);
        }
    }

    #[test]
    fn the_host_reserves_only_what_it_draws() {
        let heights = [100., 60., 140.];

        assert_eq!(
            stack_height(&heights, 0.),
            100. + 2. * STACK_LAYER_OFFSET,
            "collapsed, the deck is the front card plus the fanned edges"
        );
        assert_eq!(
            stack_height(&heights, 1.),
            100. + TOAST_GAP + 60. + TOAST_GAP + 140.
        );
        assert!(
            stack_height(&heights, 0.) < stack_height(&heights, 1.),
            "a collapsed deck must not claim the open stack's hover area"
        );
    }

    #[test]
    fn a_lone_toast_needs_no_deck_arithmetic() {
        let slots = stack_layout(&[120.], 0.);

        assert_eq!(slots.len(), 1);
        assert_eq!((slots[0].top, slots[0].inset), (0., 0.));
        assert_eq!(slots[0].height, 120.);
        assert_eq!(stack_height(&[120.], 0.), 120.);
    }

    #[test]
    fn an_empty_stack_has_no_layout() {
        assert!(stack_layout(&[], 0.).is_empty());
        assert_eq!(stack_height(&[], 0.), 0.);
    }

    #[test]
    fn invalidating_the_timer_orphans_the_in_flight_dismissal() {
        let start = Instant::now();
        let mut toast = running_toast(Duration::from_secs(3), start);
        let scheduled_epoch = toast.timer_epoch;

        let live_epoch = toast.invalidate_timer();

        assert_ne!(
            scheduled_epoch, live_epoch,
            "the epoch a pending dismiss captured must no longer match"
        );
    }

    #[test]
    fn resolves_auto_dismiss_duration_for_progress_bar() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(3));
        assert_eq!(ERROR_TIMEOUT, Duration::from_secs(5));
        assert_eq!(MAX_VISIBLE, 3);
        assert_eq!(
            auto_dismiss_timeout(NotificationLevel::Info, false, None),
            Some(DEFAULT_TIMEOUT)
        );
        assert_eq!(
            auto_dismiss_timeout(NotificationLevel::Error, false, None),
            Some(ERROR_TIMEOUT)
        );
        assert_eq!(
            auto_dismiss_timeout(
                NotificationLevel::Success,
                false,
                Some(Duration::from_millis(750))
            ),
            Some(Duration::from_millis(750))
        );
        assert_eq!(
            auto_dismiss_timeout(
                NotificationLevel::Warning,
                true,
                Some(Duration::from_secs(1))
            ),
            None
        );
    }

    #[test]
    fn calculates_progress_from_absolute_elapsed_time() {
        assert_eq!(
            remaining_fraction(Duration::from_secs(3), Duration::ZERO),
            1.
        );
        assert_eq!(
            remaining_fraction(Duration::from_secs(3), Duration::from_millis(1500)),
            0.5
        );
        assert_eq!(
            remaining_fraction(Duration::from_secs(3), Duration::from_secs(4)),
            0.
        );
        assert_eq!(remaining_fraction(Duration::ZERO, Duration::ZERO), 0.);
    }
}

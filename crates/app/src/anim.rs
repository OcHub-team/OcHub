//! Shared motion primitives.
//!
//! GPUI has no CSS-transition equivalent: style changes land whole, and
//! `AnimationExt::with_animation` only covers one-shot motion that starts when
//! an element first appears. Anything reversible — a disclosure the pointer can
//! leave halfway, a stack that collapses while it is still expanding — needs a
//! value the view owns and re-reads every frame. That is [`Transition`].
//!
//! Callers drive repaints with `Window::request_animation_frame` while
//! [`Transition::is_animating`] holds, which keeps the sampling locked to the
//! display refresh. A background timer would sample at a cadence unrelated to
//! the refresh rate and the motion would read as stutter.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Svg, Transformation, Window, point, px, radians,
};

/// Quintic ease-out: quick acknowledgement, then a quiet deceleration.
pub fn ease_out_quint(progress: f32) -> f32 {
    1. - (1. - progress.clamp(0., 1.)).powi(5)
}

/// Fraction of `duration` elapsed since `started`, clamped to `0..=1`.
///
/// Reduced motion reports a finished ramp so callers land on their end state
/// without special-casing every read site.
pub fn linear_progress(
    started: Instant,
    duration: Duration,
    now: Instant,
    reduce_motion: bool,
) -> f32 {
    if reduce_motion || duration.is_zero() {
        return 1.;
    }
    (now.saturating_duration_since(started).as_secs_f32() / duration.as_secs_f32()).clamp(0., 1.)
}

/// A scalar easing toward a target that can be re-aimed mid-flight.
///
/// [`retarget`](Self::retarget) rebases the new leg on the value currently on
/// screen rather than on the old endpoint, so reversing direction continues
/// from where the eye last saw the element instead of snapping to an extreme.
#[derive(Clone, Debug)]
pub struct Transition {
    from: f32,
    to: f32,
    started: Instant,
    /// Span of a full `0 → 1` sweep.
    full_duration: Duration,
    /// Span of the leg in flight. Scaled down from `full_duration` by the
    /// distance actually being covered, so a reversal a tenth of the way in
    /// snaps back promptly instead of crawling over the whole span.
    leg_duration: Duration,
}

impl Transition {
    /// A transition already at rest on `value`.
    pub fn settled(value: f32, full_duration: Duration) -> Self {
        Self {
            from: value,
            to: value,
            started: Instant::now(),
            full_duration,
            leg_duration: Duration::ZERO,
        }
    }

    /// The eased value at `now`.
    pub fn value(&self, now: Instant, reduce_motion: bool) -> f32 {
        if reduce_motion || self.leg_duration.is_zero() {
            return self.to;
        }
        let progress = linear_progress(self.started, self.leg_duration, now, reduce_motion);
        self.from + (self.to - self.from) * ease_out_quint(progress)
    }

    /// Whether `value` will still change on a later frame.
    pub fn is_animating(&self, now: Instant, reduce_motion: bool) -> bool {
        if reduce_motion || self.leg_duration.is_zero() {
            return false;
        }
        now.saturating_duration_since(self.started) < self.leg_duration
    }

    /// Re-aims at `to`, starting the new leg from the value on screen at `now`.
    ///
    /// Re-aiming at the target already in flight is a no-op, so a hover event
    /// repeated on every frame cannot restart the clock and stall the motion.
    pub fn retarget(&mut self, to: f32, now: Instant, reduce_motion: bool) {
        if (self.to - to).abs() <= f32::EPSILON {
            return;
        }

        let from = self.value(now, reduce_motion);
        self.from = from;
        self.to = to;
        self.started = now;
        self.leg_duration = if reduce_motion {
            Duration::ZERO
        } else {
            self.full_duration.mul_f32((to - from).abs())
        };
    }
}

/// An icon that turns to a new angle whenever the angle it is given changes.
///
/// `with_animation` cannot express this: its clock starts when the element
/// first appears, so every icon on a freshly drawn page would spin itself into
/// position, and a one-shot ramp cannot turn back when the state flips again.
/// Holding a [`Transition`] in element state instead means the first frame is
/// settled — silent on arrival, and moving only in response to a real change.
///
/// Rotation is the one transform GPUI offers and it applies to sprites alone,
/// which is why this takes an [`Svg`] rather than an arbitrary element.
pub struct RotateTo {
    id: ElementId,
    radians: f32,
    duration: Duration,
    icon: Option<Svg>,
}

impl RotateTo {
    pub fn new(id: impl Into<ElementId>, radians: f32, duration: Duration, icon: Svg) -> Self {
        Self {
            id: id.into(),
            radians,
            duration,
            icon: Some(icon),
        }
    }
}

struct RotateState {
    angle: Transition,
}

impl IntoElement for RotateTo {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RotateTo {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let target = self.radians;
        let duration = self.duration;
        let icon = self.icon.take().expect("request_layout runs once");

        window.with_element_state(
            global_id.expect("RotateTo reports an id"),
            |state: Option<RotateState>, window| {
                let now = Instant::now();
                let reduce_motion = cx.reduce_motion();
                // Debuting already at the target is what keeps a page of
                // disclosures from all turning at once when it is first drawn.
                let mut state = state.unwrap_or_else(|| RotateState {
                    angle: Transition::settled(target, duration),
                });

                state.angle.retarget(target, now, reduce_motion);
                if state.angle.is_animating(now, reduce_motion) {
                    window.request_animation_frame();
                }

                let mut element = icon
                    .with_transformation(Transformation::rotate(radians(
                        state.angle.value(now, reduce_motion),
                    )))
                    .into_any_element();
                ((element.request_layout(window, cx), element), state)
            },
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
    }
}

/// Moves a subtree during prepaint, after layout has completed. Unlike
/// `relative().top(...)`, this does not make Taffy recompute the subtree's
/// layout on every animation frame, while hitboxes and clipping still follow it.
pub struct PaintOffsetY {
    offset: Pixels,
    child: AnyElement,
}

impl PaintOffsetY {
    pub fn new(offset: Pixels, child: impl IntoElement) -> Self {
        Self {
            offset,
            child: child.into_any_element(),
        }
    }
}

impl IntoElement for PaintOffsetY {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PaintOffsetY {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_element_offset(point(px(0.), self.offset), |window| {
            self.child.prepaint(window, cx);
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Transition, ease_out_quint, linear_progress};

    const SPAN: Duration = Duration::from_millis(200);

    #[test]
    fn easing_pins_both_ends_and_decelerates() {
        assert_eq!(ease_out_quint(0.), 0.);
        assert_eq!(ease_out_quint(1.), 1.);
        assert!(
            ease_out_quint(0.5) > 0.5,
            "an ease-out covers most of the distance early"
        );
        assert_eq!(ease_out_quint(2.), 1., "input past the end stays clamped");
    }

    #[test]
    fn reduced_motion_reports_a_finished_ramp() {
        let start = Instant::now();
        assert_eq!(linear_progress(start, SPAN, start, true), 1.);
    }

    #[test]
    fn a_settled_transition_never_animates() {
        let start = Instant::now();
        let transition = Transition::settled(0., SPAN);

        assert_eq!(transition.value(start, false), 0.);
        assert!(!transition.is_animating(start, false));
    }

    #[test]
    fn retargeting_moves_toward_the_new_end_over_the_span() {
        let start = Instant::now();
        let mut transition = Transition::settled(0., SPAN);
        transition.retarget(1., start, false);

        assert_eq!(transition.value(start, false), 0.);
        assert!(transition.is_animating(start, false));
        assert_eq!(transition.value(start + SPAN, false), 1.);
        assert!(!transition.is_animating(start + SPAN, false));
    }

    #[test]
    fn reversing_mid_flight_resumes_from_the_value_on_screen() {
        let start = Instant::now();
        let mut transition = Transition::settled(0., SPAN);
        transition.retarget(1., start, false);

        let midway = start + SPAN / 2;
        let seen = transition.value(midway, false);
        assert!(seen > 0. && seen < 1., "expected to catch it in flight");

        transition.retarget(0., midway, false);

        assert!(
            (transition.value(midway, false) - seen).abs() < 1e-4,
            "the reversal must not jump away from what was on screen"
        );
        assert_eq!(transition.value(midway + SPAN, false), 0.);
    }

    #[test]
    fn a_short_reversal_finishes_sooner_than_a_full_sweep() {
        let start = Instant::now();
        let mut transition = Transition::settled(0., SPAN);
        transition.retarget(1., start, false);

        // Barely under way, so the trip back covers almost no distance.
        let nudge = start + SPAN / 20;
        transition.retarget(0., nudge, false);

        assert!(
            !transition.is_animating(nudge + SPAN / 2, false),
            "a sliver of a reversal must not occupy the full span"
        );
    }

    #[test]
    fn retargeting_at_the_current_target_does_not_restart_the_clock() {
        let start = Instant::now();
        let mut transition = Transition::settled(0., SPAN);
        transition.retarget(1., start, false);

        let midway = start + SPAN / 2;
        let seen = transition.value(midway, false);
        // A hover that keeps reporting the same state every frame.
        transition.retarget(1., midway, false);

        assert_eq!(
            transition.value(midway, false),
            seen,
            "re-aiming at the live target must not stall the motion"
        );
        assert_eq!(transition.value(start + SPAN, false), 1.);
    }

    #[test]
    fn reduced_motion_lands_on_the_target_without_animating() {
        let start = Instant::now();
        let mut transition = Transition::settled(0., SPAN);

        transition.retarget(1., start, true);

        assert_eq!(transition.value(start, true), 1.);
        assert!(!transition.is_animating(start, true));
    }
}

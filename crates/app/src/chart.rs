//! Lightweight, dependency-free charts drawn straight onto a GPUI `canvas`.
//!
//! GPUI exposes the low-level vector stack — `PathBuilder` + `Window::paint_path` +
//! gradient `Background`s — so we draw real smoothed area/line charts here instead of
//! faking them with stacks of fixed-height `div`s. Each chart is a `RenderOnce`
//! element carrying a `progress` knob in `0..=1`; wrap it in `.with_animation(..)`
//! (see callers) to get a draw-in. The progress scales the curve up from the
//! baseline, so the chart "rises" into view.

use std::sync::Arc;

use gpui::{
    App, BorderStyle, Bounds, Hsla, IntoElement, PathBuilder, Pixels, RenderOnce, SharedString,
    Styled, TextRun, Window, canvas, fill, linear_color_stop, linear_gradient, outline, point, px,
    size,
};

use crate::theme;

const STANDARD_HANDLE_RATIO: f32 = 1. / 3.;
const EXTREMUM_HANDLE_RATIO: f32 = 0.42;

/// An animated area + line chart over an evenly-spaced series of values.
#[derive(IntoElement)]
pub struct AreaChart {
    values: Arc<[f32]>,
    /// Pre-formatted tooltip text per bucket (same order as `values`).
    /// Empty disables hover. The caller must re-render on mouse move over the
    /// chart (e.g. an `on_mouse_move` → `cx.notify()` wrapper) — the paint pass
    /// reads the live mouse position each frame.
    hover_labels: Arc<[SharedString]>,
    line: Hsla,
    fill_top: Hsla,
    fill_bottom: Hsla,
    height: f32,
    progress: f32,
}

impl AreaChart {
    pub fn new(values: impl Into<Arc<[f32]>>) -> Self {
        let mut chart = Self {
            values: values.into(),
            hover_labels: Arc::from([]),
            line: theme::accent().into(),
            fill_top: theme::accent().into(),
            fill_bottom: theme::accent().into(),
            height: 180.,
            progress: 1.,
        };
        chart.set_palette(theme::accent());
        chart
    }

    /// Enable hover: show `labels[i]` in a tooltip when the pointer is over
    /// bucket `i`, with a crosshair + marker dot on the curve.
    pub fn hover_labels(mut self, labels: impl Into<Arc<[SharedString]>>) -> Self {
        self.hover_labels = labels.into();
        self
    }

    fn set_palette(&mut self, color: gpui::Rgba) {
        self.line = color.into();
        self.fill_top = color.alpha(0.26).into();
        self.fill_bottom = color.alpha(0.02).into();
    }

    /// Recolor the line + gradient fill to a brand/semantic hue.
    #[allow(dead_code)]
    pub fn color(mut self, color: gpui::Rgba) -> Self {
        self.set_palette(color);
        self
    }

    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }

    /// Animation knob: `0.` flattens the curve onto the baseline, `1.` is full height.
    pub fn progress(mut self, progress: f32) -> Self {
        self.progress = progress.clamp(0., 1.);
        self
    }
}

impl RenderOnce for AreaChart {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let AreaChart {
            values,
            hover_labels,
            line,
            fill_top,
            fill_bottom,
            height,
            progress,
        } = self;

        let geometry_values = values.clone();
        canvas(
            move |bounds, _window, _cx| chart_geometry(bounds, &geometry_values),
            move |bounds, geometry, window, cx| {
                let Some(geometry) = geometry.as_ref() else {
                    return;
                };
                paint_area(geometry, line, fill_top, fill_bottom, progress, window);
                paint_hover(bounds, geometry, &hover_labels, line, progress, window, cx);
            },
        )
        .w_full()
        .h(px(height))
    }
}

/// The canvas-space projection shared by the area fill and the hover overlay.
struct ChartGeom {
    x0: f32,
    top: f32,
    width: f32,
    usable_h: f32,
    baseline: f32,
    pts: Vec<(f32, f32)>,
}

fn chart_geometry(bounds: Bounds<Pixels>, values: &[f32]) -> Option<ChartGeom> {
    if values.len() < 2 {
        return None;
    }

    let pad_v = 8.0_f32;
    let pad_h = 2.0_f32;
    let x0 = f32::from(bounds.origin.x) + pad_h;
    let top = f32::from(bounds.origin.y) + pad_v;
    let width = (f32::from(bounds.size.width) - pad_h * 2.0).max(1.0);
    let usable_h = (f32::from(bounds.size.height) - pad_v * 2.0).max(1.0);
    let baseline = top + usable_h;

    let max_v = values
        .iter()
        .copied()
        .fold(0.0_f32, f32::max)
        .max(f32::EPSILON);
    let n = values.len();

    // Project the fully drawn series once. The draw-in animation scales these
    // points while constructing paths, so hover and area painting share this
    // allocation instead of projecting the series twice per frame.
    let pts: Vec<(f32, f32)> = values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = x0 + width * (i as f32) / ((n - 1) as f32);
            let y = baseline - (v / max_v) * usable_h;
            (x, y)
        })
        .collect();

    Some(ChartGeom {
        x0,
        top,
        width,
        usable_h,
        baseline,
        pts,
    })
}

#[allow(clippy::too_many_arguments)]
fn paint_area(
    geometry: &ChartGeom,
    line: Hsla,
    fill_top: Hsla,
    fill_bottom: Hsla,
    progress: f32,
    window: &mut Window,
) {
    let ChartGeom {
        x0, baseline, pts, ..
    } = geometry;
    let n = pts.len();
    let scaled_y = |y: f32| baseline - (baseline - y) * progress;

    // Gradient area: curve along the top, drop to the baseline, close back to the start.
    let mut fill = PathBuilder::fill();
    fill.move_to(point(px(*x0), px(*baseline)));
    fill.line_to(point(px(pts[0].0), px(scaled_y(pts[0].1))));
    smooth_through_scaled(&mut fill, pts, *baseline, progress);
    fill.line_to(point(px(pts[n - 1].0), px(*baseline)));
    fill.close();
    if let Ok(path) = fill.build() {
        window.paint_path(
            path,
            linear_gradient(
                180.,
                linear_color_stop(fill_top, 0.0),
                linear_color_stop(fill_bottom, 1.0),
            ),
        );
    }

    // Stroked line on top of the fill.
    let mut stroke = PathBuilder::stroke(px(2.0));
    stroke.move_to(point(px(pts[0].0), px(scaled_y(pts[0].1))));
    smooth_through_scaled(&mut stroke, pts, *baseline, progress);
    if let Ok(path) = stroke.build() {
        window.paint_path(path, line);
    }
}

/// Append a smooth curve through `pts[1..]`, assuming the builder's current point is
/// already `pts[0]`. Monotone cubic Hermite interpolation keeps every sample on the
/// rendered line without introducing overshoot between adjacent values.
fn smooth_through_scaled(
    builder: &mut PathBuilder,
    pts: &[(f32, f32)],
    baseline: f32,
    progress: f32,
) {
    let n = pts.len();
    let y = |value: f32| baseline - (baseline - value) * progress;
    if n < 2 {
        return;
    }
    let tangents = monotone_tangents(pts);
    for i in 0..n - 1 {
        let (x1, y1) = pts[i];
        let (x2, y2) = pts[i + 1];
        let width = x2 - x1;
        let outgoing_width = width * handle_ratio(pts, i);
        let incoming_width = width * handle_ratio(pts, i + 1);
        let control_a = (x1 + outgoing_width, y1 + tangents[i] * outgoing_width);
        let control_b = (x2 - incoming_width, y2 - tangents[i + 1] * incoming_width);
        builder.cubic_bezier_to(
            point(px(x2), px(y(y2))),
            point(px(control_a.0), px(y(control_a.1))),
            point(px(control_b.0), px(y(control_b.1))),
        );
    }
}

/// Widen the horizontal handles around a true local peak or valley. The
/// tangent there is already zero, so this rounds the turn without changing the
/// sample value or allowing the curve to overshoot it.
fn handle_ratio(pts: &[(f32, f32)], index: usize) -> f32 {
    if index == 0 || index + 1 >= pts.len() {
        return STANDARD_HANDLE_RATIO;
    }

    let incoming = pts[index].1 - pts[index - 1].1;
    let outgoing = pts[index + 1].1 - pts[index].1;
    if incoming != 0. && outgoing != 0. && incoming.signum() != outgoing.signum() {
        EXTREMUM_HANDLE_RATIO
    } else {
        STANDARD_HANDLE_RATIO
    }
}

/// Calculate shape-preserving tangents for a series with strictly increasing x.
fn monotone_tangents(pts: &[(f32, f32)]) -> Vec<f32> {
    let n = pts.len();
    if n < 2 {
        return vec![0.; n];
    }

    let slopes = pts
        .windows(2)
        .map(|pair| {
            let width = pair[1].0 - pair[0].0;
            if width.abs() <= f32::EPSILON {
                0.
            } else {
                (pair[1].1 - pair[0].1) / width
            }
        })
        .collect::<Vec<_>>();
    let mut tangents = vec![0.; n];
    tangents[0] = slopes[0];
    tangents[n - 1] = slopes[n - 2];

    for i in 1..n - 1 {
        let previous = slopes[i - 1];
        let next = slopes[i];
        if previous == 0. || next == 0. || previous.signum() != next.signum() {
            tangents[i] = 0.;
            continue;
        }

        let previous_width = pts[i].0 - pts[i - 1].0;
        let next_width = pts[i + 1].0 - pts[i].0;
        let previous_weight = 2. * next_width + previous_width;
        let next_weight = next_width + 2. * previous_width;
        tangents[i] =
            (previous_weight + next_weight) / (previous_weight / previous + next_weight / next);
    }

    tangents
}

/// Crosshair + tooltip for the bucket nearest the pointer, painted above the
/// area/line. No-op when hover labels are absent or the pointer is outside the
/// chart. The overlay uses the same draw-in progress as the curve so the marker
/// remains attached during animation as well as after it completes.
fn paint_hover(
    bounds: Bounds<Pixels>,
    geom: &ChartGeom,
    labels: &[SharedString],
    line: Hsla,
    progress: f32,
    window: &mut Window,
    cx: &mut App,
) {
    if labels.is_empty() {
        return;
    }
    let mouse = window.mouse_position();
    if !bounds.contains(&mouse) {
        return;
    }
    let n = geom.pts.len();
    let rel = ((f32::from(mouse.x) - geom.x0) / geom.width).clamp(0.0, 1.0);
    let ix = (rel * (n - 1) as f32).round() as usize;
    let Some(&(dot_x, raw_dot_y)) = geom.pts.get(ix) else {
        return;
    };
    let dot_y = geom.baseline - (geom.baseline - raw_dot_y) * progress;
    let Some(label) = labels.get(ix) else {
        return;
    };

    // Crosshair through the hovered bucket, dot on the curve.
    window.paint_quad(fill(
        Bounds::new(
            point(px(dot_x - 0.5), px(geom.top)),
            size(px(1.), px(geom.usable_h)),
        ),
        line.opacity(0.35),
    ));
    let r = 3.5_f32;
    window.paint_quad(
        fill(
            Bounds::new(
                point(px(dot_x - r), px(dot_y - r)),
                size(px(2. * r), px(2. * r)),
            ),
            line,
        )
        .corner_radii(px(r)),
    );

    // Tooltip card; flips to the left of the crosshair near the right edge.
    let font_size = px(11.);
    let line_height = px(16.);
    let run = TextRun {
        len: label.len(),
        font: window.text_style().font(),
        color: theme::text().into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_line(label.clone(), font_size, &[run], None);
    let pad_x = 8.0_f32;
    let pad_y = 4.0_f32;
    let tip_w = f32::from(shaped.width) + pad_x * 2.;
    let tip_h = f32::from(line_height) + pad_y * 2.;
    let right_edge = f32::from(bounds.origin.x) + f32::from(bounds.size.width);
    let mut tip_x = dot_x + 10.;
    if tip_x + tip_w > right_edge - 2. {
        tip_x = dot_x - 10. - tip_w;
    }
    let tip_y = (dot_y - tip_h - 10.).max(f32::from(bounds.origin.y));
    let tip_bounds = Bounds::new(point(px(tip_x), px(tip_y)), size(px(tip_w), px(tip_h)));
    window.paint_quad(fill(tip_bounds, theme::overlay()).corner_radii(px(6.)));
    window
        .paint_quad(outline(tip_bounds, theme::border(), BorderStyle::Solid).corner_radii(px(6.)));
    let _ = shaped.paint(
        point(px(tip_x + pad_x), px(tip_y + pad_y)),
        line_height,
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::{EXTREMUM_HANDLE_RATIO, STANDARD_HANDLE_RATIO, handle_ratio, monotone_tangents};

    #[test]
    fn monotone_tangents_flatten_local_extrema() {
        let points = [(0., 10.), (1., 0.), (2., 8.), (3., 8.), (4., 12.)];

        let tangents = monotone_tangents(&points);

        assert_eq!(tangents, vec![-10., 0., 0., 0., 4.]);
    }

    #[test]
    fn monotone_tangents_preserve_a_steady_slope() {
        let points = [(0., 1.), (2., 5.), (5., 11.)];

        let tangents = monotone_tangents(&points);

        assert_eq!(tangents, vec![2., 2., 2.]);
    }

    #[test]
    fn local_extrema_get_wider_horizontal_handles() {
        let points = [(0., 10.), (1., 0.), (2., 8.), (3., 4.)];

        assert_eq!(handle_ratio(&points, 0), STANDARD_HANDLE_RATIO);
        assert_eq!(handle_ratio(&points, 1), EXTREMUM_HANDLE_RATIO);
        assert_eq!(handle_ratio(&points, 2), EXTREMUM_HANDLE_RATIO);
        assert_eq!(handle_ratio(&points, 3), STANDARD_HANDLE_RATIO);
    }

    #[test]
    fn monotone_and_flat_samples_keep_standard_handles() {
        let points = [(0., 10.), (1., 5.), (2., 5.), (3., 0.)];

        assert_eq!(handle_ratio(&points, 1), STANDARD_HANDLE_RATIO);
        assert_eq!(handle_ratio(&points, 2), STANDARD_HANDLE_RATIO);
    }
}

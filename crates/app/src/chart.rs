//! Lightweight, dependency-free charts drawn straight onto a GPUI `canvas`.
//!
//! GPUI exposes the low-level vector stack — `PathBuilder` + `Window::paint_path` +
//! gradient `Background`s — so we draw real smoothed area/line charts here instead of
//! faking them with stacks of fixed-height `div`s. Each chart is a `RenderOnce`
//! element carrying a `progress` knob in `0..=1`; wrap it in `.with_animation(..)`
//! (see callers) to get a draw-in. The progress scales the curve up from the
//! baseline, so the chart "rises" into view.

use gpui::{
    canvas, linear_color_stop, linear_gradient, point, px, App, Bounds, Hsla, IntoElement,
    PathBuilder, Pixels, RenderOnce, Styled, Window,
};

use crate::theme;

/// An animated area + line chart over an evenly-spaced series of values.
#[derive(IntoElement)]
pub struct AreaChart {
    values: Vec<f32>,
    line: Hsla,
    fill_top: Hsla,
    fill_bottom: Hsla,
    height: f32,
    progress: f32,
}

impl AreaChart {
    pub fn new(values: Vec<f32>) -> Self {
        let mut chart = Self {
            values,
            line: theme::accent().into(),
            fill_top: theme::accent().into(),
            fill_bottom: theme::accent().into(),
            height: 180.,
            progress: 1.,
        };
        chart.set_palette(theme::accent());
        chart
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
            line,
            fill_top,
            fill_bottom,
            height,
            progress,
        } = self;

        canvas(
            move |_bounds, _window, _cx| (),
            move |bounds, _prepaint, window, _cx| {
                paint_area(
                    bounds,
                    &values,
                    line,
                    fill_top,
                    fill_bottom,
                    progress,
                    window,
                );
            },
        )
        .w_full()
        .h(px(height))
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_area(
    bounds: Bounds<Pixels>,
    values: &[f32],
    line: Hsla,
    fill_top: Hsla,
    fill_bottom: Hsla,
    progress: f32,
    window: &mut Window,
) {
    if values.len() < 2 {
        return;
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

    // Project each value onto the canvas, scaling its height by the animation progress.
    let pts: Vec<(f32, f32)> = values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = x0 + width * (i as f32) / ((n - 1) as f32);
            let y = baseline - (v / max_v) * usable_h * progress;
            (x, y)
        })
        .collect();

    // Gradient area: curve along the top, drop to the baseline, close back to the start.
    let mut fill = PathBuilder::fill();
    fill.move_to(point(px(x0), px(baseline)));
    fill.line_to(point(px(pts[0].0), px(pts[0].1)));
    smooth_through(&mut fill, &pts);
    fill.line_to(point(px(pts[n - 1].0), px(baseline)));
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
    stroke.move_to(point(px(pts[0].0), px(pts[0].1)));
    smooth_through(&mut stroke, &pts);
    if let Ok(path) = stroke.build() {
        window.paint_path(path, line);
    }
}

/// Append a smooth curve through `pts[1..]`, assuming the builder's current point is
/// already `pts[0]`. Uses the quadratic-midpoint technique: each segment curves to the
/// midpoint of the next pair using the shared vertex as the control point, which keeps
/// the line continuous and smooth without overshoot.
fn smooth_through(builder: &mut PathBuilder, pts: &[(f32, f32)]) {
    let n = pts.len();
    if n < 2 {
        return;
    }
    if n == 2 {
        builder.line_to(point(px(pts[1].0), px(pts[1].1)));
        return;
    }
    for i in 1..n - 1 {
        let mid_x = (pts[i].0 + pts[i + 1].0) / 2.0;
        let mid_y = (pts[i].1 + pts[i + 1].1) / 2.0;
        builder.curve_to(
            point(px(mid_x), px(mid_y)),
            point(px(pts[i].0), px(pts[i].1)),
        );
    }
    builder.line_to(point(px(pts[n - 1].0), px(pts[n - 1].1)));
}
